use branchdiff::app::{App, Effect, Focus};
use branchdiff::cache::CacheKey;
use branchdiff::diff::{FileDiff, Hunk, Line, LineKind, Status};
use branchdiff::git::{GitError, Worktree};
use branchdiff::highlight::HighlightedFile;
use branchdiff::store::{Comment, Side};
use branchdiff::ui;
use branchdiff::worker::{Loaded, Msg};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use std::sync::Arc;

fn sample_file() -> FileDiff {
    FileDiff {
        path: "src/a.rs".into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 1,
        hunks: vec![Hunk {
            header: "@@ -1,2 +1,2 @@".into(),
            lines: vec![
                Line { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "fn load() {".into() },
                Line { kind: LineKind::Removed, old_no: Some(2), new_no: None, text: "    old();".into() },
                Line { kind: LineKind::Added, old_no: None, new_no: Some(2), text: "    retry();".into() },
            ],
        }],
    }
}

fn key() -> CacheKey {
    CacheKey { worktree: "/r/feat-auth".into(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 }
}

fn app_with(files: Vec<FileDiff>) -> App {
    let mut a = App::new("develop".into());
    let wts = ["feat-auth", "fix-cache"].map(|n| Worktree { path: format!("/r/{n}").into(), head: "h".into(), branch: Some(n.into()) });
    a.handle_msg(Msg::Worktrees(Ok(wts.to_vec())));
    let _: Vec<Effect> = a.handle_msg(Msg::Loaded { path: "/r/feat-auth".into(), result: Ok(Loaded { key: key(), diffs: Arc::new(files), op: None }) });
    a
}

fn app() -> App {
    app_with(vec![sample_file()])
}

/// A file of one hunk holding `texts` as added lines.
fn file_of(texts: &[&str]) -> FileDiff {
    let lines = texts.iter().enumerate().map(|(i, t)| {
        Line { kind: LineKind::Added, old_no: None, new_no: Some(i as u32 + 1), text: (*t).into() }
    }).collect();
    FileDiff { hunks: vec![Hunk { header: "@@ -0,0 +1 @@".into(), lines }], ..sample_file() }
}

/// Columns of the side-by-side divider on each diff row with one.
fn divider_columns(buffer: &Buffer) -> Vec<u16> {
    let (left, right, bottom) = (51, buffer.area.width - 1, buffer.area.height - 2);
    (1..bottom).flat_map(|y| {
        (left..right).filter(move |&x| buffer[(x, y)].symbol() == "│")
    }).collect()
}

fn render(a: &mut App) -> String {
    render_sized(a, 80, 12)
}

fn render_sized(a: &mut App, width: u16, height: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| ui::draw(f, a)).unwrap();
    t.backend().to_string()
}

fn render_buffer(a: &mut App, width: u16, height: u16) -> Buffer {
    let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
    t.draw(|f| ui::draw(f, a)).unwrap();
    t.backend().buffer().clone()
}

#[test]
fn main_screen() {
    insta::assert_snapshot!(render(&mut app()));
}

#[test]
fn side_by_side() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char('v')));
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn loading_placeholder_for_a_worktree_with_no_data() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char(']')));
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn help_overlay_lists_every_hotkey() {
    let mut a = app();
    a.handle_key(KeyEvent::from(KeyCode::Char('?')));
    let out = render_sized(&mut a, 80, 24);
    for label in ["next worktree", "fuzzy", "comment", "reviewed", "base branch", "this help"] {
        assert!(out.to_lowercase().contains(label), "missing {label}");
    }
    insta::assert_snapshot!(out);
}

#[test]
fn error_is_shown_on_the_affected_worktree() {
    let mut a = app();
    a.handle_msg(Msg::Loaded { path: "/r/fix-cache".into(), result: Err(GitError::NoMergeBase) });
    a.handle_key(KeyEvent::from(KeyCode::Char(']')));
    assert!(render(&mut a).contains("no common history"));
}

#[test]
fn a_failed_worktree_list_is_shown() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Err(GitError::Failed("boom".into()))));
    assert!(render(&mut a).contains("Could not list worktrees: boom"));
}

#[test]
fn commented_lines_are_marked_and_their_comment_is_shown() {
    let mut a = app();
    a.focus = Focus::Diff;
    for _ in 0..3 {
        a.handle_key(KeyEvent::from(KeyCode::Char('j')));
    }
    a.handle_key(KeyEvent::from(KeyCode::Char('c')));
    for c in "why".chars() {
        a.handle_key(KeyEvent::from(KeyCode::Char(c)));
    }
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    let out = render(&mut a);
    assert!(out.contains("*    2 +"));
    assert!(out.contains(" * why"));
}

fn type_keys(a: &mut App, keys: &str) {
    for c in keys.chars() {
        a.handle_key(KeyEvent::from(KeyCode::Char(c)));
    }
}

#[test]
fn the_finder_lists_the_match_that_enter_takes_first() {
    let mut a = app();
    type_keys(&mut a, "wcache");
    let out = render(&mut a);
    assert!(out.contains("Find worktree") && out.contains("> cache"));
    assert!(!out.contains("│feat-auth"));
    assert!(out.contains("│fix-cache"));
}

#[test]
fn the_base_picker_shows_at_most_eight_branches() {
    let mut a = app();
    let branches: Vec<String> = (0..20).map(|i| format!("branch-{i:02}")).collect();
    a.handle_msg(Msg::Branches(Ok(branches)));
    let out = render_sized(&mut a, 80, 24);
    assert!(out.contains("branch-07") && !out.contains("branch-08"));
}

#[test]
fn prompts_render_at_tiny_sizes_without_panicking() {
    let mut a = app();
    a.handle_msg(Msg::Branches(Ok(vec!["main".into(), "develop".into()])));
    type_keys(&mut a, "ma");
    for (w, h) in [(1, 1), (4, 2), (10, 3), (20, 5)] {
        render_sized(&mut a, w, h);
    }
}

#[test]
fn stale_badge_is_visible() {
    let mut a = app();
    a.handle_msg(Msg::Stale("/r/feat-auth".into()));
    assert!(render(&mut a).contains("changed"));
}

#[test]
fn renders_at_tiny_sizes_without_panicking() {
    let mut a = app();
    for (w, h) in [(1, 1), (10, 3), (20, 5)] {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| ui::draw(f, &mut a)).unwrap();
    }
}

#[test]
fn a_long_prompt_shows_the_last_character_typed() {
    let mut a = app();
    a.focus = Focus::Diff;
    type_keys(&mut a, "jc");
    type_keys(&mut a, &"long comment ".repeat(10));
    type_keys(&mut a, "Z");
    let out = render_sized(&mut a, 80, 24);
    assert!(out.contains("comment Z"), "{out}");
}

#[test]
fn a_tab_indented_line_renders_with_four_spaces() {
    let mut a = app_with(vec![file_of(&["\tbody"])]);
    assert!(render(&mut a).contains("+    body"));
}

#[test]
fn a_crlf_line_has_no_stray_characters_and_stays_aligned_side_by_side() {
    let mut a = app_with(vec![file_of(&["one\r", "two", "three\r"])]);
    a.handle_key(KeyEvent::from(KeyCode::Char('v')));
    let buffer = render_buffer(&mut a, 120, 12);
    assert!(!buffer.content.iter().any(|c| c.symbol().contains('\r')));
    let columns = divider_columns(&buffer);
    assert_eq!(columns.len(), 3, "{columns:?}");
    assert!(columns.iter().all(|c| *c == columns[0]), "{columns:?}");
}

#[test]
fn a_file_with_no_hunks_says_so() {
    let mut a = app_with(vec![FileDiff { hunks: Vec::new(), ..sample_file() }]);
    assert!(render(&mut a).contains("No text changes to show"));
}

#[test]
fn a_highlighted_side_by_side_line_has_the_plain_text_and_the_span_colours() {
    let highlighted = HighlightedFile {
        hunks: vec![vec![vec![(Color::Red, "\t".into()), (Color::Green, "\tlet".into()), (Color::Blue, " x".into())]]],
    };
    let mut plain = app_with(vec![file_of(&["\t\tlet x"])]);
    plain.handle_key(KeyEvent::from(KeyCode::Char('v')));
    let mut coloured = app_with(vec![file_of(&["\t\tlet x"])]);
    coloured.handle_key(KeyEvent::from(KeyCode::Char('v')));
    coloured.handle_msg(Msg::Highlighted { key: key(), file_index: 0, lines: highlighted });

    assert_eq!(render_sized(&mut plain, 120, 12), render_sized(&mut coloured, 120, 12));

    let buffer = render_buffer(&mut coloured, 120, 12);
    let right = divider_columns(&buffer)[0] + 2;
    let colour_at = |offset: u16| buffer[(right + offset, 2)].fg;
    assert_eq!(colour_at(3), Color::Red);
    assert_eq!(colour_at(4), Color::Green);
    assert_eq!(buffer[(right + 8, 2)].symbol(), "l");
    assert_eq!(colour_at(8), Color::Green);
    assert_eq!(colour_at(12), Color::Blue);
}

#[test]
fn a_wide_character_at_the_edge_keeps_the_divider_in_its_column() {
    // The pane is 68 cells inside, so each side is 32.
    let straddling = format!("{}界", "a".repeat(31));
    let mut a = app_with(vec![file_of(&["short", &straddling, "short"])]);
    a.handle_key(KeyEvent::from(KeyCode::Char('v')));
    let columns = divider_columns(&render_buffer(&mut a, 120, 12));
    assert_eq!(columns.len(), 3, "{columns:?}");
    assert!(columns.iter().all(|c| *c == columns[0]), "{columns:?}");
}

#[test]
fn without_truecolor_no_cell_holds_an_rgb_colour() {
    let highlighted = HighlightedFile { hunks: vec![vec![vec![(Color::Rgb(200, 100, 50), "code".into())]]] };
    let mut a = app_with(vec![file_of(&["code"])]);
    a.handle_msg(Msg::Highlighted { key: key(), file_index: 0, lines: highlighted.clone() });
    let is_rgb = |c: Color| matches!(c, Color::Rgb(..));
    assert!(render_buffer(&mut a, 80, 12).content.iter().any(|c| is_rgb(c.fg) || is_rgb(c.bg)));

    a.truecolor = false;
    let buffer = render_buffer(&mut a, 80, 12);
    assert!(!buffer.content.iter().any(|c| is_rgb(c.fg) || is_rgb(c.bg)));
}

#[test]
fn an_emoji_sequence_keeps_the_divider_in_its_column() {
    let mut file = file_of(&["short", "heart ❤\u{fe0f}", "family 👩\u{200d}👩\u{200d}👧"]);
    // On the left, ahead of the divider.
    for line in &mut file.hunks[0].lines {
        (line.kind, line.old_no, line.new_no) = (LineKind::Removed, line.new_no, None);
    }
    let mut a = app_with(vec![file]);
    a.handle_key(KeyEvent::from(KeyCode::Char('v')));
    let columns = divider_columns(&render_buffer(&mut a, 120, 12));
    assert_eq!(columns.len(), 3, "{columns:?}");
    assert!(columns.iter().all(|c| *c == columns[0]), "{columns:?}");
}

#[test]
fn a_deep_path_shows_its_file_name() {
    let path = "src/components/settings/preferences/notifications/widget.rs";
    let mut a = app_with(vec![FileDiff { path: path.into(), ..sample_file() }]);
    assert!(render(&mut a).contains("ations/widget.rs"));
}

#[test]
fn a_long_worktree_name_leaves_room_for_its_badges() {
    let mut a = App::new("develop".into());
    let name = "feature/a-long-branch-name-ending-rework";
    a.handle_msg(Msg::Worktrees(Ok(vec![Worktree { path: "/r/w".into(), head: "h".into(), branch: Some(name.into()) }])));
    a.handle_msg(Msg::Stale("/r/w".into()));
    a.handle_msg(Msg::Loaded { path: "/r/w".into(), result: Err(GitError::NoMergeBase) });
    a.states[0].review.comments = (0..12).map(|i| Comment {
        path: "a".into(), line: i, side: Side::New, line_text: String::new(), body: "b".into(),
    }).collect();
    let out = render(&mut a);
    assert!(out.contains("rework +0 c12 ● ✗"), "{out}");
}

#[test]
fn the_selected_file_stays_visible_among_thousands_with_its_own_reviewed_mark() {
    let files = (0..5000).map(|i| FileDiff { path: format!("f{i}.rs"), content_id: Some(i.to_string()), ..sample_file() }).collect();
    let mut a = app_with(files);
    a.focus = Focus::Files;
    for _ in 0..4990 {
        a.handle_key(KeyEvent::from(KeyCode::Char('j')));
    }
    a.handle_key(KeyEvent::from(KeyCode::Char('x')));
    a.handle_key(KeyEvent::from(KeyCode::Char('j')));
    let out = render(&mut a);
    assert!(out.contains("✓M f4990.rs"), "{out}");
    assert!(out.contains(" M f4991.rs"), "{out}");
    assert!(a.is_reviewed(4990) && !a.is_reviewed(4991));

    let buffer = render_buffer(&mut a, 80, 12);
    let selected: Vec<u16> = (1..10).filter(|&y| buffer[(27, y)].modifier.contains(Modifier::REVERSED)).collect();
    let row: String = (27..49).map(|x| buffer[(x, selected[0])].symbol()).collect();
    assert_eq!(selected.len(), 1);
    assert!(row.contains("f4991.rs"), "{row}");
}

#[test]
fn messages_wrap_in_the_narrow_diff_area() {
    let mut a = app_with(Vec::new());
    assert!(render(&mut a).contains("branch"));
    let mut a = app_with(vec![FileDiff { binary: true, hunks: Vec::new(), ..sample_file() }]);
    assert!(render(&mut a).contains("not shown"));
}

#[test]
fn a_failed_worktree_list_shows_all_of_its_stderr_in_the_diff_area() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Err(GitError::Failed("first\nsecond line".into()))));
    assert!(render(&mut a).contains("second line"));
}

#[test]
fn the_diff_area_says_loading_before_the_worktree_list_arrives() {
    let mut a = App::new("develop".into());
    assert!(render(&mut a).contains("Loading…"));
}

fn numbered_files(count: usize) -> Vec<FileDiff> {
    (0..count).map(|i| FileDiff { path: format!("f{i:03}.rs"), ..sample_file() }).collect()
}

fn worktrees(count: usize) -> Vec<Worktree> {
    (0..count).map(|i| {
        let name = format!("wt-{i:03}");
        Worktree { path: format!("/r/{name}").into(), head: "h".into(), branch: Some(name) }
    }).collect()
}

/// The screen row holding `text`, if it's drawn.
fn row_of(out: &str, text: &str) -> Option<usize> {
    out.lines().position(|l| l.contains(text))
}

fn press(a: &mut App, key: char, times: usize) {
    for _ in 0..times {
        a.handle_key(KeyEvent::from(KeyCode::Char(key)));
    }
}

#[test]
fn moving_up_inside_the_file_window_does_not_scroll_it() {
    let mut a = app_with(numbered_files(40));
    a.focus = Focus::Files;
    for _ in 0..15 {
        press(&mut a, 'j', 1);
        render(&mut a);
    }
    // Nine rows, so f015 on the last row leaves f007 at the top.
    let before = render(&mut a);
    assert_eq!(row_of(&before, "f007.rs"), Some(1), "{before}");
    assert_eq!(row_of(&before, "f015.rs"), Some(9), "{before}");

    press(&mut a, 'k', 1);
    let after = render(&mut a);
    assert_eq!(row_of(&after, "f007.rs"), Some(1), "{after}");
    assert_eq!(row_of(&after, "f015.rs"), Some(9), "{after}");
    assert_eq!(a.current().unwrap().file_offset, 7);

    // At the top row, the next step up scrolls by one.
    press(&mut a, 'k', 7);
    render(&mut a);
    press(&mut a, 'k', 1);
    let top = render(&mut a);
    assert_eq!(row_of(&top, "f006.rs"), Some(1), "{top}");
    assert_eq!(a.current().unwrap().file_offset, 6);
}

#[test]
fn moving_up_inside_the_worktree_window_does_not_scroll_it() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40))));
    for _ in 0..15 {
        press(&mut a, 'j', 1);
        render(&mut a);
    }
    press(&mut a, 'k', 1);
    let out = render(&mut a);
    assert_eq!(row_of(&out, "wt-007"), Some(1), "{out}");
    assert_eq!(row_of(&out, "▸ wt-014"), Some(8), "{out}");
    assert_eq!(a.worktree_offset, 7);
}

#[test]
fn selections_stay_visible_through_moves_and_resizes() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40))));
    a.handle_msg(Msg::Loaded {
        path: "/r/wt-000".into(),
        result: Ok(Loaded { key: key(), diffs: Arc::new(numbered_files(300)), op: None }),
    });
    let mut seed = 7u32;
    let mut next = |limit: u32| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (seed >> 16) % limit
    };
    for _ in 0..3000 {
        a.focus = if next(2) == 0 { Focus::Files } else { Focus::Worktrees };
        let key = ['j', 'k', 'n', 'p'][next(4) as usize];
        press(&mut a, key, next(25) as usize);
        // Only the first worktree has files, so keep coming back to it.
        if next(3) == 0 {
            press(&mut a, '1', 1);
        }
        let (width, height) = (60 + next(100) as u16, 4 + next(40) as u16);
        let out = render_sized(&mut a, width, height);

        let selected = format!("▸ wt-{:03}", a.selected);
        assert!(out.contains(&selected), "{selected} missing at {width}x{height}\n{out}");
        let (file, count) = (a.current().unwrap().file, a.file_count());
        if count > 0 {
            let path = format!("f{file:03}.rs");
            assert!(out.contains(&path), "{path} missing at {width}x{height}\n{out}");
        }
    }
}

#[test]
fn a_list_that_shrinks_leaves_a_valid_offset() {
    let mut a = app_with(numbered_files(40));
    a.focus = Focus::Files;
    press(&mut a, 'j', 39);
    render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 31);

    a.handle_msg(Msg::Loaded { path: "/r/feat-auth".into(), result: Ok(Loaded { key: key(), diffs: Arc::new(numbered_files(3)), op: None }) });
    let out = render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 0);
    assert_eq!(row_of(&out, "f000.rs"), Some(1), "{out}");

    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40))));
    press(&mut a, 'j', 39);
    render(&mut a);
    assert_eq!(a.worktree_offset, 31);
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40).split_off(38))));
    let out = render(&mut a);
    assert_eq!(a.worktree_offset, 0);
    assert_eq!(row_of(&out, "wt-038"), Some(1), "{out}");
}

#[test]
fn a_taller_terminal_shows_as_much_of_the_list_as_fits() {
    let mut a = app_with(numbered_files(20));
    a.focus = Focus::Files;
    press(&mut a, 'j', 19);
    render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 11);

    let out = render_sized(&mut a, 80, 16);
    assert_eq!(a.current().unwrap().file_offset, 7);
    assert_eq!(row_of(&out, "f019.rs"), Some(13), "{out}");
}

#[test]
fn the_file_offset_survives_switching_away_and_back() {
    let mut a = app_with(numbered_files(40));
    let other = CacheKey { worktree: "/r/fix-cache".into(), ..key() };
    a.handle_msg(Msg::Loaded { path: "/r/fix-cache".into(), result: Ok(Loaded { key: other, diffs: Arc::new(numbered_files(2)), op: None }) });
    a.focus = Focus::Files;
    press(&mut a, 'j', 20);
    render(&mut a);
    press(&mut a, 'k', 3);
    let before = render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 12);

    press(&mut a, ']', 1);
    render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 0);
    press(&mut a, '[', 1);
    assert_eq!(render(&mut a), before);
    assert_eq!(a.current().unwrap().file_offset, 12);
}
