//! Failures before the UI starts, seen from outside the binary.
mod common;
use common::Repo;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Output, Stdio};

/// Runs the binary with no controlling terminal, whatever ran these tests.
fn branchdiff_without_a_terminal(cwd: &std::path::Path) -> Output {
    let dir = tempfile::tempdir().unwrap();
    branchdiff_logging_to(cwd, &dir.path().join("branchdiff.log"))
}

fn branchdiff_logging_to(cwd: &std::path::Path, log: &std::path::Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_branchdiff"));
    cmd.current_dir(cwd).stdin(Stdio::null()).env("BRANCHDIFF_LOG_FILE", log);
    #[cfg(windows)]
    // DETACHED_PROCESS prevents inheriting the test runner's console.
    cmd.creation_flags(0x0000_0008);
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.output().unwrap()
}

#[test]
fn help_and_version_need_neither_a_repository_nor_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    for (flag, expected) in [("--help", "--base"), ("--version", "branchdiff ")] {
        let out = Command::new(env!("CARGO_BIN_EXE_branchdiff"))
            .arg(flag)
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
        assert!(!out.stdout.contains(&0x1b), "{flag} entered the terminal UI");
    }
}

#[test]
fn with_no_terminal_it_exits_with_a_clean_error() {
    let repo = Repo::new();
    let out = branchdiff_without_a_terminal(&repo.main);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("Error: could not start the terminal"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn outside_a_repository_it_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = branchdiff_without_a_terminal(dir.path());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "Error: not inside a git repository");
}

#[test]
fn log_path_prints_where_the_log_goes_without_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("chosen.log");
    let out = Command::new(env!("CARGO_BIN_EXE_branchdiff"))
        .arg("--log-path")
        .env("BRANCHDIFF_LOG_FILE", &log)
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), log.to_string_lossy());
    assert!(!log.exists(), "--log-path should not start a log");
}

#[test]
fn a_failed_start_is_written_to_the_log() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("logs").join("branchdiff.log");
    let out = branchdiff_logging_to(&repo.main, &log);
    assert_eq!(out.status.code(), Some(1));
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains(concat!("branchdiff ", env!("CARGO_PKG_VERSION"), " starting")), "{text}");
    assert!(text.contains("git version "), "{text}");
    assert!(text.contains("exiting with an error: could not start the terminal"), "{text}");
}

#[test]
fn failed_git_commands_are_logged_without_verbose() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("branchdiff.log");
    let out = branchdiff_logging_to(dir.path(), &log);
    assert_eq!(out.status.code(), Some(1));
    let text = std::fs::read_to_string(log).unwrap();
    assert!(text.contains("git rev-parse ") && text.contains("--show-toplevel"), "{text}");
    assert!(text.contains("exit Some(128):"), "{text}");
}

#[test]
fn an_unusable_log_does_not_stop_startup() {
    let repo = Repo::new();
    let dir = tempfile::tempdir().unwrap();
    let out = branchdiff_logging_to(&repo.main, dir.path());
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("Error: could not start the terminal"));
}
