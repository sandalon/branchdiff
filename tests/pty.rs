//! Functional PTY/ConPTY coverage runs on every platform in CI.
//! Timing checks: cargo test --release --test pty -- --ignored

mod common;
use common::Repo;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::sync::mpsc::{sync_channel, Receiver};
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(20);

struct Session {
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    input: Box<dyn Write + Send>,
    output: Receiver<std::io::Result<Vec<u8>>>,
    bytes: Vec<u8>,
    screen: vt100::Parser,
    started: Instant,
}

impl Session {
    fn start(repo: &Repo) -> Self {
        let pty =
            native_pty_system().openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_branchdiff"));
        cmd.cwd(&repo.main);
        cmd.env("TERM", "xterm-256color");
        let started = Instant::now();
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader().unwrap();
        let input = pty.master.take_writer().unwrap();
        let (tx, output) = sync_channel(8);
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                let chunk = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) => Err(e),
                };
                let failed = chunk.is_err();
                if tx.send(chunk).is_err() || failed {
                    break;
                }
            }
        });
        Self {
            child,
            _master: pty.master,
            input,
            output,
            bytes: Vec::new(),
            screen: vt100::Parser::new(NonZeroU16::new(24).unwrap(), NonZeroU16::new(100).unwrap(), 0),
            started,
        }
    }

    fn expect(&mut self, markers: &[&str]) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            // Ratatui sends incremental cursor updates, often reusing letters
            // from the previous frame. Assert the rendered screen, not bytes.
            let text = self.screen.screen().contents();
            if markers.iter().all(|marker| text.contains(marker)) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let chunk = self.output.recv_timeout(remaining).unwrap_or_else(|e| {
                panic!("terminal did not render {markers:?}: {e}\n{text}");
            });
            let chunk = chunk.expect("could not read the terminal");
            self.screen.process(&chunk);
            self.bytes.extend(chunk);
            assert!(self.bytes.len() <= 256 * 1024, "terminal output exceeded the smoke-test limit");
        }
    }

    fn send(&mut self, keys: &[u8]) {
        self.bytes.clear();
        self.input.write_all(keys).unwrap();
        self.input.flush().unwrap();
    }

    fn assert_clean_exit(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "terminal app exited with {status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "terminal app did not exit after q");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A failed assertion must not leave a TUI process running in CI.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn terminal_renders_help_and_exits_on_q() {
    let repo = Repo::new();
    let mut session = Session::start(&repo);
    session.expect(&["Worktrees", "Files", "Diff", "? help"]);
    session.expect(&["No changes against the base branch"]);
    session.send(b"?");
    session.expect(&["previous or next worktree", "comment on the current line"]);
    session.send(b"\x1b");
    session.expect(&["No changes against the base branch"]);
    session.send(b"q");
    session.assert_clean_exit();
}

#[test]
#[ignore = "strict startup timing is measured separately from functional CI"]
fn first_frame_is_drawn_within_budget() {
    let repo = Repo::new();
    let mut samples: Vec<_> = (0..10)
        .map(|_| {
            let mut session = Session::start(&repo);
            // Initial escape sequences alone do not count as a rendered frame.
            session.expect(&["Worktrees", "Files", "Diff", "? help"]);
            session.started.elapsed()
        })
        .collect();
    samples.sort();
    let d = samples[samples.len() / 2];
    assert!(d < Duration::from_millis(100), "first frame took {d:?}");
}
