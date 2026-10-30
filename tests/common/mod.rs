#![allow(dead_code)]
use branchdiff::git::REPO_ENV_VARS;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

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
