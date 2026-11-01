use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    GitMissing,
    NotARepo,
    BaseMissing(String),
    NoMergeBase,
    NoCommits,
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
    let mut cmd = Command::new("git");
    for var in REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(cwd)
        // Keeps non-ASCII paths readable. Names with tabs or quotes are still
        // C-quoted, and the patch parser handles those.
        .args(["-c", "core.quotePath=false"])
        // Windows defaults to `core.autocrlf=true`, which strips `\r` from
        // `git diff` output on a CRLF checkout; patch lines would then not
        // match the raw bytes the untracked reader keeps. Force byte-faithful
        // output everywhere so cached, commented line text agrees.
        .args(["-c", "core.autocrlf=false"])
        // Stops `git diff` refreshing the index, which takes `index.lock` and
        // can race a concurrent `git add`. GIT_OPTIONAL_LOCKS does not cover it.
        .args(["-c", "diff.autoRefreshIndex=false"])
        .args(args)
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

fn output(raw: std::process::Output) -> Output {
    Output {
        code: raw.status.code(),
        stdout: raw.stdout,
        stderr: String::from_utf8_lossy(&raw.stderr).trim().to_string(),
    }
}

pub fn run<I, S>(cwd: &Path, args: I) -> Result<Output, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if !cwd.is_dir() {
        return Err(cwd_gone(cwd));
    }
    command(cwd, args).output().map(output).map_err(|e| spawn_error(cwd, e))
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
    Ok(list)
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
    if !cwd.is_dir() {
        return Err(cwd_gone(cwd));
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.to_string_lossy().as_bytes());
        input.push(0);
    }
    let mut child = command(cwd, ["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| spawn_error(cwd, e))?;
    let mut stdin = child.stdin.take().expect("stdin was piped");
    // Written from a second thread, because git can fill its stdout pipe while
    // we are still filling its stdin.
    let writer = thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map(output).map_err(|e| spawn_error(cwd, e))?;
    let _ = writer.join();
    // Exit code 1 means that nothing was ignored.
    if !matches!(out.code, Some(0 | 1)) {
        return Err(failed(out));
    }
    let ignored = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).count();
    Ok(ignored < paths.len())
}

/// Diff from `merge_base` to the working tree. Pins the config known to change
/// the patch format, so the parser sees the same shape under most user config.
pub fn diff(cwd: &Path, merge_base: &str) -> Result<Vec<u8>, GitError> {
    let out = run(
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
    )?;
    if out.code == Some(0) {
        Ok(out.stdout)
    } else {
        Err(failed(out))
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
