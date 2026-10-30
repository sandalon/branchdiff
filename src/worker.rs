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

const PANIC_MESSAGE_CHARS: usize = 120;
const PANIC_NO_MESSAGE: &str = "internal error in a background job";

#[derive(Debug)]
pub enum Job {
    ListWorktrees { cwd: PathBuf },
    Load { path: PathBuf, base: String },
    ListBranches { cwd: PathBuf },
    Highlight { key: CacheKey, file_index: usize, diffs: Diffs },
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
    Loaded { path: PathBuf, result: Result<Loaded, GitError> },
    Branches(Result<Vec<String>, GitError>),
    Highlighted { key: CacheKey, file_index: usize, lines: crate::highlight::HighlightedFile },
    Stale(PathBuf),
    WorktreesChanged,
}

struct Running {
    base: String,
    waiting_base: Option<String>,
}

/// What to do about a load that has just finished.
#[derive(Debug, Default)]
pub struct Finished {
    /// The waiting job, which now counts as running. Submit it.
    pub next: Option<Job>,
    /// The result is for a base the user has left, so drop it. Otherwise a late
    /// `BaseMissing` would reopen the picker. A result for the same base is
    /// valid and newer than what's on screen, so it's shown until `next` lands.
    pub superseded: bool,
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
                self.running.insert(path.clone(), Running { base: base.clone(), waiting_base: None });
                Some(Job::Load { path, base })
            }
        }
    }

    /// Call for every finished load.
    pub fn finished(&mut self, path: &Path) -> Finished {
        let Some(running) = self.running.get_mut(path) else {
            return Finished::default();
        };
        let Some(base) = running.waiting_base.take() else {
            self.running.remove(path);
            return Finished::default();
        };
        let superseded = base != running.base;
        running.base = base.clone();
        Finished { next: Some(Job::Load { path: path.to_path_buf(), base }), superseded }
    }
}

pub struct Pool {
    jobs: Sender<Job>,
}

impl Pool {
    pub fn new(threads: usize, msgs: Sender<Msg>) -> Self {
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
        Self { jobs }
    }

    pub fn submit(&self, job: Job) {
        let _ = self.jobs.send(job);
    }
}

fn run(job: Job) -> Msg {
    match job {
        Job::ListWorktrees { cwd } => Msg::Worktrees(guarded(|| {
            #[cfg(test)]
            tests::panic_if_marked(&cwd);
            git::list_worktrees(&cwd)
        })),
        Job::ListBranches { cwd } => Msg::Branches(guarded(|| {
            #[cfg(test)]
            tests::panic_if_marked(&cwd);
            git::branches(&cwd)
        })),
        Job::Load { path, base } => {
            let result = guarded(|| {
                #[cfg(test)]
                tests::panic_if_marked(&path);
                load(&path, &base)
            });
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
            Msg::Highlighted { key, file_index, lines }
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

/// One short line for a worktree row. Once `main` installs the panic hook that
/// skips workers (Task 16), this is all anyone sees of a worker panic.
fn panic_text(payload: &(dyn Any + Send)) -> String {
    let message = payload.downcast_ref::<&str>().copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .and_then(|text| text.lines().next())
        .filter(|line| !line.trim().is_empty());
    match message {
        Some(line) => format!("internal error: {}", line.chars().take(PANIC_MESSAGE_CHARS).collect::<String>()),
        None => PANIC_NO_MESSAGE.to_string(),
    }
}

fn load(path: &Path, base: &str) -> Result<Loaded, GitError> {
    let base_commit = git::merge_base(path, base)?;
    let head = String::from_utf8_lossy(&git::run(path, ["rev-parse", "HEAD"])?.stdout).trim().to_string();
    let fingerprint = git::fingerprint(path)?;
    let diffs = Arc::new(crate::diff::load(path, &base_commit)?);
    let key = CacheKey { worktree: path.to_path_buf(), base_commit, head, fingerprint };
    Ok(Loaded { key, diffs, op: git::in_progress_op(path) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const PANIC_PATH: &str = "/branchdiff-test-panic";

    /// Called inside each job's work in `run`, so that removing `guarded` from
    /// an arm kills the worker. No real job can be made to panic on demand.
    pub(super) fn panic_if_marked(path: &Path) {
        if path == Path::new(PANIC_PATH) {
            panic!("test panic on {}", thread::current().name().unwrap_or("an unnamed thread"));
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
            Msg::Highlighted { key: got, file_index, .. } => assert_eq!((got, file_index), (key, 3)),
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
            let Msg::Loaded { path, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else { panic!("not a load") };
            if let Some(job) = loads.finished(&path).next {
                pool.submit(job);
            }
        }
        assert!(loads.request(path, "develop".into()).is_some());
    }
}
