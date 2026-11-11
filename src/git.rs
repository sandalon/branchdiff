use std::cmp::Reverse;
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    GitMissing,
    NotARepo,
    BaseMissing(String),
    NoMergeBase,
    NoCommits,
    DiffTooLarge,
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::GitMissing => write!(f, "git was not found on PATH"),
            GitError::NotARepo => write!(f, "not inside a git repository"),
            GitError::BaseMissing(b) => write!(f, "base branch '{b}' not found"),
            GitError::NoMergeBase => write!(f, "no common history with the base branch"),
            GitError::NoCommits => write!(f, "this branch has no commits yet"),
            GitError::DiffTooLarge => write!(f, "Diff exceeds the memory limit; no changes are shown. Review it with git diff or reduce its size, then press r."),
            GitError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    pub toplevel: PathBuf,
    pub common_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

/// Variables that must not reach our git commands. Most are repository-scoped
/// values git sets for hooks and aliases, which would point every command at
/// the wrong repository. `GIT_DIFF_OPTS` and `GIT_EXTERNAL_DIFF` would change
/// diff output instead.
pub const REPO_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
    "GIT_DIFF_OPTS",
    "GIT_EXTERNAL_DIFF",
];

#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

fn command<I, S>(cwd: &Path, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // A repository's own config could name an fsmonitor hook, which `git
    // status` and `git diff` would run as the user just for opening it. Git's
    // built-in daemon runs no repository code, and without it a large checkout
    // on Windows can take minutes to scan, so a `true` setting is kept. The
    // value is always pinned on the command line, so a hook configured after
    // the check still never runs.
    // An empty value also disables hooks on older Git versions, which treat
    // even the string "false" as a hook command.
    let fsmonitor = if builtin_fsmonitor(cwd) { "core.fsmonitor=true" } else { "core.fsmonitor=" };
    let mut cmd = base_command(cwd);
    cmd.args(["-c", fsmonitor]).args(args);
    cmd
}

/// Whether git's built-in fsmonitor daemon is enabled for `cwd`, remembered
/// per directory since every command would otherwise pay for the lookup.
fn builtin_fsmonitor(cwd: &Path) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(&on) = seen.lock().unwrap_or_else(|e| e.into_inner()).get(cwd) {
        return on;
    }
    let on = builtin_fsmonitor_configured(cwd);
    seen.lock().unwrap_or_else(|e| e.into_inner()).insert(cwd.to_path_buf(), on);
    on
}

/// Git has the built-in daemon only on Windows and macOS. Elsewhere a `true`
/// setting does nothing useful, so it is never passed on.
fn builtin_fsmonitor_configured(cwd: &Path) -> bool {
    if !cfg!(any(windows, target_os = "macos")) || !cwd.is_dir() || !git_has_builtin_fsmonitor(cwd) {
        return false;
    }
    // A hook path is not a boolean, so `--type=bool` fails for it. `git
    // config` itself never consults fsmonitor.
    let mut cmd = base_command(cwd);
    cmd.args(["config", "--type=bool", "--get", "core.fsmonitor"]);
    match exec(cwd, cmd, None, 4096, Duration::from_secs(10)) {
        Ok(Exec::Done(out)) => out.code == Some(0) && out.stdout.trim_ascii() == b"true",
        _ => false,
    }
}

fn git_has_builtin_fsmonitor(cwd: &Path) -> bool {
    use std::sync::OnceLock;
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut cmd = base_command(cwd);
        cmd.arg("--version");
        match exec(cwd, cmd, None, 4096, Duration::from_secs(10)) {
            Ok(Exec::Done(out)) if out.code == Some(0) => version_has_builtin_fsmonitor(&out.stdout),
            _ => false,
        }
    })
}

fn version_has_builtin_fsmonitor(version: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(version) else { return false };
    let Some(version) = text.trim().strip_prefix("git version ") else { return false };
    let mut parts = version.split('.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else { return false };
    match (major.parse::<u32>(), minor.parse::<u32>()) {
        // The built-in daemon shipped in Git 2.37. Older Git may interpret
        // "true" as a hook pathname rather than a boolean.
        (Ok(major), Ok(minor)) => (major, minor) >= (2, 37),
        _ => false,
    }
}

/// Every git command's shared setup, except for `core.fsmonitor`.
fn base_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    for var in REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(cwd)
        // Keeps non-ASCII paths readable. Names with tabs or quotes are still
        // C-quoted, and the patch parser handles those.
        .args(["-c", "core.quotePath=false"])
        // Stops `git diff` refreshing the index, which takes `index.lock` and
        // can race a concurrent `git add`. GIT_OPTIONAL_LOCKS does not cover it.
        .args(["-c", "diff.autoRefreshIndex=false"])
        // Stops `git status` rewriting the index, which the watcher would see.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // `merge_base` matches on stderr text, so it must not be translated.
        .env("LC_ALL", "C")
        // A prompt would write over the TUI and hang the worker.
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn spawn_error(cwd: &Path, e: std::io::Error) -> GitError {
    match e.kind() {
        // Spawning also fails with NotFound if `cwd` vanished after the check above.
        std::io::ErrorKind::NotFound if !cwd.is_dir() => cwd_gone(cwd),
        std::io::ErrorKind::NotFound => GitError::GitMissing,
        _ => GitError::Failed(e.to_string()),
    }
}

/// Longest a git command may run. Generous, since a cold `git status` of a
/// very large checkout can take a while, but a hung hook or filter must not
/// hold a worker for good.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Most stdout kept from a git command other than `diff`, which has its own
/// limit. Status and file listings of very large checkouts stay well under it.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Stderr beyond this is read and discarded, so a noisy hook can neither fill
/// memory nor block git on a full pipe.
pub const MAX_STDERR_BYTES: usize = 64 * 1024;

pub fn run<I, S>(cwd: &Path, args: I) -> Result<Output, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_limited(cwd, args, MAX_OUTPUT_BYTES, GIT_TIMEOUT)
}

fn run_limited<I, S>(cwd: &Path, args: I, limit: usize, timeout: Duration) -> Result<Output, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match exec(cwd, command(cwd, args), None, limit, timeout)? {
        Exec::Done(out) => Ok(out),
        Exec::TooLarge => Err(GitError::Failed(format!("git output exceeded {} MiB", limit / (1024 * 1024)))),
    }
}

enum Exec {
    Done(Output),
    TooLarge,
}

/// Runs `cmd` with `input` on its stdin, or none, keeping at most `limit`
/// bytes of stdout and `MAX_STDERR_BYTES` of stderr. A command that writes
/// more than `limit` or outlives `timeout` is killed, with anything it
/// started on Unix, and reaped. Readers left blocked by a process that
/// escaped the kill hold only bounded buffers and end when it closes the pipe.
fn exec(
    cwd: &Path,
    mut cmd: Command,
    input: Option<Vec<u8>>,
    limit: usize,
    timeout: Duration,
) -> Result<Exec, GitError> {
    if !cwd.is_dir() {
        return Err(cwd_gone(cwd));
    }
    let deadline = Instant::now() + timeout;
    // Without a stdin of its own, git would share the terminal with the TUI.
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so that a kill reaches hooks and filters too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().map_err(|e| spawn_error(cwd, e))?;
    if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
        // Written from its own thread, because git can fill its stdout pipe
        // while we are still filling its stdin.
        thread::spawn(move || stdin.write_all(&bytes));
    }
    let stderr = read_prefix(child.stderr.take().expect("stderr was piped"), MAX_STDERR_BYTES, false);
    let stdout = read_prefix(child.stdout.take().expect("stdout was piped"), limit.saturating_add(1), true);
    let stdout = match stdout.recv_deadline(deadline) {
        Ok(Ok(bytes)) if bytes.len() > limit => {
            kill(&mut child);
            return Ok(Exec::TooLarge);
        }
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            kill(&mut child);
            return Err(GitError::Failed(e.to_string()));
        }
        Err(_) => return Err(timed_out(&mut child, timeout)),
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => return Err(timed_out(&mut child, timeout)),
            // Stdout has closed, so git is all but done: this is rarely reached.
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(GitError::Failed(e.to_string())),
        }
    };
    // A hook that outlives git could hold stderr open; its text isn't needed.
    let stderr = stderr.recv_deadline(deadline.min(Instant::now() + Duration::from_millis(100)));
    let stderr = stderr.ok().and_then(Result::ok).unwrap_or_default();
    Ok(Exec::Done(Output { code: status.code(), stdout, stderr: String::from_utf8_lossy(&stderr).trim().to_string() }))
}

/// Reads `pipe` on its own thread, keeping its first `keep` bytes. With `stop`
/// it stops there, and otherwise drains the rest so the writer never blocks.
fn read_prefix(
    mut pipe: impl Read + Send + 'static,
    keep: usize,
    stop: bool,
) -> crossbeam_channel::Receiver<std::io::Result<Vec<u8>>> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0; 8192];
        let result = loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break Ok(kept),
                Ok(n) => {
                    let room = keep - kept.len();
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                    if stop && n >= room {
                        break Ok(kept);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        let _ = tx.send(result);
    });
    rx
}

fn timed_out(child: &mut std::process::Child, timeout: Duration) -> GitError {
    kill(child);
    GitError::Failed(format!("git timed out after {}s", timeout.as_secs_f32()))
}

/// Kills `child` and everything in its process group, then reaps it.
fn kill(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall. The group was created for this child, which
        // has not been reaped yet, so its id cannot have been reused.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn cwd_gone(cwd: &Path) -> GitError {
    GitError::Failed(format!("{} no longer exists", cwd.display()))
}

fn failed(out: Output) -> GitError {
    if out.stderr.is_empty() {
        GitError::Failed("git failed without an error message".to_string())
    } else {
        GitError::Failed(out.stderr)
    }
}

fn ok_stdout(out: Output) -> Result<String, GitError> {
    if out.code == Some(0) {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(failed(out))
    }
}

pub fn discover(cwd: &Path) -> Result<RepoInfo, GitError> {
    let out = run(cwd, ["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"])?;
    if out.code != Some(0) {
        // Other failures, such as "detected dubious ownership", need their own text.
        return Err(if out.stderr.contains("not a git repository") { GitError::NotARepo } else { failed(out) });
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    match (lines.next(), lines.next()) {
        (Some(top), Some(common)) => Ok(RepoInfo { toplevel: top.into(), common_dir: common.into() }),
        _ => Err(GitError::NotARepo),
    }
}

pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>, GitError> {
    let text = ok_stdout(run(cwd, ["worktree", "list", "--porcelain", "-z"])?)?;
    let mut list = parse_worktrees(&text);
    // Git lists the main worktree first, and as its git dir if that lives outside
    // it, as with `--separate-git-dir` or in a submodule. Bare entries are gone by
    // now, so a directory with no `.git` here can only be such a git dir.
    if list.first().is_some_and(|w| w.path.is_dir() && !w.path.join(".git").exists()) {
        match main_worktree_root(cwd, &list[0].path) {
            Some(root) => list[0].path = root,
            None => drop(list.remove(0)),
        }
    }
    for w in list.iter_mut().filter(|w| w.branch.is_none()) {
        w.branch = rebasing_branch(&w.path);
    }
    // Git's native order always puts the main worktree first. For review it is
    // more useful to put the worktree with the newest commit or in-progress
    // edit first. The sort is stable, so equal or unavailable times retain
    // Git's deterministic order.
    let modified = all_modified(&list);
    let mut keyed: Vec<_> = modified.into_iter().zip(list).collect();
    keyed.sort_by_key(|(time, _)| Reverse(*time));
    Ok(keyed.into_iter().map(|(_, w)| w).collect())
}

/// Most `git status` runs that sorting the list starts at once.
const MAX_CONCURRENT_STATUS: usize = 8;

/// `worktree_modified` for each worktree, in order. Each one runs `git status`,
/// which takes seconds in a very large checkout, so they run side by side
/// rather than holding back the list one after another.
fn all_modified(list: &[Worktree]) -> Vec<SystemTime> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let threads = thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, MAX_CONCURRENT_STATUS);
    let times = Mutex::new(vec![UNIX_EPOCH; list.len()]);
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads.min(list.len()) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(worktree) = list.get(index) else { break };
                let time = worktree_modified(worktree);
                times.lock().unwrap_or_else(|e| e.into_inner())[index] = time;
            });
        }
    });
    times.into_inner().unwrap_or_else(|e| e.into_inner())
}

/// The newest change represented by a worktree: its HEAD commit, or the mtime
/// of a dirty path. Looking only at dirty paths avoids walking an entire large
/// checkout while still noticing edits that have not been committed yet.
fn worktree_modified(worktree: &Worktree) -> SystemTime {
    let mut newest = commit_time(worktree).unwrap_or(UNIX_EPOCH);
    let Ok(status) =
        run(&worktree.path, ["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=none"])
    else {
        return newest;
    };
    if status.code != Some(0) {
        return newest;
    }
    for path in dirty_paths(&status.stdout) {
        let path = worktree.path.join(path);
        // Creating, renaming, or deleting a path updates its parent even when
        // the path's own mtime is old or it no longer has metadata.
        for candidate in std::iter::once(path.as_path()).chain(path.parent()) {
            if let Ok(modified) = std::fs::symlink_metadata(candidate).and_then(|metadata| metadata.modified()) {
                newest = newest.max(modified);
            }
        }
    }
    newest
}

fn commit_time(worktree: &Worktree) -> Option<SystemTime> {
    if worktree.head.is_empty() {
        return None;
    }
    let text = ok_stdout(run(&worktree.path, ["show", "-s", "--format=%ct", &worktree.head]).ok()?).ok()?;
    let seconds = text.trim().parse().ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// The branch being rebased in a worktree, which git lists as detached until the
/// rebase ends. Rebasing a detached HEAD records no ref, so that gives `None`.
fn rebasing_branch(worktree: &Path) -> Option<String> {
    let dir = absolute_git_dir(worktree)?;
    let name = ["rebase-merge", "rebase-apply"]
        .iter()
        .find_map(|state| std::fs::read_to_string(dir.join(state).join("head-name")).ok())?;
    name.trim().strip_prefix("refs/heads/").map(String::from)
}

fn absolute_git_dir(cwd: &Path) -> Option<PathBuf> {
    let text = ok_stdout(run(cwd, ["rev-parse", "--absolute-git-dir"]).ok()?).ok()?;
    Some(text.trim_end().into())
}

/// The working tree of a git dir that lives outside it. A submodule's git dir
/// records it. A separate git dir doesn't, so it can be found only from inside
/// that working tree, and a linked worktree's own toplevel would be the wrong one.
fn main_worktree_root(cwd: &Path, git_dir: &Path) -> Option<PathBuf> {
    toplevel(git_dir).or_else(|| if absolute_git_dir(cwd)? == git_dir { toplevel(cwd) } else { None })
}

fn toplevel(cwd: &Path) -> Option<PathBuf> {
    let text = ok_stdout(run(cwd, ["rev-parse", "--show-toplevel"]).ok()?).ok()?;
    Some(text.trim_end().into())
}

fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for field in text.split('\0') {
        if field.is_empty() {
            out.extend(cur.take());
        } else if let Some(p) = field.strip_prefix("worktree ") {
            cur = Some(Worktree { path: p.into(), head: String::new(), branch: None });
        } else if matches!(field.split(' ').next(), Some("bare" | "prunable")) {
            cur = None; // Neither can be reviewed.
        } else if let (Some(w), Some(h)) = (cur.as_mut(), field.strip_prefix("HEAD ")) {
            w.head = h.to_string();
        } else if let (Some(w), Some(b)) = (cur.as_mut(), field.strip_prefix("branch ")) {
            w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
    }
    out.extend(cur);
    out
}

pub fn merge_base(cwd: &Path, base: &str) -> Result<String, GitError> {
    let out = run(cwd, ["merge-base", "--end-of-options", base, "HEAD"])?;
    let unresolved = out.stderr.rsplit_once("Not a valid object name ").map(|(_, name)| name);
    match (out.code, unresolved) {
        (Some(0), _) => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        (Some(1), _) => Err(GitError::NoMergeBase),
        // Checked first so that a base of "HEAD" on an unborn branch lands here.
        (Some(128), Some("HEAD")) => Err(GitError::NoCommits),
        // Git stops at `base`, so stderr says nothing about an unborn HEAD here.
        (Some(128), Some(name)) if name == base => {
            let head = run(cwd, ["rev-parse", "-q", "--verify", "HEAD"])?;
            if head.code == Some(0) {
                Err(GitError::BaseMissing(base.to_string()))
            } else {
                Err(GitError::NoCommits)
            }
        }
        _ => Err(failed(out)),
    }
}

/// Hashes `git status` plus the mtime and size of every dirty path. Status
/// alone is not enough: it does not change when a dirty file is edited again.
/// `cwd` must be the worktree root, since status paths are root-relative.
/// HEAD is not covered, so the caller combines this with HEAD.
pub fn fingerprint(cwd: &Path) -> Result<u64, GitError> {
    let out = run(cwd, ["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignore-submodules=none"])?;
    if out.code != Some(0) {
        return Err(failed(out));
    }
    let mut h = DefaultHasher::new();
    out.stdout.hash(&mut h);
    for path in dirty_paths(&out.stdout) {
        if let Ok(meta) = std::fs::symlink_metadata(cwd.join(&path)) {
            meta.len().hash(&mut h);
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            mtime.map(|d| d.as_nanos()).hash(&mut h);
        }
    }
    Ok(h.finish())
}

fn dirty_paths(status: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(status);
    let mut fields = text.split('\0');
    let mut out = Vec::new();
    while let Some(f) = fields.next() {
        match f.as_bytes().first() {
            Some(b'1') => out.extend(f.splitn(9, ' ').nth(8).map(String::from)),
            Some(b'2') => {
                out.extend(f.splitn(10, ' ').nth(9).map(String::from));
                fields.next(); // The original path of a rename follows in its own field.
            }
            Some(b'u') => out.extend(f.splitn(11, ' ').nth(10).map(String::from)),
            Some(b'?') => out.extend(f.strip_prefix("? ").map(String::from)),
            _ => {}
        }
    }
    out
}

/// `cwd` must be the worktree root, since git lists only paths below it.
pub fn untracked(cwd: &Path) -> Result<Vec<String>, GitError> {
    let text = ok_stdout(run(cwd, ["ls-files", "-z", "--others", "--exclude-standard"])?)?;
    Ok(text.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
}

/// Whether git would notice a change to any of `paths`, going by every ignore
/// source it has: nested `.gitignore` files, `info/exclude`, global excludes,
/// and the index, since a tracked file is never ignored. End a path with `/` to
/// test a directory that no longer exists, which `target/` could not match otherwise.
pub fn any_not_ignored(cwd: &Path, paths: &[PathBuf]) -> Result<bool, GitError> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.to_string_lossy().as_bytes());
        input.push(0);
    }
    let cmd = command(cwd, ["check-ignore", "-z", "--stdin"]);
    let out = match exec(cwd, cmd, Some(input), MAX_OUTPUT_BYTES, GIT_TIMEOUT)? {
        Exec::Done(out) => out,
        // Git prints only paths it was given, so this can't really happen.
        Exec::TooLarge => return Ok(true),
    };
    // Exit code 1 means that nothing was ignored.
    if !matches!(out.code, Some(0 | 1)) {
        return Err(failed(out));
    }
    let ignored = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).count();
    Ok(ignored < paths.len())
}

/// Maximum buffered tracked patch. Larger patches are rejected in full.
pub const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;

/// Diff from `merge_base` to the working tree. Pins the config known to change
/// the patch format, so the parser sees the same shape under most user config.
pub fn diff(cwd: &Path, merge_base: &str) -> Result<Vec<u8>, GitError> {
    diff_with_limit(cwd, merge_base, MAX_PATCH_BYTES)
}

fn diff_with_limit(cwd: &Path, merge_base: &str, limit: usize) -> Result<Vec<u8>, GitError> {
    let cmd = command(
        cwd,
        [
            "-c",
            "diff.suppressBlankEmpty=false",
            "diff",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--full-index",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-U3",
            "--inter-hunk-context=0",
            "--no-relative",
            "--submodule=short",
            "--ignore-submodules=none",
            // Overrides `diff.orderFile`. On Windows this relies on git's own
            // mapping of /dev/null, which is unverified until CI runs there.
            "-O/dev/null",
            "--end-of-options",
            merge_base,
            "--",
        ],
    );
    match exec(cwd, cmd, None, limit, GIT_TIMEOUT)? {
        Exec::Done(out) if out.code == Some(0) => Ok(out.stdout),
        Exec::Done(out) => Err(failed(Output { stdout: Vec::new(), ..out })),
        Exec::TooLarge => Err(GitError::DiffTooLarge),
    }
}

pub fn branches(cwd: &Path) -> Result<Vec<String>, GitError> {
    let text =
        ok_stdout(run(cwd, ["for-each-ref", "--format=%(refname:short) %(symref)", "refs/heads", "refs/remotes"])?)?;
    // A ref name cannot contain a space, so a line ending in one has an empty
    // `symref`. Symbolic refs such as `origin/HEAD` are aliases, not branches.
    Ok(text.lines().filter_map(|line| line.strip_suffix(' ')).map(String::from).collect())
}

/// The `core.editor` setting. Not `git var GIT_EDITOR`, which answers `vi` when
/// nothing is set and so hides whether the user chose one.
pub fn core_editor(cwd: &Path) -> Option<String> {
    let text = ok_stdout(run(cwd, ["config", "--get", "core.editor"]).ok()?).ok()?;
    Some(text.trim().to_string()).filter(|editor| !editor.is_empty())
}

/// The operation in progress in this worktree, if it is a rebase or a merge.
pub fn in_progress_op(cwd: &Path) -> Option<&'static str> {
    let dir = absolute_git_dir(cwd)?;
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        Some("rebase")
    } else if dir.join("MERGE_HEAD").exists() {
        Some("merge")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracked_patch_limit_is_exact_and_never_returns_a_partial_patch() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(run(dir.path(), ["init", "-q"]).unwrap().code, Some(0));
        std::fs::write(dir.path().join("a"), "old\n").unwrap();
        assert_eq!(run(dir.path(), ["add", "a"]).unwrap().code, Some(0));
        assert_eq!(
            run(dir.path(), ["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-qm", "initial"])
                .unwrap()
                .code,
            Some(0)
        );
        std::fs::write(dir.path().join("a"), "new\n").unwrap();
        let complete = diff_with_limit(dir.path(), "HEAD", 4096).unwrap();
        assert!(!complete.is_empty());
        assert_eq!(diff_with_limit(dir.path(), "HEAD", complete.len()).unwrap(), complete);
        assert_eq!(diff_with_limit(dir.path(), "HEAD", complete.len() - 1), Err(GitError::DiffTooLarge));
        // A rejected child was reaped and a later refresh can still succeed.
        assert_eq!(diff_with_limit(dir.path(), "HEAD", 4096).unwrap(), complete);
    }

    /// A repository whose aliases run `script` through the shell.
    #[cfg(unix)]
    fn alias_repo(script: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(run(dir.path(), ["init", "-q"]).unwrap().code, Some(0));
        let alias = format!("!{script}");
        assert_eq!(run(dir.path(), ["config", "alias.probe", &alias]).unwrap().code, Some(0));
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_never_ends_is_killed_at_its_deadline() {
        let dir = alias_repo("sleep 30");
        let started = std::time::Instant::now();
        let result = run_limited(dir.path(), ["probe"], 1024, Duration::from_millis(300));
        assert!(matches!(result, Err(GitError::Failed(ref e)) if e.contains("timed out")), "{result:?}");
        // Killing the process group also ends the shell and `sleep`, which
        // would otherwise hold the pipes open until they exit.
        assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn output_over_the_limit_is_rejected_without_being_buffered() {
        let dir = alias_repo("yes");
        let result = run_limited(dir.path(), ["probe"], 4096, Duration::from_secs(30));
        assert!(matches!(result, Err(GitError::Failed(ref e)) if e.contains("exceeded")), "{result:?}");
    }

    #[cfg(unix)]
    #[test]
    fn stderr_is_drained_but_only_a_prefix_is_kept() {
        let dir = alias_repo("yes noise | head -c 4194304 >&2; exit 3");
        let out = run_limited(dir.path(), ["probe"], 4096, Duration::from_secs(30)).unwrap();
        assert_eq!(out.code, Some(3));
        assert!(out.stderr.len() <= MAX_STDERR_BYTES, "kept {} bytes", out.stderr.len());
        assert!(out.stderr.starts_with("noise"));
    }

    #[cfg(unix)]
    #[test]
    fn an_exited_command_does_not_wait_for_a_descendant_holding_stderr() {
        let dir = alias_repo("sleep 3 >/dev/null &");
        let started = Instant::now();
        let out = run_limited(dir.path(), ["probe"], 4096, Duration::from_secs(10)).unwrap();
        assert_eq!(out.code, Some(0));
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn git_cannot_read_the_terminal() {
        let dir = tempfile::TempDir::new().unwrap();
        // `hash-object --stdin` would wait on an inherited terminal for good.
        let out = run_limited(dir.path(), ["hash-object", "--stdin"], 4096, Duration::from_secs(30)).unwrap();
        assert_eq!(out.code, Some(0));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    /// A repository's own config must not run code when branchdiff opens it.
    #[cfg(unix)]
    #[test]
    fn a_repository_fsmonitor_hook_is_never_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("hook-ran");
        let hook = dir.path().join("fsmonitor-hook");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert_eq!(run(&repo, ["init", "-q"]).unwrap().code, Some(0));
        let set =
            Command::new("git").current_dir(&repo).args(["config", "core.fsmonitor"]).arg(&hook).status().unwrap();
        assert!(set.success());
        std::fs::write(repo.join("a"), "x\n").unwrap();
        assert_eq!(run(&repo, ["add", "a"]).unwrap().code, Some(0));
        let _ = std::fs::remove_file(&marker);

        fingerprint(&repo).unwrap();
        diff_with_limit(&repo, "4b825dc642cb6eb9a060e54bf8d69288fbee4904", 4096).unwrap();
        assert!(!marker.exists(), "the repository's fsmonitor hook ran");
    }

    fn repo_with_fsmonitor(value: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let init = Command::new("git").current_dir(dir.path()).args(["init", "-q"]).status().unwrap();
        assert!(init.success());
        if let Some(value) = value {
            let set =
                Command::new("git").current_dir(dir.path()).args(["config", "core.fsmonitor", value]).status().unwrap();
            assert!(set.success());
        }
        dir
    }

    /// Git's own daemon runs no repository code, and large checkouts on
    /// Windows depend on it, so it stays on where git supports it.
    #[test]
    fn the_builtin_fsmonitor_daemon_is_kept_where_git_has_one() {
        let dir = repo_with_fsmonitor(Some("true"));
        assert_eq!(
            builtin_fsmonitor_configured(dir.path()),
            cfg!(any(windows, target_os = "macos")) && git_has_builtin_fsmonitor(dir.path())
        );
    }

    #[test]
    fn old_or_unknown_git_versions_cannot_enable_a_boolean_fsmonitor_hook() {
        for version in ["git version 2.35.1", "git version 2.36.0", "unknown", "git version 2.x"] {
            assert!(!version_has_builtin_fsmonitor(version.as_bytes()), "{version}");
        }
        for version in [
            "git version 2.37.0",
            "git version 2.46.0.windows.1",
            "git version 2.39.5 (Apple Git-154)",
            "git version 3.0.0",
        ] {
            assert!(version_has_builtin_fsmonitor(version.as_bytes()), "{version}");
        }
    }

    #[test]
    fn a_hook_configured_after_the_first_command_is_still_overridden() {
        let dir = repo_with_fsmonitor(Some("true"));
        let before: Vec<_> = command(dir.path(), ["status"]).get_args().map(|a| a.to_owned()).collect();
        let set = Command::new("git")
            .current_dir(dir.path())
            .args(["config", "core.fsmonitor", "hooks/should-not-run"])
            .status()
            .unwrap();
        assert!(set.success());
        let after: Vec<_> = command(dir.path(), ["status"]).get_args().map(|a| a.to_owned()).collect();
        assert_eq!(before, after);
        assert!(after.iter().any(|a| a == "core.fsmonitor=" || a == "core.fsmonitor=true"));
    }

    #[test]
    fn an_fsmonitor_hook_or_no_setting_turns_fsmonitor_off() {
        for value in [None, Some("false"), Some("/tmp/some-hook"), Some("hooks/fsmonitor-watchman")] {
            let dir = repo_with_fsmonitor(value);
            assert!(!builtin_fsmonitor_configured(dir.path()), "{value:?}");
        }
    }

    #[test]
    fn worktrees_sorted_in_parallel_keep_their_order_by_activity() {
        let list: Vec<_> = (0..20)
            .map(|i| Worktree { path: format!("/branchdiff-missing-{i}").into(), head: String::new(), branch: None })
            .collect();
        // None of these exist, so every time is the epoch and the order holds.
        assert_eq!(all_modified(&list), vec![UNIX_EPOCH; 20]);
    }

    #[test]
    fn parse_worktrees_strips_the_ref_prefix_only_once() {
        let wts = parse_worktrees("worktree /a\0HEAD abc\0branch refs/heads/refs/heads/x\0\0");
        assert_eq!(wts[0].branch.as_deref(), Some("refs/heads/x"));
    }

    #[test]
    fn failed_never_gives_a_blank_message() {
        let out = Output { code: Some(1), stdout: Vec::new(), stderr: String::new() };
        assert!(!failed(out).to_string().is_empty());
    }

    #[test]
    fn failed_keeps_git_stderr() {
        let out = Output { code: Some(1), stdout: Vec::new(), stderr: "fatal: boom".to_string() };
        assert_eq!(failed(out), GitError::Failed("fatal: boom".to_string()));
    }
}
