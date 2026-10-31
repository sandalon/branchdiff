# branchdiff First Release Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the first release of `branchdiff`, a fast cross-platform TUI for reviewing git worktrees against a base branch, as specified in `docs/plans/2026-10-30-branchdiff-design.md`.

**Architecture:** A single Rust binary. The UI thread draws only from in-memory state in `App`. A worker pool runs the `git` CLI and returns results over channels, and a watcher thread marks worktrees stale. `App::handle_key` and `App::handle_msg` are pure state transitions that return `Effect`s, so all behaviour is testable without a terminal.

**Tech Stack:** Rust 2021 (minimum 1.88), ratatui 0.30 (use its `ratatui::crossterm` re-export; do not add crossterm directly), notify 8, syntect 5.3, two-face 0.5, nucleo-matcher 0.3, arboard 3, clap 4, serde, crossbeam-channel, insta, criterion.

**Working directory:** `.worktrees/mvp`, branch `feature/mvp`. Commit messages end with:

```
Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
```

**Requires:** git 2.36 or newer, for `worktree list --porcelain -z`.

**Status of this plan's code:** Git commands and output formats were probed against git 2.46, and the dependency set and the ratatui, notify and syntect calls were compiled. The rest of the code here has not been compiled. Treat it as the intended design and fix small API mismatches as you go, keeping the tests' intent.

**Completed tasks:** the code on this branch supersedes this document for any task already done. Reviews changed `git.rs` and `diff.rs` after those sections were written.

---

## Facts verified against real git

Do not re-derive these. They are why the code below looks the way it does.

| Behaviour | Consequence |
| --- | --- |
| `git merge-base` exits 1 with empty output when there is no merge base | Maps to `GitError::NoMergeBase` |
| `git merge-base` exits 128 when the base ref is missing | Maps to `GitError::BaseMissing` |
| `git status --porcelain=v2` does not change when an already-dirty file is edited again | The fingerprint must include each dirty file's mtime and size |
| A renamed file in the patch has `rename from` and `rename to` lines and no `---`/`+++` lines | Parse paths from those lines |
| Binary files have a `Binary files a/x and b/x differ` line and no hunks | Parse as `binary = true` |
| `.worktrees/` in `.gitignore` only matches once the directory exists | Check ignores with a trailing slash |
| `git rev-parse --git-common-dir` from a linked worktree returns the main `.git` | Use it for comment storage and watching |
| `git worktree list --porcelain -z` separates fields with NUL and records with an empty field | Parse by splitting on NUL |

## File structure

```
Cargo.toml
src/main.rs        CLI entry, terminal setup, event loop
src/lib.rs         Module exports
src/git.rs         Runs git; repo discovery, worktree list, merge base, fingerprint
src/diff.rs        Unified patch parser and data types
src/worker.rs      Thread pool, Job and Msg types
src/cache.rs       CacheKey and the shared Diffs type
src/store.rs       Comment and reviewed-mark persistence
src/watch.rs       File watcher with debounce and path filtering
src/app.rs         App state, handle_key, handle_msg, Effect
src/export.rs      Markdown prompt rendering
src/highlight.rs   syntect highlighting
src/ui.rs          ratatui drawing
src/fuzzy.rs       Worktree fuzzy finder
src/editor.rs      Editor command construction
tests/common/mod.rs   Temp repo fixture
tests/git_layer.rs    Git integration tests
tests/ui_snapshots.rs UI snapshots
tests/perf.rs         Speed targets
benches/render.rs     Criterion bench
.github/workflows/ci.yml
.github/workflows/release.yml
```

---

## Task 1: Project skeleton

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`
- Modify: `.gitignore`

**Step 1: Write `Cargo.toml`**

```toml
[package]
name = "branchdiff"
version = "0.1.0"
edition = "2021"
rust-version = "1.88"
resolver = "3"
description = "Fast terminal UI for reviewing git worktrees against a base branch"

[lib]
bench = false

[[bin]]
name = "branchdiff"
path = "src/main.rs"
bench = false

[dependencies]
anyhow = "1"
arboard = { version = "3", default-features = false, features = ["wayland-data-control"] }
clap = { version = "4", features = ["derive"] }
crossbeam-channel = "0.5"
ignore = "0.4"
notify = "8"
nucleo-matcher = "0.3"
ratatui = "0.30"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
syntect = { version = "5.3", default-features = false, features = ["parsing", "default-syntaxes", "default-themes", "regex-fancy"] }
toml = "1"
unicode-width = "0.2"

[dev-dependencies]
criterion = "0.8"
insta = "1"
tempfile = "3"

[profile.release]
lto = "thin"
codegen-units = 1
strip = true

[[bench]]
name = "render"
harness = false
```

**Step 2: Create empty modules**

`src/lib.rs`:

```rust
pub mod app;
pub mod cache;
pub mod diff;
pub mod editor;
pub mod export;
pub mod fuzzy;
pub mod git;
pub mod highlight;
pub mod store;
pub mod ui;
pub mod watch;
pub mod worker;
```

Create each listed file as empty. Create `benches/render.rs` containing `fn main() {}`. `src/main.rs`:

```rust
fn main() -> anyhow::Result<()> {
    Ok(())
}
```

**Step 3: Add `target/` to `.gitignore`**

The file should contain:

```
.worktrees/
target/
*.snap.new
*.pending-snap
```

**Step 4: Verify it builds**

Run: `cargo build`
Expected: finishes with no errors.

**Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore src benches
git commit -m "chore: add project skeleton"
```

---

## Task 2: Test fixture and git runner

**Files:**
- Create: `tests/common/mod.rs`, `tests/git_layer.rs`
- Modify: `src/git.rs`

**Step 1: Write the fixture**

`tests/common/mod.rs`:

```rust
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

pub struct Repo {
    _dir: TempDir,
    pub main: PathBuf,
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(cwd).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

impl Repo {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "develop"]);
        git(&main, &["config", "user.email", "test@example.com"]);
        git(&main, &["config", "user.name", "Test"]);
        std::fs::write(main.join("a.txt"), "one\ntwo\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-qm", "init"]);
        Self { _dir: dir, main }
    }

    pub fn add_worktree(&self, name: &str) -> PathBuf {
        let path = self.main.parent().unwrap().join(name);
        git(&self.main, &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()]);
        path
    }

    pub fn commit_all(&self, cwd: &Path, msg: &str) {
        git(cwd, &["add", "."]);
        git(cwd, &["commit", "-qm", msg]);
    }
}
```

**Step 2: Write the failing tests**

`tests/git_layer.rs`:

```rust
mod common;
use branchdiff::git::{self, GitError};
use common::{git as run, Repo};

#[test]
fn discovers_common_dir_from_linked_worktree() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let info = git::discover(&wt).unwrap();
    assert_eq!(info.common_dir.canonicalize().unwrap(), repo.main.join(".git").canonicalize().unwrap());
}

#[test]
fn discover_fails_outside_a_repo() {
    let dir = tempfile::TempDir::new().unwrap();
    assert!(matches!(git::discover(dir.path()), Err(GitError::NotARepo)));
}

#[test]
fn lists_worktrees_with_branches() {
    let repo = Repo::new();
    repo.add_worktree("feat");
    let wts = git::list_worktrees(&repo.main).unwrap();
    let branches: Vec<_> = wts.iter().map(|w| w.branch.clone().unwrap()).collect();
    assert_eq!(branches, vec!["develop", "feat"]);
}

#[test]
fn lists_detached_head_without_branch() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["checkout", "-q", "--detach"]);
    let wts = git::list_worktrees(&repo.main).unwrap();
    assert!(wts.iter().any(|w| w.branch.is_none()));
}

#[test]
fn merge_base_ignores_later_base_commits() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let fork = run(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(repo.main.join("later.txt"), "x\n").unwrap();
    repo.commit_all(&repo.main, "later");
    assert_eq!(git::merge_base(&wt, "develop").unwrap(), fork);
}

#[test]
fn merge_base_reports_missing_base() {
    let repo = Repo::new();
    assert!(matches!(git::merge_base(&repo.main, "nope"), Err(GitError::BaseMissing(_))));
}

#[test]
fn merge_base_reports_unrelated_histories() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["checkout", "-q", "--orphan", "lonely"]);
    run(&wt, &["commit", "-qm", "orphan", "--allow-empty"]);
    assert!(matches!(git::merge_base(&wt, "develop"), Err(GitError::NoMergeBase)));
}
```

**Step 3: Run tests to verify they fail**

Run: `cargo test --test git_layer`
Expected: FAIL to compile, `discover` not found.

**Step 4: Implement `src/git.rs`**

```rust
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
```

**Step 5: Run tests to verify they pass**

Run: `cargo test --test git_layer`
Expected: 7 passed.

**Step 6: Add fingerprint and status tests, then run them**

Append to `tests/git_layer.rs`:

```rust
#[test]
fn fingerprint_changes_when_dirty_file_is_edited_again() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("a.txt"), "dirty 1\n").unwrap();
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(repo.main.join("a.txt"), "dirty 22\n").unwrap();
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn fingerprint_is_stable_without_changes() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("a.txt"), "dirty\n").unwrap();
    assert_eq!(git::fingerprint(&repo.main).unwrap(), git::fingerprint(&repo.main).unwrap());
}
```

Run: `cargo test --test git_layer`
Expected: 9 passed.

**Step 7: Commit**

```bash
git add src/git.rs tests
git commit -m "feat: add git runner, discovery, worktrees, merge base, fingerprint"
```

---

## Task 3: Patch parser

**Files:**
- Modify: `src/diff.rs`

**Step 1: Write the failing tests**

Add at the end of `src/diff.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const MODIFY: &str = "diff --git a/a.txt b/a.txt\nindex 814f4a4..475bbc3 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n+three\n";

    #[test]
    fn parses_modified_file() {
        let files = parse(MODIFY.as_bytes());
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!((f.path.as_str(), f.status), ("a.txt", Status::Modified));
        assert_eq!((f.added, f.removed), (2, 1));
        let lines = &f.hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(1), Some(1)));
        assert_eq!((lines[1].old_no, lines[1].new_no), (Some(2), None));
        assert_eq!((lines[2].old_no, lines[2].new_no), (None, Some(2)));
        assert_eq!(lines[3].new_no, Some(3));
    }

    #[test]
    fn parses_pure_rename_with_no_hunks() {
        let p = "diff --git a/big.txt b/moved.txt\nsimilarity index 100%\nrename from big.txt\nrename to moved.txt\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!((f.status, f.path.as_str(), f.old_path.as_deref()), (Status::Renamed, "moved.txt", Some("big.txt")));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_binary_file() {
        let p = "diff --git a/bin.dat b/bin.dat\nindex 8352675..9388380 100644\nBinary files a/bin.dat and b/bin.dat differ\n";
        assert!(parse(p.as_bytes())[0].binary);
    }

    #[test]
    fn parses_added_and_deleted_files() {
        let p = "diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..3e75765\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+new\ndiff --git a/o.txt b/o.txt\ndeleted file mode 100644\nindex 3e75765..0000000\n--- a/o.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n";
        let files = parse(p.as_bytes());
        assert_eq!((files[0].status, files[0].path.as_str()), (Status::Added, "n.txt"));
        assert_eq!((files[1].status, files[1].path.as_str()), (Status::Deleted, "o.txt"));
    }

    #[test]
    fn keeps_paths_with_spaces() {
        let p = "diff --git a/my file.txt b/my file.txt\n--- a/my file.txt\n+++ b/my file.txt\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(parse(p.as_bytes())[0].path, "my file.txt");
    }

    #[test]
    fn unquotes_special_paths() {
        let p = "diff --git \"a/t\\tb.txt\" \"b/t\\tb.txt\"\n--- \"a/t\\tb.txt\"\n+++ \"b/t\\tb.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(parse(p.as_bytes())[0].path, "t\tb.txt");
    }

    #[test]
    fn body_lines_that_look_like_headers_stay_in_the_hunk() {
        let p = "diff --git a/s.md b/s.md\n--- a/s.md\n+++ b/s.md\n@@ -1,2 +1,2 @@\n--- old heading\n+++ new heading\n context\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!((f.added, f.removed), (1, 1));
    }

    #[test]
    fn keeps_invalid_utf8_lines() {
        let mut p = b"diff --git a/l.txt b/l.txt\n--- a/l.txt\n+++ b/l.txt\n@@ -1 +1 @@\n-a\n+".to_vec();
        p.extend([0xff, 0xfe, b'\n']);
        assert_eq!(parse(&p)[0].added, 1);
    }

    #[test]
    fn no_newline_marker_is_not_a_line() {
        let p = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n";
        assert_eq!(parse(p.as_bytes())[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse(b"").is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib diff`
Expected: FAIL to compile, `parse` not found.

**Step 3: Implement the parser above the tests**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status { Modified, Added, Deleted, Renamed, Untracked }

impl Status {
    pub fn letter(self) -> char {
        match self {
            Status::Modified => 'M',
            Status::Added => 'A',
            Status::Deleted => 'D',
            Status::Renamed => 'R',
            Status::Untracked => '?',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind { Context, Added, Removed }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    pub binary: bool,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn empty() -> Self {
        Self { path: String::new(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 0, removed: 0, hunks: Vec::new() }
    }
}

pub fn parse(input: &[u8]) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for raw in input.split(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        // Inside a hunk the counts decide, so body lines that look like
        // headers are never misread.
        if old_left > 0 || new_left > 0 {
            let Some(file) = cur.as_mut() else { continue };
            let Some(hunk) = file.hunks.last_mut() else { continue };
            let (kind, text) = match line.as_bytes().first() {
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'-') => (LineKind::Removed, &line[1..]),
                Some(b' ') => (LineKind::Context, &line[1..]),
                Some(b'\\') => continue,
                None => (LineKind::Context, ""),
                _ => continue,
            };
            let (o, n) = match kind {
                LineKind::Added => (None, Some(new_no)),
                LineKind::Removed => (Some(old_no), None),
                LineKind::Context => (Some(old_no), Some(new_no)),
            };
            if o.is_some() { old_no += 1; old_left -= 1; }
            if n.is_some() { new_no += 1; new_left -= 1; }
            match kind {
                LineKind::Added => file.added += 1,
                LineKind::Removed => file.removed += 1,
                LineKind::Context => {}
            }
            hunk.lines.push(Line { kind, old_no: o, new_no: n, text: text.to_string() });
            continue;
        }

        if line.starts_with("diff --git ") {
            files.extend(cur.take());
            let mut f = FileDiff::empty();
            f.path = path_from_git_header(&line).unwrap_or_default();
            cur = Some(f);
            continue;
        }
        let Some(file) = cur.as_mut() else { continue };
        if line.starts_with("new file mode") {
            file.status = Status::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = Status::Deleted;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            file.status = Status::Renamed;
            file.old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            file.path = unquote(p);
        } else if line.starts_with("Binary files ") {
            file.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ") {
            if p != "/dev/null" { file.path = strip_side(&unquote(p)); }
        } else if let Some(p) = line.strip_prefix("+++ ") {
            if p != "/dev/null" { file.path = strip_side(&unquote(p)); }
        } else if line.starts_with("@@ ") {
            if let Some((o, ol, n, nl)) = parse_hunk_header(&line) {
                (old_no, old_left, new_no, new_left) = (o, ol, n, nl);
                file.hunks.push(Hunk { header: line.to_string(), lines: Vec::new() });
            }
        }
    }
    files.extend(cur);
    files
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = line.split(' ');
    parts.next()?;
    let (o, ol) = parse_range(parts.next()?.strip_prefix('-')?)?;
    let (n, nl) = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some((o, ol, n, nl))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

fn strip_side(p: &str) -> String {
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
}

/// Only used until `---`, `+++` or `rename to` lines give an exact path, which
/// matters for binary and mode-only changes. Both sides match in those cases,
/// so the split point is the middle of the string.
fn path_from_git_header(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let close = rest[1..].find("\" ")? + 1;
        return Some(strip_side(&unquote(&rest[close + 2..])));
    }
    let mid = rest.len() / 2;
    // `get` because a rename's midpoint can land inside a multi-byte character.
    Some(strip_side(rest.get(mid + 1..)?))
}

/// Reverses git's C-style quoting. Unquoted input is returned as is.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + (n - b'0') as u32;
                        it.next();
                    }
                }
                bytes.push(v as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}
```

Note on `&line[1..]`: `line` is lossy UTF-8 and the first byte is ASCII in these arms, so slicing at 1 is safe.

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib diff`
Expected: 10 passed.

**Step 5: Commit**

```bash
git add src/diff.rs
git commit -m "feat: add unified patch parser"
```

---

## Task 4: Load a full worktree diff

**Files:**
- Modify: `src/diff.rs`, `tests/git_layer.rs`

**Step 1: Write the failing tests**

Append to `tests/git_layer.rs`:

```rust
use branchdiff::diff::{self, Status};

fn loaded(cwd: &std::path::Path) -> Vec<diff::FileDiff> {
    let mb = git::merge_base(cwd, "develop").unwrap();
    diff::load(cwd, &mb).unwrap()
}

#[test]
fn includes_committed_and_uncommitted_changes() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    repo.commit_all(&wt, "c");
    std::fs::write(wt.join("a.txt"), "one\ntwo\nthree\ndirty\n").unwrap();
    let files = loaded(&wt);
    assert_eq!((files.len(), files[0].added), (1, 2));
}

#[test]
fn excludes_changes_only_on_the_base() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(repo.main.join("later.txt"), "x\n").unwrap();
    repo.commit_all(&repo.main, "later");
    assert!(loaded(&wt).is_empty());
}

#[test]
fn shows_untracked_files_as_new_with_their_content() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("new.txt"), "hello\nworld\n").unwrap();
    let files = loaded(&wt);
    assert_eq!((files[0].status, files[0].path.as_str(), files[0].added), (Status::Untracked, "new.txt", 2));
}

#[test]
fn respects_gitignore_for_untracked_files() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join(".gitignore"), "skip.txt\n").unwrap();
    std::fs::write(wt.join("skip.txt"), "x\n").unwrap();
    assert_eq!(loaded(&wt).iter().filter(|f| f.path == "skip.txt").count(), 0);
}

#[test]
fn detects_renames() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["mv", "a.txt", "b.txt"]);
    repo.commit_all(&wt, "mv");
    let files = loaded(&wt);
    assert_eq!((files[0].status, files[0].old_path.as_deref()), (Status::Renamed, Some("a.txt")));
}

#[test]
fn flags_binary_files() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("b.dat"), [0u8, 1, 2]).unwrap();
    repo.commit_all(&repo.main, "bin");
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("b.dat"), [3u8, 4, 0]).unwrap();
    assert!(loaded(&wt)[0].binary);
}

#[test]
fn flags_untracked_binary_files() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("n.dat"), [0u8, 159, 146]).unwrap();
    assert!(loaded(&wt)[0].binary);
}

#[test]
fn untracked_files_get_a_content_id_that_follows_their_bytes() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("n.dat"), [0u8, 1, 2]).unwrap();
    let first = loaded(&wt)[0].content_id.clone();
    assert!(first.is_some());
    std::fs::write(wt.join("n.dat"), [0u8, 1, 3]).unwrap();
    assert_ne!(first, loaded(&wt)[0].content_id);
}

#[test]
fn ignores_user_diff_config_that_changes_output() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&repo.main, &["config", "diff.mnemonicPrefix", "true"]);
    run(&repo.main, &["config", "diff.noprefix", "true"]);
    std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
    assert_eq!(loaded(&wt)[0].path, "a.txt");
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --test git_layer`
Expected: FAIL to compile, `diff::load` not found.

**Step 3: Implement `load`**

Add to `src/diff.rs` above the tests module:

```rust
use crate::git::{self, GitError};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Everything the worktree changed since `merge_base`, including untracked files.
pub fn load(cwd: &Path, merge_base: &str) -> Result<Vec<FileDiff>, GitError> {
    let mut files = parse(&git::diff(cwd, merge_base)?);
    for path in git::untracked(cwd)? {
        files.push(untracked_file(cwd, path));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn untracked_file(cwd: &Path, path: String) -> FileDiff {
    let mut f = FileDiff::empty();
    f.status = Status::Untracked;
    let bytes = std::fs::read(cwd.join(&path)).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    f.content_id = Some(format!("{:x}", hasher.finish()));
    // Same heuristic git uses: a NUL in the first 8000 bytes means binary.
    if bytes.iter().take(8000).any(|b| *b == 0) {
        f.binary = true;
    } else {
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<Line> = text.lines().enumerate().map(|(i, l)| Line {
            kind: LineKind::Added,
            old_no: None,
            new_no: Some(i as u32 + 1),
            text: l.to_string(),
        }).collect();
        f.added = lines.len() as u32;
        if !lines.is_empty() {
            f.hunks.push(Hunk { header: format!("@@ -0,0 +1,{} @@", lines.len()), lines });
        }
    }
    f.path = path;
    f
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: all pass.

**Step 5: Commit**

```bash
git add src/diff.rs tests/git_layer.rs
git commit -m "feat: load worktree diffs including untracked files"
```

---

## Task 5: Cache key

`WorktreeState` (Task 9) holds each worktree's latest diff in memory, and that is the cache: switching back is a redraw with no git calls. A separate cache structure would hold the same data twice, so this task only adds the key that identifies a diff's inputs.

**Files:**
- Modify: `src/cache.rs`

**Step 1: Implement**

```rust
use crate::diff::FileDiff;
use std::path::PathBuf;
use std::sync::Arc;

pub type Diffs = Arc<Vec<FileDiff>>;

/// Identifies the inputs of a loaded diff. Results for an older key are stale.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub worktree: PathBuf,
    pub base_commit: String,
    pub head: String,
    pub fingerprint: u64,
}
```

**Step 2: Verify it builds**

Run: `cargo build`
Expected: finishes with no errors.

**Step 3: Commit**

```bash
git add src/cache.rs
git commit -m "feat: add diff cache key"
```

---

## Task 6: Worker pool

**Files:**
- Modify: `src/worker.rs`

**Step 1: Write the failing tests**

```rust
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
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib worker`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
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
```

Move the `use` of `unbounded` to the test module if the compiler flags it as unused elsewhere. `Pool::new` uses it, so it should be fine.

Add a stub `src/highlight.rs` so this compiles:

```rust
use crate::diff::FileDiff;

#[derive(Debug, Clone, Default)]
pub struct HighlightedFile;

pub fn highlight_file(_file: &FileDiff) -> HighlightedFile {
    HighlightedFile
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib worker`
Expected: 7 passed.

**Step 5: Commit**

```bash
git add src/worker.rs src/highlight.rs
git commit -m "feat: add worker pool with request generations"
```

---

## Task 7: Comment and reviewed-mark store

**Files:**
- Modify: `src/store.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn comment(body: &str) -> Comment {
        Comment { path: "a.rs".into(), line: 3, side: Side::New, line_text: "let x = 1;".into(), body: body.into() }
    }

    #[test]
    fn round_trips_a_review() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let mut r = Review::default();
        r.comments.push(comment("fix"));
        r.reviewed.insert("a.rs".into(), 42);
        store.save(&ReviewId::Branch("feat".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feat".into())), r);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        assert_eq!(Store::new(dir.path()).load(&ReviewId::Branch("x".into())), Review::default());
    }

    #[test]
    fn branches_with_slashes_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let mut r = Review::default();
        r.comments.push(comment("slash"));
        store.save(&ReviewId::Branch("feature/x".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feature-x".into())), Review::default());
        assert_eq!(store.load(&ReviewId::Branch("feature/x".into())), r);
    }

    #[test]
    fn detached_head_is_keyed_by_commit() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        store.save(&ReviewId::Commit("abc".into()), &Review::default()).unwrap();
        assert!(store.path_for(&ReviewId::Commit("abc".into())).to_string_lossy().contains("commit-"));
    }

    #[test]
    fn failed_save_leaves_old_file_intact() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let mut r = Review::default();
        r.comments.push(comment("keep"));
        store.save(&id, &r).unwrap();
        // A temp file left by an interrupted save must not affect loading.
        std::fs::write(store.path_for(&id).with_extension("json.tmp"), "{ truncated").unwrap();
        assert_eq!(store.load(&id), r);
    }

    #[test]
    fn corrupt_file_is_preserved_and_loads_empty() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        std::fs::write(store.path_for(&id), "not json").unwrap();
        assert_eq!(store.load(&id), Review::default());
        assert!(store.path_for(&id).with_extension("json.corrupt").exists());
    }

    #[test]
    fn flags_comment_outdated_when_line_text_changes() {
        let c = comment("x");
        assert!(!c.is_outdated(Some("let x = 1;")));
        assert!(c.is_outdated(Some("let x = 2;")));
        assert!(c.is_outdated(None));
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib store`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which file version `Comment::line` counts in. A replaced line has the
/// same number on both sides, so the number alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side { Old, New }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub path: String,
    pub line: u32,
    pub side: Side,
    pub line_text: String,
    pub body: String,
}

impl Comment {
    /// `current` is the text now at this comment's line, if the line still exists.
    pub fn is_outdated(&self, current: Option<&str>) -> bool {
        current != Some(self.line_text.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub comments: Vec<Comment>,
    /// Maps a file path to a hash of its diff when it was marked reviewed.
    pub reviewed: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReviewId {
    Branch(String),
    Commit(String),
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(git_common_dir: &Path) -> Self {
        Self { dir: git_common_dir.join("branchdiff") }
    }

    pub fn path_for(&self, id: &ReviewId) -> PathBuf {
        let name = match id {
            ReviewId::Branch(b) => format!("branch-{}", encode(b)),
            ReviewId::Commit(c) => format!("commit-{}", encode(c)),
        };
        self.dir.join(format!("{name}.json"))
    }

    pub fn load(&self, id: &ReviewId) -> Review {
        let path = self.path_for(id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Review::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|_| {
            let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
            Review::default()
        })
    }

    /// Writes to a temp file then renames, so a crash cannot corrupt the review.
    pub fn save(&self, id: &ReviewId, review: &Review) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(review)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Percent-encodes anything but `[A-Za-z0-9._-]`, so branch names map to one
/// flat, unique file name on every platform.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib store`
Expected: 7 passed.

**Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: persist comments and reviewed marks in the git common dir"
```

---

## Task 8: Markdown export

**Files:**
- Modify: `src/export.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str, line: u32, body: &str) -> Comment {
        Comment { path: path.into(), line, side: Side::New, line_text: "code".into(), body: body.into() }
    }

    #[test]
    fn groups_by_file_in_line_order() {
        let md = render("feat", "develop", &[c("b.rs", 9, "second"), c("a.rs", 7, "late"), c("a.rs", 2, "early")]);
        let (a, b) = (md.find("### `a.rs`").unwrap(), md.find("### `b.rs`").unwrap());
        assert!(a < b);
        assert!(md.find("early").unwrap() < md.find("late").unwrap());
    }

    #[test]
    fn includes_branch_base_line_and_code() {
        let md = render("feat", "develop", &[c("a.rs", 2, "fix this")]);
        assert!(md.contains("`feat`") && md.contains("`develop`"));
        assert!(md.contains("Line 2") && md.contains("fix this") && md.contains("code"));
    }

    #[test]
    fn marks_comments_on_removed_lines() {
        let mut comment = c("a.rs", 2, "why remove this");
        comment.side = Side::Old;
        assert!(render("feat", "develop", &[comment]).contains("Line 2 (removed)"));
    }

    #[test]
    fn empty_comments_render_empty() {
        assert_eq!(render("feat", "develop", &[]), "");
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib export`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
use crate::store::{Comment, Side};
use std::collections::BTreeMap;

pub fn render(branch: &str, base: &str, comments: &[Comment]) -> String {
    if comments.is_empty() {
        return String::new();
    }
    let mut by_file: BTreeMap<&str, Vec<&Comment>> = BTreeMap::new();
    for c in comments {
        by_file.entry(&c.path).or_default().push(c);
    }
    let mut out = format!("## Review of `{branch}` against `{base}`\n\nPlease address these review comments:\n");
    for (path, mut items) in by_file {
        items.sort_by_key(|c| c.line);
        out.push_str(&format!("\n### `{path}`\n"));
        for c in items {
            let removed = if c.side == Side::Old { " (removed)" } else { "" };
            out.push_str(&format!("\n- Line {}{removed}: `{}`\n  {}\n", c.line, c.line_text.trim(), c.body));
        }
    }
    out
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib export`
Expected: 4 passed.

**Step 5: Commit**

```bash
git add src/export.rs
git commit -m "feat: render review comments as a Markdown prompt"
```

---

## Task 9: App state, worktree switching and navigation

This is the core of the app. `App` holds all UI state. `handle_key` and `handle_msg` change state and return `Effect`s, which `main` executes. Nothing in here touches git, the filesystem or the terminal.

**Files:**
- Modify: `src/app.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheKey;
    use crate::diff::{FileDiff, Status};
    use crate::git::Worktree;
    use crate::worker::{Loaded, Msg};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use std::sync::Arc;

    fn wt(name: &str) -> Worktree {
        Worktree { path: format!("/r/{name}").into(), head: "h".into(), branch: Some(name.into()) }
    }

    fn file(path: &str) -> FileDiff {
        FileDiff { path: path.into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 0, hunks: vec![] }
    }

    fn app(names: &[&str]) -> App {
        let mut a = App::new("develop".into());
        a.handle_msg(Msg::Worktrees(Ok(names.iter().map(|n| wt(n)).collect())));
        a
    }

    fn press(a: &mut App, c: char) -> Vec<Effect> {
        a.handle_key(KeyEvent::from(KeyCode::Char(c)))
    }

    fn deliver(a: &mut App, name: &str, files: Vec<FileDiff>) {
        let path: std::path::PathBuf = format!("/r/{name}").into();
        let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: Arc::new(files), op: None }) });
    }

    #[test]
    fn worktree_list_requests_a_load_for_each_worktree() {
        let mut a = App::new("develop".into());
        let fx = a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("b")])));
        assert_eq!(fx.iter().filter(|e| matches!(e, Effect::Load { .. })).count(), 2);
    }

    #[test]
    fn brackets_switch_worktrees_and_wrap() {
        let mut a = app(&["a", "b", "c"]);
        press(&mut a, ']');
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("b"));
        press(&mut a, '[');
        press(&mut a, '[');
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("c"));
    }

    #[test]
    fn switching_works_from_any_focus() {
        let mut a = app(&["a", "b"]);
        a.handle_key(KeyEvent::from(KeyCode::Tab));
        a.handle_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Diff);
        press(&mut a, ']');
        assert_eq!(a.selected, 1);
    }

    #[test]
    fn number_keys_jump_and_ignore_out_of_range() {
        let mut a = app(&["a", "b"]);
        press(&mut a, '2');
        assert_eq!(a.selected, 1);
        press(&mut a, '9');
        assert_eq!(a.selected, 1);
    }

    #[test]
    fn each_worktree_remembers_its_position() {
        let mut a = app(&["a", "b"]);
        deliver(&mut a, "a", vec![file("1"), file("2"), file("3")]);
        press(&mut a, 'n');
        press(&mut a, 'n');
        press(&mut a, ']');
        press(&mut a, '[');
        assert_eq!(a.current().unwrap().file, 2);
    }

    #[test]
    fn new_worktree_appears_and_selection_follows_its_path() {
        let mut a = app(&["a", "b"]);
        press(&mut a, ']');
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("aa"), wt("b")])));
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("b"));
    }

    #[test]
    fn removed_selected_worktree_shows_notice_and_moves_on() {
        let mut a = app(&["a", "b", "c"]);
        press(&mut a, ']');
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("c")])));
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("c"));
        assert!(a.notice.as_deref().unwrap().contains('b'));
    }

    #[test]
    fn stale_message_badges_but_does_not_replace_the_diff() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file("1")]);
        let fx = a.handle_msg(Msg::Stale("/r/a".into()));
        assert!(a.current().unwrap().stale);
        assert_eq!(a.current().unwrap().diffs.as_ref().unwrap().len(), 1);
        assert!(fx.is_empty());
    }

    #[test]
    fn r_requests_a_reload_of_the_selected_worktree() {
        let mut a = app(&["a"]);
        let fx = press(&mut a, 'r');
        assert!(fx.contains(&Effect::ReloadWorktrees));
        assert!(fx.iter().any(|e| matches!(e, Effect::Load { .. })));
    }

    #[test]
    fn a_later_worktree_list_clears_a_failed_one() {
        let mut a = App::new("develop".into());
        a.handle_msg(Msg::Worktrees(Err(crate::git::GitError::Failed("boom".into()))));
        assert!(a.fatal.is_some());
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a")])));
        assert!(a.fatal.is_none());
    }

    #[test]
    fn q_quits() {
        let mut a = app(&["a"]);
        press(&mut a, 'q');
        assert!(a.should_quit);
    }

    #[test]
    fn question_mark_toggles_help_and_esc_closes_it() {
        let mut a = app(&["a"]);
        press(&mut a, '?');
        assert!(a.help_open);
        press(&mut a, '?');
        assert!(!a.help_open);
        press(&mut a, '?');
        a.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!a.help_open);
    }

    #[test]
    fn keys_do_nothing_with_no_worktrees_loaded() {
        let mut a = App::new("develop".into());
        for c in [']', '[', 'n', 'p', 'j', 'k', 'c', 'e', 'x', 'y'] {
            press(&mut a, c);
        }
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib app`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
use crate::cache::{CacheKey, Diffs};
use crate::git::{GitError, Worktree};
use crate::store::Review;
use crate::worker::Msg;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus { Worktrees, Files, Diff }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView { Unified, SideBySide }

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Load { path: PathBuf, base: String },
    ReloadWorktrees,
}

/// Everything remembered for one worktree across switches.
#[derive(Default)]
pub struct WorktreeState {
    pub key: Option<CacheKey>,
    pub diffs: Option<Diffs>,
    pub error: Option<GitError>,
    pub op: Option<&'static str>,
    pub stale: bool,
    pub file: usize,
    pub cursor: usize,
    pub scroll: usize,
    pub review: Review,
}

pub struct App {
    pub base: String,
    pub worktrees: Vec<Worktree>,
    pub states: Vec<WorktreeState>,
    pub selected: usize,
    pub focus: Focus,
    pub view: DiffView,
    pub help_open: bool,
    pub notice: Option<String>,
    pub fatal: Option<GitError>,
    pub should_quit: bool,
}

impl App {
    pub fn new(base: String) -> Self {
        Self {
            base,
            worktrees: Vec::new(),
            states: Vec::new(),
            selected: 0,
            focus: Focus::Worktrees,
            view: DiffView::Unified,
            help_open: false,
            notice: None,
            fatal: None,
            should_quit: false,
        }
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        self.worktrees.get(self.selected)
    }

    pub fn current(&self) -> Option<&WorktreeState> {
        self.states.get(self.selected)
    }

    pub fn current_mut(&mut self) -> Option<&mut WorktreeState> {
        self.states.get_mut(self.selected)
    }

    pub fn file_count(&self) -> usize {
        self.current().and_then(|s| s.diffs.as_ref()).map_or(0, |d| d.len())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        self.notice = None;
        if self.help_open {
            if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                self.help_open = false;
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.help_open = true,
            KeyCode::Char(']') => self.step_worktree(1),
            KeyCode::Char('[') => self.step_worktree(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.worktrees.len() {
                    self.selected = i;
                }
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Worktrees => Focus::Files,
                    Focus::Files => Focus::Diff,
                    Focus::Diff => Focus::Worktrees,
                };
            }
            KeyCode::Char('n') => self.step_file(1),
            KeyCode::Char('p') => self.step_file(-1),
            KeyCode::Char('j') | KeyCode::Down => self.move_vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_vertical(-1),
            KeyCode::Char('v') => {
                self.view = match self.view {
                    DiffView::Unified => DiffView::SideBySide,
                    DiffView::SideBySide => DiffView::Unified,
                };
            }
            KeyCode::Char('r') => return self.reload_selected(),
            _ => {}
        }
        Vec::new()
    }

    fn reload_selected(&mut self) -> Vec<Effect> {
        let base = self.base.clone();
        let Some(path) = self.selected_worktree().map(|w| w.path.clone()) else { return Vec::new() };
        // The list too, since a branch switch inside a worktree changes its row.
        vec![Effect::ReloadWorktrees, Effect::Load { path, base }]
    }

    fn step_worktree(&mut self, delta: isize) {
        let n = self.worktrees.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        }
    }

    fn step_file(&mut self, delta: isize) {
        let n = self.file_count() as isize;
        if let Some(s) = self.current_mut() {
            if n > 0 {
                s.file = (s.file as isize + delta).clamp(0, n - 1) as usize;
                s.cursor = 0;
                s.scroll = 0;
            }
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        match self.focus {
            Focus::Worktrees => self.step_worktree(delta),
            Focus::Files => self.step_file(delta),
            Focus::Diff => {
                let max = self.diff_row_count().saturating_sub(1);
                if let Some(s) = self.current_mut() {
                    s.cursor = (s.cursor as isize + delta).clamp(0, max as isize) as usize;
                }
            }
        }
    }

    /// Rows in the current file: one per hunk header plus one per line.
    pub fn diff_row_count(&self) -> usize {
        self.current_file().map_or(0, |f| f.hunks.iter().map(|h| 1 + h.lines.len()).sum())
    }

    pub fn current_file(&self) -> Option<&crate::diff::FileDiff> {
        let s = self.current()?;
        s.diffs.as_ref()?.get(s.file)
    }

    pub fn handle_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Worktrees(Ok(list)) => {
                self.fatal = None;
                self.apply_worktrees(list)
            }
            Msg::Worktrees(Err(e)) => {
                self.fatal = Some(e);
                Vec::new()
            }
            Msg::Loaded { path, result } => {
                if let Some(i) = self.index_of(&path) {
                    let s = &mut self.states[i];
                    match result {
                        Ok(l) => {
                            s.file = s.file.min(l.diffs.len().saturating_sub(1));
                            s.key = Some(l.key);
                            s.diffs = Some(l.diffs);
                            s.op = l.op;
                            s.error = None;
                            s.stale = false;
                        }
                        Err(e) => s.error = Some(e),
                    }
                }
                Vec::new()
            }
            Msg::Stale(path) => {
                if let Some(i) = self.index_of(&path) {
                    self.states[i].stale = true;
                }
                Vec::new()
            }
            Msg::WorktreesChanged => vec![Effect::ReloadWorktrees],
            Msg::Branches(_) | Msg::Highlighted { .. } => Vec::new(),
        }
    }

    fn index_of(&self, path: &std::path::Path) -> Option<usize> {
        self.worktrees.iter().position(|w| w.path == path)
    }

    /// Replaces the list, keeping per-worktree state by path and following
    /// the selected worktree to its new position.
    fn apply_worktrees(&mut self, list: Vec<Worktree>) -> Vec<Effect> {
        let selected_path = self.selected_worktree().map(|w| w.path.clone());
        let old_selected = self.selected;
        let removed = selected_path.as_ref().filter(|p| !list.iter().any(|w| &w.path == *p)).cloned();

        let mut old: Vec<(PathBuf, WorktreeState)> = self.worktrees.drain(..)
            .map(|w| w.path)
            .zip(self.states.drain(..))
            .collect();

        let mut effects = Vec::new();
        for w in &list {
            let state = match old.iter().position(|(p, _)| *p == w.path) {
                Some(i) => old.swap_remove(i).1,
                None => {
                    effects.push(Effect::Load { path: w.path.clone(), base: self.base.clone() });
                    WorktreeState::default()
                }
            };
            self.states.push(state);
        }
        self.worktrees = list;

        self.selected = match (&selected_path, &removed) {
            (Some(_), Some(gone)) => {
                self.notice = Some(format!("Worktree removed: {}", gone.display()));
                old_selected.min(self.worktrees.len().saturating_sub(1))
            }
            (Some(p), None) => self.index_of(p).unwrap_or(0),
            _ => 0,
        };
        effects
    }
}
```

Note: `main` drops a result whose base the user has since changed, using `Loads::finished`, before it calls `handle_msg`, so `App` applies every `Msg::Loaded` it receives.

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib app`
Expected: 13 passed.

**Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat: add app state with runtime worktree switching"
```

---

## Task 10: Comments, reviewed marks and export keys

**Files:**
- Modify: `src/app.rs`, `src/store.rs` (derive `Hash` on `Side`, which `HashSet<(u32, Side)>` needs)

**Step 1: Write the failing tests**

Add to the `tests` module in `src/app.rs`:

```rust
    fn file_with_line(text: &str) -> FileDiff {
        FileDiff {
            path: "a.rs".into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1 @@".into(),
                lines: vec![Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: text.into() }],
            }],
        }
    }

    fn app_on_line() -> App {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_line("let x = 1;")]);
        a.focus = Focus::Diff;
        press(&mut a, 'j'); // Row 0 is the hunk header, so row 1 is the line.
        a
    }

    fn type_comment(a: &mut App, text: &str) -> Vec<Effect> {
        press(a, 'c');
        for ch in text.chars() {
            press(a, ch);
        }
        a.handle_key(KeyEvent::from(KeyCode::Enter))
    }

    #[test]
    fn c_then_enter_saves_a_comment_on_the_current_line() {
        let mut a = app_on_line();
        let fx = type_comment(&mut a, "fix this");
        let c = &a.current().unwrap().review.comments[0];
        assert_eq!((c.line, c.line_text.as_str(), c.body.as_str()), (1, "let x = 1;", "fix this"));
        assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
    }

    #[test]
    fn typing_in_a_comment_does_not_trigger_hotkeys() {
        let mut a = app_on_line();
        type_comment(&mut a, "q?]v1");
        assert!(!a.should_quit && !a.help_open);
        assert_eq!(a.selected, 0);
        assert_eq!(a.view, DiffView::Unified);
        assert_eq!(a.current().unwrap().review.comments[0].body, "q?]v1");
    }

    #[test]
    fn esc_cancels_a_comment() {
        let mut a = app_on_line();
        press(&mut a, 'c');
        press(&mut a, 'x');
        a.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(a.input.is_none());
        assert!(a.current().unwrap().review.comments.is_empty());
    }

    #[test]
    fn empty_comment_is_not_saved() {
        let mut a = app_on_line();
        type_comment(&mut a, "");
        assert!(a.current().unwrap().review.comments.is_empty());
    }

    #[test]
    fn comment_on_a_hunk_header_is_refused() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_line("x")]);
        a.focus = Focus::Diff;
        press(&mut a, 'c');
        assert!(a.input.is_none());
    }

    #[test]
    fn x_marks_reviewed_and_a_changed_file_resets() {
        let mut a = app_on_line();
        press(&mut a, 'x');
        assert!(a.is_reviewed(0));
        deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
        assert!(!a.is_reviewed(0));
    }

    #[test]
    fn one_x_marks_a_file_whose_old_mark_went_stale() {
        let mut a = app_on_line();
        press(&mut a, 'x');
        deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
        assert!(!a.is_reviewed(0));
        press(&mut a, 'x');
        assert!(a.is_reviewed(0));
    }

    #[test]
    fn a_save_records_the_review_it_was_made_for() {
        let mut a = app(&["a", "b"]);
        deliver(&mut a, "a", vec![file_with_line("x")]);
        let fx = press(&mut a, 'x');
        press(&mut a, ']');
        assert!(matches!(&fx[0], Effect::SaveReview { id: ReviewId::Branch(b), review }
            if b == "a" && review.reviewed.len() == 1));
    }

    #[test]
    fn only_the_last_save_per_review_is_kept() {
        let save = |name: &str, marks: usize| Effect::SaveReview {
            id: ReviewId::Branch(name.into()),
            review: Review { comments: vec![], reviewed: (0..marks).map(|i| (i.to_string(), 0)).collect() },
        };
        let kept = coalesce_saves(vec![save("a", 1), save("b", 1), Effect::ReloadWorktrees, save("a", 2)]);
        assert_eq!(kept, vec![save("b", 1), Effect::ReloadWorktrees, save("a", 2)]);
    }

    #[test]
    fn d_deletes_only_the_comment_on_the_current_line() {
        let mut a = app_on_line();
        type_comment(&mut a, "one");
        type_comment(&mut a, "two");
        a.states[0].review.comments.push(Comment {
            path: "other.rs".into(), line: 1, side: Side::New, line_text: "x".into(), body: "keep".into(),
        });
        let fx = press(&mut a, 'd');
        let bodies: Vec<_> = a.current().unwrap().review.comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(bodies, vec!["one", "keep"]);
        assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
    }

    #[test]
    fn d_with_no_comment_here_changes_nothing_and_says_so() {
        let mut a = app_on_line();
        assert!(press(&mut a, 'd').is_empty());
        assert!(a.notice.is_some());
    }

    #[test]
    fn deleting_a_comment_recounts_outdated() {
        let mut a = app_on_line();
        type_comment(&mut a, "fix");
        deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
        assert_eq!(a.outdated_comments(), 1);
        a.states[0].review.comments[0].line_text = "let x = 2;".into();
        press(&mut a, 'd');
        assert_eq!(a.outdated_comments(), 0);
    }

    #[test]
    fn shift_d_clears_after_y_and_saves() {
        let mut a = app_on_line();
        type_comment(&mut a, "fix");
        press(&mut a, 'D');
        assert!(matches!(a.input, Some(Input::ConfirmClear)));
        assert!(a.current().unwrap().review.comments.len() == 1);
        let fx = press(&mut a, 'y');
        assert!(a.current().unwrap().review.comments.is_empty());
        assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
    }

    #[test]
    fn any_other_key_cancels_a_clear() {
        let mut a = app_on_line();
        type_comment(&mut a, "fix");
        press(&mut a, 'D');
        assert!(press(&mut a, 'q').is_empty());
        assert!(a.input.is_none() && !a.should_quit);
        assert_eq!(a.current().unwrap().review.comments.len(), 1);
    }

    #[test]
    fn shift_d_with_no_comments_says_so() {
        let mut a = app_on_line();
        press(&mut a, 'D');
        assert!(a.input.is_none() && a.notice.is_some());
    }

    #[test]
    fn x_again_clears_the_mark() {
        let mut a = app_on_line();
        press(&mut a, 'x');
        press(&mut a, 'x');
        assert!(!a.is_reviewed(0));
    }

    #[test]
    fn comment_is_outdated_after_its_line_changes() {
        let mut a = app_on_line();
        type_comment(&mut a, "fix");
        assert_eq!(a.outdated_comments(), 0);
        deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
        assert_eq!(a.outdated_comments(), 1);
    }

    #[test]
    fn comment_on_a_replaced_line_is_not_outdated() {
        let mut replaced = file_with_line("TWO");
        replaced.hunks[0].lines = vec![
            Line { kind: LineKind::Removed, old_no: Some(1), new_no: None, text: "two".into() },
            Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: "TWO".into() },
        ];
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![replaced]);
        a.focus = Focus::Diff;
        press(&mut a, 'j');
        press(&mut a, 'j');
        type_comment(&mut a, "fix");
        assert_eq!(a.current().unwrap().review.comments[0].side, crate::store::Side::New);
        assert_eq!(a.outdated_comments(), 0);
    }

    #[test]
    fn changing_a_binary_file_resets_its_reviewed_mark() {
        let binary = |id: &str| FileDiff {
            path: "b.dat".into(), old_path: None, status: Status::Modified, binary: true,
            content_id: Some(id.into()), added: 0, removed: 0, hunks: vec![],
        };
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![binary("1..2")]);
        press(&mut a, 'x');
        assert!(a.is_reviewed(0));
        deliver(&mut a, "a", vec![binary("1..3")]);
        assert!(!a.is_reviewed(0));
    }

    #[test]
    fn y_copies_and_shift_y_writes_the_export() {
        let mut a = app_on_line();
        type_comment(&mut a, "fix");
        assert!(matches!(press(&mut a, 'y').as_slice(), [Effect::CopyExport(md)] if md.contains("fix")));
        assert!(matches!(press(&mut a, 'Y').as_slice(), [Effect::WriteExport(md)] if md.contains("fix")));
    }

    #[test]
    fn export_with_no_comments_shows_a_notice() {
        let mut a = app_on_line();
        assert!(press(&mut a, 'y').is_empty());
        assert!(a.notice.is_some());
    }

    #[test]
    fn e_opens_the_editor_at_the_current_line() {
        let mut a = app_on_line();
        assert!(matches!(press(&mut a, 'e').as_slice(), [Effect::OpenEditor { line: 1, .. }]));
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib app`
Expected: FAIL to compile, `Effect::SaveReview` and `App::input` not found.

**Step 3: Implement**

Extend `Effect`:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Load { path: PathBuf, base: String },
    ReloadWorktrees,
    SaveReview { id: ReviewId, review: Review },
    CopyExport(String),
    WriteExport(String),
    OpenEditor { path: PathBuf, line: u32 },
}
```

Add to `App` the field `pub input: Option<Input>`, initialised to `None`, and this type:

```rust
/// A text prompt. While set, keys go here rather than to hotkeys.
pub enum Input {
    Comment { path: String, line: u32, side: Side, line_text: String, text: String },
    ConfirmClear,
}
```

In `handle_key`, immediately before the `switch_worktree` call, so text input is never read as a hotkey:

```rust
        if self.input.is_some() {
            return self.handle_input_key(key);
        }
```

Add these arms to the main `match key.code`:

```rust
            KeyCode::Char('c') => self.start_comment(),
            KeyCode::Char('d') => return self.delete_comment(),
            KeyCode::Char('D') => self.start_clear(),
            KeyCode::Char('x') => return self.toggle_reviewed(),
            KeyCode::Char('e') => return self.open_editor(),
            KeyCode::Char('y') => return self.export(Effect::CopyExport),
            KeyCode::Char('Y') => return self.export(Effect::WriteExport),
```

Add these methods:

```rust
    /// The diff line under the cursor. `None` on a hunk header.
    pub fn line_at_cursor(&self) -> Option<&crate::diff::Line> {
        let mut row = self.current()?.cursor;
        for hunk in &self.current_file()?.hunks {
            if row == 0 {
                return None;
            }
            row -= 1;
            if row < hunk.lines.len() {
                return hunk.lines.get(row);
            }
            row -= hunk.lines.len();
        }
        None
    }

    fn start_comment(&mut self) {
        let Some(path) = self.current_file().map(|f| f.path.clone()) else { return };
        let Some(line) = self.line_at_cursor() else {
            self.notice = Some("Move to a diff line to comment".into());
            return;
        };
        let Some((no, side)) = line_position(line) else { return };
        let line_text = line.text.clone();
        self.input = Some(Input::Comment { path, line: no, side, line_text, text: String::new() });
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if matches!(self.input, Some(Input::ConfirmClear)) {
            self.input = None;
            return if key.code == KeyCode::Char('y') { self.clear_comments() } else { Vec::new() };
        }
        let Some(Input::Comment { text, .. }) = self.input.as_mut() else { return Vec::new() };
        match key.code {
            KeyCode::Char(c) => text.push(c),
            KeyCode::Backspace => { text.pop(); }
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                if let Some(Input::Comment { path, line, side, line_text, text }) = self.input.take() {
                    if !text.trim().is_empty() {
                        if let Some(s) = self.current_mut() {
                            s.review.comments.push(Comment { path, line, side, line_text, body: text });
                            s.outdated = count_outdated(s);
                            return self.save_effect();
                        }
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// The comment on the line under the cursor, newest first when there are several.
    pub fn comment_at_cursor(&self) -> Option<&Comment> {
        let path = &self.current_file()?.path;
        let (line, side) = line_position(self.line_at_cursor()?)?;
        self.current()?.review.comments.iter().rev()
            .find(|c| &c.path == path && c.line == line && c.side == side)
    }

    /// Positions in the current file that have comments, for the diff's markers.
    pub fn commented_lines(&self) -> HashSet<(u32, Side)> {
        let (Some(s), Some(file)) = (self.current(), self.current_file()) else { return HashSet::new() };
        s.review.comments.iter().filter(|c| c.path == file.path).map(|c| (c.line, c.side)).collect()
    }

    fn delete_comment(&mut self) -> Vec<Effect> {
        let Some(comment) = self.comment_at_cursor().cloned() else {
            self.notice = Some("No comment on this line".into());
            return Vec::new();
        };
        let Some(s) = self.current_mut() else { return Vec::new() };
        if let Some(i) = s.review.comments.iter().rposition(|c| *c == comment) {
            s.review.comments.remove(i);
        }
        s.outdated = count_outdated(s);
        self.save_effect()
    }

    fn start_clear(&mut self) {
        if self.current().is_some_and(|s| !s.review.comments.is_empty()) {
            self.input = Some(Input::ConfirmClear);
        } else {
            self.notice = Some("No comments to clear".into());
        }
    }

    fn clear_comments(&mut self) -> Vec<Effect> {
        let Some(s) = self.current_mut() else { return Vec::new() };
        s.review.comments.clear();
        s.outdated = 0;
        self.save_effect()
    }

    /// Built when the edit happens, because later keys in the same batch can
    /// change the selection before `main` performs this.
    fn save_effect(&self) -> Vec<Effect> {
        match (self.selected_worktree(), self.current()) {
            (Some(w), Some(s)) => vec![Effect::SaveReview { id: ReviewId::of(w), review: s.review.clone() }],
            _ => Vec::new(),
        }
    }

    fn toggle_reviewed(&mut self) -> Vec<Effect> {
        let Some((path, hash)) = self.current_file().map(|f| (f.path.clone(), content_hash(f))) else {
            return Vec::new();
        };
        let Some(s) = self.current_mut() else { return Vec::new() };
        // Compare hashes: a mark left over from older content shows as unreviewed.
        if s.review.reviewed.get(&path) == Some(&hash) {
            s.review.reviewed.remove(&path);
        } else {
            s.review.reviewed.insert(path, hash);
        }
        self.save_effect()
    }

    pub fn is_reviewed(&self, index: usize) -> bool {
        let Some(s) = self.current() else { return false };
        let Some(f) = s.diffs.as_ref().and_then(|d| d.get(index)) else { return false };
        s.review.reviewed.get(&f.path) == Some(&content_hash(f))
    }

    /// Counted on load and when comments change, because this runs every frame.
    pub fn outdated_comments(&self) -> usize {
        self.current().map_or(0, |s| s.outdated)
    }

    fn export(&mut self, effect: fn(String) -> Effect) -> Vec<Effect> {
        let Some(w) = self.selected_worktree() else { return Vec::new() };
        let branch = w.branch.clone().unwrap_or_else(|| w.head.clone());
        let Some(s) = self.current() else { return Vec::new() };
        let md = crate::export::render(&branch, &self.base, &s.review.comments);
        if md.is_empty() {
            self.notice = Some("No comments to export".into());
            return Vec::new();
        }
        vec![effect(md)]
    }

    fn open_editor(&mut self) -> Vec<Effect> {
        let Some(root) = self.selected_worktree().map(|w| w.path.clone()) else { return Vec::new() };
        let Some(file) = self.current_file() else { return Vec::new() };
        let path = root.join(&file.path);
        let line = self.line_at_cursor().and_then(|l| l.new_no).unwrap_or(1);
        vec![Effect::OpenEditor { path, line }]
    }
```

When `apply_worktrees` finds the selected worktree was removed, cancel any open comment or clear prompt, since it no longer has a target and would otherwise act on the next worktree:

```rust
if matches!(self.input, Some(Input::Comment { .. } | Input::ConfirmClear)) {
    self.input = None;
}
```

Add a test for each prompt type: open it on worktree `a`, remove `a`, press Enter or `y`, and assert `b`'s review is unchanged and no save happens.

Add `pub outdated: usize` to `WorktreeState`, and `s.outdated = count_outdated(s);` in the `Ok(l)` arm of `Msg::Loaded`, directly after `s.replace_diffs(l.diffs);`.

Add these free functions and imports:

```rust
use crate::store::{fnv1a, Comment, ReviewId, Side};
use std::collections::HashSet;

/// Identifies a file's content cheaply, since this runs for every file on
/// every frame. Files with no `content_id`, such as mode-only changes, pure
/// renames and dirty submodules, fall back to hashing hunks.
///
/// Saved in reviews, so it uses `fnv1a`, which is stable across Rust releases.
fn content_hash(file: &crate::diff::FileDiff) -> u64 {
    if let Some(id) = &file.content_id {
        return fnv1a(id.as_bytes());
    }
    let mut bytes = Vec::new();
    for line in file.hunks.iter().flat_map(|h| &h.lines) {
        bytes.push(line.kind as u8);
        bytes.extend_from_slice(line.text.as_bytes());
        bytes.push(b'\n');
    }
    fnv1a(&bytes)
}

/// Keeps the last save per review, since each one costs an fsync and holding
/// `x` would otherwise queue saves faster than they finish.
pub fn coalesce_saves(effects: Vec<Effect>) -> Vec<Effect> {
    let mut seen = HashSet::new();
    let mut kept: Vec<Effect> = effects.into_iter().rev().filter(|e| match e {
        Effect::SaveReview { id, .. } => seen.insert(id.clone()),
        _ => true,
    }).collect();
    kept.reverse();
    kept
}

/// Where a diff line sits for a comment: the new file's number, or the old
/// file's for a removed line.
pub fn line_position(line: &crate::diff::Line) -> Option<(u32, Side)> {
    match (line.new_no, line.old_no) {
        (Some(n), _) => Some((n, Side::New)),
        (None, Some(o)) => Some((o, Side::Old)),
        (None, None) => None,
    }
}

/// Counts comments whose line text is no longer at that line in the diff.
fn count_outdated(s: &WorktreeState) -> usize {
    let Some(diffs) = s.diffs.as_ref() else { return 0 };
    s.review.comments.iter().filter(|c| {
        // Filter rather than find: a typechange gives two entries with one path.
        let current = diffs.iter()
            .filter(|f| f.path == c.path)
            .flat_map(|f| f.hunks.iter().flat_map(|h| &h.lines))
            .find(|l| match c.side {
                Side::New => l.new_no == Some(c.line),
                Side::Old => l.new_no.is_none() && l.old_no == Some(c.line),
            })
            .map(|l| l.text.as_str());
        c.is_outdated(current)
    }).count()
}
```


**Step 4: Run tests to verify they pass**

Run: `cargo test --lib app`
Expected: all pass.

**Step 5: Commit**

```bash
git add src/app.rs src/store.rs
git commit -m "feat: add comments, reviewed marks, export and editor actions"
```

---

## Task 11: Fuzzy worktree finder and base branch picker

**Files:**
- Modify: `src/fuzzy.rs`, `src/app.rs`

**Step 1: Write the failing tests**

In `src/fuzzy.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_matches_and_drops_non_matches() {
        let items = vec!["feat-auth".to_string(), "fix-cache".to_string()];
        assert_eq!(filter("auth", &items), vec![0]);
    }

    #[test]
    fn empty_query_keeps_everything_in_order() {
        let items = vec!["b".to_string(), "a".to_string()];
        assert_eq!(filter("", &items), vec![0, 1]);
    }
}
```

Add to the tests in `src/app.rs`:

```rust
    #[test]
    fn w_finds_and_selects_a_worktree() {
        let mut a = app(&["feat-auth", "fix-cache"]);
        press(&mut a, 'w');
        for c in "cache".chars() {
            press(&mut a, c);
        }
        a.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(a.selected, 1);
        assert!(a.input.is_none());
    }

    #[test]
    fn b_changes_the_base_and_reloads_every_worktree() {
        let mut a = app(&["a", "b"]);
        let fx = press(&mut a, 'b');
        assert!(matches!(fx.as_slice(), [Effect::ListBranches]));
        a.handle_msg(Msg::Branches(Ok(vec!["develop".into(), "main".into()])));
        for c in "main".chars() {
            press(&mut a, c);
        }
        let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(a.base, "main");
        assert_eq!(fx.iter().filter(|e| matches!(e, Effect::Load { .. })).count(), 2);
    }

    #[test]
    fn a_base_change_reloads_the_selected_worktree_first() {
        let mut a = app(&["a", "b", "c"]);
        press(&mut a, '3');
        press(&mut a, 'b');
        a.handle_msg(Msg::Branches(Ok(vec!["develop".into(), "main".into()])));
        for c in "main".chars() {
            press(&mut a, c);
        }
        let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(&fx[0], Effect::Load { path, .. } if path.ends_with("c")));
    }

    #[test]
    fn a_late_branch_list_keeps_an_open_comment() {
        let mut a = app_on_line();
        press(&mut a, 'c');
        press(&mut a, 'x');
        a.handle_msg(Msg::Branches(Ok(vec!["develop".into()])));
        assert!(matches!(a.input, Some(Input::Comment { .. })));
    }

    #[test]
    fn missing_base_opens_the_base_picker() {
        let mut a = app(&["a"]);
        let path: std::path::PathBuf = "/r/a".into();
        let fx = a.handle_msg(Msg::Loaded { path, result: Err(crate::git::GitError::BaseMissing("develop".into())) });
        assert!(matches!(fx.as_slice(), [Effect::ListBranches]));
    }

    #[test]
    fn a_missing_older_base_does_not_reopen_the_picker() {
        let mut a = app(&["a"]);
        a.base = "main".into();
        let fx = a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::BaseMissing("develop".into())) });
        assert!(fx.is_empty());
    }

    #[test]
    fn one_worktree_failing_leaves_the_others_usable() {
        let mut a = app(&["a", "b"]);
        a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::NoMergeBase) });
        deliver(&mut a, "b", vec![file("1")]);
        assert!(a.states[0].error.is_some());
        press(&mut a, ']');
        assert_eq!(a.file_count(), 1);
    }
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib`
Expected: FAIL to compile.

**Step 3: Implement `src/fuzzy.rs`**

```rust
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Indices of `items` matching `query`, best match first.
pub fn filter(query: &str, items: &[String]) -> Vec<usize> {
    if query.is_empty() {
        return (0..items.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(query, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = items.iter().enumerate()
        .filter_map(|(i, s)| pattern.score(Utf32Str::new(s, &mut buf), &mut matcher).map(|score| (i, score)))
        .collect();
    scored.sort_by_key(|&(i, score)| (std::cmp::Reverse(score), i));
    scored.into_iter().map(|(i, _)| i).collect()
}
```

**Step 4: Extend `src/app.rs`**

Add `ListBranches` to `Effect` and new variants to `Input`:

```rust
pub enum Input {
    Comment { path: String, line: u32, side: Side, line_text: String, text: String },
    Worktree { query: String },
    Base { query: String, branches: Vec<String> },
    ConfirmClear,
}
```

Add `w` to `switch_worktree`, so it works from any screen, including help:

```rust
            KeyCode::Char('w') => {
                self.help_open = false;
                self.input = Some(Input::Worktree { query: String::new() });
            }
```

Add a test that `w` opens the finder and closes help when help is open.

Add this to the main match:

```rust
            KeyCode::Char('b') => return vec![Effect::ListBranches],
```

Replace `handle_input_key`:

```rust
    fn handle_input_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if matches!(self.input, Some(Input::ConfirmClear)) {
            self.input = None;
            return if key.code == KeyCode::Char('y') { self.clear_comments() } else { Vec::new() };
        }
        let Some(input) = self.input.as_mut() else { return Vec::new() };
        let text = match input {
            Input::Comment { text, .. } => text,
            Input::Worktree { query } | Input::Base { query, .. } => query,
            Input::ConfirmClear => return Vec::new(),
        };
        match key.code {
            KeyCode::Char(c) => text.push(c),
            KeyCode::Backspace => { text.pop(); }
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => return self.submit_input(),
            _ => {}
        }
        Vec::new()
    }

    pub fn worktree_labels(&self) -> Vec<String> {
        self.worktrees.iter().map(worktree_label).collect()
    }

    fn submit_input(&mut self) -> Vec<Effect> {
        match self.input.take() {
            Some(Input::Comment { path, line, side, line_text, text }) if !text.trim().is_empty() => {
                if let Some(s) = self.current_mut() {
                    s.review.comments.push(Comment { path, line, side, line_text, body: text });
                    s.outdated = count_outdated(s);
                    return self.save_effect();
                }
            }
            Some(Input::Worktree { query }) => {
                if let Some(&i) = crate::fuzzy::filter(&query, &self.worktree_labels()).first() {
                    self.selected = i;
                }
            }
            Some(Input::Base { query, branches }) => {
                if let Some(&i) = crate::fuzzy::filter(&query, &branches).first() {
                    self.base = branches[i].clone();
                    return self.reload_all();
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn reload_all(&mut self) -> Vec<Effect> {
        let selected = self.selected_worktree().map(|w| w.path.clone());
        let mut paths: Vec<PathBuf> = self.worktrees.iter().map(|w| w.path.clone()).collect();
        // The worktree on screen first, since the pool runs jobs in order.
        paths.sort_by_key(|p| Some(p) != selected.as_ref());
        paths.into_iter().map(|path| Effect::Load { path, base: self.base.clone() }).collect()
    }
```

Add this free function:

```rust
pub fn worktree_label(w: &Worktree) -> String {
    w.branch.clone().unwrap_or_else(|| {
        let short: String = w.head.chars().take(8).collect();
        format!("({short})")
    })
}
```

In `handle_msg`, replace the `Msg::Branches` handling and the `Err` arm of `Msg::Loaded`:

```rust
            Msg::Branches(Ok(branches)) => {
                // A late reply must not wipe a half-written comment.
                if self.input.is_none() {
                    self.input = Some(Input::Base { query: String::new(), branches });
                }
                Vec::new()
            }
            Msg::Branches(Err(e)) => {
                self.notice = Some(e.to_string());
                Vec::new()
            }
            Msg::Highlighted { .. } => Vec::new(),
```

```rust
                        Err(e) => {
                            // A result for an older base can arrive after the base has changed.
                            let missing = matches!(&e, GitError::BaseMissing(b) if *b == self.base);
                            s.error = Some(e);
                            if missing && self.input.is_none() {
                                return vec![Effect::ListBranches];
                            }
                        }
```

**Step 5: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: all pass.

**Step 6: Commit**

```bash
git add src/fuzzy.rs src/app.rs
git commit -m "feat: add worktree finder and runtime base branch picker"
```

---

## Task 12: Syntax highlighting

Highlighting runs on workers only. The UI draws plain text first, then swaps in colour when `Msg::Highlighted` arrives.

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock`, `src/highlight.rs`, `src/app.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Hunk, Line, LineKind, Status};

    fn file(path: &str, text: &str) -> FileDiff {
        FileDiff {
            path: path.into(), old_path: None, status: Status::Added, binary: false, content_id: None, added: 1, removed: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1 @@".into(),
                lines: vec![Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: text.into() }],
            }],
        }
    }

    #[test]
    fn colours_known_languages_and_keeps_the_text() {
        let out = highlight_file(&file("a.rs", "fn main() {}"));
        let line = &out.hunks[0][0];
        assert!(line.len() > 1);
        assert_eq!(line.iter().map(|(_, s)| s.as_str()).collect::<String>(), "fn main() {}");
    }

    #[test]
    fn unknown_extension_yields_one_plain_span() {
        let out = highlight_file(&file("a.unknownext", "text"));
        assert_eq!(out.hunks[0][0].len(), 1);
    }

    #[test]
    fn files_over_the_line_limit_stay_plain() {
        let mut big = file("a.rs", "fn main() {}");
        let line = big.hunks[0].lines[0].clone();
        big.hunks[0].lines = vec![line; MAX_FILE_LINES + 1];
        assert!(highlight_file(&big).hunks.is_empty());
    }

    #[test]
    fn oversized_files_are_not_highlighted() {
        let long = "x".repeat(MAX_LINE_LEN + 1);
        let out = highlight_file(&file("a.rs", &long));
        assert_eq!(out.hunks[0][0].len(), 1);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib highlight`
Expected: FAIL to compile, `hunks` field not found.

**Step 3: Add the extra syntaxes**

syntect's bundled set has no TypeScript, TSX or TOML. Add this to `[dependencies]`:

```toml
two-face = { version = "0.5", default-features = false, features = ["syntect-fancy"] }
```

`two-face` replaces syntect's bundled syntaxes, so remove the now-unused feature from syntect:

```toml
syntect = { version = "5.3", default-features = false, features = ["parsing", "default-themes", "regex-fancy"] }
```

Add a test to the module above:

```rust
    #[test]
    fn colours_typescript_tsx_and_toml() {
        for (path, text) in [("a.ts", "const x: number = 1;"), ("a.tsx", "const a = <div>hi</div>;"), ("a.toml", "a = \"b\"")] {
            assert!(highlight_file(&file(path, text)).hunks[0][0].len() > 1, "{path}");
        }
    }
```

**Step 4: Implement**

```rust
use crate::diff::FileDiff;
use ratatui::style::Color;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

pub const MAX_LINE_LEN: usize = 2_000;
/// Syntect manages about 1,000 lines a second on real TypeScript, so this
/// bounds how long one highlight can hold a worker.
pub const MAX_FILE_LINES: usize = 5_000;

pub type Span = (Color, String);

#[derive(Debug, Clone, Default)]
pub struct HighlightedFile {
    /// Spans for each line of each hunk, parallel to `FileDiff::hunks`.
    pub hunks: Vec<Vec<Vec<Span>>>,
}

fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes.themes.remove("base16-ocean.dark").expect("bundled theme");
        (two_face::syntax::extra_newlines(), theme)
    })
}

pub fn highlight_file(file: &FileDiff) -> HighlightedFile {
    // Empty, not plain copies: the UI falls back to raw text, and copying every
    // line of a huge file would cost more than it saves.
    if file.hunks.iter().map(|h| h.lines.len()).sum::<usize>() > MAX_FILE_LINES {
        return HighlightedFile::default();
    }
    let (syntaxes, theme) = assets();
    let ext = file.path.rsplit('.').next().unwrap_or("");
    let syntax = syntaxes.find_syntax_by_extension(ext);
    let hunks = file.hunks.iter().map(|hunk| {
        let mut state = syntax.map(|s| HighlightLines::new(s, theme));
        hunk.lines.iter().map(|line| {
            if line.text.len() > MAX_LINE_LEN {
                return plain(&line.text);
            }
            let with_newline = format!("{}\n", line.text);
            let spans = state.as_mut().and_then(|h| {
                h.highlight_line(&with_newline, syntaxes).ok()
            });
            match spans {
                Some(spans) => spans.iter().map(|(style, text)| {
                    let c = style.foreground;
                    (Color::Rgb(c.r, c.g, c.b), text.trim_end_matches('\n').to_string())
                }).filter(|(_, t)| !t.is_empty()).collect(),
                None => plain(&line.text),
            }
        }).collect()
    }).collect();
    HighlightedFile { hunks }
}

fn plain(text: &str) -> Vec<Span> {
    vec![(Color::Reset, text.to_string())]
}
```

Add `Effect::Highlight { key: CacheKey, file_index: usize, diffs: Diffs }` to `app.rs`, and a `pub highlights: HashMap<usize, HighlightedFile>` field to `WorktreeState`, which needs `derive(Default)` to remain valid. Add:

```rust
    /// Requests colour for the current file if it does not have it yet.
    pub fn highlight_effect(&self) -> Option<Effect> {
        let s = self.current()?;
        // A worktree with no changes has no file to highlight.
        s.diffs.as_ref()?.get(s.file)?;
        if s.highlights.contains_key(&s.file) {
            return None;
        }
        Some(Effect::Highlight { key: s.key.clone()?, file_index: s.file, diffs: s.diffs.clone()? })
    }
```

Add a test in `src/app.rs` that `highlight_effect` returns `None` for a worktree whose loaded diff is empty, using `deliver(&mut a, "a", vec![])`.

Handle the message, ignoring results for a superseded diff:

```rust
            Msg::Highlighted { key, file_index, lines } => {
                if let Some(s) = self.states.iter_mut().find(|s| s.key.as_ref() == Some(&key)) {
                    s.highlights.insert(file_index, lines);
                }
                Vec::new()
            }
```

In the `Ok(l)` arm of `Msg::Loaded`, before `s.key` is assigned, add `if s.key.as_ref() != Some(&l.key) { s.highlights.clear(); }`. An equal key means the same diff, so a refresh with no changes keeps its colours. Add a test that reloading with an unchanged key keeps `highlights` and a changed key clears them. `main` calls `highlight_effect` after every event, and de-duplicates with a `HashSet` of in-flight `(CacheKey, file_index)` pairs. The full key is needed because clean worktrees share a fingerprint.

**Step 5: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: all pass.

**Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/highlight.rs src/app.rs
git commit -m "feat: highlight diffs on worker threads"
```

---

## Task 13: UI drawing

**Files:**
- Modify: `src/ui.rs`
- Create: `tests/ui_snapshots.rs`

**Step 1: Write the failing snapshot tests**

`tests/ui_snapshots.rs`:

```rust
use branchdiff::app::{App, Effect, Focus};
use branchdiff::cache::CacheKey;
use branchdiff::diff::{FileDiff, Hunk, Line, LineKind, Status};
use branchdiff::git::{GitError, Worktree};
use branchdiff::ui;
use branchdiff::worker::{Loaded, Msg};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::Terminal;
use std::sync::Arc;

fn sample_file() -> FileDiff {
    FileDiff {
        path: "src/a.rs".into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 1,
        hunks: vec![Hunk {
            header: "@@ -1,2 +1,2 @@".into(),
            lines: vec![
                Line { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "fn load() {".into() },
                Line { kind: LineKind::Removed, old_no: Some(2), new_no: None, text: "    old();".into() },
                Line { kind: LineKind::Added, old_no: None, new_no: Some(2), text: "    retry();".into() },
            ],
        }],
    }
}

fn app() -> App {
    let mut a = App::new("develop".into());
    let wts = ["feat-auth", "fix-cache"].map(|n| Worktree { path: format!("/r/{n}").into(), head: "h".into(), branch: Some(n.into()) });
    a.handle_msg(Msg::Worktrees(Ok(wts.to_vec())));
    let path: std::path::PathBuf = "/r/feat-auth".into();
    let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
    let _: Vec<Effect> = a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: Arc::new(vec![sample_file()]), op: None }) });
    a
}

fn render(a: &mut App) -> String {
    render_sized(a, 80, 12)
}

fn render_sized(a: &mut App, width: u16, height: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| ui::draw(f, a)).unwrap();
    t.backend().to_string()
}

#[test]
fn main_screen() {
    insta::assert_snapshot!(render(&mut app()));
}

#[test]
fn side_by_side() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char('v')));
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn loading_placeholder_for_a_worktree_with_no_data() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char(']')));
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn help_overlay_lists_every_hotkey() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char('?')));
    let out = render_sized(&mut a, 80, 24);
    for label in ["next worktree", "fuzzy", "comment", "reviewed", "base branch", "this help"] {
        assert!(out.to_lowercase().contains(label), "missing {label}");
    }
    insta::assert_snapshot!(out);
}

#[test]
fn error_is_shown_on_the_affected_worktree() {
    let mut a = app();
    a.handle_msg(Msg::Loaded { path: "/r/fix-cache".into(), result: Err(GitError::NoMergeBase) });
    a.handle_key(KeyEvent::from(KeyCode::Char(']')));
    assert!(render(&mut a).contains("no common history"));
}

#[test]
fn a_failed_worktree_list_is_shown() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Err(GitError::Failed("boom".into()))));
    assert!(render(&mut a).contains("Could not list worktrees: boom"));
}

#[test]
fn commented_lines_are_marked_and_their_comment_is_shown() {
    let mut a = app();
    a.focus = Focus::Diff;
    for _ in 0..3 {
        a.handle_key(KeyEvent::from(KeyCode::Char('j')));
    }
    a.handle_key(KeyEvent::from(KeyCode::Char('c')));
    for c in "why".chars() {
        a.handle_key(KeyEvent::from(KeyCode::Char(c)));
    }
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    let out = render(&mut a);
    assert!(out.contains("*    2 +"));
    assert!(out.contains(" * why"));
}

#[test]
fn stale_badge_is_visible() {
    let mut a = app();
    a.handle_msg(Msg::Stale("/r/feat-auth".into()));
    assert!(render(&mut a).contains("changed"));
}

#[test]
fn renders_at_tiny_sizes_without_panicking() {
    let mut a = app();
    for (w, h) in [(1, 1), (10, 3), (20, 5)] {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| ui::draw(f, &mut a)).unwrap();
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --test ui_snapshots`
Expected: FAIL to compile, `ui::draw` not found.

**Step 3: Implement `src/ui.rs`**

```rust
use crate::app::{line_position, worktree_label, App, DiffView, Focus, Input};
use crate::diff::{FileDiff, Line, LineKind};
use crate::store::Side;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use std::collections::HashSet;

const HELP: &[(&str, &str)] = &[
    ("[ / ]", "previous or next worktree"),
    ("1-9", "jump to a worktree"),
    ("w", "fuzzy find a worktree"),
    ("Tab", "move between areas"),
    ("j / k", "move and scroll"),
    ("n / p", "next or previous file"),
    ("v", "unified or side-by-side diff"),
    ("c", "comment on the current line"),
    ("d / D", "delete a comment, or clear all"),
    ("e", "open in $EDITOR"),
    ("x", "mark file as reviewed"),
    ("y / Y", "copy comments, or write to a file"),
    ("r", "refresh or retry"),
    ("b", "change the base branch"),
    ("?", "open or close this help"),
    ("q", "quit"),
];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [left, files, diff] = Layout::horizontal([
        Constraint::Length(26), Constraint::Length(24), Constraint::Min(0),
    ]).areas(main);

    draw_worktrees(f, app, left);
    draw_files(f, app, files);
    draw_diff(f, app, diff);
    draw_status(f, app, status);

    if app.help_open {
        draw_help(f);
    }
    if app.input.is_some() {
        draw_input(f, app);
    }
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused { Style::new().fg(Color::Cyan) } else { Style::new() };
    Block::default().borders(Borders::ALL).border_style(style).title(title)
}

fn draw_worktrees(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.worktrees.iter().zip(&app.states).map(|(w, s)| {
        let added: u32 = s.diffs.as_ref().map_or(0, |d| d.iter().map(|x| x.added).sum());
        let mut label = format!("{} +{added}", worktree_label(w));
        if !s.review.comments.is_empty() {
            label.push_str(&format!(" c{}", s.review.comments.len()));
        }
        if s.stale { label.push_str(" ●"); }
        if s.op.is_some() { label.push_str(" !"); }
        if s.error.is_some() { label.push_str(" ✗"); }
        ListItem::new(label)
    }).collect();
    let mut state = ListState::default().with_selected(Some(app.selected));
    let list = List::new(items)
        .block(block("Worktrees", app.focus == Focus::Worktrees))
        .highlight_symbol("▸ ")
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
    let Some(s) = app.current() else {
        f.render_widget(block("Files", false), area);
        return;
    };
    let items: Vec<ListItem> = s.diffs.iter().flat_map(|d| d.iter()).enumerate().map(|(i, x)| {
        let mark = if app.is_reviewed(i) { "✓" } else { " " };
        ListItem::new(format!("{mark}{} {}", x.status.letter(), x.path))
    }).collect();
    let mut state = ListState::default().with_selected(Some(s.file));
    let list = List::new(items)
        .block(block("Files", app.focus == Focus::Files))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state);
}

/// Builds only the visible rows, so frame cost does not grow with diff size.
fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let Some(s) = app.current_mut() else {
        f.render_widget(block("Diff", false), area);
        return;
    };
    if s.cursor < s.scroll {
        s.scroll = s.cursor;
    } else if visible > 0 && s.cursor >= s.scroll + visible {
        s.scroll = s.cursor + 1 - visible;
    }
    let app = &*app;
    let Some(s) = app.current() else { return };
    let focused = app.focus == Focus::Diff;

    let message = match (&s.error, &s.diffs) {
        (Some(e), _) => Some(e.to_string()),
        (None, None) => Some("Loading…".to_string()),
        (None, Some(d)) if d.is_empty() => Some("No changes against the base branch".to_string()),
        _ => None,
    };
    if let Some(m) = message {
        f.render_widget(Paragraph::new(m).block(block("Diff", focused)), area);
        return;
    }
    let Some(file) = app.current_file() else { return };
    if file.binary {
        f.render_widget(Paragraph::new("Binary or very large file not shown").block(block("Diff", focused)), area);
        return;
    }
    let highlight = s.highlights.get(&s.file);
    let commented = app.commented_lines();
    let rows = match app.view {
        DiffView::Unified => unified_rows(file, highlight, &commented, s.cursor, s.scroll, visible),
        DiffView::SideBySide => split_rows(file, &commented, s.cursor, s.scroll, visible, area.width.saturating_sub(2) as usize),
    };
    f.render_widget(Paragraph::new(rows).block(block("Diff", focused)), area);
}

fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Added => Style::new().bg(Color::Rgb(16, 48, 24)),
        LineKind::Removed => Style::new().bg(Color::Rgb(56, 16, 16)),
        LineKind::Context => Style::new(),
    }
}

/// Gutter mark for a line that has a comment.
fn comment_mark(commented: &HashSet<(u32, Side)>, line: &Line) -> &'static str {
    if line_position(line).is_some_and(|p| commented.contains(&p)) { "*" } else { " " }
}

fn unified_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
) -> Vec<TextLine<'a>> {
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    'outer: for (hi, hunk) in file.hunks.iter().enumerate() {
        if row + 1 + hunk.lines.len() <= scroll {
            row += 1 + hunk.lines.len();
            continue;
        }
        if row >= scroll {
            rows.push(mark(TextLine::from(hunk.header.as_str()).style(Style::new().fg(Color::Cyan)), row == cursor));
        }
        row += 1;
        for (li, line) in hunk.lines.iter().enumerate() {
            if rows.len() >= visible {
                break 'outer;
            }
            if row >= scroll {
                let prefix = match line.kind {
                    LineKind::Added => "+",
                    LineKind::Removed => "-",
                    LineKind::Context => " ",
                };
                let number = line.new_no.or(line.old_no).unwrap_or(0);
                let gutter = comment_mark(commented, line);
                let mut spans = vec![Span::raw(format!("{gutter}{number:>5} {prefix}"))];
                match highlight.and_then(|h| h.hunks.get(hi)).and_then(|h| h.get(li)) {
                    Some(parts) => spans.extend(parts.iter().map(|(c, t)| Span::styled(t.as_str(), Style::new().fg(*c)))),
                    None => spans.push(Span::raw(line.text.as_str())),
                }
                rows.push(mark(TextLine::from(spans).style(line_style(line.kind)), row == cursor));
            }
            row += 1;
        }
        if rows.len() >= visible {
            break;
        }
    }
    rows
}

fn split_rows<'a>(
    file: &'a FileDiff,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
    width: usize,
) -> Vec<TextLine<'a>> {
    let side = width.saturating_sub(4) / 2;
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    for hunk in &file.hunks {
        if rows.len() >= visible {
            break;
        }
        if row >= scroll {
            rows.push(mark(TextLine::from(hunk.header.as_str()).style(Style::new().fg(Color::Cyan)), row == cursor));
        }
        row += 1;
        for line in &hunk.lines {
            if rows.len() >= visible {
                break;
            }
            if row >= scroll {
                let left = if line.old_no.is_some() { line.text.as_str() } else { "" };
                let right = if line.new_no.is_some() { line.text.as_str() } else { "" };
                let gutter = comment_mark(commented, line);
                let text = format!("{gutter}{} │ {}", pad(left, side), pad(right, side));
                rows.push(mark(TextLine::from(text).style(line_style(line.kind)), row == cursor));
            }
            row += 1;
        }
    }
    rows
}

fn pad(s: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push_str(&" ".repeat(width - used));
    out
}

fn mark(line: TextLine<'_>, selected: bool) -> TextLine<'_> {
    if selected { line.patch_style(Style::new().add_modifier(Modifier::REVERSED)) } else { line }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut text = format!(" base: {}", app.base);
    if let Some(e) = &app.fatal {
        text = format!(" Could not list worktrees: {e}");
    } else if let Some(n) = &app.notice {
        text = format!(" {n}");
    } else if let Some(c) = app.comment_at_cursor() {
        text = format!(" * {}", c.body);
    } else if let (Some(w), Some(s)) = (app.selected_worktree(), app.current()) {
        let count = s.diffs.as_ref().map_or(0, |d| d.len());
        text.push_str(&format!("   {}   {count} files   {} comments", worktree_label(w), s.review.comments.len()));
        let outdated = app.outdated_comments();
        if outdated > 0 { text.push_str(&format!(" ({outdated} outdated)")); }
        if s.stale { text.push_str("   ● changed"); }
        if let Some(op) = s.op { text.push_str(&format!("   ! {op} in progress")); }
    }
    text.push_str("   ? help");
    f.render_widget(Paragraph::new(text), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (w, h) = (width.min(area.width), height.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 50, HELP.len() as u16 + 2);
    let rows: Vec<TextLine> = HELP.iter().map(|(k, d)| TextLine::from(format!(" {k:<7} {d}"))).collect();
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block("Help", true)), area);
}

fn draw_input(f: &mut Frame, app: &App) {
    let Some(input) = &app.input else { return };
    let (title, text, options) = match input {
        Input::Comment { text, .. } => ("Comment (Enter saves, Esc cancels)", text.as_str(), Vec::new()),
        Input::ConfirmClear => ("Clear all comments for this worktree?", "y to clear, any other key cancels", Vec::new()),
        Input::Worktree { query } => ("Find worktree", query.as_str(), {
            let labels = app.worktree_labels();
            crate::fuzzy::filter(query, &labels).into_iter().map(|i| labels[i].clone()).collect()
        }),
        Input::Base { query, branches } => ("Base branch", query.as_str(), {
            crate::fuzzy::filter(query, branches).into_iter().map(|i| branches[i].clone()).collect()
        }),
    };
    let area = centered(f.area(), 60, (options.len().min(8) as u16) + 3);
    let mut rows = vec![TextLine::from(format!("> {text}"))];
    rows.extend(options.into_iter().take(8).map(TextLine::from));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block(title, true)), area);
}
```

**Step 4: Handle tabs, carriage returns and content-free files**

ratatui drops control characters, so a tab-indented line would lose its indentation, and a trailing `\r` would throw off width calculations. `Line.text` stays faithful to the file, because comments and reviewed marks compare against it, so convert only when drawing. Add to `src/ui.rs`:

```rust
use std::borrow::Cow;

const TAB: &str = "    ";

/// Text as drawn: tabs become spaces and carriage returns are removed.
fn display_text(s: &str) -> Cow<'_, str> {
    if s.contains(['\t', '\r']) {
        Cow::Owned(s.replace('\t', TAB).replace('\r', ""))
    } else {
        Cow::Borrowed(s)
    }
}
```

Use it for every piece of diff text in `unified_rows` and `split_rows`, including highlighted spans, for example `Span::raw(display_text(&line.text))`, and pass the result to `pad` in `split_rows`. Do not change `Line.text`.

In `draw_diff`, next to the binary check, show a message for files with no hunks, such as mode-only changes and empty files, so they don't look like a failed load:

```rust
    if file.hunks.is_empty() {
        f.render_widget(Paragraph::new("No text changes to show").block(block("Diff", focused)), area);
        return;
    }
```

Add these tests to `tests/ui_snapshots.rs`: a tab-indented line renders with four spaces, a CRLF line renders with no stray characters and keeps its neighbours aligned in side-by-side view, and a file with no hunks shows `No text changes to show`.

**Step 5: Review and accept snapshots**

Run: `cargo test --test ui_snapshots`
Expected: snapshot tests report new snapshots; non-snapshot tests pass.

Run: `cargo install cargo-insta && cargo insta review`
Check each screen by eye against the layout in the design doc, then accept.

**Step 6: Run tests to verify they pass**

Run: `cargo test --test ui_snapshots`
Expected: 12 passed.

**Step 7: Commit**

```bash
git add src/ui.rs tests/ui_snapshots.rs tests/snapshots
git commit -m "feat: draw worktree, file and diff panes with help overlay"
```

---

## Task 14: File watcher

**Files:**
- Modify: `src/watch.rs`

**Step 1: Write the failing tests**

```rust
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
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib watch`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
use crate::worker::Msg;
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(200);

pub enum Command {
    SetWorktrees(Vec<PathBuf>),
}

/// Picks the deepest root containing `path`, so a nested worktree such as
/// `.worktrees/a` is not reported as a change to the main worktree.
pub fn owning_worktree(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    let root = roots.iter()
        .filter(|r| path.starts_with(r))
        .max_by_key(|r| r.components().count())?;
    if let Ok(rel) = path.strip_prefix(root) {
        let mut parts = rel.components().map(|c| c.as_os_str());
        if parts.next().is_some_and(|c| c == ".git") {
            let name = parts.next().and_then(|c| c.to_str());
            if !matches!(name, Some("index" | "HEAD")) {
                return None;
            }
        }
    }
    Some(root.clone())
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

fn ignore_matcher(root: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    b.add(root.join(".gitignore"));
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// Starts the watcher thread. Send `SetWorktrees` whenever the list changes.
pub fn spawn(common_dir: PathBuf, msgs: Sender<Msg>) -> notify::Result<Sender<Command>> {
    let (fs_tx, fs_rx) = unbounded::<notify::Result<Event>>();
    let (cmd_tx, cmd_rx) = unbounded::<Command>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |e| {
        let _ = fs_tx.send(e);
    })?;
    // Non-recursive: `.git/worktrees` may not exist until the first linked worktree does.
    watcher.watch(&common_dir, RecursiveMode::NonRecursive)?;
    let worktrees_dir = common_dir.join("worktrees");
    let _ = watcher.watch(&worktrees_dir, RecursiveMode::NonRecursive);

    thread::spawn(move || run(watcher, worktrees_dir, fs_rx, cmd_rx, msgs));
    Ok(cmd_tx)
}

fn run(
    mut watcher: RecommendedWatcher,
    worktrees_dir: PathBuf,
    fs_rx: Receiver<notify::Result<Event>>,
    cmd_rx: Receiver<Command>,
    msgs: Sender<Msg>,
) {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut ignores: HashMap<PathBuf, Gitignore> = HashMap::new();
    let mut bursts = Debouncer::new(DEBOUNCE);
    let mut list_changed = Debouncer::new(DEBOUNCE);

    loop {
        while let Ok(Command::SetWorktrees(next)) = cmd_rx.try_recv() {
            let (old, new): (HashSet<_>, HashSet<_>) = (roots.iter().collect(), next.iter().collect());
            for gone in old.difference(&new) {
                let _ = watcher.unwatch(gone);
            }
            for added in new.difference(&old) {
                let _ = watcher.watch(added, RecursiveMode::Recursive);
            }
            ignores = next.iter().map(|r| (r.clone(), ignore_matcher(r))).collect();
            roots = next;
        }

        match fs_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(event)) => {
                let now = Instant::now();
                for path in &event.paths {
                    if path.starts_with(&worktrees_dir) || path == &worktrees_dir {
                        list_changed.record(worktrees_dir.clone(), now);
                        continue;
                    }
                    let Some(root) = owning_worktree(path, &roots) else { continue };
                    let ignored = ignores.get(&root).is_some_and(|m| {
                        m.matched_path_or_any_parents(path, path.is_dir()).is_ignore()
                    });
                    if !ignored {
                        bursts.record(root, now);
                    }
                }
            }
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let now = Instant::now();
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
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib watch`
Expected: 6 passed.

**Step 5: Add an end-to-end watcher test and run it**

Append to `tests/git_layer.rs`:

```rust
#[test]
fn watcher_reports_stale_worktree_and_new_worktrees() {
    use branchdiff::{watch, worker::Msg};
    use std::time::Duration;

    let repo = Repo::new();
    let info = git::discover(&repo.main).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let cmds = watch::spawn(info.common_dir.clone(), tx).unwrap();
    let main = repo.main.canonicalize().unwrap();
    cmds.send(watch::Command::SetWorktrees(vec![main.clone()])).unwrap();
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(main.join("a.txt"), "edited\n").unwrap();
    let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(got, Msg::Stale(p) if p == main));

    repo.add_worktree("later");
    let got = (0..20).find_map(|_| rx.recv_timeout(Duration::from_secs(1)).ok().filter(|m| matches!(m, Msg::WorktreesChanged)));
    assert!(got.is_some());
}
```

Run: `cargo test --test git_layer watcher`
Expected: 1 passed. If it fails on macOS because the temp path is a symlink, canonicalize roots in `main` and in `owning_worktree` callers. Real paths from `git worktree list` are already canonical, so the fix belongs in the test.

**Step 6: Commit**

```bash
git add src/watch.rs tests/git_layer.rs
git commit -m "feat: watch worktrees and the worktree list for changes"
```

---

## Task 15: Editor launch and clipboard

**Files:**
- Modify: `src/editor.rs`

**Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn args(editor: &str) -> (String, Vec<String>) {
        command(editor, Path::new("/r/a.rs"), 7)
    }

    #[test]
    fn terminal_editors_use_plus_line() {
        assert_eq!(args("vim").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("nvim").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("nano").1, vec!["+7", "/r/a.rs"]);
    }

    #[test]
    fn vscode_family_uses_goto_and_wait() {
        assert_eq!(args("code").1, vec!["--wait", "--goto", "/r/a.rs:7"]);
        assert_eq!(args("cursor").1, vec!["--wait", "--goto", "/r/a.rs:7"]);
    }

    #[test]
    fn sublime_and_helix_use_path_colon_line() {
        assert_eq!(args("subl").1, vec!["--wait", "/r/a.rs:7"]);
        assert_eq!(args("hx").1, vec!["/r/a.rs:7"]);
    }

    #[test]
    fn keeps_existing_arguments_and_full_paths() {
        let (prog, a) = args("/usr/bin/vim -u NONE");
        assert_eq!(prog, "/usr/bin/vim");
        assert_eq!(a, vec!["-u", "NONE", "+7", "/r/a.rs"]);
    }

    #[test]
    fn unknown_editors_just_get_the_path() {
        assert_eq!(args("myed").1, vec!["/r/a.rs"]);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --lib editor`
Expected: FAIL to compile.

**Step 3: Implement**

```rust
use std::path::Path;

pub fn default_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "vi".into() })
}

/// Builds the program and arguments that open `path` at `line`.
pub fn command(editor: &str, path: &Path, line: u32) -> (String, Vec<String>) {
    let mut parts = editor.split_whitespace().map(String::from);
    let prog = parts.next().unwrap_or_else(|| "vi".into());
    let mut args: Vec<String> = parts.collect();
    let name = Path::new(&prog).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let file = path.display().to_string();
    let at_line = format!("{file}:{line}");
    match name.as_str() {
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "kak" | "micro" => {
            args.extend([format!("+{line}"), file]);
        }
        "code" | "code-insiders" | "cursor" | "windsurf" | "codium" => {
            args.extend(["--wait".to_string(), "--goto".to_string(), at_line]);
        }
        "subl" => args.extend(["--wait".to_string(), at_line]),
        "hx" | "helix" => args.push(at_line),
        _ => args.push(file),
    }
    (prog, args)
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --lib editor`
Expected: 5 passed.

**Step 5: Commit**

```bash
git add src/editor.rs
git commit -m "feat: build editor commands that open a file at a line"
```

---

## Task 16: Event loop and CLI

**Files:**
- Modify: `src/main.rs`

**Step 1: Implement `src/main.rs`**

This is thin glue. `App` is already tested, so verification here is by running the binary.

```rust
use anyhow::{bail, Context, Result};
use arboard::Clipboard;
use branchdiff::app::{coalesce_saves, App, Effect};
use branchdiff::cache::CacheKey;
use branchdiff::git::{self, GitError, RepoInfo};
use branchdiff::store::{ReviewId, Store};
use branchdiff::worker::{Job, Loads, Msg, Pool, WORKER_THREAD};
use branchdiff::{editor, ui, watch};
use clap::Parser;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use ratatui::DefaultTerminal;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Base branch to compare against
    #[arg(long)]
    base: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct Config {
    base: Option<String>,
}

/// Reads `base` from `<git-common-dir>/branchdiff/config.toml`.
fn configured_base(info: &RepoInfo) -> Option<String> {
    let text = std::fs::read_to_string(info.common_dir.join("branchdiff/config.toml")).ok()?;
    toml::from_str::<Config>(&text).ok()?.base
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let info = match git::discover(&cwd) {
        Ok(i) => i,
        Err(e @ (GitError::GitMissing | GitError::NotARepo)) => bail!("{e}"),
        Err(e) => return Err(e).context("could not read the repository"),
    };
    let base = cli.base.or_else(|| configured_base(&info)).unwrap_or_else(|| "develop".into());

    let mut terminal = ratatui::init();
    keep_terminal_on_worker_panic();
    set_bracketed_paste(true);
    let result = run(&mut terminal, info, base);
    set_bracketed_paste(false);
    ratatui::restore();
    result
}

const MAX_HIGHLIGHTS: usize = 2;

/// Without this, a pasted newline arrives as Enter and the rest of the paste
/// runs as hotkeys. Best effort, because some terminals don't support it.
fn set_bracketed_paste(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on { execute!(out, EnableBracketedPaste) } else { execute!(out, DisableBracketedPaste) };
}

/// ratatui's hook restores the terminal on a panic from any thread. Workers
/// catch their own panics and carry on, so the app must keep its screen.
fn keep_terminal_on_worker_panic() {
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some(WORKER_THREAD) {
            ratatui_hook(info);
        }
    }));
}

struct Runtime {
    pool: Pool,
    loads: Loads,
    store: Store,
    common_dir: PathBuf,
    /// Opened on first copy, then kept alive because on Linux this process
    /// hosts the copied text.
    clipboard: Option<Clipboard>,
    paused: Arc<AtomicBool>,
    repo: PathBuf,
    watch: Option<Sender<watch::Command>>,
    highlighting: HashSet<(CacheKey, usize)>,
}

/// Polls rather than blocking in `read`, so it can stop consuming keys while
/// an editor owns the terminal.
fn spawn_input(tx: Sender<Event>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        if paused.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if !event::poll(Duration::from_millis(20)).unwrap_or(false) {
            continue;
        }
        let Ok(ev) = event::read() else { break };
        if tx.send(ev).is_err() {
            break;
        }
    });
}

fn run(terminal: &mut DefaultTerminal, info: RepoInfo, base: String) -> Result<()> {
    let (msg_tx, msg_rx) = unbounded::<Msg>();
    let (key_tx, key_rx) = unbounded::<Event>();
    let paused = Arc::new(AtomicBool::new(false));
    spawn_input(key_tx, paused.clone());

    // Workers mostly wait on git, so few CPUs is no reason for few threads.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 8));
    let mut rt = Runtime {
        pool: Pool::new(threads, msg_tx.clone()),
        loads: Loads::default(),
        store: Store::new(&info.common_dir),
        common_dir: info.common_dir.clone(),
        clipboard: None,
        paused,
        repo: info.toplevel.clone(),
        watch: watch::spawn(info.common_dir.clone(), msg_tx).ok(),
        highlighting: HashSet::new(),
    };
    let mut app = App::new(base);
    rt.pool.submit(Job::ListWorktrees { cwd: rt.repo.clone() });

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.should_quit {
            return Ok(());
        }
        let effects = next_effects(&mut app, &mut rt, &key_rx, &msg_rx)?;
        for effect in coalesce_saves(effects) {
            perform(effect, &mut app, &mut rt, terminal)?;
        }
        if let Some(effect) = app.highlight_effect() {
            perform(effect, &mut app, &mut rt, terminal)?;
        }
    }
}

/// Blocks for one event, then drains anything else queued, so a burst of key
/// repeats or results produces one redraw rather than many.
fn next_effects(app: &mut App, rt: &mut Runtime, keys: &Receiver<Event>, msgs: &Receiver<Msg>) -> Result<Vec<Effect>> {
    let mut effects = Vec::new();
    select! {
        recv(keys) -> ev => effects.extend(on_event(app, ev?)),
        recv(msgs) -> msg => effects.extend(on_msg(app, rt, msg?)),
        default(Duration::from_secs(60)) => {}
    }
    while let Ok(ev) = keys.try_recv() {
        effects.extend(on_event(app, ev));
    }
    while let Ok(msg) = msgs.try_recv() {
        effects.extend(on_msg(app, rt, msg));
    }
    Ok(effects)
}

fn on_event(app: &mut App, ev: Event) -> Vec<Effect> {
    match ev {
        Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
        Event::Paste(text) => {
            app.paste(&text);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn on_msg(app: &mut App, rt: &mut Runtime, msg: Msg) -> Vec<Effect> {
    let mut changed_during_load = None;
    if let Msg::Stale(path) = &msg {
        rt.loads.mark_changed(path);
    }
    if let Msg::Loaded { path, .. } = &msg {
        let done = rt.loads.finished(path);
        if let Some(job) = done.next {
            rt.pool.submit(job);
        }
        if done.superseded {
            return Vec::new();
        }
        if done.changed {
            changed_during_load = Some(path.clone());
        }
    }
    let effects = match msg {
        Msg::Worktrees(Ok(list)) => {
            if let Some(tx) = &rt.watch {
                let _ = tx.send(watch::Command::SetWorktrees(list.iter().map(|w| w.path.clone()).collect()));
            }
            // A branch switch keeps the path but changes the review, and saving
            // the old one would overwrite the new branch's comments.
            let reload: Vec<bool> = list.iter()
                .map(|w| !app.worktrees.iter().any(|o| o.path == w.path && ReviewId::of(o) == ReviewId::of(w)))
                .collect();
            let effects = app.handle_msg(Msg::Worktrees(Ok(list)));
            for (i, needed) in reload.into_iter().enumerate() {
                if needed {
                    let id = ReviewId::of(&app.worktrees[i]);
                    let loaded = rt.store.load(&id);
                    if let Some(path) = loaded.moved_aside {
                        app.notice = Some(format!("Review could not be read, kept at {}", path.display()));
                    } else if let Some(path) = loaded.left_in_place {
                        app.notice = Some(format!("Review could not be read or moved, still at {}", path.display()));
                    }
                    app.set_review(i, loaded.review);
                }
            }
            effects
        }
        Msg::Highlighted { ref key, file_index, .. } => {
            // A reload with an unchanged key must be able to highlight again.
            rt.highlighting.remove(&(key.clone(), file_index));
            app.handle_msg(msg)
        }
        other => app.handle_msg(other),
    };
    // A loaded result clears the badge, so restore it for edits it can't include.
    if let Some(path) = changed_during_load {
        app.handle_msg(Msg::Stale(path));
    }
    effects
}

fn perform(effect: Effect, app: &mut App, rt: &mut Runtime, terminal: &mut DefaultTerminal) -> Result<()> {
    match effect {
        Effect::Load { path, base } => {
            if let Some(job) = rt.loads.request(path, base) {
                rt.pool.submit(job);
            }
        }
        Effect::ReloadWorktrees => rt.pool.submit(Job::ListWorktrees { cwd: rt.repo.clone() }),
        Effect::ListBranches => rt.pool.submit(Job::ListBranches { cwd: rt.repo.clone() }),
        Effect::Highlight { key, file_index, diffs } => {
            // Holding `n` would otherwise queue a job per file passed and
            // bury refreshes. A skipped file is requested again on return.
            if rt.highlighting.len() < MAX_HIGHLIGHTS && rt.highlighting.insert((key.clone(), file_index)) {
                rt.pool.submit(Job::Highlight { key, file_index, diffs });
            }
        }
        Effect::SaveReview { id, review } => {
            if let Err(e) = rt.store.save(&id, &review) {
                app.notice = Some(format!("Could not save review: {e}"));
            }
        }
        Effect::CopyExport(md) => {
            if rt.clipboard.is_none() {
                rt.clipboard = Clipboard::new().ok();
            }
            let copied = rt.clipboard.as_mut().is_some_and(|c| c.set_text(md.clone()).is_ok());
            app.notice = Some(if copied {
                "Copied review comments".to_string()
            } else {
                format!("Clipboard unavailable. {}", write_export(rt, &md))
            });
        }
        Effect::WriteExport(md) => app.notice = Some(write_export(rt, &md)),
        Effect::OpenEditor { path, line } => open_editor(terminal, &path, line, app, rt)?,
    }
    Ok(())
}

/// Writes under the git common dir, so an export never appears as a change
/// in the worktree being reviewed.
fn write_export(rt: &Runtime, md: &str) -> String {
    let dir = rt.common_dir.join("branchdiff/exports");
    let path = dir.join("latest.md");
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, md)) {
        Ok(()) => format!("Wrote {}", path.display()),
        Err(e) => format!("Could not write export: {e}"),
    }
}

fn open_editor(terminal: &mut DefaultTerminal, path: &Path, line: u32, app: &mut App, rt: &Runtime) -> Result<()> {
    let (prog, args) = editor::command(&editor::default_editor(), path, line);
    rt.paused.store(true, Ordering::SeqCst);
    set_bracketed_paste(false);
    ratatui::restore();
    let status = std::process::Command::new(&prog).args(&args).status();
    // Not `ratatui::init`, which would stack a new panic hook over ours.
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    set_bracketed_paste(true);
    terminal.clear()?;
    rt.paused.store(false, Ordering::SeqCst);
    if let Err(e) = status {
        app.notice = Some(format!("Could not start {prog}: {e}"));
    }
    Ok(())
}
```

Add this method to `App` in `src/app.rs`, with a test that replacing a review recounts `outdated`:

```rust
    pub fn set_review(&mut self, index: usize, review: Review) {
        // An open prompt targets the review being replaced, as after a branch switch.
        if index == self.selected && matches!(self.input, Some(Input::Comment { .. } | Input::ConfirmClear)) {
            self.input = None;
        }
        if let Some(s) = self.states.get_mut(index) {
            s.review = review;
            s.outdated = count_outdated(s);
        }
    }
```

Also test that replacing the selected worktree's review cancels an open comment and clear prompt, and that replacing another worktree's leaves them open.

Add change tracking to `Loads` in `src/worker.rs`, with tests: `pub fn mark_changed(&mut self, path: &Path)` records that a worktree changed while its load runs, and does nothing if no load is running. `Finished` gains `pub changed: bool`, which is true only when a change was marked and no waiting load follows, because a waiting load starts after the change and includes it. Promoting a waiting job clears the mark.

Two behaviours to preserve while wiring this up:

1. `Msg::Stale` must not trigger a reload, or the diff would move under the cursor. The worktree keeps showing its old data until `r`.
2. `r` always reruns the diff. A switch never does, because `WorktreeState` already holds the data.

**Step 2: Build and run the tests**

Run: `cargo build && cargo test`
Expected: all pass.

**Step 3: Try it by hand against this repo**

Run from the main checkout: `.worktrees/mvp/target/debug/branchdiff --base master`

Check each of these:
- Both worktrees are listed, and `]`, `[`, `1`, `2` and `w` all switch.
- From another terminal, `git worktree add /tmp/bd-try -b bd-try` appears in the list, and `git worktree remove /tmp/bd-try` removes it. Delete the branch afterwards with `git branch -D bd-try`.
- Editing a file in a worktree shows the "changed" badge and leaves the diff in place; `r` refreshes it.
- `c`, `x`, `y`, `e`, `b` and `?` all behave as in the design.
- Quitting restores the terminal, including after pressing `e`.

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: add CLI and event loop"
```

---

## Task 17: Speed tests and benchmark

**Files:**
- Create: `tests/perf.rs`
- Modify: `benches/render.rs`

**Step 1: Write the speed tests**

`tests/perf.rs`:

```rust
//! Run with: cargo test --release --test perf -- --ignored
//! Debug builds are not representative, so these are ignored by default.

use branchdiff::app::App;
use branchdiff::cache::CacheKey;
use branchdiff::diff::{FileDiff, Hunk, Line, LineKind, Status};
use branchdiff::git::Worktree;
use branchdiff::ui;
use branchdiff::worker::{Loaded, Msg};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::Terminal;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKTREES: usize = 10;
const FILES: usize = 5_000;
const LINES: usize = 40;
const BUDGET: Duration = Duration::from_millis(16);

fn big_diff() -> Vec<FileDiff> {
    (0..FILES).map(|f| FileDiff {
        path: format!("src/module_{f}.rs"), old_path: None, status: Status::Modified, binary: false, content_id: Some(format!("{f}")),
        added: LINES as u32, removed: 0,
        hunks: vec![Hunk {
            header: format!("@@ -0,0 +1,{LINES} @@"),
            lines: (0..LINES).map(|i| Line {
                kind: LineKind::Added, old_no: None, new_no: Some(i as u32 + 1),
                text: format!("let value_{i} = compute({i});"),
            }).collect(),
        }],
    }).collect()
}

fn loaded_app() -> App {
    let mut a = App::new("develop".into());
    let wts: Vec<Worktree> = (0..WORKTREES).map(|i| Worktree {
        path: format!("/r/w{i}").into(), head: "h".into(), branch: Some(format!("w{i}")),
    }).collect();
    a.handle_msg(Msg::Worktrees(Ok(wts)));
    let diffs = Arc::new(big_diff());
    for i in 0..WORKTREES {
        let path: std::path::PathBuf = format!("/r/w{i}").into();
        let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: diffs.clone(), op: None }) });
    }
    a
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn timed(app: &mut App, key: KeyCode) -> Duration {
    let mut t = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let samples = (0..50).map(|_| {
        let start = Instant::now();
        app.handle_key(KeyEvent::from(key));
        t.draw(|f| ui::draw(f, app)).unwrap();
        start.elapsed()
    }).collect();
    median(samples)
}

#[test]
#[ignore]
fn switching_to_a_cached_worktree_is_within_budget() {
    let d = timed(&mut loaded_app(), KeyCode::Char(']'));
    assert!(d < BUDGET, "worktree switch took {d:?}");
}

#[test]
#[ignore]
fn keypress_to_redraw_is_within_budget() {
    let mut a = loaded_app();
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    let d = timed(&mut a, KeyCode::Char('j'));
    assert!(d < BUDGET, "redraw took {d:?}");
}
```

`first usable frame` is measured on the real binary in Step 3, since it includes process start.

**Step 2: Run them**

Run: `cargo test --release --test perf -- --ignored`
Expected: 2 passed. If `switching_to_a_cached_worktree_is_within_budget` fails, the likely cause is `draw_files` building 5,000 `ListItem`s each frame. Build only the visible window, as `unified_rows` does, then rerun.

**Step 3: Add a startup check**

Append to `tests/perf.rs`:

```rust
#[test]
#[ignore]
fn help_output_starts_within_the_first_frame_budget() {
    let exe = env!("CARGO_BIN_EXE_branchdiff");
    let samples: Vec<Duration> = (0..10).map(|_| {
        let start = Instant::now();
        std::process::Command::new(exe).arg("--version").output().unwrap();
        start.elapsed()
    }).collect();
    let d = median(samples);
    assert!(d < Duration::from_millis(100), "startup took {d:?}");
}
```

This only covers process start. The first frame draws before git returns because `run` draws an empty `App` immediately, so the two are close, but confirm by hand with a real repo once.

**Step 4: Write `benches/render.rs`**

```rust
use branchdiff::diff;
use criterion::{criterion_group, criterion_main, Criterion};

fn parse_large_patch(c: &mut Criterion) {
    const LINES: usize = 300;
    let mut patch = String::new();
    for f in 0..5_000 {
        patch.push_str(&format!("diff --git a/f{f} b/f{f}\n--- a/f{f}\n+++ b/f{f}\n@@ -1,{LINES} +1,{LINES} @@\n"));
        for i in 0..LINES {
            patch.push_str(&format!(" let value_{i} = compute_the_thing({i}, \"some typical line of code\");\n"));
        }
    }
    c.bench_function("parse about 100 MB", |b| b.iter(|| diff::parse(patch.as_bytes())));
}

criterion_group!(benches, parse_large_patch);
criterion_main!(benches);
```

Run: `cargo bench`
Expected: completes and reports a time for `parse about 100 MB`, which was about 0.1 s for a shape this size when measured in review.

**Step 5: Commit**

```bash
git add tests/perf.rs benches/render.rs src
git commit -m "test: enforce speed targets"
```

---

## Task 18: CI, release builds and README

**Files:**
- Create: `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `README.md`

**Step 1: Write `.github/workflows/ci.yml`**

```yaml
name: CI
on:
  push:
    branches: [master]
  pull_request:

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test
      - run: cargo test --release --test perf -- --ignored

  msrv:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.88
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --locked
```

**Step 2: Write `.github/workflows/release.yml`**

```yaml
name: Release
on:
  push:
    tags: ['v*']

permissions:
  contents: write

jobs:
  build:
    strategy:
      matrix:
        include:
          - { os: ubuntu-latest, target: x86_64-unknown-linux-gnu, ext: '' }
          - { os: ubuntu-24.04-arm, target: aarch64-unknown-linux-gnu, ext: '' }
          - { os: macos-latest, target: aarch64-apple-darwin, ext: '' }
          - { os: macos-latest, target: x86_64-apple-darwin, ext: '' }
          - { os: windows-latest, target: x86_64-pc-windows-msvc, ext: '.exe' }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - run: cargo build --release --locked --target ${{ matrix.target }}
      - name: Rename binary
        shell: bash
        run: cp "target/${{ matrix.target }}/release/branchdiff${{ matrix.ext }}" "branchdiff-${{ matrix.target }}${{ matrix.ext }}"
      - uses: softprops/action-gh-release@v2
        with:
          files: branchdiff-${{ matrix.target }}${{ matrix.ext }}
```

The Linux build depends on the system libc. `arboard` is built with `wayland-data-control`, so it supports both X11 and Wayland. If a runner is missing headers, add an `apt-get install` step for `libxcb1-dev`.

**Step 3: Write `README.md`**

Cover: what it does, install from a release binary or `cargo install --path .`, usage (`branchdiff`, `branchdiff --base main`), the base branch config file at `<git-common-dir>/branchdiff/config.toml` with `base = "main"`, the hotkey table from the design doc, the fact that comments are stored under `<git-common-dir>/branchdiff/` and never modify a worktree, and that on Linux without a clipboard manager, copied text is lost when `branchdiff` quits, that a commit inside a real submodule may not show the "changed" badge until `r` is pressed, and that two `branchdiff` sessions reviewing the same branch, or two detached worktrees on the same commit, overwrite each other's comments when they save, and that review files are private to their owner, so another user sharing the same `.git` sees theirs moved aside.

**Step 4: Verify the final state**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: no warnings, all tests pass.

Run: `cargo test --release --test perf -- --ignored`
Expected: 3 passed.

**Step 5: Commit**

```bash
git add .github README.md
git commit -m "chore: add CI, release builds and README"
```

---

## Design coverage

| Design requirement | Task |
| --- | --- |
| Runtime worktree switching from any screen | 9, 11 |
| Live worktree list | 9, 14, 16 |
| Per-worktree position | 9 |
| UI never waits on git | 6, 9, 16 |
| Late results dropped, one running load per worktree | 6, 16 |
| Fork-point diff with uncommitted and untracked changes | 2, 4 |
| Diffs held in memory, identified by base, HEAD and working tree | 5, 6, 9 |
| Prefetching | 9 (every worktree loads when listed) |
| Unified and side-by-side diffs with highlighting | 12, 13 |
| Comments, export and outdated flag | 7, 8, 10 |
| Reviewed marks that reset on change | 10 |
| Jump to editor | 10, 15, 16 |
| Base branch via flag, config and runtime picker | 11, 16 |
| Help overlay | 9, 13 |
| Error handling table | 2, 9, 11, 13, 16 |
| Speed targets | 17 |
| CI on three platforms, prebuilt binaries | 18 |
