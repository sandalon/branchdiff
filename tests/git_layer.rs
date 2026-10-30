mod common;
use branchdiff::git::{self, GitError};
use common::{git as run, Repo};
use std::time::{Duration, UNIX_EPOCH};

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

#[test]
fn merge_base_reports_unborn_branch() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["checkout", "-q", "--orphan", "lonely"]);
    let err = git::merge_base(&wt, "develop").unwrap_err();
    assert_eq!(err, GitError::NoCommits);
    assert_eq!(err.to_string(), "this branch has no commits yet");
}

#[test]
fn merge_base_with_head_as_base() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let head = run(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(git::merge_base(&wt, "HEAD").unwrap(), head);
    run(&wt, &["checkout", "-q", "--orphan", "lonely"]);
    assert_eq!(git::merge_base(&wt, "HEAD"), Err(GitError::NoCommits));
}

#[test]
fn merge_base_reports_missing_base_with_head_in_its_name() {
    let repo = Repo::new();
    assert_eq!(git::merge_base(&repo.main, "HEADER"), Err(GitError::BaseMissing("HEADER".to_string())));
}

#[test]
fn branches_skips_symbolic_refs() {
    let repo = Repo::new();
    let parent = repo.main.parent().unwrap();
    run(parent, &["clone", "-q", "main", "clone"]);
    let branches = git::branches(&parent.join("clone")).unwrap();
    assert!(branches.iter().any(|b| b == "origin/develop"), "{branches:?}");
    assert!(!branches.iter().any(|b| b == "origin"), "{branches:?}");
    assert!(!branches.iter().any(|b| b.ends_with("/HEAD")), "{branches:?}");
}

#[test]
fn fingerprint_changes_when_renamed_file_is_edited_again() {
    let repo = Repo::new();
    run(&repo.main, &["mv", "a.txt", "b.txt"]);
    std::fs::write(repo.main.join("b.txt"), "edit 1\n").unwrap();
    let status = run(&repo.main, &["status", "--porcelain=v2"]);
    assert!(status.starts_with("2 R"), "expected a rename record: {status}");
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(repo.main.join("b.txt"), "edit 22\n").unwrap();
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn merge_base_prefers_no_commits_when_base_is_another_branch() {
    let dir = tempfile::TempDir::new().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    assert_eq!(git::merge_base(dir.path(), "develop"), Err(GitError::NoCommits));
}

#[test]
fn merge_base_prefers_no_commits_when_base_is_the_unborn_branch() {
    let dir = tempfile::TempDir::new().unwrap();
    run(dir.path(), &["init", "-q", "-b", "main"]);
    assert_eq!(git::merge_base(dir.path(), "main"), Err(GitError::NoCommits));
}

#[test]
fn merge_base_prefers_no_commits_on_orphan_worktree_with_missing_base() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["checkout", "-q", "--orphan", "lonely"]);
    assert_eq!(git::merge_base(&wt, "nope"), Err(GitError::NoCommits));
}

const CHILD_COMMON_DIR: &str = "BRANCHDIFF_TEST_COMMON_DIR";

#[test]
fn run_ignores_inherited_git_dir() {
    let repo = Repo::new();
    let other = Repo::new();
    let child = "inherited_git_dir_child";
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", child])
        .current_dir(&repo.main)
        .env("GIT_DIR", other.main.join(".git"))
        .env(CHILD_COMMON_DIR, repo.main.join(".git"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.contains("1 passed"), "{stdout}");
}

#[test]
#[ignore = "run in a child process by run_ignores_inherited_git_dir"]
fn inherited_git_dir_child() {
    let Some(expected) = std::env::var_os(CHILD_COMMON_DIR) else { return };
    let info = git::discover(&std::env::current_dir().unwrap()).unwrap();
    let expected = std::path::PathBuf::from(expected);
    assert_eq!(info.common_dir.canonicalize().unwrap(), expected.canonicalize().unwrap());
}

fn set_mtime(path: &std::path::Path, secs: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

#[test]
fn diff_never_writes_the_index() {
    let repo = Repo::new();
    let head = run(&repo.main, &["rev-parse", "HEAD"]).trim().to_string();
    set_mtime(&repo.main.join("a.txt"), 1_000_000_000);
    let index = repo.main.join(".git/index");
    let before = (std::fs::read(&index).unwrap(), std::fs::metadata(&index).unwrap().modified().unwrap());

    assert!(git::diff(&repo.main, &head).unwrap().is_empty());

    let after = (std::fs::read(&index).unwrap(), std::fs::metadata(&index).unwrap().modified().unwrap());
    assert!(before == after, "git diff rewrote the index");
}

#[test]
fn diff_output_ignores_user_config() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("a.txt"), "1\n\n3\n4\n5\n").unwrap();
    std::fs::write(repo.main.join("z.txt"), "1\n2\n3\n4\n5\n").unwrap();
    repo.commit_all(&repo.main, "base");
    let head = run(&repo.main, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(repo.main.join("a.txt"), "1\n\n3\nchanged\n5\n").unwrap();
    std::fs::write(repo.main.join("z.txt"), "1\n2\nchanged\n4\n5\n").unwrap();
    let plain = git::diff(&repo.main, &head).unwrap();

    let order = repo.main.parent().unwrap().join("order");
    std::fs::write(&order, "z.txt\na.txt\n").unwrap();
    for (key, value) in [
        ("diff.suppressBlankEmpty", "true"),
        ("diff.context", "1"),
        ("diff.orderFile", order.to_str().unwrap()),
        ("diff.noprefix", "true"),
    ] {
        run(&repo.main, &["config", key, value]);
        let configured = git::diff(&repo.main, &head).unwrap();
        assert_eq!(String::from_utf8_lossy(&configured), String::from_utf8_lossy(&plain), "with {key}");
    }
}

/// Leaves `feat` in a linked worktree mid-merge with `a.txt` conflicted.
fn conflicted_worktree(repo: &Repo) -> std::path::PathBuf {
    let wt = repo.add_worktree("feat");
    std::fs::write(repo.main.join("a.txt"), "from develop\n").unwrap();
    repo.commit_all(&repo.main, "develop side");
    std::fs::write(wt.join("a.txt"), "from feat\n").unwrap();
    repo.commit_all(&wt, "feat side");
    assert!(!common::git_unchecked(&wt, &["merge", "--no-edit", "develop"]).status.success());
    wt
}

#[test]
fn in_progress_op_is_per_worktree() {
    let repo = Repo::new();
    let wt = conflicted_worktree(&repo);
    assert_eq!(git::in_progress_op(&wt), Some("merge"));
    assert_eq!(git::in_progress_op(&repo.main), None);
}

#[test]
fn fingerprint_changes_when_conflicted_file_is_edited_again() {
    let repo = Repo::new();
    let wt = conflicted_worktree(&repo);
    let status = run(&wt, &["status", "--porcelain=v2"]);
    assert!(status.starts_with("u "), "expected an unmerged record: {status}");
    std::fs::write(wt.join("a.txt"), "resolved 1\n").unwrap();
    let first = git::fingerprint(&wt).unwrap();
    std::fs::write(wt.join("a.txt"), "resolved 22\n").unwrap();
    assert_ne!(first, git::fingerprint(&wt).unwrap());
}

#[test]
fn fingerprint_changes_when_untracked_file_is_edited_again() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("new.txt"), "draft 1\n").unwrap();
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(repo.main.join("new.txt"), "draft 22\n").unwrap();
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn fingerprint_sees_a_same_size_edit_through_mtime() {
    let repo = Repo::new();
    let file = repo.main.join("a.txt");
    std::fs::write(&file, "aaaa\n").unwrap();
    set_mtime(&file, 1_000_000_000);
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(&file, "bbbb\n").unwrap();
    set_mtime(&file, 1_000_000_001);
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn merge_base_treats_a_flag_like_base_as_a_name() {
    let repo = Repo::new();
    assert!(git::merge_base(&repo.main, "--octopus").is_err());
}

#[test]
fn list_worktrees_skips_prunable() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::remove_dir_all(&wt).unwrap();
    let wts = git::list_worktrees(&repo.main).unwrap();
    let branches: Vec<_> = wts.iter().map(|w| w.branch.clone().unwrap()).collect();
    assert_eq!(branches, vec!["develop"]);
}

#[test]
fn list_worktrees_skips_bare() {
    let repo = Repo::new();
    let parent = repo.main.parent().unwrap();
    run(parent, &["clone", "-q", "--bare", "main", "bare.git"]);
    let bare = parent.join("bare.git");
    let wt = parent.join("linked");
    run(&bare, &["worktree", "add", "-q", "-b", "linked", wt.to_str().unwrap()]);
    let wts = git::list_worktrees(&bare).unwrap();
    let branches: Vec<_> = wts.iter().map(|w| w.branch.clone()).collect();
    assert_eq!(branches, vec![Some("linked".to_string())]);
}

#[test]
fn discover_keeps_other_failures_distinct_from_not_a_repo() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join(".git"), "garbage\n").unwrap();
    assert!(matches!(git::discover(dir.path()), Err(GitError::Failed(_))), "{:?}", git::discover(dir.path()));
}
