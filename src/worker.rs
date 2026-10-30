use crate::cache::{CacheKey, Diffs};
use crate::git::{self, GitError, Worktree};
use crossbeam_channel::{unbounded, Receiver, Sender};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

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
            thread::spawn(move || {
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
        Job::ListWorktrees { cwd } => Msg::Worktrees(git::list_worktrees(&cwd)),
        Job::ListBranches { cwd } => Msg::Branches(git::branches(&cwd)),
        Job::Load { path, base, generation } => {
            let result = load(&path, &base);
            Msg::Loaded { path, generation, result }
        }
        Job::Highlight { key, file_index, diffs } => {
            let lines = diffs.get(file_index).map(crate::highlight::highlight_file).unwrap_or_default();
            Msg::Highlighted { key, file_index, lines }
        }
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
}
