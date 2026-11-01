//! Each test runs in its own process, in parallel, because several watchers in
//! one process stall in `FSEventStreamStart` for seconds, which loses events. So
//! they are all `#[ignore]`d, and `run_each_test_in_its_own_process` runs them.
//! To run one by hand: `cargo test --test watcher -- --ignored --exact <name>`.

mod common;
use branchdiff::git;
use branchdiff::watch::{self, Command};
use branchdiff::worker::Msg;
use common::{git as run, real, repo_with_submodule, Repo, SeparateGitDir};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(30);

/// Long enough for the watcher to report anything it was going to.
/// `BRANCHDIFF_TEST_QUIET_MS` raises it for a slow machine.
fn quiet() -> Duration {
    let ms = std::env::var("BRANCHDIFF_TEST_QUIET_MS").ok().and_then(|ms| ms.parse().ok());
    Duration::from_millis(ms.unwrap_or(1000))
}

#[test]
fn run_each_test_in_its_own_process() {
    let exe = std::env::current_exe().unwrap();
    let list = Process::new(&exe).args(["--list", "--ignored"]).output().unwrap();
    let list = String::from_utf8(list.stdout).unwrap();
    let names: Vec<_> = list.lines().filter_map(|l| l.strip_suffix(": test")).collect();
    assert!(!names.is_empty(), "no tests listed:\n{list}");
    let spawn = |name: &&str| -> Child {
        let args = ["--ignored", "--exact", name];
        Process::new(&exe).args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
    };
    let children: Vec<_> = names.iter().map(|name| (*name, spawn(name))).collect();
    let mut failed = Vec::new();
    for (name, child) in children {
        let out = child.wait_with_output().unwrap();
        if !out.status.success() {
            failed.push(format!("{name}:\n{}", String::from_utf8_lossy(&out.stdout)));
        }
    }
    assert!(failed.is_empty(), "{} of {} failed\n{}", failed.len(), names.len(), failed.join("\n"));
}

struct Session {
    msgs: Receiver<Msg>,
    cmds: Sender<Command>,
}

impl Session {
    fn start(common_dir: &Path, roots: &[&Path]) -> Self {
        let (tx, msgs) = crossbeam_channel::unbounded();
        let cmds = watch::spawn(common_dir.to_path_buf(), tx).unwrap();
        let session = Self { msgs, cmds };
        session.set_roots(roots);
        session.settle();
        session
    }

    fn set_roots(&self, roots: &[&Path]) {
        self.cmds.send(Command::SetWorktrees(roots.iter().map(|r| r.to_path_buf()).collect())).unwrap();
        for root in roots.iter().filter(|r| r.exists()) {
            self.wait_until_watched(root);
        }
    }

    /// `SetWorktrees` is applied on the watcher's thread, a stream takes a varying
    /// time to start, and a write before that is lost, so no fixed wait is enough.
    /// Rewrites a file with its own content until the watcher reports it.
    fn wait_until_watched(&self, root: &Path) {
        let file = root.join("a.txt");
        let text = std::fs::read(&file).unwrap();
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            std::fs::write(&file, &text).unwrap();
            let retry = Instant::now() + Duration::from_secs(1);
            while let Ok(msg) = self.msgs.recv_deadline(retry) {
                if matches!(msg, Msg::Stale(p) if p == root) {
                    return;
                }
            }
        }
        panic!("{} is not being watched", root.display());
    }

    /// Discards earlier events, which can still be on their way.
    fn settle(&self) {
        std::thread::sleep(Duration::from_millis(300));
        while self.msgs.recv_timeout(Duration::from_millis(600)).is_ok() {}
    }

    fn saw(&self, wanted: impl Fn(&Msg) -> bool) -> bool {
        let deadline = Instant::now() + PATIENCE;
        while let Ok(msg) = self.msgs.recv_deadline(deadline) {
            if wanted(&msg) {
                return true;
            }
        }
        false
    }

    fn next_stale(&self) -> Option<PathBuf> {
        let deadline = Instant::now() + PATIENCE;
        while let Ok(msg) = self.msgs.recv_deadline(deadline) {
            if let Msg::Stale(root) = msg {
                return Some(root);
            }
        }
        None
    }

    fn saw_stale(&self, root: &Path) -> bool {
        self.saw(|m| matches!(m, Msg::Stale(p) if p == root))
    }

    fn saw_list_change(&self) -> bool {
        self.saw(|m| matches!(m, Msg::WorktreesChanged))
    }

    /// Commits whatever is staged, and fails if any other worktree gets a badge.
    fn assert_commit_marks_only(&self, root: &Path) {
        run(root, &["commit", "-qm", "work", "--allow-empty"]);
        assert_eq!(self.next_stale().as_deref(), Some(root));
        while let Ok(msg) = self.msgs.recv_timeout(quiet()) {
            if let Msg::Stale(other) = msg {
                assert_eq!(other, root);
            }
        }
    }

    fn assert_no_msg(&self, context: &str) {
        if let Ok(msg) = self.msgs.recv_timeout(quiet()) {
            panic!("{context}: {msg:?}");
        }
    }

    fn assert_list_unchanged(&self, context: &str) {
        let deadline = Instant::now() + quiet();
        while let Ok(msg) = self.msgs.recv_deadline(deadline) {
            assert!(!matches!(msg, Msg::WorktreesChanged), "{context}");
        }
    }
}

fn repo_with_main() -> (Repo, PathBuf, PathBuf) {
    let repo = Repo::new();
    let main = real(&repo.main);
    let common = real(&git::discover(&repo.main).unwrap().common_dir);
    (repo, main, common)
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn commit_in_a_linked_worktree_marks_it_without_touching_the_list() {
    let (repo, main, common) = repo_with_main();
    let linked = real(&repo.add_worktree("feat"));
    std::fs::write(linked.join("new.txt"), "new\n").unwrap();
    let session = Session::start(&common, &[&main, &linked]);

    run(&linked, &["add", "new.txt"]);
    assert!(session.saw_stale(&linked), "staging");
    session.settle();
    run(&linked, &["commit", "-qm", "work"]);
    assert!(session.saw_stale(&linked), "committing");
    session.settle();
    run(&linked, &["commit", "-qm", "nothing staged", "--allow-empty"]);
    assert!(session.saw_stale(&linked), "committing with an untouched index");
    session.assert_list_unchanged("a commit is not a new worktree");
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn commit_with_an_untouched_index_marks_the_main_worktree() {
    let (_repo, main, common) = repo_with_main();
    let session = Session::start(&common, &[&main]);
    run(&main, &["commit", "-qm", "empty", "--allow-empty"]);
    assert!(session.saw_stale(&main));
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn switching_branch_refreshes_the_list_from_any_worktree() {
    let (repo, main, common) = repo_with_main();
    let linked = real(&repo.add_worktree("feat"));
    let session = Session::start(&common, &[&main, &linked]);
    for (root, branch) in [(&main, "next-main"), (&linked, "next-linked")] {
        run(root, &["switch", "-qc", branch]);
        assert!(session.saw_list_change(), "{branch}");
        session.settle();
    }
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn git_commands_in_a_linked_worktree_leave_the_list_alone() {
    let (repo, main, common) = repo_with_main();
    let linked = real(&repo.add_worktree("feat"));
    let session = Session::start(&common, &[&main, &linked]);
    for args in [&["status"][..], &["diff"], &["log", "-1"], &["fetch", "-q", ".", "feat"], &["tag", "t"]] {
        run(&linked, args);
    }
    session.assert_list_unchanged("nothing was added or removed");
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn every_ignore_source_is_respected() {
    let (_repo, main, common) = repo_with_main();
    let excludes = main.parent().unwrap().join("global-excludes");
    std::fs::write(&excludes, "*.global\n").unwrap();
    run(&main, &["config", "core.excludesFile", excludes.to_str().unwrap()]);
    std::fs::write(main.join(".gitignore"), "target/\n").unwrap();
    std::fs::create_dir(main.join("pkg")).unwrap();
    std::fs::write(main.join("pkg/.gitignore"), "dist/\n").unwrap();
    std::fs::write(main.join(".git/info/exclude"), "*.local\n").unwrap();
    let session = Session::start(&common, &[&main]);

    std::fs::create_dir_all(main.join("target/debug")).unwrap();
    std::fs::write(main.join("target/debug/out"), "x").unwrap();
    std::fs::create_dir(main.join("pkg/dist")).unwrap();
    std::fs::write(main.join("pkg/dist/out.js"), "x").unwrap();
    std::fs::write(main.join("notes.local"), "x").unwrap();
    std::fs::write(main.join("notes.global"), "x").unwrap();
    session.assert_no_msg("writing");

    std::fs::remove_dir_all(main.join("target")).unwrap();
    session.assert_no_msg("cleaning a build");
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_changed_gitignore_takes_effect_at_once() {
    let (_repo, main, common) = repo_with_main();
    std::fs::write(main.join(".gitignore"), "old/\n").unwrap();
    let session = Session::start(&common, &[&main]);

    std::fs::write(main.join(".gitignore"), "new/\n").unwrap();
    assert!(session.saw_stale(&main), "the rule change is itself a change");
    session.settle();
    std::fs::create_dir(main.join("new")).unwrap();
    std::fs::write(main.join("new/out"), "x").unwrap();
    session.assert_no_msg("newly ignored");
    std::fs::create_dir(main.join("old")).unwrap();
    std::fs::write(main.join("old/out"), "x").unwrap();
    assert!(session.saw_stale(&main), "no longer ignored");
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn edits_to_a_tracked_file_count_even_if_it_matches_an_ignore_rule() {
    let (_repo, main, common) = repo_with_main();
    std::fs::write(main.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(main.join("kept.log"), "x\n").unwrap();
    run(&main, &["add", "-f", "kept.log", ".gitignore"]);
    run(&main, &["commit", "-qm", "keep"]);
    let session = Session::start(&common, &[&main]);
    std::fs::write(main.join("kept.log"), "y\n").unwrap();
    assert!(session.saw_stale(&main));
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn steady_ignored_writes_do_not_hold_back_a_real_change() {
    let (_repo, main, common) = repo_with_main();
    std::fs::write(main.join(".gitignore"), "*.log\n").unwrap();
    let session = Session::start(&common, &[&main]);

    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (stop, log) = (stop.clone(), main.join("server.log"));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::fs::write(&log, "tick\n").unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    std::fs::write(main.join("a.txt"), "edited\n").unwrap();
    let seen = session.saw_stale(&main);
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(seen);
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_root_missing_at_first_is_watched_once_it_exists() {
    let (repo, main, common) = repo_with_main();
    let later = main.parent().unwrap().join("later");
    let session = Session::start(&common, &[&main, &later]);

    repo.add_worktree("later");
    session.set_roots(&[&main, &later]);
    session.settle();
    std::fs::write(later.join("a.txt"), "edited\n").unwrap();
    assert!(session.saw_stale(&later));
}

// Windows raises no event for the watched root itself.
#[cfg(not(windows))]
#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn deleting_a_worktree_without_git_refreshes_the_list() {
    let (repo, main, common) = repo_with_main();
    let linked = real(&repo.add_worktree("feat"));
    let session = Session::start(&common, &[&main, &linked]);
    std::fs::remove_dir_all(&linked).unwrap();
    assert!(session.saw_list_change());
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_commit_in_a_nested_worktree_marks_only_that_worktree() {
    let (repo, main, common) = repo_with_main();
    std::fs::write(main.join(".gitignore"), ".worktrees/\n").unwrap();
    let nested = main.join(".worktrees/inner");
    run(&repo.main, &["worktree", "add", "-q", "-b", "inner", nested.to_str().unwrap()]);
    let session = Session::start(&common, &[&main, &nested]);

    run(&nested, &["commit", "-qm", "work", "--allow-empty"]);
    assert_eq!(session.next_stale(), Some(nested.clone()));
    while let Ok(msg) = session.msgs.recv_timeout(quiet()) {
        assert!(!matches!(msg, Msg::Stale(p) if p == main));
    }
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_bare_repository_reports_every_new_worktree_and_their_commits() {
    let repo = Repo::new();
    let base = real(repo.main.parent().unwrap());
    let bare = base.join("bare.git");
    run(&base, &["clone", "-q", "--bare", "main", "bare.git"]);
    let session = Session::start(&bare, &[]);

    let first = base.join("first");
    run(&bare, &["worktree", "add", "-q", first.to_str().unwrap(), "develop"]);
    assert!(session.saw_list_change(), "first");
    session.set_roots(&[&first]);
    session.settle();
    let second = base.join("second");
    run(&bare, &["worktree", "add", "-q", "-b", "second", second.to_str().unwrap(), "develop"]);
    assert!(session.saw_list_change(), "second");
    session.set_roots(&[&first, &second]);
    session.settle();

    std::fs::write(first.join("new.txt"), "new\n").unwrap();
    run(&first, &["add", "new.txt"]);
    assert!(session.saw_stale(&first), "staging");
    session.settle();
    run(&first, &["-c", "user.name=T", "-c", "user.email=t@example.com", "commit", "-qm", "work"]);
    assert!(session.saw_stale(&first), "committing");
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn the_thread_ends_when_the_command_sender_is_dropped() {
    let (_repo, _main, common) = repo_with_main();
    let (tx, rx) = crossbeam_channel::unbounded();
    let cmds = watch::spawn(common, tx).unwrap();
    drop(cmds);
    // The thread holds the only sender, so the channel closes when it ends.
    assert!(matches!(rx.recv_timeout(PATIENCE), Err(RecvTimeoutError::Disconnected)));
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_commit_in_a_normal_repository_marks_only_the_worktree_it_happened_in() {
    let (repo, main, common) = repo_with_main();
    let linked = real(&repo.add_worktree("feat"));
    let session = Session::start(&common, &[&main, &linked]);
    session.assert_commit_marks_only(&main);
    session.assert_commit_marks_only(&linked);
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_commit_in_a_separate_git_dir_marks_its_main_worktree() {
    let repo = SeparateGitDir::new();
    run(&repo.worktree, &["add", "a.txt"]);
    let session = Session::start(&repo.git_dir, &[&repo.worktree]);
    session.assert_commit_marks_only(&repo.worktree);
    // Nothing is staged now, so only the reflog changes.
    session.assert_commit_marks_only(&repo.worktree);
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn a_commit_in_a_submodule_marks_its_main_worktree() {
    let repo = Repo::new();
    let (_superproject, sub) = repo_with_submodule(&repo);
    run(&sub, &["add", "a.txt"]);
    let common = git::discover(&sub).unwrap().common_dir;
    let session = Session::start(&common, &[&sub]);
    session.assert_commit_marks_only(&sub);
    session.assert_commit_marks_only(&sub);
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn switching_branch_refreshes_the_list_when_dot_git_is_a_file() {
    let separate = SeparateGitDir::new();
    let repo = Repo::new();
    let (_superproject, sub) = repo_with_submodule(&repo);
    let common = git::discover(&sub).unwrap().common_dir;
    for (common, root) in [(&separate.git_dir, &separate.worktree), (&common, &sub)] {
        let session = Session::start(common, &[root]);
        run(root, &["switch", "-qc", "next"]);
        assert!(session.saw_list_change(), "{}", root.display());
    }
}

#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn commits_in_a_separate_git_dir_mark_the_right_worktree() {
    let repo = SeparateGitDir::new();
    let linked = repo.worktree.parent().unwrap().join("linked");
    run(&repo.worktree, &["worktree", "add", "-q", "-b", "linked", linked.to_str().unwrap()]);
    let session = Session::start(&repo.git_dir, &[&repo.worktree, &linked]);
    session.assert_commit_marks_only(&linked);
    session.assert_commit_marks_only(&repo.worktree);
}

/// The linked worktree shares the name `sub` with the submodule's git dir, which
/// is what the main worktree's `.git` file ends in.
#[test]
#[ignore = "run by run_each_test_in_its_own_process"]
fn commits_in_a_submodule_mark_the_right_worktree_even_with_the_same_name() {
    let repo = Repo::new();
    let (superproject, sub) = repo_with_submodule(&repo);
    let linked = superproject.parent().unwrap().join("linked/sub");
    run(&sub, &["worktree", "add", "-q", "-b", "other", linked.to_str().unwrap()]);
    let common = git::discover(&sub).unwrap().common_dir;
    // Main goes last, so that it would take over the shared name if it were kept.
    let session = Session::start(&common, &[&linked, &sub]);
    session.assert_commit_marks_only(&linked);
    session.assert_commit_marks_only(&sub);
}
