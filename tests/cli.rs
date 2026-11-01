//! Failures before the UI starts, seen from outside the binary.
#![cfg(unix)]

mod common;
use common::Repo;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};

/// Runs the binary with no controlling terminal, whatever ran these tests.
fn branchdiff_without_a_terminal(cwd: &std::path::Path) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_branchdiff"));
    cmd.current_dir(cwd).stdin(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.output().unwrap()
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
