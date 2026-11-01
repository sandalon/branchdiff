use crate::git;
use crate::worker::Msg;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use notify::event::{EventKind, RemoveKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often pending work is looked at. Nothing wakes the thread when none is.
const TICK: Duration = Duration::from_millis(50);
const IDLE: Duration = Duration::from_secs(3600);
/// Agents can write steadily, which would otherwise hold back a report for good.
const MAX_WAIT: Duration = Duration::from_secs(2);

/// The files git rewrites when a worktree's state changes: staging, moving to a
/// branch, and every commit or reset, which may leave the other two untouched.
const GIT_STATE: [&str; 3] = [".git/index", ".git/HEAD", ".git/logs/HEAD"];

pub enum Command {
    SetWorktrees(Vec<PathBuf>),
}

/// Picks the deepest root containing `path`, so a nested worktree such as
/// `.worktrees/a` is not reported as a change to the main worktree.
pub fn owning_worktree(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let root = roots.iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())?;
    let rel = path.strip_prefix(root).ok()?;
    let git_internal = rel.iter().any(|c| c == ".git");
    if git_internal && !GIT_STATE.iter().any(|f| rel == Path::new(f)) {
        return None;
    }
    Some(root.clone())
}

/// What a path's change means.
#[derive(Debug, PartialEq, Eq)]
enum Change {
    /// A file in this worktree, which counts unless git ignores it.
    File(PathBuf),
    /// This worktree's index or reflog.
    State(PathBuf),
    /// This worktree's HEAD, so its branch may differ in the list as well.
    Head(PathBuf),
    /// The set of worktrees, or an unknown worktree's HEAD.
    List,
}

fn classify(path: &Path, worktrees_dir: &Path, worktrees: &Worktrees) -> Option<Change> {
    if let Ok(rel) = path.strip_prefix(worktrees_dir) {
        let parts: Vec<_> = rel.iter().map(|c| c.to_str().unwrap_or_default()).collect();
        let owner = rel.iter().next().and_then(|name| worktrees.linked.get(name)).cloned();
        return match parts.as_slice() {
            [] | [_] | [_, "gitdir"] => Some(Change::List),
            [_, "HEAD"] => Some(owner.map_or(Change::List, Change::Head)),
            [_, "index"] | [_, "logs", "HEAD"] => owner.map(Change::State),
            _ => None,
        };
    }
    if let (Some(main), Some(rel)) = (&worktrees.main_with_git_file, strip_common(path, worktrees_dir)) {
        return if rel == Path::new("HEAD") {
            Some(Change::Head(main.clone()))
        } else if rel == Path::new("index") || rel == Path::new("logs/HEAD") {
            Some(Change::State(main.clone()))
        } else {
            None
        };
    }
    let root = owning_worktree(path, &worktrees.roots)?;
    let rel = path.strip_prefix(&root).ok()?;
    Some(if rel == Path::new(".git/HEAD") {
        Change::Head(root)
    } else if rel.starts_with(".git") {
        Change::State(root)
    } else {
        Change::File(root)
    })
}

/// Linux reports every open and close, so without this our own reads of a
/// worktree would mark it as changed. A real write also sends a `Modify`.
fn is_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

fn strip_common<'a>(path: &'a Path, worktrees_dir: &Path) -> Option<&'a Path> {
    path.strip_prefix(worktrees_dir.parent()?).ok()
}

/// Where a worktree's `.git` file points, which a submodule gives as a relative
/// path. `.git` is a directory in an ordinary main worktree, which gives `None`.
fn git_dir_from_file(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?;
    let dir = text.strip_prefix("gitdir:")?.trim();
    root.join(dir).canonicalize().ok()
}

struct Pending {
    first: Instant,
    last: Instant,
}

/// Reports a key once no new event has arrived for the debounce window, or
/// once it has been pending for `MAX_WAIT`, whichever comes first.
pub struct Debouncer {
    window: Duration,
    pending: HashMap<PathBuf, Pending>,
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self { window, pending: HashMap::new() }
    }

    pub fn record(&mut self, key: PathBuf, now: Instant) {
        self.pending.entry(key).or_insert(Pending { first: now, last: now }).last = now;
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn due(&mut self, now: Instant) -> Vec<PathBuf> {
        let window = self.window;
        let due: Vec<PathBuf> = self.pending.iter()
            .filter(|(_, p)| now.duration_since(p.last) >= window || now.duration_since(p.first) >= MAX_WAIT)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &due {
            self.pending.remove(k);
        }
        due
    }
}

fn is_watch_limit(error: &notify::Error) -> bool {
    matches!(error.kind, notify::ErrorKind::MaxFilesWatch)
}

/// Tells the user about the watch limit, once, however often it is hit: a root
/// that hit it is retried on every `SetWorktrees`.
struct WatchLimit {
    msgs: Sender<Msg>,
    reported: bool,
}

impl WatchLimit {
    fn new(msgs: Sender<Msg>) -> Self {
        Self { msgs, reported: false }
    }

    fn note(&mut self, error: &notify::Error) {
        if is_watch_limit(error) && !self.reported {
            self.reported = true;
            let _ = self.msgs.send(Msg::WatchLimit);
        }
    }
}

/// The OS dropped events, so a change anywhere may have gone unseen.
fn roots_to_rescan(event: &Event, watched: &HashSet<PathBuf>) -> Vec<PathBuf> {
    if event.need_rescan() {
        watched.iter().cloned().collect()
    } else {
        Vec::new()
    }
}

/// Starts the watcher thread. Send `SetWorktrees` whenever the list changes.
/// The thread ends when the returned sender is dropped.
pub fn spawn(common_dir: PathBuf, msgs: Sender<Msg>) -> notify::Result<Sender<Command>> {
    let (fs_tx, fs_rx) = unbounded::<notify::Result<Event>>();
    let (cmd_tx, cmd_rx) = unbounded::<Command>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |e| {
        let _ = fs_tx.send(e);
    })?;
    // Recursive, because `worktrees` may not exist yet, is deleted along with the
    // last linked worktree, and holds their index files. A bare repository has
    // no worktree of its own to cover it.
    let mut limit = WatchLimit::new(msgs.clone());
    if let Err(e) = watcher.watch(&common_dir, RecursiveMode::Recursive) {
        // The caller treats an error as having no watcher, which would hide the
        // limit, and whatever was watched before it was reached still works.
        if !is_watch_limit(&e) {
            return Err(e);
        }
        limit.note(&e);
    }
    let worktrees_dir = common_dir.join("worktrees");

    thread::spawn(move || run(watcher, worktrees_dir, fs_rx, cmd_rx, msgs, limit));
    Ok(cmd_tx)
}

/// The worktrees being watched. A root that could not be watched stays out of
/// `watched`, so that the next `SetWorktrees` tries it again.
#[derive(Default)]
struct Worktrees {
    roots: Vec<PathBuf>,
    watched: HashSet<PathBuf>,
    /// Each directory name under `<common>/worktrees`, and its worktree.
    linked: HashMap<OsString, PathBuf>,
    /// The main worktree, if its `.git` is a file, so that its index and HEAD
    /// are directly in the common dir: a separate git dir or a submodule.
    main_with_git_file: Option<PathBuf>,
}

impl Worktrees {
    fn set(&mut self, watcher: &mut RecommendedWatcher, next: Vec<PathBuf>, limit: &mut WatchLimit, worktrees_dir: &Path) {
        let gone: Vec<_> = self.watched.iter().filter(|r| !next.contains(r)).cloned().collect();
        for root in gone {
            self.forget(watcher, &root);
        }
        for root in &next {
            if self.watched.contains(root) {
                continue;
            }
            match watcher.watch(root, RecursiveMode::Recursive) {
                Ok(()) => {
                    self.watched.insert(root.clone());
                }
                Err(e) => limit.note(&e),
            }
        }
        self.find_git_dirs(&next, worktrees_dir);
        self.roots = next;
    }

    /// A `.git` file points at the common dir itself from the main worktree, and
    /// at `<common>/worktrees/<name>` from a linked one. The names can't be
    /// compared alone: a submodule `a` can have a linked worktree `a`.
    fn find_git_dirs(&mut self, roots: &[PathBuf], worktrees_dir: &Path) {
        let common = worktrees_dir.parent().and_then(|c| c.canonicalize().ok());
        self.linked.clear();
        self.main_with_git_file = None;
        for root in roots {
            let Some(git_dir) = git_dir_from_file(root) else { continue };
            if Some(&git_dir) == common.as_ref() {
                self.main_with_git_file = Some(root.clone());
            } else if let Some(name) = git_dir.file_name() {
                self.linked.insert(name.to_os_string(), root.clone());
            }
        }
    }

    fn forget(&mut self, watcher: &mut RecommendedWatcher, root: &Path) {
        // Fails if the directory went first, which leaves nothing to undo.
        let _ = watcher.unwatch(root);
        self.watched.remove(root);
    }
}

/// `git::any_not_ignored` for one root's pending paths. A failure counts as a
/// change, since a badge too many is better than a missed one.
fn any_counts(root: &Path, paths: HashSet<PathBuf>) -> bool {
    let paths: Vec<_> = paths.into_iter().collect();
    git::any_not_ignored(root, &paths).unwrap_or(true)
}

/// A directory that is gone can only match a rule like `target/` with its
/// slash. Paths compare equal without it, so `insert` would not store it.
fn add_path(paths: &mut HashSet<PathBuf>, path: &Path, kind: &EventKind) {
    if matches!(kind, EventKind::Remove(RemoveKind::Folder)) {
        paths.replace(path.join(""));
    } else {
        paths.insert(path.to_path_buf());
    }
}

fn run(
    mut watcher: RecommendedWatcher,
    worktrees_dir: PathBuf,
    fs_rx: Receiver<notify::Result<Event>>,
    cmd_rx: Receiver<Command>,
    msgs: Sender<Msg>,
    mut limit: WatchLimit,
) {
    let mut worktrees = Worktrees::default();
    // Asked about in batches, because each batch costs a process. A write to an
    // ignored path must not extend a burst, or a busy log would hold back the
    // badge for a real edit, so this is checked ahead of `bursts` on a timer.
    let mut unchecked: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    let mut first_unchecked = Instant::now();
    let mut bursts = Debouncer::new(DEBOUNCE);
    let mut list_changed = Debouncer::new(DEBOUNCE);

    loop {
        let idle = unchecked.is_empty() && bursts.is_empty() && list_changed.is_empty();
        select! {
            recv(cmd_rx) -> cmd => match cmd {
                Ok(Command::SetWorktrees(next)) => worktrees.set(&mut watcher, next, &mut limit, &worktrees_dir),
                Err(_) => return,
            },
            recv(fs_rx) -> event => {
                let Ok(event) = event else { return };
                let now = Instant::now();
                match &event {
                    Ok(event) => {
                        for root in roots_to_rescan(event, &worktrees.watched) {
                            bursts.record(root, now);
                        }
                    }
                    Err(e) => limit.note(e),
                }
                for event in event.iter().filter(|e| is_change(&e.kind)) {
                    for path in &event.paths {
                        match classify(path, &worktrees_dir, &worktrees) {
                            Some(Change::File(root)) if path == &root && !root.exists() => {
                                worktrees.forget(&mut watcher, &root);
                                list_changed.record(worktrees_dir.clone(), now);
                            }
                            Some(Change::File(root)) => {
                                if unchecked.is_empty() {
                                    first_unchecked = now;
                                }
                                add_path(unchecked.entry(root).or_default(), path, &event.kind);
                            }
                            Some(Change::State(root)) => bursts.record(root, now),
                            Some(Change::Head(root)) => {
                                bursts.record(root, now);
                                list_changed.record(worktrees_dir.clone(), now);
                            }
                            Some(Change::List) => list_changed.record(worktrees_dir.clone(), now),
                            None => {}
                        }
                    }
                }
            },
            default(if idle { IDLE } else { TICK }) => {}
        }

        let now = Instant::now();
        if now.duration_since(first_unchecked) >= DEBOUNCE {
            for (root, paths) in unchecked.drain() {
                if any_counts(&root, paths) {
                    bursts.record(root, now);
                }
            }
        }
        for root in bursts.due(now) {
            if msgs.send(Msg::Stale(root)).is_err() {
                return;
            }
        }
        if !list_changed.due(now).is_empty() && msgs.send(Msg::WorktreesChanged).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::Flag;
    use std::path::Path;

    fn roots() -> Vec<PathBuf> {
        vec!["/r/main".into(), "/r/main/.worktrees/a".into(), "/r/other".into()]
    }

    fn owner(p: &str) -> Option<PathBuf> {
        owning_worktree(Path::new(p), &roots())
    }

    #[test]
    fn nested_worktree_owns_its_own_changes() {
        assert_eq!(owner("/r/main/.worktrees/a/src/x.rs"), Some("/r/main/.worktrees/a".into()));
    }

    #[test]
    fn main_worktree_owns_its_files() {
        assert_eq!(owner("/r/main/src/x.rs"), Some("/r/main".into()));
    }

    #[test]
    fn git_internals_are_ignored_except_index_and_head() {
        assert_eq!(owner("/r/main/.git/objects/ab/cd"), None);
        assert_eq!(owner("/r/main/.git/index"), Some("/r/main".into()));
        assert_eq!(owner("/r/main/.git/HEAD"), Some("/r/main".into()));
    }

    #[test]
    fn paths_outside_every_worktree_are_ignored() {
        assert_eq!(owner("/elsewhere/x"), None);
    }

    #[test]
    fn similar_prefixes_do_not_match() {
        assert_eq!(owner("/r/other2/x"), None);
    }

    #[test]
    fn debounce_emits_one_event_per_burst() {
        let mut d = Debouncer::new(Duration::from_millis(200));
        let t0 = Instant::now();
        d.record("/r/a".into(), t0);
        d.record("/r/a".into(), t0 + Duration::from_millis(100));
        assert!(d.due(t0 + Duration::from_millis(250)).is_empty());
        assert_eq!(d.due(t0 + Duration::from_millis(301)), vec![PathBuf::from("/r/a")]);
        assert!(d.due(t0 + Duration::from_millis(600)).is_empty());
    }

    #[test]
    fn steady_events_report_at_the_maximum_wait() {
        let mut d = Debouncer::new(Duration::from_millis(200));
        let t0 = Instant::now();
        for ms in (0..2000).step_by(100) {
            let at = t0 + Duration::from_millis(ms);
            d.record("/r/a".into(), at);
            assert!(d.due(at).is_empty(), "{ms}");
        }
        let at = t0 + MAX_WAIT;
        d.record("/r/a".into(), at);
        assert_eq!(d.due(at), vec![PathBuf::from("/r/a")]);
    }

    #[test]
    fn the_maximum_wait_restarts_after_a_report() {
        let mut d = Debouncer::new(Duration::from_millis(200));
        let t0 = Instant::now();
        d.record("/r/a".into(), t0);
        d.record("/r/a".into(), t0 + MAX_WAIT);
        assert_eq!(d.due(t0 + MAX_WAIT).len(), 1);
        let next = t0 + MAX_WAIT + Duration::from_millis(100);
        d.record("/r/a".into(), next);
        assert!(d.due(next).is_empty());
    }

    #[test]
    fn the_maximum_wait_is_per_key() {
        let mut d = Debouncer::new(Duration::from_millis(200));
        let t0 = Instant::now();
        d.record("/r/old".into(), t0);
        let at = t0 + MAX_WAIT;
        d.record("/r/old".into(), at);
        d.record("/r/new".into(), at);
        assert_eq!(d.due(at), vec![PathBuf::from("/r/old")]);
    }

    #[test]
    fn only_the_inotify_limit_counts_as_the_watch_limit() {
        assert!(is_watch_limit(&notify::Error::new(notify::ErrorKind::MaxFilesWatch)));
        assert!(!is_watch_limit(&notify::Error::path_not_found()));
        assert!(!is_watch_limit(&notify::Error::watch_not_found()));
        assert!(!is_watch_limit(&notify::Error::generic("no")));
    }

    #[test]
    fn the_watch_limit_is_reported_once() {
        let (tx, rx) = unbounded();
        let mut limit = WatchLimit::new(tx);
        limit.note(&notify::Error::path_not_found());
        assert_eq!(rx.try_iter().count(), 0, "not the limit");
        limit.note(&notify::Error::new(notify::ErrorKind::MaxFilesWatch));
        assert!(matches!(rx.try_recv(), Ok(Msg::WatchLimit)));
        limit.note(&notify::Error::new(notify::ErrorKind::MaxFilesWatch));
        assert_eq!(rx.try_iter().count(), 0, "already reported");
    }

    #[test]
    fn dropped_events_mark_every_watched_root() {
        let watched = HashSet::from([PathBuf::from("/r/a"), PathBuf::from("/r/b")]);
        let rescan = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        let mut roots = roots_to_rescan(&rescan, &watched);
        roots.sort();
        assert_eq!(roots, vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]);
    }

    #[test]
    fn ordinary_events_mark_nothing_wholesale() {
        let watched = HashSet::from([PathBuf::from("/r/a")]);
        assert!(roots_to_rescan(&Event::new(EventKind::Other), &watched).is_empty());
        let edit = Event::new(EventKind::Remove(RemoveKind::File)).add_path("/r/a/x".into());
        assert!(roots_to_rescan(&edit, &watched).is_empty());
    }

    #[test]
    fn the_reflog_counts_because_a_commit_may_touch_nothing_else() {
        assert_eq!(owner("/r/main/.git/logs/HEAD"), Some("/r/main".into()));
        assert_eq!(owner("/r/main/.git/logs/refs/heads/x"), None);
    }

    #[test]
    fn a_repository_inside_a_worktree_is_not_its_files() {
        assert_eq!(owner("/r/main/vendor/lib/.git/index"), None);
        assert_eq!(owner("/r/main/vendor/lib/.git/objects/ab/cd"), None);
        assert_eq!(owner("/r/main/vendor/lib/x.rs"), Some("/r/main".into()));
    }

    #[test]
    fn a_name_that_only_ends_in_dot_git_is_a_file() {
        assert_eq!(owner("/r/main/repo.git/index"), Some("/r/main".into()));
    }

    fn classified(p: &str) -> Option<Change> {
        let linked = HashMap::from([(OsString::from("a"), PathBuf::from("/r/main/.worktrees/a"))]);
        let worktrees = Worktrees { roots: roots(), linked, ..Worktrees::default() };
        classify(Path::new(p), Path::new("/r/main/.git/worktrees"), &worktrees)
    }

    fn classified_with_git_file(p: &str, main: Option<&str>) -> Option<Change> {
        let linked = HashMap::from([(OsString::from("a"), PathBuf::from("/r/a"))]);
        let worktrees = Worktrees {
            roots: vec!["/r/wt".into(), "/r/a".into()],
            linked,
            main_with_git_file: main.map(PathBuf::from),
            ..Worktrees::default()
        };
        classify(Path::new(p), Path::new("/r/sep.git/worktrees"), &worktrees)
    }

    #[test]
    fn git_state_in_the_common_dir_belongs_to_a_main_worktree_whose_git_is_a_file() {
        let (wt, main) = (|| PathBuf::from("/r/wt"), Some("/r/wt"));
        assert_eq!(classified_with_git_file("/r/sep.git/index", main), Some(Change::State(wt())));
        assert_eq!(classified_with_git_file("/r/sep.git/logs/HEAD", main), Some(Change::State(wt())));
        assert_eq!(classified_with_git_file("/r/sep.git/HEAD", main), Some(Change::Head(wt())));
    }

    #[test]
    fn scratch_files_in_that_common_dir_mean_nothing() {
        for name in ["index.lock", "HEAD.lock", "ORIG_HEAD", "objects/ab/cd", "refs/heads/x", "logs/refs/heads/x"] {
            assert_eq!(classified_with_git_file(&format!("/r/sep.git/{name}"), Some("/r/wt")), None, "{name}");
        }
    }

    #[test]
    fn linked_worktrees_keep_their_own_state_beside_such_a_main_worktree() {
        let linked = Some(Change::State("/r/a".into()));
        assert_eq!(classified_with_git_file("/r/sep.git/worktrees/a/index", Some("/r/wt")), linked);
    }

    #[test]
    fn the_common_dir_means_nothing_without_such_a_main_worktree() {
        assert_eq!(classified_with_git_file("/r/sep.git/index", None), None);
        assert_eq!(classified_with_git_file("/r/sep.git/HEAD", None), None);
    }

    #[test]
    fn a_linked_worktrees_git_state_belongs_to_it_and_not_to_the_list() {
        let a = || PathBuf::from("/r/main/.worktrees/a");
        assert_eq!(classified("/r/main/.git/worktrees/a/index"), Some(Change::State(a())));
        assert_eq!(classified("/r/main/.git/worktrees/a/logs/HEAD"), Some(Change::State(a())));
        assert_eq!(classified("/r/main/.git/worktrees/a/HEAD"), Some(Change::Head(a())));
    }

    #[test]
    fn locks_and_scratch_files_of_a_linked_worktree_mean_nothing() {
        for name in ["index.lock", "HEAD.lock", "ORIG_HEAD", "AUTO_MERGE.lock", "COMMIT_EDITMSG", "rebase-merge/done"] {
            assert_eq!(classified(&format!("/r/main/.git/worktrees/a/{name}")), None, "{name}");
        }
    }

    #[test]
    fn worktrees_coming_and_going_change_the_list() {
        for path in ["/r/main/.git/worktrees", "/r/main/.git/worktrees/new", "/r/main/.git/worktrees/a/gitdir"] {
            assert_eq!(classified(path), Some(Change::List), "{path}");
        }
    }

    #[test]
    fn the_git_state_of_an_unlisted_worktree_waits_for_the_list() {
        assert_eq!(classified("/r/main/.git/worktrees/new/index"), None);
        assert_eq!(classified("/r/main/.git/worktrees/new/HEAD"), Some(Change::List));
    }

    #[test]
    fn files_in_the_common_dir_that_git_rewrites_constantly_mean_nothing() {
        for name in ["index.lock", "ORIG_HEAD", "FETCH_HEAD", "packed-refs", "config", "refs/heads/x"] {
            assert_eq!(classified(&format!("/r/main/.git/{name}")), None, "{name}");
        }
    }

    #[test]
    fn the_main_worktrees_files_and_git_state_are_told_apart() {
        let main = || PathBuf::from("/r/main");
        assert_eq!(classified("/r/main/src/x.rs"), Some(Change::File(main())));
        assert_eq!(classified("/r/main/.git/index"), Some(Change::State(main())));
        assert_eq!(classified("/r/main/.git/logs/HEAD"), Some(Change::State(main())));
        assert_eq!(classified("/r/main/.git/HEAD"), Some(Change::Head(main())));
    }

    #[test]
    fn reading_a_file_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, DataChange, ModifyKind};
        assert!(!is_change(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!is_change(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(is_change(&EventKind::Modify(ModifyKind::Data(DataChange::Any))));
        assert!(is_change(&EventKind::Remove(RemoveKind::File)));
    }
}
