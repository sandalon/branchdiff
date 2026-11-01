//! Run with: cargo test --release --test pty -- --ignored
//! Measures the real first frame by running the binary in a pseudoterminal.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::Read;
use std::time::{Duration, Instant};

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
#[ignore]
fn first_frame_is_drawn_within_budget() {
    let exe = env!("CARGO_BIN_EXE_branchdiff");
    let samples: Vec<Duration> = (0..10)
        .map(|_| {
            let pty =
                native_pty_system().openpty(PtySize { rows: 24, cols: 100, pixel_width: 0, pixel_height: 0 }).unwrap();
            let cmd = CommandBuilder::new(exe);
            let mut child = pty.slave.spawn_command(cmd).unwrap();
            drop(pty.slave);
            let mut reader = pty.master.try_clone_reader().unwrap();
            let start = Instant::now();
            let mut buf = [0u8; 65536];
            let n = reader.read(&mut buf).unwrap();
            assert!(n > 0, "the binary produced no output");
            let elapsed = start.elapsed();
            drop(reader);
            let _ = child.kill();
            let _ = child.wait();
            elapsed
        })
        .collect();
    let d = median(samples);
    assert!(d < Duration::from_millis(100), "first frame took {d:?}");
}
