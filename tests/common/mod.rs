#![allow(dead_code)]
use branchdiff::git::REPO_ENV_VARS;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `canonicalize` gives `\\?\C:\...` on Windows, which never equals git's `C:/...`.
pub fn real(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap();
    if cfg!(windows) {
        if let Some(s) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            return PathBuf::from(s);
        }
    }
    path
}

pub struct Repo {
    _dir: TempDir,
    pub main: PathBuf,
}

/// Runs git with no inherited repository state or user config, because these
/// tests may themselves run from a linked worktree or a git hook.
pub fn git_unchecked(cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new("git");
    for var in REPO_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.current_dir(cwd)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = git_unchecked(cwd, args);
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

/// A repository whose git dir is outside its working tree, with `a.txt`
/// committed on `develop` and then edited, so a diff from `develop` has content.
pub struct SeparateGitDir {
    _dir: TempDir,
    pub worktree: PathBuf,
    pub git_dir: PathBuf,
}

impl SeparateGitDir {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let base = real(dir.path());
        let (worktree, git_dir) = (base.join("worktree"), base.join("elsewhere.git"));
        let separate = format!("--separate-git-dir={}", git_dir.display());
        git(&base, &["init", "-q", "-b", "develop", &separate, worktree.to_str().unwrap()]);
        git(&worktree, &["config", "user.email", "test@example.com"]);
        git(&worktree, &["config", "user.name", "Test"]);
        std::fs::write(worktree.join("a.txt"), "one\n").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-qm", "init"]);
        git(&worktree, &["switch", "-qc", "feat"]);
        std::fs::write(worktree.join("a.txt"), "two\n").unwrap();
        Self { _dir: dir, worktree, git_dir }
    }
}

/// A superproject with `Repo::main` added as the submodule `sub`, on a new
/// branch, with `a.txt` edited. Returns the superproject and the submodule.
pub fn repo_with_submodule(repo: &Repo) -> (PathBuf, PathBuf) {
    let base = real(repo.main.parent().unwrap());
    let superproject = base.join("super");
    std::fs::create_dir(&superproject).unwrap();
    git(&superproject, &["init", "-q", "-b", "develop"]);
    git(&superproject, &["config", "user.email", "test@example.com"]);
    git(&superproject, &["config", "user.name", "Test"]);
    git(&superproject, &["commit", "-qm", "init", "--allow-empty"]);
    let source = base.join("main");
    git(&superproject, &["-c", "protocol.file.allow=always", "submodule", "--quiet", "add", source.to_str().unwrap(), "sub"]);
    let sub = superproject.join("sub");
    git(&sub, &["config", "user.email", "test@example.com"]);
    git(&sub, &["config", "user.name", "Test"]);
    git(&sub, &["switch", "-qc", "feat"]);
    std::fs::write(sub.join("a.txt"), "edited\n").unwrap();
    (superproject, sub)
}
