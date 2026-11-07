use branchdiff::app::{App, Effect, Focus};
use branchdiff::cache::CacheKey;
use branchdiff::diff::{FileDiff, Hunk, Line, LineKind, Status};
use branchdiff::git::{GitError, Worktree};
use branchdiff::highlight::HighlightedFile;
use branchdiff::store::{Comment, Side};
use branchdiff::ui;
use branchdiff::worker::{Loaded, Msg};
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

const HEART: &str = "❤\u{fe0f}";

fn sample_file() -> FileDiff {
    FileDiff {
        path: "src/a.rs".into(),
        old_path: None,
        status: Status::Modified,
        binary: false,
        content_id: None,
        added: 1,
        removed: 1,
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

/// A file of one hunk holding `texts` as added lines.
fn file_of(texts: &[&str]) -> FileDiff {
    let lines = texts
        .iter()
        .enumerate()
        .map(|(i, t)| Line { kind: LineKind::Added, old_no: None, new_no: Some(i as u32 + 1), text: (*t).into() })
        .collect();
    FileDiff { hunks: vec![Hunk { header: "@@ -0,0 +1 @@".into(), lines }], ..sample_file() }
}

/// Lines `"line1"` onward, as added lines of one hunk.
fn long_file(count: usize) -> FileDiff {
    let lines: Vec<String> = (1..=count).map(|i| format!("line{i}")).collect();
    file_of(&lines.iter().map(String::as_str).collect::<Vec<_>>())
}

fn numbered_files(count: usize) -> Vec<FileDiff> {
    (0..count).map(|i| FileDiff { path: format!("f{i:03}.rs"), ..sample_file() }).collect()
}

/// One context, one removed and one added line, whose text is unique on screen.
fn added_and_removed() -> FileDiff {
    FileDiff {
        hunks: vec![Hunk {
            header: "@@ -1 +1 @@".into(),
            lines: vec![
                Line { kind: LineKind::Context, old_no: Some(1), new_no: Some(1), text: "%".into() },
                Line { kind: LineKind::Removed, old_no: Some(2), new_no: None, text: "Z".into() },
                Line { kind: LineKind::Added, old_no: None, new_no: Some(2), text: "Q".into() },
            ],
        }],
        ..sample_file()
    }
}

fn key() -> CacheKey {
    CacheKey { worktree: "/r/feat-auth".into(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 }
}

fn worktrees(count: usize) -> Vec<Worktree> {
    (0..count)
        .map(|i| {
            let name = format!("wt-{i:03}");
            Worktree { path: format!("/r/{name}").into(), head: "h".into(), branch: Some(name) }
        })
        .collect()
}

fn comments(count: u32, path: &str) -> Vec<Comment> {
    (0..count)
        .map(|i| Comment { path: path.into(), line: i, side: Side::New, line_text: String::new(), body: "b".into() })
        .collect()
}

fn app_with(files: Vec<FileDiff>) -> App {
    let mut a = App::new("develop".into());
    let wts = ["feat-auth", "fix-cache"].map(|n| Worktree {
        path: format!("/r/{n}").into(),
        head: "h".into(),
        branch: Some(n.into()),
    });
    a.handle_msg(Msg::Worktrees(Ok(wts.to_vec())));
    let _: Vec<Effect> = a.handle_msg(Msg::Loaded {
        path: "/r/feat-auth".into(),
        result: Ok(Loaded { key: key(), diffs: Arc::new(files), op: None }),
    });
    a
}

fn app() -> App {
    app_with(vec![sample_file()])
}

/// An app listing one worktree at `/r/w` called `name`, with nothing loaded.
fn one_worktree(name: &str) -> App {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(vec![Worktree {
        path: "/r/w".into(),
        head: "h".into(),
        branch: Some(name.into()),
    }])));
    a
}

/// An app with one worktree called `name`, with every badge a row can have.
fn badged(name: &str, added: u32, comment_count: u32) -> App {
    let mut a = one_worktree(name);
    let key = CacheKey { worktree: "/r/w".into(), ..key() };
    let file = FileDiff { added, ..sample_file() };
    a.handle_msg(Msg::Loaded {
        path: "/r/w".into(),
        result: Ok(Loaded { key, diffs: Arc::new(vec![file]), op: Some("rebase") }),
    });
    a.handle_msg(Msg::Stale("/r/w".into()));
    a.handle_msg(Msg::Loaded { path: "/r/w".into(), result: Err(GitError::NoMergeBase) });
    a.states[0].review.comments = comments(comment_count, "src/a.rs");
    a
}

/// Presses each character of `keys` in turn.
fn press(a: &mut App, keys: &str) {
    for c in keys.chars() {
        a.handle_key(KeyEvent::from(KeyCode::Char(c)));
    }
}

/// Delivers colour for the first file: spans per line per hunk.
fn highlight(a: &mut App, hunks: Vec<Vec<Vec<(Color, String)>>>) {
    a.handle_msg(Msg::Highlighted { key: key(), file_index: 0, lines: HighlightedFile { hunks } });
}

/// One line's spans.
fn spans(parts: &[(Color, &str)]) -> Vec<(Color, String)> {
    parts.iter().map(|(c, t)| (*c, (*t).to_string())).collect()
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

/// The screen row holding `text`, if it's drawn.
fn row_of(out: &str, text: &str) -> Option<usize> {
    out.lines().position(|l| l.contains(text))
}

fn status_line(out: &str) -> String {
    out.lines().last().unwrap().trim_matches('"').trim_end().to_string()
}

/// The width of the worktree pane, and of the file pane.
fn pane_width(terminal: u16) -> u16 {
    (terminal / 5).clamp(16, 30)
}

/// Columns of the side-by-side divider on each diff row with one.
fn divider_columns(buffer: &Buffer) -> Vec<u16> {
    let (left, right, bottom) = (2 * pane_width(buffer.area.width) + 1, buffer.area.width - 1, buffer.area.height - 2);
    (1..bottom).flat_map(|y| (left..right).filter(move |&x| buffer[(x, y)].symbol() == "│")).collect()
}

/// Checks that the divider is on all of three diff rows, in one column.
fn assert_divider_aligned(buffer: &Buffer) {
    let columns = divider_columns(buffer);
    assert_eq!(columns.len(), 3, "{columns:?}");
    assert!(columns.iter().all(|c| *c == columns[0]), "{columns:?}");
}

fn is_reversed(cell: &Cell) -> bool {
    cell.modifier.contains(Modifier::REVERSED)
}

/// The screen row and text of each reversed row in the file list.
fn reversed_file_rows(buffer: &Buffer) -> Vec<(u16, String)> {
    let pane = pane_width(buffer.area.width);
    let (left, right) = (pane + 1, 2 * pane - 1);
    (1..buffer.area.height - 2)
        .filter(|&y| is_reversed(&buffer[(left, y)]))
        .map(|y| (y, (left..right).map(|x| buffer[(x, y)].symbol()).collect()))
        .collect()
}

/// The cells inside the diff pane on screen row `y`, skipping the second half
/// of each wide character, which the terminal draws from the first.
fn diff_row(buffer: &Buffer, y: u16) -> Vec<&Cell> {
    let mut x = 2 * pane_width(buffer.area.width) + 1;
    let mut cells = Vec::new();
    while x < buffer.area.width - 1 {
        let cell = &buffer[(x, y)];
        cells.push(cell);
        x += cell.symbol().width().max(1) as u16;
    }
    cells
}

fn buffer_text(buffer: &Buffer) -> String {
    buffer.content.iter().map(|cell| cell.symbol()).collect()
}

/// The cell drawing `symbol`, which must be unique on screen.
fn cell_of<'a>(buffer: &'a Buffer, symbol: &str) -> &'a Cell {
    buffer.content.iter().find(|c| c.symbol() == symbol).unwrap()
}

/// The column and row where `text` first starts on screen.
fn position_of(buffer: &Buffer, text: &str) -> (u16, u16) {
    for y in 0..buffer.area.height {
        let row: Vec<&str> = (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect();
        if let Some(x) = (0..row.len()).find(|&x| row[x..].concat().starts_with(text)) {
            return (x as u16, y);
        }
    }
    panic!("{text:?} is not on screen");
}

fn is_light(colour: Color) -> bool {
    matches!(colour, Color::Rgb(r, g, b) if r > 150 && g > 150 && b > 150)
}

#[test]
fn main_screen() {
    insta::assert_snapshot!(render(&mut app()));
}

#[test]
fn side_by_side() {
    let mut a = app();
    press(&mut a, "v");
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn full_screen_diff_replaces_both_navigation_panes() {
    let mut a = app();
    a.focus = Focus::Diff;
    press(&mut a, "f");
    let out = render(&mut a);
    let buffer = render_buffer(&mut a, 80, 12);

    assert!(!out.contains("Worktrees") && !out.contains("Files"));
    assert!(out.contains("Diff"));
    assert_eq!(buffer[(0, 0)].symbol(), "┌");
    assert_eq!(buffer[(79, 0)].symbol(), "┐");
}

#[test]
fn the_full_screen_pane_follows_focus() {
    let mut a = app();
    press(&mut a, "f");
    let out = render(&mut a);
    assert!(out.contains("Worktrees") && !out.contains("Files") && !out.contains("Diff"), "{out}");

    a.handle_key(KeyEvent::from(KeyCode::Tab));
    let out = render(&mut a);
    assert!(out.contains("Files") && !out.contains("Worktrees") && !out.contains("Diff"), "{out}");
}

#[test]
fn loading_placeholder_for_a_worktree_with_no_data() {
    let mut a = app();
    press(&mut a, "]");
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn help_overlay_lists_every_hotkey() {
    let mut a = app();
    press(&mut a, "?");
    let out = render_sized(&mut a, 80, 24);
    for label in
        ["next worktree", "fuzzy", "comment", "reviewed", "base branch", "this help", "esc", "close help or a prompt"]
    {
        assert!(out.to_lowercase().contains(label), "missing {label}");
    }
    let descriptions = ["previous or next worktree", "next area", "previous area", "toggle word wrap", "quit"];
    let columns: Vec<usize> = descriptions
        .iter()
        .map(|description| out.lines().find(|line| line.contains(description)).unwrap().find(description).unwrap())
        .collect();
    assert!(columns.iter().all(|column| *column == columns[0]), "description columns: {columns:?}");
    assert!(out.contains("Shift+Tab / Left   previous area"));
    insta::assert_snapshot!(out);
}

#[test]
fn error_is_shown_on_the_affected_worktree() {
    let mut a = app();
    a.handle_msg(Msg::Loaded { path: "/r/fix-cache".into(), result: Err(GitError::NoMergeBase) });
    press(&mut a, "]");
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
    press(&mut a, "jjjcwhy");
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    let out = render(&mut a);
    assert!(out.contains("*    2 +"));
    assert!(out.contains(" * why"));
}

#[test]
fn the_finder_lists_the_match_that_enter_takes_first() {
    let mut a = app();
    press(&mut a, "wcache");
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
fn stale_badge_is_visible() {
    let mut a = app();
    a.handle_msg(Msg::Stale("/r/feat-auth".into()));
    assert!(render(&mut a).contains("changed"));
}

#[test]
fn a_long_prompt_shows_the_last_character_typed() {
    let mut a = app();
    a.focus = Focus::Diff;
    press(&mut a, "jc");
    press(&mut a, &"long comment ".repeat(10));
    press(&mut a, "Z");
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
    // Context, because only text on the left can move the divider.
    let context = |texts: &[&str]| {
        let mut file = file_of(texts);
        for line in &mut file.hunks[0].lines {
            (line.kind, line.old_no) = (LineKind::Context, line.new_no);
        }
        file
    };
    let mut crlf = app_with(vec![context(&["one\r", "two", "three\r"])]);
    let mut lf = app_with(vec![context(&["one", "two", "three"])]);
    assert_eq!(render_sized(&mut crlf, 120, 12), render_sized(&mut lf, 120, 12));

    press(&mut crlf, "v");
    press(&mut lf, "v");
    assert_eq!(render_sized(&mut crlf, 120, 12), render_sized(&mut lf, 120, 12));
    assert_divider_aligned(&render_buffer(&mut crlf, 120, 12));
}

#[test]
fn shift_w_wraps_long_unified_lines_and_marks_every_continuation_as_selected() {
    let text = format!("{}TAIL", "a".repeat(60));
    let mut a = app_with(vec![file_of(&[&text])]);
    a.focus = Focus::Diff;
    press(&mut a, "j");
    assert!(!render(&mut a).contains("TAIL"));

    press(&mut a, "W");
    let buffer = render_buffer(&mut a, 80, 12);
    assert!(buffer_text(&buffer).contains("TAIL"));
    assert!(diff_row(&buffer, 2).iter().all(|cell| is_reversed(cell)));
    assert!(diff_row(&buffer, 3).iter().all(|cell| is_reversed(cell)));
}

#[test]
fn word_wrap_keeps_the_side_by_side_divider_aligned() {
    let text = format!("{}TAIL", "x".repeat(45));
    let mut file = file_of(&[&text]);
    file.hunks[0].lines[0].kind = LineKind::Context;
    file.hunks[0].lines[0].old_no = Some(1);
    let mut a = app_with(vec![file]);
    press(&mut a, "vW");

    let buffer = render_buffer(&mut a, 80, 12);
    let columns = divider_columns(&buffer);
    assert!(columns.len() >= 2, "{columns:?}");
    assert!(columns.iter().all(|column| *column == columns[0]), "{columns:?}");
    assert!(buffer_text(&buffer).matches("TAIL").count() >= 2);
}

#[test]
fn wrapped_diff_scrolling_keeps_the_logical_cursor_visible() {
    let text = "x".repeat(60);
    let lines: Vec<String> = (1..=8).map(|i| format!("{i}{text}")).collect();
    let mut a = app_with(vec![file_of(&lines.iter().map(String::as_str).collect::<Vec<_>>())]);
    a.focus = Focus::Diff;
    press(&mut a, "W");
    press(&mut a, &"j".repeat(6));

    let buffer = render_buffer(&mut a, 80, 12);
    assert_eq!(a.current().unwrap().scroll, 3);
    assert!(diff_row(&buffer, 7).iter().all(|cell| is_reversed(cell)));
    assert!(diff_row(&buffer, 8).iter().all(|cell| is_reversed(cell)));
}

#[test]
fn a_file_with_no_hunks_says_so() {
    let mut a = app_with(vec![FileDiff { hunks: Vec::new(), ..sample_file() }]);
    assert!(render(&mut a).contains("No text changes to show"));
}

#[test]
fn a_highlighted_side_by_side_line_has_the_plain_text_and_the_span_colours() {
    let line = spans(&[(Color::Red, "\t"), (Color::Green, "\tlet"), (Color::Blue, " x")]);
    let mut plain = app_with(vec![file_of(&["\t\tlet x"])]);
    press(&mut plain, "v");
    let mut coloured = app_with(vec![file_of(&["\t\tlet x"])]);
    press(&mut coloured, "v");
    highlight(&mut coloured, vec![vec![line]]);

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
    // The pane is 72 cells wide with 70 inside, so each side is 33.
    let straddling = format!("{}界", "a".repeat(32));
    let mut a = app_with(vec![file_of(&["short", &straddling, "short"])]);
    press(&mut a, "v");
    assert_divider_aligned(&render_buffer(&mut a, 120, 12));
}

#[test]
fn without_truecolor_no_cell_holds_an_rgb_colour() {
    let mut a = app_with(vec![file_of(&["code"])]);
    highlight(&mut a, vec![vec![spans(&[(Color::Rgb(200, 100, 50), "code")])]]);
    let is_rgb = |c: Color| matches!(c, Color::Rgb(..));
    assert!(render_buffer(&mut a, 80, 12).content.iter().any(|c| is_rgb(c.fg) || is_rgb(c.bg)));

    a.truecolor = false;
    let buffer = render_buffer(&mut a, 80, 12);
    assert!(!buffer.content.iter().any(|c| is_rgb(c.fg) || is_rgb(c.bg)));
}

#[test]
fn an_emoji_sequence_keeps_the_divider_in_its_column() {
    let mut file = file_of(&["short", &format!("heart {HEART}"), "family 👩\u{200d}👩\u{200d}👧"]);
    // On the left, ahead of the divider.
    for line in &mut file.hunks[0].lines {
        (line.kind, line.old_no, line.new_no) = (LineKind::Removed, line.new_no, None);
    }
    let mut a = app_with(vec![file]);
    press(&mut a, "v");
    assert_divider_aligned(&render_buffer(&mut a, 120, 12));
}

#[test]
fn a_deep_path_shows_its_file_name() {
    let path = "src/components/settings/preferences/notifications/widget.rs";
    let mut a = app_with(vec![FileDiff { path: path.into(), ..sample_file() }]);
    assert!(render(&mut a).contains("…/widget.rs"));
}

#[test]
fn a_long_worktree_name_leaves_room_for_its_badges() {
    let mut a = one_worktree("feature/a-long-branch-name-ending-rework");
    a.handle_msg(Msg::Stale("/r/w".into()));
    a.handle_msg(Msg::Loaded { path: "/r/w".into(), result: Err(GitError::NoMergeBase) });
    a.states[0].review.comments = comments(12, "a");
    let out = render_sized(&mut a, 150, 12);
    assert!(out.contains("rework +0 c12 ● ✗"), "{out}");
}

#[test]
fn the_selected_file_stays_visible_among_thousands_with_its_own_reviewed_mark() {
    let files = (0..5000)
        .map(|i| FileDiff { path: format!("f{i}.rs"), content_id: Some(i.to_string()), ..sample_file() })
        .collect();
    let mut a = app_with(files);
    a.focus = Focus::Files;
    press(&mut a, &"j".repeat(4990));
    press(&mut a, "xj");
    let out = render(&mut a);
    assert!(out.contains("✓M f4990.rs"), "{out}");
    assert!(out.contains(" M f4991.rs"), "{out}");
    assert!(a.is_reviewed(4990) && !a.is_reviewed(4991));

    let buffer = render_buffer(&mut a, 80, 12);
    let selected = reversed_file_rows(&buffer);
    assert_eq!(selected.len(), 1, "{selected:?}");
    assert!(selected[0].1.contains("f4991.rs"), "{selected:?}");
}

#[test]
fn messages_wrap_in_the_narrow_diff_area() {
    // 60 columns leaves 26 cells inside the diff area.
    let mut a = app_with(Vec::new());
    assert!(render_sized(&mut a, 60, 12).contains("branch"));
    let mut a = app_with(vec![FileDiff { binary: true, hunks: Vec::new(), ..sample_file() }]);
    assert!(render_sized(&mut a, 60, 12).contains("not shown"));
    let mut a = app();
    let long = format!("{} the-last-words", "error ".repeat(30));
    a.handle_msg(Msg::Loaded { path: "/r/feat-auth".into(), result: Err(GitError::Failed(long)) });
    assert!(render(&mut a).contains("the-last-words"));
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

#[test]
fn moving_up_inside_the_file_window_does_not_scroll_it() {
    let mut a = app_with(numbered_files(40));
    a.focus = Focus::Files;
    for _ in 0..15 {
        press(&mut a, "j");
        render(&mut a);
    }
    // Nine rows, so f015 on the last row leaves f007 at the top.
    let before = render(&mut a);
    assert_eq!(row_of(&before, "f007.rs"), Some(1), "{before}");
    assert_eq!(row_of(&before, "f015.rs"), Some(9), "{before}");

    press(&mut a, "k");
    let after = render(&mut a);
    assert_eq!(row_of(&after, "f007.rs"), Some(1), "{after}");
    assert_eq!(row_of(&after, "f015.rs"), Some(9), "{after}");
    assert_eq!(a.current().unwrap().file_offset, 7);

    // At the top row, the next step up scrolls by one.
    press(&mut a, &"k".repeat(7));
    render(&mut a);
    press(&mut a, "k");
    let top = render(&mut a);
    assert_eq!(row_of(&top, "f006.rs"), Some(1), "{top}");
    assert_eq!(a.current().unwrap().file_offset, 6);
}

#[test]
fn moving_up_inside_the_worktree_window_does_not_scroll_it() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40))));
    for _ in 0..15 {
        press(&mut a, "j");
        render(&mut a);
    }
    press(&mut a, "k");
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
    for _ in 0..600 {
        a.focus = if next(2) == 0 { Focus::Files } else { Focus::Worktrees };
        let key = ["j", "k", "n", "p"][next(4) as usize];
        press(&mut a, &key.repeat(next(25) as usize));
        // Only the first worktree has files, so keep coming back to it.
        if next(3) == 0 {
            press(&mut a, "1");
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
    press(&mut a, &"j".repeat(39));
    render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 31);

    a.handle_msg(Msg::Loaded {
        path: "/r/feat-auth".into(),
        result: Ok(Loaded { key: key(), diffs: Arc::new(numbered_files(3)), op: None }),
    });
    let out = render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 0);
    assert_eq!(row_of(&out, "f000.rs"), Some(1), "{out}");

    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Ok(worktrees(40))));
    press(&mut a, &"j".repeat(39));
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
    press(&mut a, &"j".repeat(19));
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
    a.handle_msg(Msg::Loaded {
        path: "/r/fix-cache".into(),
        result: Ok(Loaded { key: other, diffs: Arc::new(numbered_files(2)), op: None }),
    });
    a.focus = Focus::Files;
    press(&mut a, &"j".repeat(20));
    render(&mut a);
    press(&mut a, &"k".repeat(3));
    let before = render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 12);

    press(&mut a, "]");
    render(&mut a);
    assert_eq!(a.current().unwrap().file_offset, 0);
    press(&mut a, "[");
    assert_eq!(render(&mut a), before);
    assert_eq!(a.current().unwrap().file_offset, 12);
}

#[test]
fn the_side_panes_scale_with_the_terminal_between_limits() {
    for (width, pane) in [(60, 16), (80, 16), (100, 20), (120, 24), (150, 30), (200, 30)] {
        let out = render_sized(&mut app(), width, 12);
        let top: Vec<char> = out.lines().next().unwrap().trim_matches('"').chars().collect();
        let starts: Vec<usize> = (0..top.len()).filter(|&i| top[i] == '┌').collect();
        assert_eq!(starts, [0, pane, 2 * pane], "{out}");
    }
}

#[test]
fn narrow_terminals_render_with_every_screen_open() {
    let mut picker = app();
    picker.handle_msg(Msg::Branches(Ok(vec!["main".into(), "develop".into()])));
    press(&mut picker, "ma");
    let mut screens = ["", "v", "?", "wf"].map(|keys| {
        let mut a = app();
        press(&mut a, keys);
        a
    });
    for a in screens.iter_mut().chain([&mut picker]) {
        for width in 1..50 {
            for height in 1..14 {
                render_sized(a, width, height);
            }
        }
    }
}

#[test]
fn a_row_keeps_every_badge_when_they_fit_with_a_short_name() {
    let out = render_sized(&mut badged("feature/auth", 1234, 12), 120, 12);
    assert!(out.contains("▸ …uth +1234 c12 ● ! ✗"), "{out}");
}

#[test]
fn a_narrow_row_gives_up_its_counts_before_its_warnings() {
    let out = render_sized(&mut badged("feature/auth", 1234, 12), 80, 12);
    assert!(out.contains("▸ …/auth ● ! ✗"), "{out}");

    let mut a = badged("feature/auth", 1234, 12);
    a.states[0].stale = false;
    a.states[0].op = None;
    a.states[0].error = None;
    let out = render_sized(&mut a, 80, 12);
    assert!(out.contains("…re/auth c12"), "{out}");
}

#[test]
fn warnings_come_first_in_the_status_bar() {
    let mut a = badged("feat-auth", 1, 2);
    a.states[0].error = None;
    a.states[0].outdated = 2;
    let line = status_line(&render(&mut a));
    assert!(line.starts_with(" ! rebase in progress   ● changed   (2 outdated)   base: develop"), "{line}");
}

#[test]
fn a_long_branch_is_elided_in_the_status_bar_and_the_rest_stays() {
    let mut a = badged("feature/a-very-long-branch-name-for-the-authentication-rework", 1, 2);
    let state = &mut a.states[0];
    (state.error, state.op, state.stale) = (None, None, false);
    let line = status_line(&render(&mut a));
    assert!(line.contains("-rework   1 file   2 comments   ? help"), "{line}");
    assert!(line.contains("base: develop   …"), "{line}");
}

#[test]
fn the_status_bar_drops_its_help_hint_whole_when_it_does_not_fit() {
    let mut a = app();
    for width in 40..70 {
        let line = status_line(&render_sized(&mut a, width, 12));
        assert!(line.contains("? help") || !line.contains('?'), "{line}");
    }
    let line = status_line(&render_sized(&mut a, 44, 12));
    assert!(!line.contains('?') && line.ends_with("0 comments"), "{line}");
}

#[test]
fn counts_in_the_status_bar_are_pluralised() {
    let two = FileDiff { path: "src/b.rs".into(), ..sample_file() };
    let cases = [
        (vec![sample_file()], 1, "1 file   1 comment"),
        (vec![sample_file(), two], 2, "2 files   2 comments"),
        (vec![], 0, "0 files   0 comments"),
    ];
    for (files, count, expected) in cases {
        let mut a = app_with(files);
        a.states[0].review.comments = comments(count, "nowhere");
        let line = status_line(&render(&mut a));
        assert!(line.contains(expected), "{line}");
    }
}

#[test]
fn added_and_removed_rows_set_a_light_foreground_on_their_dark_background() {
    let plain = |t: &'static str| spans(&[(Color::Reset, t)]);
    for highlighted in [false, true] {
        for view in [false, true] {
            let mut a = app_with(vec![added_and_removed()]);
            if highlighted {
                highlight(&mut a, vec![vec![plain("%"), plain("Z"), plain("Q")]]);
            }
            if view {
                press(&mut a, "v");
            }
            let buffer = render_buffer(&mut a, 120, 12);
            assert!(is_light(cell_of(&buffer, "Z").fg), "{:?}", cell_of(&buffer, "Z"));
            assert!(is_light(cell_of(&buffer, "Q").fg), "{:?}", cell_of(&buffer, "Q"));
            assert_eq!(cell_of(&buffer, "%").fg, Color::Reset, "context keeps the terminal's colour");
        }
    }
}

#[test]
fn highlighted_spans_keep_their_colours_on_added_and_removed_rows() {
    for view in [false, true] {
        let mut a = app_with(vec![added_and_removed()]);
        let lines = [(Color::Yellow, "%"), (Color::Red, "Z"), (Color::Green, "Q")].map(|part| spans(&[part]));
        highlight(&mut a, vec![lines.to_vec()]);
        if view {
            press(&mut a, "v");
        }
        let buffer = render_buffer(&mut a, 120, 12);
        assert_eq!(cell_of(&buffer, "Z").fg, Color::Red);
        assert_eq!(cell_of(&buffer, "Q").fg, Color::Green);
    }
}

#[test]
fn a_name_of_emoji_sequences_leaves_room_for_its_badges() {
    let mut a = one_worktree(&format!("{}-end", HEART.repeat(30)));
    a.handle_msg(Msg::Stale("/r/w".into()));
    for width in [80, 120, 200] {
        let out = render_sized(&mut a, width, 12);
        assert!(out.contains("-end +0 ●"), "{out}");
    }
}

#[test]
fn a_prompt_of_emoji_sequences_shows_its_end() {
    let mut a = app();
    a.focus = Focus::Diff;
    press(&mut a, "jc");
    a.paste(&"⚠\u{fe0f}".repeat(60));
    a.paste("END");
    let out = render_sized(&mut a, 80, 24);
    assert!(out.contains("END"), "{out}");
}

#[test]
fn finder_rows_keep_the_end_of_names_that_share_a_prefix() {
    let shared = format!("feature/{}", "shared-prefix-".repeat(8));
    let [alpha, beta] = [format!("{shared}alpha"), format!("{shared}beta")];
    let mut a = App::new("develop".into());
    let list =
        [&alpha, &beta].map(|n| Worktree { path: format!("/r/{n}").into(), head: "h".into(), branch: Some(n.clone()) });
    a.handle_msg(Msg::Worktrees(Ok(list.to_vec())));
    press(&mut a, "wfeature");
    let out = render_sized(&mut a, 80, 24);
    assert!(out.contains("│…") && out.contains("shared-prefix-alpha"), "{out}");
    assert!(out.contains("shared-prefix-beta"), "{out}");
}

#[test]
fn base_picker_rows_keep_the_end_of_names_that_share_a_prefix() {
    let shared = format!("release/{}", "shared-prefix-".repeat(8));
    let mut a = app();
    a.handle_msg(Msg::Branches(Ok(vec![format!("{shared}alpha"), format!("{shared}beta")])));
    let out = render_sized(&mut a, 80, 24);
    assert!(out.contains("shared-prefix-alpha") && out.contains("shared-prefix-beta"), "{out}");
}

#[test]
fn a_long_base_branch_leaves_the_worktree_name_and_counts_in_the_status_bar() {
    let mut a = app();
    a.base = "release/a-rather-long-base-branch-name-for-testing/1.0".into();
    let line = status_line(&render(&mut a));
    assert!(line.contains("base: …"), "{line}");
    assert!(line.contains("/1.0"), "{line}");
    assert!(line.contains("   feat-auth   1 file   0 comments"), "{line}");
}

#[test]
fn a_path_with_control_characters_stays_on_one_row() {
    let path = |p: &str| FileDiff { path: p.into(), ..sample_file() };
    let mut a = app_with(vec![path("a.rs"), path("b\nc\td.rs"), path("e.rs")]);
    a.focus = Focus::Files;
    press(&mut a, &"j".repeat(2));
    let out = render(&mut a);
    assert_eq!(row_of(&out, "a.rs"), Some(1), "{out}");
    assert_eq!(row_of(&out, "b c d.rs"), Some(2), "{out}");
    assert_eq!(row_of(&out, "e.rs"), Some(3), "{out}");
    let selected = reversed_file_rows(&render_buffer(&mut a, 80, 12));
    assert_eq!(selected.len(), 1, "{selected:?}");
    assert_eq!(selected[0].0, 3, "{selected:?}");
}

#[test]
fn the_diff_cursor_stays_on_screen_when_moved_past_the_window() {
    let mut a = app_with(vec![long_file(40)]);
    a.focus = Focus::Diff;
    press(&mut a, &"j".repeat(30));
    let buffer = render_buffer(&mut a, 80, 12);
    assert_eq!(a.current().unwrap().scroll, 22);
    let row = diff_row(&buffer, 9);
    let text: String = row.iter().map(|c| c.symbol()).collect();
    assert!(text.contains("line30"), "{text}");
    assert!(is_reversed(row[0]), "{text}");
}

#[test]
fn the_highlight_is_on_the_selected_file_in_a_scrolled_list() {
    let mut a = app_with(numbered_files(40));
    a.focus = Focus::Files;
    press(&mut a, &"j".repeat(20));
    render(&mut a);
    press(&mut a, &"k".repeat(4));
    let buffer = render_buffer(&mut a, 80, 12);
    let selected = reversed_file_rows(&buffer);
    assert_eq!(selected.len(), 1, "{selected:?}");
    assert_eq!(selected[0].0, 5, "{selected:?}");
    assert!(selected[0].1.contains("f016.rs"), "{selected:?}");
}

#[test]
fn a_tab_in_a_highlighted_unified_span_is_expanded() {
    let mut a = app_with(vec![file_of(&["\tbody"])]);
    highlight(&mut a, vec![vec![spans(&[(Color::Red, "\t"), (Color::Green, "body")])]]);
    assert!(render(&mut a).contains("+    body"));
}

#[test]
fn the_help_hint_is_whole_or_absent_beside_a_notice_or_a_comment() {
    let mut noticed = app();
    noticed.notice = Some("Something happened".into());
    let mut commented = app();
    commented.focus = Focus::Diff;
    press(&mut commented, "jcwhy");
    commented.handle_key(KeyEvent::from(KeyCode::Enter));
    assert!(render_sized(&mut commented, 80, 12).contains(" * why   ? help"));
    for a in [&mut noticed, &mut commented] {
        for width in 3..60 {
            let line = status_line(&render_sized(a, width, 12));
            assert!(line.contains("? help") || !line.contains('?'), "{line}");
        }
    }
}

#[test]
fn the_status_bar_shortens_a_name_only_when_it_has_to() {
    let mut a = app();
    let name = "feat-auth";
    for width in 40..70 {
        let line = status_line(&render_sized(&mut a, width, 12));
        // 48 cells hold the bar without the hint, and 57 hold it with the hint.
        let whole = line.contains(name);
        let hint = line.ends_with("? help");
        match width {
            ..48 => assert!(!whole && !hint, "{width}: {line}"),
            48..52 => assert!(whole && !hint, "{width}: {line}"),
            52..57 => assert!(hint, "{width}: {line}"),
            _ => assert!(whole && hint, "{width}: {line}"),
        }
    }
}

#[test]
fn the_diff_refills_when_the_terminal_grows() {
    // A header and 40 lines make 41 rows.
    let mut a = app_with(vec![long_file(40)]);
    a.focus = Focus::Diff;
    press(&mut a, &"j".repeat(40));
    render(&mut a);
    assert_eq!(a.current().unwrap().scroll, 32);

    // 30 rows leave 27 for the diff.
    let out = render_sized(&mut a, 80, 30);
    assert_eq!(a.current().unwrap().scroll, 14);
    assert_eq!(row_of(&out, "line40"), Some(27), "{out}");
}

#[test]
fn a_shrunk_diff_keeps_the_cursor_on_screen() {
    let mut a = app_with(vec![long_file(40)]);
    a.focus = Focus::Diff;
    press(&mut a, &"j".repeat(40));
    render(&mut a);
    a.handle_msg(Msg::Loaded {
        path: "/r/feat-auth".into(),
        result: Ok(Loaded { key: key(), diffs: Arc::new(vec![long_file(3)]), op: None }),
    });
    let out = render(&mut a);
    assert_eq!(a.current().unwrap().scroll, 0);
    assert_eq!(row_of(&out, "line1"), Some(2), "{out}");
}

#[test]
fn tint_and_cursor_reach_the_pane_edge_in_unified_view() {
    let emoji = format!("heart {HEART} family 👩\u{200d}👩\u{200d}👧");
    for text in ["plain", "日本語の行", &emoji, "\ttabbed\r"] {
        let mut file = file_of(&[text, text]);
        file.hunks[0].lines[1].kind = LineKind::Removed;
        let mut a = app_with(vec![file]);
        a.focus = Focus::Diff;
        press(&mut a, "j");
        let buffer = render_buffer(&mut a, 80, 12);
        let cursor = diff_row(&buffer, 2);
        assert!(cursor.iter().all(|c| c.bg == Color::Rgb(16, 48, 24)), "{text}");
        assert!(cursor.iter().all(|c| is_reversed(c)), "{text}");
        let removed = diff_row(&buffer, 3);
        assert!(removed.iter().all(|c| c.bg == Color::Rgb(56, 16, 16)), "{text}");
    }
}

#[test]
fn the_cursor_reaches_the_pane_edge_on_context_and_header_rows() {
    let mut a = app();
    a.focus = Focus::Diff;
    for view in [false, true] {
        if view {
            press(&mut a, "v");
        }
        let buffer = render_buffer(&mut a, 81, 12);
        assert!(diff_row(&buffer, 1).iter().all(|c| is_reversed(c)), "header, split {view}");
        press(&mut a, "j");
        let buffer = render_buffer(&mut a, 81, 12);
        assert!(diff_row(&buffer, 2).iter().all(|c| is_reversed(c)), "context, split {view}");
        press(&mut a, "k");
    }
}

/// A full-screen file list with a reviewed file, a rename, a binary file and
/// a path too long for the screen.
fn full_screen_file_list() -> App {
    let deep = format!("{}/deep.rs", ["very_long_folder_name"; 5].join("/"));
    let files = vec![
        FileDiff {
            path: "src/app/tests.rs".into(),
            content_id: Some("1".into()),
            added: 17,
            removed: 0,
            ..sample_file()
        },
        FileDiff { path: "src/ui.rs".into(), added: 333, removed: 33, ..sample_file() },
        FileDiff {
            path: "src/new_name.rs".into(),
            old_path: Some("src/old_name.rs".into()),
            status: Status::Renamed,
            added: 2,
            removed: 1,
            ..sample_file()
        },
        FileDiff {
            path: "assets/logo.png".into(),
            status: Status::Added,
            binary: true,
            hunks: vec![],
            ..sample_file()
        },
        FileDiff { path: deep, ..sample_file() },
    ];
    let mut a = app_with(files);
    a.focus = Focus::Files;
    press(&mut a, "xf");
    a
}

#[test]
fn full_screen_files() {
    insta::assert_snapshot!(render(&mut full_screen_file_list()));
}

#[test]
fn full_screen_files_on_a_narrow_terminal() {
    insta::assert_snapshot!(render_sized(&mut full_screen_file_list(), 40, 12));
}

#[test]
fn full_screen_files_dim_folders_but_not_file_names() {
    let buffer = render_buffer(&mut full_screen_file_list(), 80, 12);
    let (x, y) = position_of(&buffer, "src/ui.rs");
    assert!(buffer[(x, y)].modifier.contains(Modifier::DIM), "the folder is dimmed");
    assert!(!buffer[(x + 4, y)].modifier.contains(Modifier::DIM), "the file name is not");
}

#[test]
fn the_narrow_file_list_is_not_dimmed() {
    let mut a = full_screen_file_list();
    press(&mut a, "f");
    let buffer = render_buffer(&mut a, 80, 12);
    let (x, y) = position_of(&buffer, "src/ui.rs");
    assert!(!buffer[(x, y)].modifier.contains(Modifier::DIM));
}

#[test]
fn full_screen_worktrees() {
    let mut a = app();
    a.states[0].review.comments = comments(2, "src/a.rs");
    press(&mut a, "f");
    insta::assert_snapshot!(render(&mut a));
}

#[test]
fn full_screen_worktrees_dim_the_path() {
    let mut a = app();
    press(&mut a, "f");
    let buffer = render_buffer(&mut a, 80, 12);
    let (x, y) = position_of(&buffer, "/r/fix-cache");
    assert!(buffer[(x, y)].modifier.contains(Modifier::DIM));
}

#[test]
fn full_screen_worktrees_drop_the_path_when_it_has_no_room() {
    let mut a = app();
    press(&mut a, "f");
    let out = render_sized(&mut a, 30, 8);
    assert!(!out.contains("/r/"), "{out}");
    assert!(out.contains("feat-auth +1"), "falls back to the narrow row: {out}");
}
