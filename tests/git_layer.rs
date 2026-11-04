mod common;
use branchdiff::cache::CacheKey;
use branchdiff::git::{self, GitError};
use branchdiff::worker::{Job, Msg, Pool};
use common::{git as run, real, repo_with_submodule, Repo, SeparateGitDir};
use crossbeam_channel::{unbounded, Receiver};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn discovers_common_dir_from_linked_worktree() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let info = git::discover(&wt).unwrap();
    assert_eq!(real(&info.common_dir), real(&repo.main.join(".git")));
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

fn commit_at(cwd: &std::path::Path, message: &str, seconds: u64) {
    run(cwd, &["add", "."]);
    let date = format!("@{seconds} +0000");
    let mut command = std::process::Command::new("git");
    for var in git::REPO_ENV_VARS {
        command.env_remove(var);
    }
    let output = command
        .current_dir(cwd)
        .args(["commit", "-qm", message])
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .output()
        .unwrap();
    assert!(output.status.success(), "git commit: {}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn worktrees_are_sorted_by_latest_commit_then_dirty_file() {
    let repo = Repo::new();
    let feat = repo.add_worktree("feat");
    std::fs::write(repo.main.join("main.txt"), "main\n").unwrap();
    commit_at(&repo.main, "main", 2_000_000_000);
    std::fs::write(feat.join("feat.txt"), "feat\n").unwrap();
    commit_at(&feat, "feat", 2_000_000_100);

    let branches = || {
        git::list_worktrees(&repo.main)
            .unwrap()
            .into_iter()
            .map(|worktree| worktree.branch.unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(branches(), vec!["feat", "develop"]);

    let dirty = repo.main.join("a.txt");
    std::fs::write(&dirty, "newest uncommitted edit\n").unwrap();
    set_mtime(&dirty, 3_000_000_000);
    assert_eq!(branches(), vec!["develop", "feat"]);
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
    assert_eq!(real(&info.common_dir), real(&expected));
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
    std::fs::write(repo.main.join("a.txt"), "1\n\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n").unwrap();
    std::fs::write(repo.main.join("z.txt"), "1\n2\n3\n4\n5\n").unwrap();
    repo.commit_all(&repo.main, "base");
    let head = run(&repo.main, &["rev-parse", "HEAD"]).trim().to_string();
    // The two a.txt edits are close enough for `diff.interHunkContext` to join.
    std::fs::write(repo.main.join("a.txt"), "1\n\n3\nchanged\n5\n6\n7\n8\n9\n10\n11\nchanged\n13\n").unwrap();
    std::fs::write(repo.main.join("z.txt"), "1\n2\nchanged\n4\n5\n").unwrap();
    let plain = git::diff(&repo.main, &head).unwrap();
    assert_eq!(String::from_utf8_lossy(&plain).matches("\n@@ -").count(), 3);

    let order = repo.main.parent().unwrap().join("order");
    std::fs::write(&order, "z.txt\na.txt\n").unwrap();
    for (key, value) in [
        ("diff.suppressBlankEmpty", "true"),
        ("diff.context", "1"),
        ("diff.orderFile", order.to_str().unwrap()),
        ("diff.noprefix", "true"),
        ("diff.interHunkContext", "10"),
        ("core.abbrev", "12"),
    ] {
        run(&repo.main, &["config", key, value]);
        let configured = git::diff(&repo.main, &head).unwrap();
        assert_eq!(String::from_utf8_lossy(&configured), String::from_utf8_lossy(&plain), "with {key}");
    }
}

#[test]
fn diff_index_lines_hold_full_hashes() {
    let repo = Repo::new();
    std::fs::write(repo.main.join("a.txt"), "one\n").unwrap();
    repo.commit_all(&repo.main, "base");
    let head = run(&repo.main, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(repo.main.join("a.txt"), "two\n").unwrap();
    std::fs::write(repo.main.join("new.txt"), "").unwrap();
    run(&repo.main, &["add", "new.txt"]);

    let files = branchdiff::diff::parse(&git::diff(&repo.main, &head).unwrap());
    assert_eq!(files.len(), 2);
    for f in &files {
        let id = f.content_id.as_deref().unwrap();
        let (old, new) = id.split_once("..").unwrap();
        assert_eq!((old.len(), new.len()), (40, 40), "{id}");
        assert!(old.chars().chain(new.chars()).all(|c| c.is_ascii_hexdigit()), "{id}");
    }
}

/// Makes `feat`, in a linked worktree, and `develop` disagree about `a.txt`.
fn diverged_worktree(repo: &Repo) -> std::path::PathBuf {
    let wt = repo.add_worktree("feat");
    std::fs::write(repo.main.join("a.txt"), "from develop\n").unwrap();
    repo.commit_all(&repo.main, "develop side");
    std::fs::write(wt.join("a.txt"), "from feat\n").unwrap();
    repo.commit_all(&wt, "feat side");
    wt
}

/// Leaves `feat` in a linked worktree mid-merge with `a.txt` conflicted.
fn conflicted_worktree(repo: &Repo) -> std::path::PathBuf {
    let wt = diverged_worktree(repo);
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
fn in_progress_op_sees_a_rebase_in_a_linked_worktree() {
    let repo = Repo::new();
    let wt = diverged_worktree(&repo);
    assert!(!common::git_unchecked(&wt, &["rebase", "develop"]).status.success());
    assert_eq!(git::in_progress_op(&wt), Some("rebase"));
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
fn fingerprint_sees_a_same_mtime_edit_through_size() {
    let repo = Repo::new();
    let file = repo.main.join("a.txt");
    std::fs::write(&file, "aaaa\n").unwrap();
    set_mtime(&file, 1_000_000_000);
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(&file, "aaaaaaaa\n").unwrap();
    set_mtime(&file, 1_000_000_000);
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn merge_base_treats_a_flag_like_base_as_a_name() {
    let repo = Repo::new();
    assert_eq!(git::merge_base(&repo.main, "--octopus"), Err(GitError::BaseMissing("--octopus".to_string())));
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

/// Commits a nested repository at `sm` as a gitlink and returns its path.
fn add_gitlink(repo: &Repo) -> std::path::PathBuf {
    let sm = repo.main.join("sm");
    std::fs::create_dir(&sm).unwrap();
    run(&sm, &["init", "-q", "-b", "main"]);
    run(&sm, &["config", "user.email", "test@example.com"]);
    run(&sm, &["config", "user.name", "Test"]);
    std::fs::write(sm.join("f.txt"), "one\n").unwrap();
    repo.commit_all(&sm, "sm init");
    run(&repo.main, &["add", "sm"]);
    repo.commit_all(&repo.main, "add gitlink");
    sm
}

#[test]
fn fingerprint_sees_a_moved_submodule_despite_config() {
    let repo = Repo::new();
    let sm = add_gitlink(&repo);
    run(&repo.main, &["config", "diff.ignoreSubmodules", "all"]);
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(sm.join("f.txt"), "two\n").unwrap();
    repo.commit_all(&sm, "sm moved");
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn fingerprint_sees_a_dirty_submodule_despite_config() {
    let repo = Repo::new();
    let sm = add_gitlink(&repo);
    run(&repo.main, &["config", "diff.ignoreSubmodules", "all"]);
    let first = git::fingerprint(&repo.main).unwrap();
    std::fs::write(sm.join("f.txt"), "dirty\n").unwrap();
    assert_ne!(first, git::fingerprint(&repo.main).unwrap());
}

#[test]
fn diff_keeps_a_patch_block_for_submodules_despite_config() {
    let repo = Repo::new();
    let sm = add_gitlink(&repo);
    let head = run(&repo.main, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(sm.join("f.txt"), "two\n").unwrap();
    repo.commit_all(&sm, "sm moved");
    run(&repo.main, &["config", "diff.submodule", "log"]);
    run(&repo.main, &["config", "diff.ignoreSubmodules", "all"]);
    let diff = git::diff(&repo.main, &head).unwrap();
    let text = String::from_utf8_lossy(&diff);
    assert!(text.contains("diff --git a/sm b/sm"), "{text}");
}

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

/// The link is to a FIFO so that following it would hang, which is why this
/// loads on a thread. A worker that hangs would stall its load forever.
#[cfg(unix)]
#[test]
fn untracked_symlinks_show_their_target_and_are_not_followed() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let status = std::process::Command::new("mkfifo").arg(wt.join("pipe")).status().unwrap();
    assert!(status.success());
    std::os::unix::fs::symlink("pipe", wt.join("link")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(loaded(&wt)));
    let files = rx.recv_timeout(Duration::from_secs(10)).expect("load hung on a symlink");
    let lines: Vec<_> = files[0].hunks.iter().flat_map(|h| &h.lines).map(|l| l.text.as_str()).collect();
    assert_eq!((files[0].path.as_str(), lines), ("link", vec!["pipe"]));
}

#[test]
fn untracked_lines_keep_carriage_returns_like_patch_lines_do() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
    let texts = |files: Vec<diff::FileDiff>| -> Vec<String> {
        files[0].hunks.iter().flat_map(|h| &h.lines).map(|l| l.text.clone()).collect()
    };
    let untracked = texts(loaded(&wt));
    run(&wt, &["add", "-N", "crlf.txt"]);
    assert_eq!(untracked, vec!["one\r", "two\r"]);
    assert_eq!(untracked, texts(loaded(&wt)));
}

/// Sparse, so the test costs no disk. The start is text, so only the size can
/// make this count as binary.
#[test]
fn oversized_untracked_files_are_not_read_but_still_change_their_content_id() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let path = wt.join("big.log");
    std::fs::write(&path, "x\n".repeat(5000)).unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_len(diff::MAX_UNTRACKED_BYTES + 1).unwrap();
    let first = loaded(&wt).remove(0);
    assert!(first.binary);
    assert!(first.hunks.is_empty());
    file.set_len(diff::MAX_UNTRACKED_BYTES + 2).unwrap();
    assert_ne!(first.content_id, loaded(&wt)[0].content_id);
}

#[test]
fn over_cap_content_id_follows_mtime_alone() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    let file = std::fs::File::create(wt.join("big.log")).unwrap();
    file.set_len(diff::MAX_UNTRACKED_BYTES + 1).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(1_000)).unwrap();
    let first = loaded(&wt)[0].content_id.clone();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(2_000)).unwrap();
    assert_ne!(first, loaded(&wt)[0].content_id);
}

#[test]
fn untracked_hunk_headers_match_git() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("one.txt"), "only\n").unwrap();
    std::fs::write(wt.join("two.txt"), "x\ny\n").unwrap();
    let headers =
        |files: Vec<diff::FileDiff>| -> Vec<String> { files.iter().map(|f| f.hunks[0].header.clone()).collect() };
    let untracked = headers(loaded(&wt));
    assert_eq!(untracked, vec!["@@ -0,0 +1 @@", "@@ -0,0 +1,2 @@"]);
    run(&wt, &["add", "-N", "."]);
    assert_eq!(untracked, headers(loaded(&wt)));
}

/// Five lines, since growing one at a time would leave room for eight.
#[test]
fn untracked_lines_hold_no_spare_capacity() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("new.txt"), "1\n2\n3\n4\n5\n").unwrap();
    let lines = &loaded(&wt)[0].hunks[0].lines;
    assert_eq!((lines.len(), lines.capacity()), (5, 5));
}

#[test]
fn any_not_ignored_follows_every_ignore_source_and_the_index() {
    let repo = Repo::new();
    let main = &repo.main;
    std::fs::write(main.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::create_dir(main.join("pkg")).unwrap();
    std::fs::write(main.join("pkg/.gitignore"), "dist/\n").unwrap();
    std::fs::write(main.join(".git/info/exclude"), "*.local\n").unwrap();
    std::fs::write(main.join("kept.log"), "x\n").unwrap();
    run(main, &["add", "-f", "kept.log"]);
    run(main, &["commit", "-qm", "keep"]);

    let check = |paths: &[&str]| {
        let paths: Vec<_> = paths.iter().map(|p| main.join(p)).collect();
        git::any_not_ignored(main, &paths).unwrap()
    };
    assert!(!check(&["target/out", "x.log", "pkg/dist/out.js", "y.local"]));
    // A trailing slash is how a directory that no longer exists can still match `target/`.
    assert!(!check(&["target/"]));
    assert!(check(&["target/out", "src/lib.rs"]));
    assert!(check(&["kept.log"]));
    assert!(check(&["a.txt"]));
}

fn paths(wts: &[git::Worktree]) -> Vec<std::path::PathBuf> {
    wts.iter().map(|w| w.path.clone()).collect()
}

fn assert_reviewable(root: &std::path::Path) {
    let base = git::merge_base(root, "develop").unwrap();
    let files = branchdiff::diff::load(root, &base).unwrap();
    assert_eq!(files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.txt"]);
}

#[test]
fn a_separate_git_dir_lists_the_working_tree() {
    let repo = SeparateGitDir::new();
    let wts = git::list_worktrees(&repo.worktree).unwrap();
    assert_eq!(paths(&wts), vec![repo.worktree.clone()]);
    assert_eq!(wts[0].branch.as_deref(), Some("feat"));
    assert_reviewable(&wts[0].path);
}

#[test]
fn a_separate_git_dir_lists_the_working_tree_from_a_subdirectory() {
    let repo = SeparateGitDir::new();
    std::fs::create_dir(repo.worktree.join("deep")).unwrap();
    assert_eq!(paths(&git::list_worktrees(&repo.worktree.join("deep")).unwrap()), vec![repo.worktree.clone()]);
}

#[test]
fn a_separate_git_dir_keeps_its_linked_worktrees() {
    let repo = SeparateGitDir::new();
    let linked = repo.worktree.parent().unwrap().join("linked");
    run(&repo.worktree, &["worktree", "add", "-q", "-b", "linked", linked.to_str().unwrap()]);
    assert_eq!(paths(&git::list_worktrees(&repo.worktree).unwrap()), vec![repo.worktree.clone(), linked]);
}

/// The git dir has no record of its working tree, so from elsewhere it can't be
/// found. Listing the linked worktree twice, or the git dir, would both be wrong.
#[test]
fn a_separate_git_dir_seen_from_a_linked_worktree_lists_only_what_it_can_find() {
    let repo = SeparateGitDir::new();
    let linked = repo.worktree.parent().unwrap().join("linked");
    run(&repo.worktree, &["worktree", "add", "-q", "-b", "linked", linked.to_str().unwrap()]);
    assert_eq!(paths(&git::list_worktrees(&linked).unwrap()), vec![linked.clone()]);
    assert!(!repo.git_dir.join(".git").exists());
}

#[test]
fn a_submodule_lists_its_working_tree() {
    let repo = Repo::new();
    let (_superproject, sub) = repo_with_submodule(&repo);
    let wts = git::list_worktrees(&sub).unwrap();
    assert_eq!(paths(&wts), vec![sub.clone()]);
    assert_eq!(wts[0].branch.as_deref(), Some("feat"));
    assert_reviewable(&wts[0].path);
}

#[test]
fn a_submodule_seen_from_its_linked_worktree_keeps_both() {
    let repo = Repo::new();
    let (_superproject, sub) = repo_with_submodule(&repo);
    let linked = sub.parent().unwrap().parent().unwrap().join("sub-linked");
    run(&sub, &["worktree", "add", "-q", "-b", "other", linked.to_str().unwrap()]);
    assert_eq!(paths(&git::list_worktrees(&linked).unwrap()), vec![sub, linked]);
}

/// Leaves `feat` stopped at a conflict, partway through rebasing onto `develop`.
fn stop_at_a_rebase_conflict(repo: &Repo, wt: &std::path::Path, args: &[&str]) {
    std::fs::write(wt.join("a.txt"), "ours\n").unwrap();
    repo.commit_all(wt, "feat");
    std::fs::write(repo.main.join("a.txt"), "theirs\n").unwrap();
    repo.commit_all(&repo.main, "develop");
    let mut rebase = vec!["rebase"];
    rebase.extend_from_slice(args);
    rebase.push("develop");
    assert!(!common::git_unchecked(wt, &rebase).status.success());
}

fn branch_of(wt: &std::path::Path) -> Option<String> {
    let list = git::list_worktrees(wt).unwrap();
    list.into_iter().find(|w| w.path == real(wt)).unwrap().branch
}

#[test]
fn a_worktree_keeps_its_branch_during_a_rebase() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    stop_at_a_rebase_conflict(&repo, &wt, &[]);
    assert_eq!(branch_of(&wt).as_deref(), Some("feat"));
    assert_eq!(
        git::list_worktrees(&repo.main).unwrap().iter().filter(|w| w.branch.as_deref() == Some("feat")).count(),
        1
    );

    run(&wt, &["rebase", "--abort"]);
    assert_eq!(branch_of(&wt).as_deref(), Some("feat"));
}

#[test]
fn a_worktree_keeps_its_branch_during_an_apply_rebase() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    stop_at_a_rebase_conflict(&repo, &wt, &["--apply"]);
    assert_eq!(branch_of(&wt).as_deref(), Some("feat"));
}

#[test]
fn a_main_worktree_keeps_its_branch_during_a_rebase() {
    let repo = Repo::new();
    run(&repo.main, &["checkout", "-q", "-b", "feat"]);
    std::fs::write(repo.main.join("a.txt"), "ours\n").unwrap();
    repo.commit_all(&repo.main, "feat");
    run(&repo.main, &["checkout", "-q", "develop"]);
    std::fs::write(repo.main.join("a.txt"), "theirs\n").unwrap();
    repo.commit_all(&repo.main, "develop");
    run(&repo.main, &["checkout", "-q", "feat"]);
    assert!(!common::git_unchecked(&repo.main, &["rebase", "develop"]).status.success());
    assert_eq!(branch_of(&repo.main).as_deref(), Some("feat"));
}

#[test]
fn a_detached_rebase_has_no_branch() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    std::fs::write(wt.join("a.txt"), "ours\n").unwrap();
    repo.commit_all(&wt, "feat");
    run(&wt, &["checkout", "-q", "--detach"]);
    std::fs::write(repo.main.join("a.txt"), "theirs\n").unwrap();
    repo.commit_all(&repo.main, "develop");
    assert!(!common::git_unchecked(&wt, &["rebase", "develop"]).status.success());
    assert_eq!(branch_of(&wt), None);
}

#[test]
fn a_detached_worktree_with_no_rebase_has_no_branch() {
    let repo = Repo::new();
    let wt = repo.add_worktree("feat");
    run(&wt, &["checkout", "-q", "--detach"]);
    assert_eq!(branch_of(&wt), None);
}

/// Without `GIT_OPTIONAL_LOCKS=0`, `git status` refreshes a stat-dirty entry and
/// rewrites the index, which the watcher would report as a change in the worktree.
#[test]
fn a_fingerprint_leaves_a_stat_dirty_index_untouched() {
    let repo = Repo::new();
    let index = repo.main.join(".git/index");
    set_mtime(&repo.main.join("a.txt"), 1_000_000);
    let before = std::fs::read(&index).unwrap();

    git::fingerprint(&repo.main).unwrap();
    assert!(std::fs::read(&index).unwrap() == before, "the index was rewritten");
}

/// Loads `path` with a real worker, for a key to probe against.
fn loaded_key(pool: &Pool, msgs: &Receiver<Msg>, path: &std::path::Path) -> CacheKey {
    pool.submit(Job::Load { path: path.to_path_buf(), base: "develop".into() });
    match msgs.recv_timeout(Duration::from_secs(10)).unwrap() {
        Msg::Loaded { result: Ok(loaded), .. } => loaded.key,
        other => panic!("unexpected {other:?}"),
    }
}

fn probe(pool: &Pool, msgs: &Receiver<Msg>, key: &CacheKey) -> bool {
    pool.submit(Job::Probe { key: key.clone() });
    match msgs.recv_timeout(Duration::from_secs(10)).unwrap() {
        Msg::Probed { path, changed } => {
            assert_eq!(path, key.worktree);
            changed
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn pool() -> (Pool, Receiver<Msg>) {
    let (tx, rx) = unbounded();
    (Pool::new(1, tx), rx)
}

#[test]
fn a_probe_sees_no_change_from_what_only_touches_the_index_or_ignored_files() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    std::fs::write(repo.main.join(".gitignore"), "target/\n").unwrap();
    repo.commit_all(&repo.main, "ignore");
    std::fs::write(repo.main.join("a.txt"), "dirty\n").unwrap();
    let key = loaded_key(&pool, &msgs, &repo.main);
    assert!(!probe(&pool, &msgs, &key));

    run(&repo.main, &["status"]);
    assert!(!probe(&pool, &msgs, &key));

    std::fs::create_dir(repo.main.join("target")).unwrap();
    std::fs::write(repo.main.join("target/out"), "built").unwrap();
    assert!(!probe(&pool, &msgs, &key));
}

#[test]
fn a_probe_sees_no_change_from_a_touched_clean_file() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    let key = loaded_key(&pool, &msgs, &repo.main);
    set_mtime(&repo.main.join("a.txt"), 1_000_000);
    run(&repo.main, &["status"]);
    assert!(!probe(&pool, &msgs, &key));
}

#[test]
fn a_probe_sees_an_edit_a_stage_and_a_commit() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    let clean = loaded_key(&pool, &msgs, &repo.main);

    std::fs::write(repo.main.join("a.txt"), "edit\n").unwrap();
    assert!(probe(&pool, &msgs, &clean));
    let edited = loaded_key(&pool, &msgs, &repo.main);

    run(&repo.main, &["add", "a.txt"]);
    assert!(probe(&pool, &msgs, &edited));
    let staged = loaded_key(&pool, &msgs, &repo.main);

    run(&repo.main, &["commit", "-qm", "edit"]);
    assert!(probe(&pool, &msgs, &staged));
}

#[test]
fn a_probe_sees_a_new_head_when_the_fingerprint_is_unchanged() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    let key = loaded_key(&pool, &msgs, &repo.main);

    run(&repo.main, &["commit", "-q", "--allow-empty", "-m", "empty"]);
    assert_eq!(git::fingerprint(&repo.main).unwrap(), key.fingerprint);
    assert!(probe(&pool, &msgs, &key));
}

#[test]
fn a_probe_sees_a_second_edit_of_a_dirty_file_and_a_new_untracked_file() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    std::fs::write(repo.main.join("a.txt"), "first\n").unwrap();
    let key = loaded_key(&pool, &msgs, &repo.main);

    std::fs::write(repo.main.join("a.txt"), "second, longer\n").unwrap();
    assert!(probe(&pool, &msgs, &key));
    let key = loaded_key(&pool, &msgs, &repo.main);
    std::fs::write(repo.main.join("new.txt"), "new\n").unwrap();
    assert!(probe(&pool, &msgs, &key));
}

#[test]
fn a_reverted_edit_of_a_clean_file_is_no_change() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    let key = loaded_key(&pool, &msgs, &repo.main);
    std::fs::write(repo.main.join("a.txt"), "edit\n").unwrap();
    assert!(probe(&pool, &msgs, &key));
    std::fs::write(repo.main.join("a.txt"), "one\ntwo\n").unwrap();
    assert!(!probe(&pool, &msgs, &key));
}

#[test]
fn a_probe_that_cannot_run_git_reports_a_change() {
    let repo = Repo::new();
    let (pool, msgs) = pool();
    let mut key = loaded_key(&pool, &msgs, &repo.main);
    key.worktree = "/definitely/not/a/repo".into();
    assert!(probe(&pool, &msgs, &key));
}

#[test]
fn core_editor_is_read_from_the_repository_config() {
    let repo = Repo::new();
    run(&repo.main, &["config", "core.editor", "  nano -w  "]);
    assert_eq!(git::core_editor(&repo.main).as_deref(), Some("nano -w"));
}

#[test]
fn a_blank_core_editor_is_no_editor() {
    let repo = Repo::new();
    run(&repo.main, &["config", "core.editor", "   "]);
    assert_eq!(git::core_editor(&repo.main), None);
}

#[test]
fn core_editor_is_none_where_git_cannot_answer() {
    assert_eq!(git::core_editor("/definitely/not/a/repo".as_ref()), None);
}

#[test]
fn a_missing_core_editor_is_no_editor() {
    let child = "missing_core_editor_child";
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", child])
        .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && stdout.contains("1 passed"), "{stdout}");
}

/// A child process, because the user's own config could set `core.editor`.
#[test]
#[ignore = "run in a child process by a_missing_core_editor_is_no_editor"]
fn missing_core_editor_child() {
    let repo = Repo::new();
    assert_eq!(git::core_editor(&repo.main), None);
}

#[test]
fn a_superproject_does_not_list_its_submodules() {
    let repo = Repo::new();
    let (superproject, _sub) = repo_with_submodule(&repo);
    assert_eq!(paths(&git::list_worktrees(&superproject).unwrap()), vec![superproject]);
}

#[test]
fn a_bare_repository_is_skipped_when_seen_from_its_linked_worktree() {
    let repo = Repo::new();
    let base = real(repo.main.parent().unwrap());
    run(&base, &["clone", "-q", "--bare", "main", "bare.git"]);
    let linked = base.join("linked");
    run(&base.join("bare.git"), &["worktree", "add", "-q", "-b", "linked", linked.to_str().unwrap()]);
    assert_eq!(paths(&git::list_worktrees(&linked).unwrap()), vec![linked]);
}
