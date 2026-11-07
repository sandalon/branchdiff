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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_branchdiff"));
    cmd.current_dir(cwd).stdin(Stdio::null());
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
