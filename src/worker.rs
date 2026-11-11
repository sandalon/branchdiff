use crate::cache::{CacheKey, Diffs};
use crate::git::{self, GitError, Worktree};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::any::Any;
use std::collections::HashMap;
use std::panic::{catch_unwind, UnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

/// The name of every worker thread. A panic hook can use it to tell a worker's
/// panic, which `guarded` contains, from one that ends the program.
pub const WORKER_THREAD: &str = "branchdiff-worker";

/// Loads that may run at once. Each may briefly hold up to `MAX_DIFF_BYTES`
/// while it reads and parses, so this bounds what loads cost together.
pub const MAX_CONCURRENT_LOADS: usize = 2;

const PANIC_MESSAGE_CHARS: usize = 120;
const PANIC_NO_MESSAGE: &str = "internal error in a background job";

#[derive(Debug)]
pub enum Job {
    ListWorktrees { cwd: PathBuf },
    Load { path: PathBuf, base: String },
    ListBranches { cwd: PathBuf },
    Highlight { key: CacheKey, file_index: usize, diffs: Diffs },
    Probe { key: CacheKey },
}

#[derive(Debug)]
pub struct Loaded {
    pub key: CacheKey,
    pub diffs: Diffs,
    pub op: Option<&'static str>,
}

#[derive(Debug)]
pub enum Msg {
    Worktrees(Result<Vec<Worktree>, GitError>),
    Loaded {
        path: PathBuf,
        result: Result<Loaded, GitError>,
    },
    Branches(Result<Vec<String>, GitError>),
    Highlighted {
        key: CacheKey,
        /// The exact diff this job coloured; a branch transition can recreate
        /// the same cache key while retiring the original diff.
        source: std::sync::Weak<Vec<crate::diff::FileDiff>>,
        file_index: usize,
        lines: crate::highlight::HighlightedFile,
    },
    Stale(PathBuf),
    /// Answers `Job::Probe`. `changed` means the worktree no longer matches its key.
    Probed {
        path: PathBuf,
        changed: bool,
    },
    WorktreesChanged,
    WatchLimit,
}

struct Running {
    base: String,
    waiting_base: Option<String>,
    changed: bool,
    invalidated: bool,
}

/// What to do about a load that has just finished.
#[derive(Debug, Default)]
pub struct Finished {
    /// The waiting job, which now counts as running. Submit it.
    pub next: Option<Job>,
    /// The result belongs to a retired review identity or a base the user has
    /// left, so drop it. Otherwise a late `BaseMissing` would reopen the picker.
    /// A same-base refresh of the current identity can show until `next` lands.
    pub superseded: bool,
    /// The worktree changed during this load, so the result may predate that
    /// change. Never set with `next`, which starts after the change.
    pub changed: bool,
}

/// Allows one running load per worktree, because a load can take seconds and
/// repeated refreshes would otherwise run side by side. A request made during
/// a load replaces any waiting one, so the newest always runs next.
#[derive(Default)]
pub struct Loads {
    running: HashMap<PathBuf, Running>,
}

impl Loads {
    /// Answers the job to submit now, if the worktree has no load running.
    pub fn request(&mut self, path: PathBuf, base: String) -> Option<Job> {
        match self.running.get_mut(&path) {
            Some(running) => {
                running.waiting_base = Some(base);
                None
            }
            None => {
                self.running.insert(
                    path.clone(),
                    Running { base: base.clone(), waiting_base: None, changed: false, invalidated: false },
                );
                Some(Job::Load { path, base })
            }
        }
    }

    /// Retires a load for a removed worktree or an earlier review identity.
    /// Keep it running so a replacement waits rather than racing it. Any
    /// waiting request also predates the identity change and must be replaced.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(running) = self.running.get_mut(path) {
            running.invalidated = true;
            running.waiting_base = None;
        }
    }

    /// Records that a worktree changed. Does nothing if no load is running.
    pub fn mark_changed(&mut self, path: &Path) {
        if let Some(running) = self.running.get_mut(path) {
            running.changed = true;
        }
    }

    pub fn is_running(&self, path: &Path) -> bool {
        self.running.contains_key(path)
    }

    /// Call for every finished load.
    pub fn finished(&mut self, path: &Path) -> Finished {
        let Some(running) = self.running.get_mut(path) else {
            return Finished::default();
        };
        let changed = std::mem::take(&mut running.changed);
        let invalidated = std::mem::take(&mut running.invalidated);
        let Some(base) = running.waiting_base.take() else {
            self.running.remove(path);
            return Finished { changed, superseded: invalidated, ..Finished::default() };
        };
        let superseded = invalidated || base != running.base;
        running.base = base.clone();
        Finished { next: Some(Job::Load { path: path.to_path_buf(), base }), superseded, changed: false }
    }
}

/// What a report that a worktree changed calls for.
#[derive(Debug)]
pub enum Reaction {
    Nothing,
    /// Submit this job, which finds out whether anything really changed.
    Submit(Job),
    /// Tell the app. There's no loaded diff to compare against.
    Badge,
}

/// What to do about a probe that has just finished.
#[derive(Debug, Default)]
pub struct ProbeDone {
    /// A probe for a change seen during this one, which now counts as running.
    pub next: Option<Job>,
    /// The diff on screen no longer matches its worktree, so show the badge.
    pub raise: bool,
}

struct RunningProbe {
    key: CacheKey,
    waiting: bool,
}

/// Decides when a reported change needs a probe. Tools such as `git status`
/// rewrite the index without changing anything a diff shows, so a report alone
/// isn't enough to raise the badge. Allows one running probe per worktree and
/// one waiting behind it, so a burst of reports can't queue up work, yet a report
/// that arrives during a probe is always checked afterwards.
#[derive(Default)]
pub struct Probes {
    running: HashMap<PathBuf, RunningProbe>,
}

impl Probes {
    /// Call when a worktree may have changed. `loading` says a load is running,
    /// which `Loads::finished` reports on later. `key` is the worktree's diff.
    pub fn on_change(&mut self, loading: bool, key: Option<CacheKey>) -> Reaction {
        if loading {
            return Reaction::Nothing;
        }
        let Some(key) = key else { return Reaction::Badge };
        match self.running.get_mut(&key.worktree) {
            Some(running) => {
                running.waiting = true;
                Reaction::Nothing
            }
            None => {
                self.running.insert(key.worktree.clone(), RunningProbe { key: key.clone(), waiting: false });
                Reaction::Submit(Job::Probe { key })
            }
        }
    }

    /// Call for every finished probe. `current` is the worktree's key now. If a
    /// refresh replaced the key during the probe, the answer is about a diff
    /// that has gone, and that refresh reports its own changes through `Loads`.
    pub fn finished(&mut self, path: &Path, changed: bool, current: Option<CacheKey>) -> ProbeDone {
        let Some(running) = self.running.remove(path) else { return ProbeDone::default() };
        let raise = changed && current.as_ref() == Some(&running.key);
        let next = current.filter(|_| running.waiting).map(|key| {
            self.running.insert(path.to_path_buf(), RunningProbe { key: key.clone(), waiting: false });
            Job::Probe { key }
        });
        ProbeDone { next, raise }
    }
}

/// Allows one worktree list at a time. The newest list applied wins, so one that
/// took longer than a later one would undo what that one found. A request made
/// during a list waits and starts once that list has finished, which also means
/// a burst of requests runs just one more.
#[derive(Default)]
pub struct Lists {
    running: bool,
    waiting: bool,
}

impl Lists {
    /// Answers whether to submit a list now.
    pub fn request(&mut self) -> bool {
        self.waiting = self.running;
        self.running = true;
        !self.waiting
    }

    /// Call for every finished list. Answers whether to submit the waiting one.
    pub fn finished(&mut self) -> bool {
        self.running = std::mem::take(&mut self.waiting);
        self.running
    }
}

/// Loads run on threads of their own, at most `MAX_CONCURRENT_LOADS` of them,
/// so that waiting loads never keep probes and highlights from a thread.
pub struct Pool {
    jobs: Sender<Job>,
    loads: Sender<Job>,
}

impl Pool {
    pub fn new(threads: usize, msgs: Sender<Msg>) -> Self {
        Self { jobs: spawn(threads, &msgs), loads: spawn(threads.min(MAX_CONCURRENT_LOADS), &msgs) }
    }

    pub fn submit(&self, job: Job) {
        let lane = if matches!(job, Job::Load { .. }) { &self.loads } else { &self.jobs };
        let _ = lane.send(job);
    }
}

fn spawn(threads: usize, msgs: &Sender<Msg>) -> Sender<Job> {
    let (jobs, rx): (Sender<Job>, Receiver<Job>) = unbounded();
    for _ in 0..threads {
        let (rx, msgs) = (rx.clone(), msgs.clone());
        // A failed spawn leaves a smaller pool, which is better than ending
        // the program. If every spawn fails, `submit` discards all jobs and
        // `Loads` holds each worktree as running for good.
        let _ = thread::Builder::new().name(WORKER_THREAD.into()).spawn(move || {
            for job in rx {
                if msgs.send(run(job)).is_err() {
                    break;
                }
            }
        });
    }
    jobs
}

fn run(job: Job) -> Msg {
    match job {
        Job::ListWorktrees { cwd } => {
            let started = std::time::Instant::now();
            let result = guarded(|| {
                #[cfg(test)]
                tests::panic_if_marked(&cwd);
                git::list_worktrees(&cwd)
            });
            let seconds = started.elapsed().as_secs_f64();
            match &result {
                Ok(list) => crate::log!("listed {} worktrees in {seconds:.3}s", list.len()),
                Err(e) => crate::log!("listing worktrees failed after {seconds:.3}s: {e}"),
            }
            Msg::Worktrees(result)
        }
        Job::ListBranches { cwd } => Msg::Branches(guarded(|| {
            #[cfg(test)]
            tests::panic_if_marked(&cwd);
            git::branches(&cwd)
        })),
        Job::Load { path, base } => {
            let started = std::time::Instant::now();
            let result = guarded(|| {
                #[cfg(test)]
                tests::panic_if_marked(&path);
                load(&path, &base)
            });
            let (seconds, shown) = (started.elapsed().as_secs_f64(), path.display());
            match &result {
                Ok(loaded) => {
                    crate::log!("loaded {shown} against {base}: {} files in {seconds:.3}s", loaded.diffs.len())
                }
                Err(e) => crate::log!("loading {shown} against {base} failed after {seconds:.3}s: {e}"),
            }
            Msg::Loaded { path, result }
        }
        Job::Highlight { key, file_index, diffs } => {
            // An empty result still has to be sent: `main` tracks highlights in
            // flight, and one that never reported back would never run again.
            let highlight = || {
                #[cfg(test)]
                tests::panic_if_marked(&key.worktree);
                Ok(diffs.get(file_index).map(crate::highlight::highlight_file).unwrap_or_default())
            };
            let lines = guarded(highlight).unwrap_or_default();
            Msg::Highlighted { key, source: Arc::downgrade(&diffs), file_index, lines }
        }
        Job::Probe { key } => {
            let path = key.worktree.clone();
            let compare = || {
                #[cfg(test)]
                tests::panic_if_marked(&path);
                probe(&key)
            };
            // A probe that fails can't say the diff is current, so it counts as a change.
            let changed = guarded(std::panic::AssertUnwindSafe(compare)).unwrap_or(true);
            Msg::Probed { path, changed }
        }
    }
}

/// Runs a job's work so that a panic becomes that job's error. The job still
/// reports back, which `Loads` relies on, and its worker thread lives on.
fn guarded<T, F>(f: F) -> Result<T, GitError>
where
    F: FnOnce() -> Result<T, GitError> + UnwindSafe,
{
    catch_unwind(f).unwrap_or_else(|payload| Err(GitError::Failed(panic_text(payload.as_ref()))))
}

/// One short line for a worktree row. `main`'s panic hook skips workers, so
/// this is all anyone sees of a worker panic.
fn panic_text(payload: &(dyn Any + Send)) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .and_then(|text| text.lines().next())
        .filter(|line| !line.trim().is_empty());
    match message {
        Some(line) => format!("internal error: {}", line.chars().take(PANIC_MESSAGE_CHARS).collect::<String>()),
        None => PANIC_NO_MESSAGE.to_string(),
    }
}

fn head(path: &Path) -> Result<String, GitError> {
    Ok(String::from_utf8_lossy(&git::run(path, ["rev-parse", "HEAD"])?.stdout).trim().to_string())
}

/// Answers whether the inputs of the diff behind `key` have changed.
fn probe(key: &CacheKey) -> Result<bool, GitError> {
    Ok(head(&key.worktree)? != key.head || git::fingerprint(&key.worktree)? != key.fingerprint)
}

fn load(path: &Path, base: &str) -> Result<Loaded, GitError> {
    let base_commit = git::merge_base(path, base)?;
    let head = head(path)?;
    let fingerprint = git::fingerprint(path)?;
    let diffs = Arc::new(crate::diff::load(path, &base_commit)?);
    let key = CacheKey { worktree: path.to_path_buf(), base_commit, head, fingerprint };
    Ok(Loaded { key, diffs, op: git::in_progress_op(path) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    const PANIC_PATH: &str = "/branchdiff-test-panic";

    /// Called inside each job's work in `run`, so that removing `guarded` from
    /// an arm kills the worker. No real job can be made to panic on demand.
    pub(super) fn panic_if_marked(path: &Path) {
        if path == Path::new(PANIC_PATH) {
            panic!("test panic on {}", thread::current().name().unwrap_or("an unnamed thread"));
        }
        // A job on a path named HOLD_NAME runs until that path is removed.
        if path.file_name() == Some(std::ffi::OsStr::new(HOLD_NAME)) {
            HELD.fetch_add(1, Ordering::SeqCst);
            while path.exists() {
                thread::sleep(Duration::from_millis(5));
            }
            HELD.fetch_sub(1, Ordering::SeqCst);
        }
    }

    const HOLD_NAME: &str = "branchdiff-test-hold";
    static HELD: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn loads_beyond_the_limit_wait_without_holding_up_other_jobs() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(4, tx);
        let dirs: Vec<_> = (0..MAX_CONCURRENT_LOADS + 2).map(|_| tempfile::TempDir::new().unwrap()).collect();
        for dir in &dirs {
            let path = dir.path().join(HOLD_NAME);
            std::fs::write(&path, "").unwrap();
            pool.submit(Job::Load { path, base: "develop".into() });
        }
        let key = CacheKey { worktree: "a".into(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        pool.submit(Job::Highlight { key, file_index: 0, diffs: Arc::new(vec![]) });
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Highlighted { .. }));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(HELD.load(Ordering::SeqCst), MAX_CONCURRENT_LOADS);
        for dir in &dirs {
            std::fs::remove_file(dir.path().join(HOLD_NAME)).unwrap();
        }
        for _ in &dirs {
            assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Loaded { .. }));
        }
    }

    fn panicked_on_worker<T: std::fmt::Debug>(result: Result<T, GitError>) {
        let text = format!("internal error: test panic on {WORKER_THREAD}");
        assert_eq!(result.unwrap_err(), GitError::Failed(text));
    }

    #[test]
    fn pool_runs_submitted_jobs_and_reports_errors_as_messages() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(2, tx);
        pool.submit(Job::Load { path: "/definitely/not/a/repo".into(), base: "develop".into() });
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Loaded { result: Err(_), .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn highlighting_a_missing_file_gives_an_empty_result() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(1, tx);
        let key = CacheKey { worktree: "a".into(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        pool.submit(Job::Highlight { key, file_index: 0, diffs: Arc::new(vec![]) });
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Msg::Highlighted { .. }));
    }

    fn base_of(job: Option<Job>) -> String {
        match job {
            Some(Job::Load { base, .. }) => base,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_invalidated_load_is_discarded_even_with_the_same_waiting_base() {
        let mut loads = Loads::default();
        loads.request("a".into(), "main".into());
        loads.invalidate(Path::new("a"));
        assert!(loads.request("a".into(), "main".into()).is_none());
        let done = loads.finished(Path::new("a"));
        assert!(done.superseded);
        assert_eq!(base_of(done.next), "main");
        assert!(!loads.finished(Path::new("a")).superseded);
    }

    #[test]
    fn invalidating_retires_waiting_work_from_the_previous_identity() {
        let mut loads = Loads::default();
        loads.request("a".into(), "main".into());
        loads.request("a".into(), "develop".into());
        loads.invalidate(Path::new("a"));
        let done = loads.finished(Path::new("a"));
        assert!(done.superseded && done.next.is_none());
        assert!(loads.request("a".into(), "main".into()).is_some());
    }

    #[test]
    fn only_one_load_runs_per_worktree() {
        let mut loads = Loads::default();
        assert!(loads.request("a".into(), "develop".into()).is_some());
        assert!(loads.request("a".into(), "develop".into()).is_none());
        assert!(loads.request("b".into(), "develop".into()).is_some());
    }

    #[test]
    fn the_newest_waiting_request_runs_after_the_current_load() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "main".into());
        assert_eq!(base_of(loads.finished("a".as_ref()).next), "main");
        // The waiting job is now running, so another request waits for it.
        assert!(loads.request("a".into(), "develop".into()).is_none());
    }

    #[test]
    fn a_waiting_request_for_the_same_base_keeps_the_finished_result() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "develop".into());
        let done = loads.finished("a".as_ref());
        assert!(!done.superseded);
        assert_eq!(base_of(done.next), "develop");
    }

    #[test]
    fn a_waiting_request_for_another_base_supersedes_the_finished_result() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "main".into());
        let done = loads.finished("a".as_ref());
        assert!(done.superseded);
        assert_eq!(base_of(done.next), "main");
    }

    #[test]
    fn a_base_changed_and_changed_back_keeps_the_finished_result() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "main".into());
        loads.request("a".into(), "develop".into());
        let done = loads.finished("a".as_ref());
        assert!(!done.superseded);
        assert_eq!(base_of(done.next), "develop");
    }

    #[test]
    fn a_promoted_job_is_compared_using_its_own_base() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "main".into());
        assert!(loads.finished("a".as_ref()).superseded);

        // `main` is running now, so `develop` is the change here.
        loads.request("a".into(), "develop".into());
        assert!(loads.finished("a".as_ref()).superseded);
        loads.request("a".into(), "develop".into());
        assert!(!loads.finished("a".as_ref()).superseded);
    }

    #[test]
    fn a_finished_load_with_nothing_waiting_frees_the_worktree() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        let done = loads.finished("a".as_ref());
        assert!(done.next.is_none());
        assert!(!done.superseded);
        assert!(loads.request("a".into(), "develop".into()).is_some());
    }

    #[test]
    fn finishing_an_unknown_worktree_does_nothing() {
        let done = Loads::default().finished("a".as_ref());
        assert!(done.next.is_none());
        assert!(!done.superseded);
    }

    #[test]
    fn a_change_during_a_load_is_reported_when_it_finishes() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.mark_changed("a".as_ref());
        assert!(loads.finished("a".as_ref()).changed);
    }

    #[test]
    fn a_finished_load_with_no_change_reports_none() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        assert!(!loads.finished("a".as_ref()).changed);
    }

    #[test]
    fn a_change_with_no_load_running_is_forgotten() {
        let mut loads = Loads::default();
        loads.mark_changed("a".as_ref());
        loads.request("a".into(), "develop".into());
        assert!(!loads.finished("a".as_ref()).changed);
    }

    #[test]
    fn a_change_to_one_worktree_leaves_another_alone() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("b".into(), "develop".into());
        loads.mark_changed("a".as_ref());
        assert!(!loads.finished("b".as_ref()).changed);
    }

    #[test]
    fn a_waiting_load_includes_the_change_and_clears_the_mark() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.mark_changed("a".as_ref());
        loads.request("a".into(), "develop".into());
        let done = loads.finished("a".as_ref());
        assert!(done.next.is_some());
        assert!(!done.changed);
        // The promoted load started after the change.
        assert!(!loads.finished("a".as_ref()).changed);
    }

    #[test]
    fn a_change_during_a_promoted_load_is_reported() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.request("a".into(), "develop".into());
        loads.finished("a".as_ref());
        loads.mark_changed("a".as_ref());
        assert!(loads.finished("a".as_ref()).changed);
    }

    #[test]
    fn a_new_load_does_not_inherit_a_finished_loads_change() {
        let mut loads = Loads::default();
        loads.request("a".into(), "develop".into());
        loads.mark_changed("a".as_ref());
        loads.finished("a".as_ref());
        loads.request("a".into(), "develop".into());
        assert!(!loads.finished("a".as_ref()).changed);
    }

    fn key(fingerprint: u64) -> CacheKey {
        CacheKey { worktree: "a".into(), base_commit: "b".into(), head: "h".into(), fingerprint }
    }

    /// Starts a probe of `a` against `key`, checking that one was due.
    fn start_probe(probes: &mut Probes, key: CacheKey) {
        assert_eq!(probed_key(probes.on_change(false, Some(key.clone()))), key);
    }

    fn probed_key(reaction: Reaction) -> CacheKey {
        match reaction {
            Reaction::Submit(Job::Probe { key }) => key,
            other => panic!("unexpected {other:?}"),
        }
    }

    fn next_key(job: Option<Job>) -> Option<CacheKey> {
        match job {
            Some(Job::Probe { key }) => Some(key),
            None => None,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_change_during_a_load_waits_for_the_load() {
        let mut probes = Probes::default();
        assert!(matches!(probes.on_change(true, Some(key(1))), Reaction::Nothing));
        assert!(matches!(probes.on_change(true, None), Reaction::Nothing));
        // It started nothing, so a probe can still run.
        start_probe(&mut probes, key(1));
    }

    #[test]
    fn a_change_with_no_key_has_nothing_to_compare_so_raises_the_badge() {
        assert!(matches!(Probes::default().on_change(false, None), Reaction::Badge));
    }

    #[test]
    fn only_one_probe_runs_per_worktree() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        for _ in 0..50 {
            assert!(matches!(probes.on_change(false, Some(key(1))), Reaction::Nothing));
        }
        let other = CacheKey { worktree: "b".into(), ..key(1) };
        assert_eq!(probed_key(probes.on_change(false, Some(other.clone()))), other);
    }

    #[test]
    fn a_burst_during_a_probe_runs_exactly_one_more() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        for _ in 0..50 {
            probes.on_change(false, Some(key(1)));
        }
        assert_eq!(next_key(probes.finished("a".as_ref(), false, Some(key(1))).next), Some(key(1)));
        // The re-run counts as running, and nothing waits behind it.
        assert!(matches!(probes.on_change(false, Some(key(1))), Reaction::Nothing));
        assert_eq!(next_key(probes.finished("a".as_ref(), false, Some(key(1))).next), Some(key(1)));
        assert!(next_key(probes.finished("a".as_ref(), false, Some(key(1))).next).is_none());
        start_probe(&mut probes, key(1));
    }

    #[test]
    fn a_finished_probe_with_nothing_waiting_frees_the_worktree() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        assert!(probes.finished("a".as_ref(), false, Some(key(1))).next.is_none());
        start_probe(&mut probes, key(1));
    }

    #[test]
    fn a_probe_raises_the_badge_only_for_a_change() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        assert!(!probes.finished("a".as_ref(), false, Some(key(1))).raise);
        start_probe(&mut probes, key(1));
        assert!(probes.finished("a".as_ref(), true, Some(key(1))).raise);
    }

    #[test]
    fn a_probe_of_a_key_since_replaced_says_nothing() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        // A refresh landed during the probe, which compared against the old diff.
        assert!(!probes.finished("a".as_ref(), true, Some(key(2))).raise);
    }

    #[test]
    fn a_waiting_probe_compares_against_the_newest_key() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        probes.on_change(false, Some(key(1)));
        assert_eq!(next_key(probes.finished("a".as_ref(), false, Some(key(2))).next), Some(key(2)));
        assert!(probes.finished("a".as_ref(), true, Some(key(2))).raise);
    }

    #[test]
    fn a_removed_worktree_ends_its_probes() {
        let mut probes = Probes::default();
        start_probe(&mut probes, key(1));
        probes.on_change(false, Some(key(1)));
        let done = probes.finished("a".as_ref(), true, None);
        assert!(!done.raise);
        assert!(done.next.is_none());
        start_probe(&mut probes, key(1));
    }

    #[test]
    fn finishing_an_unknown_probe_does_nothing() {
        let done = Probes::default().finished("a".as_ref(), true, Some(key(1)));
        assert!(!done.raise);
        assert!(done.next.is_none());
    }

    #[test]
    fn only_one_worktree_list_runs_at_a_time() {
        let mut lists = Lists::default();
        assert!(lists.request());
        assert!(!lists.request());
    }

    #[test]
    fn a_burst_during_a_list_runs_exactly_one_more_after_it() {
        let mut lists = Lists::default();
        lists.request();
        for _ in 0..50 {
            assert!(!lists.request());
        }
        assert!(lists.finished());
        // The re-run counts as running, and nothing waits behind it.
        assert!(!lists.request());
        assert!(lists.finished());
        assert!(!lists.finished());
        assert!(lists.request());
    }

    #[test]
    fn a_finished_list_with_nothing_waiting_allows_a_new_one() {
        let mut lists = Lists::default();
        lists.request();
        assert!(!lists.finished());
        assert!(lists.request());
    }

    #[test]
    fn a_stray_finish_leaves_nothing_running() {
        let mut lists = Lists::default();
        assert!(!lists.finished());
        assert!(lists.request());
    }

    #[test]
    fn loads_report_whether_one_is_running() {
        let mut loads = Loads::default();
        assert!(!loads.is_running("a".as_ref()));
        loads.request("a".into(), "develop".into());
        assert!(loads.is_running("a".as_ref()));
        loads.finished("a".as_ref());
        assert!(!loads.is_running("a".as_ref()));
    }

    #[test]
    fn guarded_passes_results_through() {
        assert_eq!(guarded(|| Ok(1)), Ok(1));
        assert_eq!(guarded(|| Err::<u8, _>(GitError::NoCommits)), Err(GitError::NoCommits));
    }

    #[test]
    fn guarded_turns_a_panic_into_an_error_holding_its_message() {
        let literal = guarded(|| -> Result<(), GitError> { panic!("boom") });
        assert_eq!(literal, Err(GitError::Failed("internal error: boom".into())));
        let owned = guarded(|| -> Result<(), GitError> { std::panic::panic_any("boom".to_string()) });
        assert_eq!(owned, Err(GitError::Failed("internal error: boom".into())));
    }

    #[test]
    fn a_panic_without_a_usable_message_gets_a_generic_one() {
        assert_eq!(panic_text(&7), PANIC_NO_MESSAGE);
        assert_eq!(panic_text(&""), PANIC_NO_MESSAGE);
        assert_eq!(panic_text(&"   "), PANIC_NO_MESSAGE);
    }

    #[test]
    fn a_panic_message_stays_short_enough_for_a_worktree_row() {
        assert_eq!(panic_text(&"first line\nsecond line"), "internal error: first line");
        let long = "é".repeat(500);
        assert_eq!(panic_text(&long).chars().count(), "internal error: ".len() + PANIC_MESSAGE_CHARS);
    }

    #[test]
    fn workers_run_on_a_thread_with_the_worker_name() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(1, tx);
        pool.submit(Job::ListBranches { cwd: PANIC_PATH.into() });
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Msg::Branches(result) => panicked_on_worker(result),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn every_panicking_job_still_reports_and_the_worker_survives() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(1, tx);
        let next = || rx.recv_timeout(Duration::from_secs(5)).unwrap();

        pool.submit(Job::ListWorktrees { cwd: PANIC_PATH.into() });
        match next() {
            Msg::Worktrees(result) => panicked_on_worker(result),
            other => panic!("unexpected {other:?}"),
        }
        pool.submit(Job::ListBranches { cwd: PANIC_PATH.into() });
        match next() {
            Msg::Branches(result) => panicked_on_worker(result),
            other => panic!("unexpected {other:?}"),
        }
        pool.submit(Job::Load { path: PANIC_PATH.into(), base: "develop".into() });
        match next() {
            Msg::Loaded { path, result } => {
                assert_eq!(path, Path::new(PANIC_PATH));
                panicked_on_worker(result);
            }
            other => panic!("unexpected {other:?}"),
        }
        let key = CacheKey { worktree: PANIC_PATH.into(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        pool.submit(Job::Highlight { key: key.clone(), file_index: 3, diffs: Arc::new(vec![]) });
        match next() {
            Msg::Highlighted { key: got, file_index, .. } => assert_eq!((got, file_index), (key.clone(), 3)),
            other => panic!("unexpected {other:?}"),
        }
        pool.submit(Job::Probe { key });
        match next() {
            // A failed probe counts as a change.
            Msg::Probed { path, changed } => assert_eq!((path.as_path(), changed), (Path::new(PANIC_PATH), true)),
            other => panic!("unexpected {other:?}"),
        }

        // This pool has one thread, so an answer proves it outlived the panics.
        pool.submit(Job::Load { path: "/definitely/not/a/repo".into(), base: "develop".into() });
        match next() {
            Msg::Loaded { result: Err(e), .. } => assert!(!e.to_string().contains("panic")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_panicking_load_frees_its_worktree() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(1, tx);
        let mut loads = Loads::default();
        let path = PathBuf::from(PANIC_PATH);
        pool.submit(loads.request(path.clone(), "develop".into()).unwrap());
        assert!(loads.request(path.clone(), "develop".into()).is_none());

        for _ in 0..2 {
            let Msg::Loaded { path, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else {
                panic!("not a load")
            };
            if let Some(job) = loads.finished(&path).next {
                pool.submit(job);
            }
        }
        assert!(loads.request(path, "develop".into()).is_some());
    }
}
