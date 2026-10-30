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
