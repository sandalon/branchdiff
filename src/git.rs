use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitError {
    GitMissing,
    NotARepo,
    BaseMissing(String),
    NoMergeBase,
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitError::GitMissing => write!(f, "git was not found on PATH"),
            GitError::NotARepo => write!(f, "not inside a git repository"),
            GitError::BaseMissing(b) => write!(f, "base branch '{b}' not found"),
            GitError::NoMergeBase => write!(f, "no common history with the base branch"),
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

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

pub fn run<I, S>(cwd: &Path, args: I) -> Result<Output, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if !cwd.is_dir() {
        return Err(GitError::Failed(format!("{} no longer exists", cwd.display())));
    }
    let out = Command::new("git")
        .current_dir(cwd)
        // Keeps paths unquoted so the patch parser sees real names.
        .args(["-c", "core.quotePath=false"])
        .args(args)
        // Stops `git status` rewriting the index, which the watcher would see.
        .env("GIT_OPTIONAL_LOCKS", "0")
        // `merge_base` matches on stderr text, so it must not be translated.
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => GitError::GitMissing,
            _ => GitError::Failed(e.to_string()),
        })?;
    Ok(Output {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn ok_stdout(out: Output) -> Result<String, GitError> {
    if out.code == Some(0) {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(GitError::Failed(out.stderr))
    }
}

pub fn discover(cwd: &Path) -> Result<RepoInfo, GitError> {
    let out = run(cwd, ["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"])?;
    if out.code != Some(0) {
        return Err(GitError::NotARepo);
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
    Ok(parse_worktrees(&text))
}

fn parse_worktrees(text: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for field in text.split('\0') {
        if field.is_empty() {
            out.extend(cur.take());
        } else if let Some(p) = field.strip_prefix("worktree ") {
            cur = Some(Worktree { path: p.into(), head: String::new(), branch: None });
        } else if let (Some(w), Some(h)) = (cur.as_mut(), field.strip_prefix("HEAD ")) {
            w.head = h.to_string();
        } else if let (Some(w), Some(b)) = (cur.as_mut(), field.strip_prefix("branch ")) {
            w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    out.extend(cur);
    out
}

pub fn merge_base(cwd: &Path, base: &str) -> Result<String, GitError> {
    let out = run(cwd, ["merge-base", base, "HEAD"])?;
    match out.code {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Some(1) => Err(GitError::NoMergeBase),
        Some(128) if out.stderr.contains("Not a valid object name") => {
            Err(GitError::BaseMissing(base.to_string()))
        }
        _ => Err(GitError::Failed(out.stderr)),
    }
}

/// Hashes `git status` plus the mtime and size of every dirty path. Status
/// alone is not enough: it does not change when a dirty file is edited again.
pub fn fingerprint(cwd: &Path) -> Result<u64, GitError> {
    let out = run(cwd, ["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
    if out.code != Some(0) {
        return Err(GitError::Failed(out.stderr));
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

pub fn untracked(cwd: &Path) -> Result<Vec<String>, GitError> {
    let text = ok_stdout(run(cwd, ["ls-files", "-z", "--others", "--exclude-standard"])?)?;
    Ok(text.split('\0').filter(|s| !s.is_empty()).map(String::from).collect())
}

/// Diff from `merge_base` to the working tree, with config that would change
/// the output format turned off.
pub fn diff(cwd: &Path, merge_base: &str) -> Result<Vec<u8>, GitError> {
    let out = run(cwd, [
        "diff", "-M", "--no-color", "--no-ext-diff", "--no-textconv",
        "--src-prefix=a/", "--dst-prefix=b/", merge_base,
    ])?;
    if out.code == Some(0) { Ok(out.stdout) } else { Err(GitError::Failed(out.stderr)) }
}

pub fn branches(cwd: &Path) -> Result<Vec<String>, GitError> {
    let text = ok_stdout(run(cwd, ["for-each-ref", "--format=%(refname:short)", "refs/heads", "refs/remotes"])?)?;
    Ok(text.lines().map(String::from).collect())
}

/// Non-empty while a rebase or merge is in progress in this worktree.
pub fn in_progress_op(cwd: &Path) -> Option<&'static str> {
    let text = ok_stdout(run(cwd, ["rev-parse", "--absolute-git-dir"]).ok()?).ok()?;
    let dir = PathBuf::from(text.trim());
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        Some("rebase")
    } else if dir.join("MERGE_HEAD").exists() {
        Some("merge")
    } else {
        None
    }
}
