//! Run with: cargo test --release --test perf -- --ignored
//! Debug builds are not representative, so these are ignored by default.

use branchdiff::app::App;
use branchdiff::cache::CacheKey;
use branchdiff::diff::{FileDiff, Hunk, Line, LineKind, Status};
use branchdiff::git::Worktree;
use branchdiff::ui;
use branchdiff::worker::{Loaded, Msg};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKTREES: usize = 10;
const FILES: usize = 5_000;
const LINES: usize = 40;
const BUDGET: Duration = Duration::from_millis(16);

fn big_diff() -> Vec<FileDiff> {
    (0..FILES)
        .map(|f| FileDiff {
            path: format!("src/module_{f}.rs"),
            old_path: None,
            old_mode: None,
            new_mode: None,
            status: Status::Modified,
            binary: false,
            content_id: Some(format!("{f}")),
            added: LINES as u32,
            removed: 0,
            hunks: vec![Hunk {
                header: format!("@@ -0,0 +1,{LINES} @@"),
                lines: (0..LINES)
                    .map(|i| Line {
                        kind: LineKind::Added,
                        old_no: None,
                        new_no: Some(i as u32 + 1),
                        text: format!("let value_{i} = compute({i});"),
                    })
                    .collect(),
            }],
        })
        .collect()
}

fn loaded_app() -> App {
    let mut a = App::new("develop".into());
    let wts: Vec<Worktree> = (0..WORKTREES)
        .map(|i| Worktree { path: format!("/r/w{i}").into(), head: "h".into(), branch: Some(format!("w{i}")) })
        .collect();
    a.handle_msg(Msg::Worktrees(Ok(wts)));
    let diffs = Arc::new(big_diff());
    for i in 0..WORKTREES {
        let path: PathBuf = format!("/r/w{i}").into();
        let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: diffs.clone(), op: None }) });
    }
    a
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn timed(app: &mut App, key: KeyCode) -> Duration {
    let mut t = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let samples = (0..50)
        .map(|_| {
            let start = Instant::now();
            app.handle_key(KeyEvent::from(key));
            t.draw(|f| ui::draw(f, app)).unwrap();
            start.elapsed()
        })
        .collect();
    median(samples)
}

#[test]
#[ignore]
fn switching_to_a_cached_worktree_is_within_budget() {
    let d = timed(&mut loaded_app(), KeyCode::Char(']'));
    assert!(d < BUDGET, "worktree switch took {d:?}");
}

#[test]
#[ignore]
fn keypress_to_redraw_is_within_budget() {
    let mut a = loaded_app();
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    let d = timed(&mut a, KeyCode::Char('j'));
    assert!(d < BUDGET, "redraw took {d:?}");
}

#[test]
#[ignore]
fn full_screen_keypress_to_redraw_is_within_budget() {
    let mut a = loaded_app();
    a.handle_key(KeyEvent::from(KeyCode::Char('f')));
    let d = timed(&mut a, KeyCode::Char('j'));
    assert!(d < BUDGET, "full-screen redraw took {d:?}");
}

#[test]
#[ignore]
fn word_wrap_toggle_and_redraw_is_within_budget() {
    let d = timed(&mut loaded_app(), KeyCode::Char('W'));
    assert!(d < BUDGET, "word-wrap toggle and redraw took {d:?}");
}

fn app_of(worktrees: usize, files: usize) -> App {
    let mut a = App::new("develop".into());
    let wts: Vec<Worktree> = (0..worktrees)
        .map(|i| Worktree { path: format!("/r/w{i}").into(), head: "h".into(), branch: Some(format!("w{i}")) })
        .collect();
    a.handle_msg(Msg::Worktrees(Ok(wts)));
    let diffs: Arc<Vec<FileDiff>> = Arc::new(
        (0..files)
            .map(|f| FileDiff {
                path: format!("src/module_{f}.rs"),
                old_path: None,
                old_mode: None,
                new_mode: None,
                status: Status::Modified,
                binary: false,
                content_id: Some(format!("{f}")),
                added: 1,
                removed: 0,
                hunks: Vec::new(),
            })
            .collect(),
    );
    for i in 0..worktrees {
        let path: PathBuf = format!("/r/w{i}").into();
        let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
        a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: diffs.clone(), op: None }) });
    }
    a
}

#[test]
#[ignore]
fn frame_time_does_not_grow_with_list_length() {
    let frame = |worktrees, files| timed(&mut app_of(worktrees, files), KeyCode::Char('j'));
    let (small, many_files, many_worktrees) = (frame(10, 500), frame(10, 50_000), frame(5_000, 500));
    let limit = small * 5;
    assert!(many_files < limit, "{small:?} grew to {many_files:?} with more files");
    assert!(many_worktrees < limit, "{small:?} grew to {many_worktrees:?} with more worktrees");
}

#[test]
#[ignore]
fn help_output_starts_within_the_first_frame_budget() {
    let exe = env!("CARGO_BIN_EXE_branchdiff");
    let samples: Vec<Duration> = (0..10)
        .map(|_| {
            let start = Instant::now();
            std::process::Command::new(exe).arg("--version").output().unwrap();
            start.elapsed()
        })
        .collect();
    let d = median(samples);
    assert!(d < Duration::from_millis(100), "startup took {d:?}");
}
