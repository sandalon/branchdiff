use crate::cache::{CacheKey, Diffs};
use crate::git::{self, GitError, Worktree};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::any::Any;
use std::collections::HashMap;
use std::panic::{catch_unwind, UnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

/// The name of every worker thread. A panic hook can use it to tell a worker's
/// panic, which `guarded` contains, from one that ends the program.
pub const WORKER_THREAD: &str = "branchdiff-worker";

const PANIC_MESSAGE_CHARS: usize = 120;
const PANIC_NO_MESSAGE: &str = "internal error in a background job";

#[derive(Debug)]
pub enum Job {
    ListWorktrees { cwd: PathBuf },
    Load { path: PathBuf, base: String, generation: u64 },
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
    Loaded { path: PathBuf, generation: u64, result: Result<Loaded, GitError> },
    Branches(Result<Vec<String>, GitError>),
    Highlighted { key: CacheKey, file_index: usize, lines: crate::highlight::HighlightedFile },
    Stale(PathBuf),
    WorktreesChanged,
}

/// Tracks the newest request per worktree so older results can be dropped.
#[derive(Default)]
pub struct Generations {
    counter: AtomicU64,
    latest: Mutex<HashMap<PathBuf, u64>>,
}

impl Generations {
    pub fn next(&self, path: &Path) -> u64 {
        let g = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        self.latest.lock().unwrap().insert(path.to_path_buf(), g);
        g
    }

    pub fn is_current(&self, path: &Path, generation: u64) -> bool {
        self.latest.lock().unwrap().get(path) == Some(&generation)
    }
}

/// Allows one running load per worktree, because a load can take seconds and
/// repeated refreshes would otherwise run side by side. A request made during
/// a load replaces any waiting one, so the newest always runs next.
#[derive(Default)]
pub struct Loads {
    running: HashMap<PathBuf, Option<Job>>,
}

impl Loads {
    /// Answers the job to submit now, if the worktree has no load running.
    pub fn request(&mut self, gens: &Generations, path: PathBuf, base: String) -> Option<Job> {
        let generation = gens.next(&path);
        let job = Job::Load { path: path.clone(), base, generation };
        match self.running.get_mut(&path) {
            Some(waiting) => {
                *waiting = Some(job);
                None
            }
            None => {
                self.running.insert(path, None);
                Some(job)
            }
        }
    }

    /// Call for every finished load, stale or not. Answers the job to submit next.
    pub fn finished(&mut self, path: &Path) -> Option<Job> {
        let job = self.running.remove(path).flatten()?;
        self.running.insert(path.to_path_buf(), None);
        Some(job)
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
        Job::Load { path, base, generation } => {
            let result = guarded(|| {
                #[cfg(test)]
                tests::panic_if_marked(&path);
                load(&path, &base)
            });
            Msg::Loaded { path, generation, result }
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
    fn newest_job_wins_when_superseded() {
        let gens = Generations::default();
        let first = gens.next("wt".as_ref());
        let second = gens.next("wt".as_ref());
        assert!(!gens.is_current("wt".as_ref(), first));
        assert!(gens.is_current("wt".as_ref(), second));
    }

    #[test]
    fn generations_are_per_worktree() {
        let gens = Generations::default();
        let a = gens.next("a".as_ref());
        gens.next("b".as_ref());
        assert!(gens.is_current("a".as_ref(), a));
    }

    #[test]
    fn pool_runs_submitted_jobs_and_reports_errors_as_messages() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(2, tx);
        pool.submit(Job::Load { path: "/definitely/not/a/repo".into(), base: "develop".into(), generation: 1 });
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

    #[test]
    fn only_one_load_runs_per_worktree() {
        let (gens, mut loads) = (Generations::default(), Loads::default());
        assert!(loads.request(&gens, "a".into(), "develop".into()).is_some());
        assert!(loads.request(&gens, "a".into(), "develop".into()).is_none());
        assert!(loads.request(&gens, "b".into(), "develop".into()).is_some());
    }

    #[test]
    fn the_newest_waiting_request_runs_after_the_current_load() {
        let (gens, mut loads) = (Generations::default(), Loads::default());
        loads.request(&gens, "a".into(), "develop".into());
        loads.request(&gens, "a".into(), "develop".into());
        loads.request(&gens, "a".into(), "main".into());
        match loads.finished("a".as_ref()) {
            Some(Job::Load { base, generation, .. }) => {
                assert_eq!(base, "main");
                assert!(gens.is_current("a".as_ref(), generation));
            }
            other => panic!("unexpected {other:?}"),
        }
        // The waiting job is now running, so another request waits for it.
        assert!(loads.request(&gens, "a".into(), "develop".into()).is_none());
    }

    #[test]
    fn a_finished_load_with_nothing_waiting_frees_the_worktree() {
        let (gens, mut loads) = (Generations::default(), Loads::default());
        loads.request(&gens, "a".into(), "develop".into());
        assert!(loads.finished("a".as_ref()).is_none());
        assert!(loads.request(&gens, "a".into(), "develop".into()).is_some());
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
        pool.submit(Job::Load { path: PANIC_PATH.into(), base: "develop".into(), generation: 9 });
        match next() {
            Msg::Loaded { path, generation, result } => {
                assert_eq!(path, Path::new(PANIC_PATH));
                assert_eq!(generation, 9);
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
        pool.submit(Job::Load { path: "/definitely/not/a/repo".into(), base: "develop".into(), generation: 10 });
        match next() {
            Msg::Loaded { generation: 10, result: Err(e), .. } => assert!(!e.to_string().contains("panic")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_panicking_load_frees_its_worktree() {
        let (tx, rx) = unbounded();
        let pool = Pool::new(1, tx);
        let (gens, mut loads) = (Generations::default(), Loads::default());
        let path = PathBuf::from(PANIC_PATH);
        pool.submit(loads.request(&gens, path.clone(), "develop".into()).unwrap());
        assert!(loads.request(&gens, path.clone(), "develop".into()).is_none());

        for _ in 0..2 {
            let Msg::Loaded { path, .. } = rx.recv_timeout(Duration::from_secs(5)).unwrap() else { panic!("not a load") };
            if let Some(job) = loads.finished(&path) {
                pool.submit(job);
            }
        }
        assert!(loads.request(&gens, path, "develop".into()).is_some());
    }
}
