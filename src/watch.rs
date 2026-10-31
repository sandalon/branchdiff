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

/// `linked` maps each directory name under `worktrees_dir` to its worktree.
fn classify(
    path: &Path,
    worktrees_dir: &Path,
    roots: &[PathBuf],
    linked: &HashMap<OsString, PathBuf>,
) -> Option<Change> {
    if let Ok(rel) = path.strip_prefix(worktrees_dir) {
        let parts: Vec<_> = rel.iter().map(|c| c.to_str().unwrap_or_default()).collect();
        let owner = rel.iter().next().and_then(|name| linked.get(name)).cloned();
        return match parts.as_slice() {
            [] | [_] | [_, "gitdir"] => Some(Change::List),
            [_, "HEAD"] => Some(owner.map_or(Change::List, Change::Head)),
            [_, "index"] | [_, "logs", "HEAD"] => owner.map(Change::State),
            _ => None,
        };
    }
    let root = owning_worktree(path, roots)?;
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

/// The name of a linked worktree's directory under `<common>/worktrees`, read
/// from its `.git` file. The main worktree's `.git` is a directory.
fn linked_name(root: &Path) -> Option<OsString> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?;
    let dir = text.strip_prefix("gitdir:")?.trim();
    Path::new(dir).file_name().map(OsString::from)
}

/// Reports a key once no new event has arrived for the debounce window.
pub struct Debouncer {
    window: Duration,
    pending: HashMap<PathBuf, Instant>,
}

impl Debouncer {
    pub fn new(window: Duration) -> Self {
        Self { window, pending: HashMap::new() }
    }

    pub fn record(&mut self, key: PathBuf, now: Instant) {
        self.pending.insert(key, now);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn due(&mut self, now: Instant) -> Vec<PathBuf> {
        let window = self.window;
        let due: Vec<PathBuf> = self.pending.iter()
            .filter(|(_, t)| now.duration_since(**t) >= window)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &due {
            self.pending.remove(k);
        }
        due
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
    watcher.watch(&common_dir, RecursiveMode::Recursive)?;
    let worktrees_dir = common_dir.join("worktrees");

    thread::spawn(move || run(watcher, worktrees_dir, fs_rx, cmd_rx, msgs));
    Ok(cmd_tx)
}

/// The worktrees being watched. A root that could not be watched stays out of
/// `watched`, so that the next `SetWorktrees` tries it again.
#[derive(Default)]
struct Worktrees {
    roots: Vec<PathBuf>,
    watched: HashSet<PathBuf>,
    linked: HashMap<OsString, PathBuf>,
}

impl Worktrees {
    fn set(&mut self, watcher: &mut RecommendedWatcher, next: Vec<PathBuf>) {
        let gone: Vec<_> = self.watched.iter().filter(|r| !next.contains(r)).cloned().collect();
        for root in gone {
            self.forget(watcher, &root);
        }
        for root in &next {
            if !self.watched.contains(root) && watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                self.watched.insert(root.clone());
            }
        }
        self.linked = next.iter().filter_map(|r| Some((linked_name(r)?, r.clone()))).collect();
        self.roots = next;
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
                Ok(Command::SetWorktrees(next)) => worktrees.set(&mut watcher, next),
                Err(_) => return,
            },
            recv(fs_rx) -> event => {
                let Ok(event) = event else { return };
                let now = Instant::now();
                for event in event.iter().filter(|e| is_change(&e.kind)) {
                    for path in &event.paths {
                        match classify(path, &worktrees_dir, &worktrees.roots, &worktrees.linked) {
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
        classify(Path::new(p), Path::new("/r/main/.git/worktrees"), &roots(), &linked)
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
