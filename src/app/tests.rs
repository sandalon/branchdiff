use super::*;
use crate::cache::CacheKey;
use crate::diff::{FileDiff, Hunk, Line, LineKind, Status};
use crate::git::Worktree;
use crate::worker::{Loaded, Msg};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;

fn wt(name: &str) -> Worktree {
    Worktree { path: format!("/r/{name}").into(), head: "h".into(), branch: Some(name.into()) }
}

fn file(path: &str) -> FileDiff {
    FileDiff { path: path.into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 0, hunks: vec![] }
}

/// A file showing `rows` rows: one hunk header and `rows - 1` lines.
fn file_with_rows(path: &str, rows: usize) -> FileDiff {
    let line = Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: "x".into() };
    FileDiff { hunks: vec![Hunk { header: "@@".into(), lines: vec![line; rows - 1] }], ..file(path) }
}

fn app(names: &[&str]) -> App {
    let mut a = App::new("develop".into());
    a.handle_msg(list(names));
    a
}

fn press(a: &mut App, c: char) -> Vec<Effect> {
    a.handle_key(KeyEvent::from(KeyCode::Char(c)))
}

fn type_text(a: &mut App, text: &str) {
    for c in text.chars() {
        press(a, c);
    }
}

/// Row 0 is the hunk header, so row 1 is the line.
fn focus_first_line(a: &mut App) {
    a.focus = Focus::Diff;
    press(a, 'j');
}

fn deliver(a: &mut App, name: &str, files: Vec<FileDiff>) {
    let path: std::path::PathBuf = format!("/r/{name}").into();
    let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 1 };
    a.handle_msg(Msg::Loaded { path, result: Ok(Loaded { key, diffs: Arc::new(files), op: None }) });
}

fn list(names: &[&str]) -> Msg {
    Msg::Worktrees(Ok(names.iter().map(|n| wt(n)).collect()))
}

fn set_cursor_scroll(a: &mut App, cursor: usize, scroll: usize) {
    let s = a.current_mut().unwrap();
    (s.cursor, s.scroll) = (cursor, scroll);
}

fn position(a: &App) -> (usize, usize, usize) {
    let s = a.current().unwrap();
    (s.file, s.cursor, s.scroll)
}

fn with(c: char, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), modifiers)
}

/// Three worktrees whose loaded diffs and positions all differ.
fn prepared() -> App {
    let names = ["a", "b", "c"];
    let mut a = app(&names);
    for (i, name) in names.iter().enumerate() {
        let files = (1..=3).map(|n| file_with_rows(&format!("{name}-{n}"), 100)).collect();
        deliver(&mut a, name, files);
        let s = &mut a.states[i];
        (s.file, s.cursor, s.scroll) = (i, 20 + i, 10 + i);
    }
    a
}

/// Checks one worktree from `prepared` still has everything it started with.
fn assert_kept(a: &App, name: &str) {
    let i = ["a", "b", "c"].iter().position(|n| *n == name).unwrap();
    let row = a.worktrees.iter().position(|w| w.branch.as_deref() == Some(name)).unwrap();
    let s = &a.states[row];
    assert_eq!((s.file, s.cursor, s.scroll), (i, 20 + i, 10 + i), "position of {name}");
    assert_eq!(s.diffs.as_ref().unwrap()[0].path, format!("{name}-1"));
    assert!(s.key.as_ref().unwrap().worktree.ends_with(name));
}

fn file_with_line(text: &str) -> FileDiff {
    FileDiff {
        path: "a.rs".into(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 1, removed: 0,
        hunks: vec![Hunk {
            header: "@@ -0,0 +1 @@".into(),
            lines: vec![Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: text.into() }],
        }],
    }
}

fn app_on_line() -> App {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_line("let x = 1;")]);
    focus_first_line(&mut a);
    a
}

fn type_comment(a: &mut App, text: &str) -> Vec<Effect> {
    press(a, 'c');
    type_text(a, text);
    a.handle_key(KeyEvent::from(KeyCode::Enter))
}

fn numbered(kind: LineKind, old_no: Option<u32>, new_no: Option<u32>) -> Line {
    Line { kind, old_no, new_no, text: "x".into() }
}

fn context(old: u32, new: u32) -> Line {
    numbered(LineKind::Context, Some(old), Some(new))
}

fn removed(old: u32) -> Line {
    numbered(LineKind::Removed, Some(old), None)
}

fn hunk(header: &str, lines: Vec<Line>) -> Hunk {
    Hunk { header: header.into(), lines }
}

/// The line `e` opens with the cursor on `row`, or `None` if it opens nothing.
fn editor_line(f: FileDiff, row: usize) -> Option<u32> {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![f]);
    set_cursor_scroll(&mut a, row, 0);
    match press(&mut a, 'e').as_slice() {
        [Effect::OpenEditor { line, .. }] => Some(*line),
        _ => None,
    }
}

fn comment_prompt_with(typed: &str) -> App {
    let mut a = app_on_line();
    press(&mut a, 'c');
    type_text(&mut a, typed);
    a
}

fn pasted(text: &str) -> String {
    let mut a = comment_prompt_with("");
    a.paste(text);
    match &a.input {
        Some(Input::Comment { text, .. }) => text.clone(),
        _ => panic!("the prompt closed"),
    }
}

fn note(path: &str, line: u32, side: Side, line_text: &str) -> Comment {
    Comment { path: path.into(), line, side, line_text: line_text.into(), body: "note".into() }
}

/// Opens the finder on the last of three worktrees and enters `query`.
fn submit_finder(query: &str) -> (App, Vec<Effect>) {
    let mut a = app(&["a", "b", "c"]);
    press(&mut a, '3');
    press(&mut a, 'w');
    type_text(&mut a, query);
    let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
    (a, fx)
}

/// Opens the base picker and enters `query`.
fn submit_base(query: &str) -> (App, Vec<Effect>) {
    let mut a = app(&["a", "b"]);
    press(&mut a, 'b');
    a.handle_msg(Msg::Branches(Ok(vec!["alpha".into(), "main".into()])));
    type_text(&mut a, query);
    let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
    (a, fx)
}

fn base_missing(a: &mut App, name: &str) -> Vec<Effect> {
    let path = format!("/r/{name}").into();
    a.handle_msg(Msg::Loaded { path, result: Err(crate::git::GitError::BaseMissing("develop".into())) })
}

fn query(a: &App) -> Option<&str> {
    match &a.input {
        Some(Input::Worktree { query } | Input::Base { query, .. }) => Some(query),
        _ => None,
    }
}

#[test]
fn worktree_list_requests_a_load_for_each_worktree() {
    let mut a = App::new("develop".into());
    let fx = a.handle_msg(list(&["a", "b"]));
    assert_eq!(fx.iter().filter(|e| matches!(e, Effect::Load { .. })).count(), 2);
}

#[test]
fn brackets_switch_worktrees_and_wrap() {
    let mut a = app(&["a", "b", "c"]);
    press(&mut a, ']');
    assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("b"));
    press(&mut a, '[');
    press(&mut a, '[');
    assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("c"));
}

#[test]
fn switching_works_from_any_focus() {
    let mut a = app(&["a", "b"]);
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    assert_eq!(a.focus, Focus::Diff);
    press(&mut a, ']');
    assert_eq!(a.selected, 1);
}

#[test]
fn number_keys_jump_and_ignore_out_of_range() {
    let mut a = app(&["a", "b"]);
    press(&mut a, '2');
    assert_eq!(a.selected, 1);
    press(&mut a, '9');
    assert_eq!(a.selected, 1);
}

#[test]
fn each_worktree_remembers_its_position() {
    let mut a = app(&["a", "b"]);
    let files = || vec![file_with_rows("1", 100), file_with_rows("2", 100), file_with_rows("3", 100)];
    deliver(&mut a, "a", files());
    deliver(&mut a, "b", files());
    press(&mut a, 'n');
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    for (away, back) in [(']', '['), ('2', '1'), ('j', 'k')] {
        press(&mut a, away);
        assert_eq!(a.selected, 1, "{away}/{back}");
        assert_eq!(position(&a), (0, 0, 0), "{away}/{back}");
        press(&mut a, 'n');
        set_cursor_scroll(&mut a, 7, 3);
        press(&mut a, back);
        assert_eq!(a.selected, 0, "{away}/{back}");
        assert_eq!(position(&a), (2, 50, 40), "{away}/{back}");
        press(&mut a, away);
        assert_eq!(position(&a), (1, 7, 3), "{away}/{back}");
        press(&mut a, 'p');
        set_cursor_scroll(&mut a, 0, 0);
        press(&mut a, back);
    }
}

#[test]
fn states_stay_with_their_worktrees_when_git_reorders_the_list() {
    for order in [["c", "b", "a"], ["b", "c", "a"], ["c", "a", "b"]] {
        for selected in 0..3 {
            let mut a = prepared();
            a.selected = selected;
            let name = a.selected_worktree().unwrap().branch.clone();
            a.handle_msg(list(&order));
            assert_eq!(a.selected_worktree().unwrap().branch, name);
            for name in ["a", "b", "c"] {
                assert_kept(&a, name);
            }
        }
    }
}

#[test]
fn states_stay_with_their_worktrees_when_one_appears_between_them() {
    for selected in 0..3 {
        let mut a = prepared();
        a.selected = selected;
        let name = a.selected_worktree().unwrap().branch.clone();
        a.handle_msg(list(&["a", "new", "b", "c"]));
        assert_eq!(a.selected_worktree().unwrap().branch, name);
        for name in ["a", "b", "c"] {
            assert_kept(&a, name);
        }
        let s = &a.states[1];
        assert!(s.diffs.is_none() && s.key.is_none());
        assert_eq!((s.file, s.cursor, s.scroll), (0, 0, 0));
    }
}

#[test]
fn a_list_refresh_loads_only_new_worktrees() {
    let mut a = prepared();
    assert!(a.handle_msg(list(&["a", "b", "c"])).is_empty());
    assert!(a.handle_msg(list(&["c", "a", "b"])).is_empty());
    for name in ["a", "b", "c"] {
        assert_kept(&a, name);
    }
    let fx = a.handle_msg(list(&["a", "new", "b", "c"]));
    assert_eq!(fx, vec![Effect::Load { path: "/r/new".into(), base: "develop".into() }]);
}

#[test]
fn a_load_clears_stale_and_error_and_records_the_operation() {
    let mut a = app(&["a"]);
    let s = a.current_mut().unwrap();
    s.stale = true;
    s.error = Some(crate::git::GitError::NoMergeBase);
    let path: std::path::PathBuf = "/r/a".into();
    let key = CacheKey { worktree: path.clone(), base_commit: "b".into(), head: "h".into(), fingerprint: 2 };
    let loaded = Loaded { key: key.clone(), diffs: Arc::new(vec![file("1")]), op: Some("rebase") };
    a.handle_msg(Msg::Loaded { path, result: Ok(loaded) });
    let s = a.current().unwrap();
    assert!(!s.stale && s.error.is_none());
    assert_eq!(s.op, Some("rebase"));
    assert_eq!(s.key, Some(key));
    deliver(&mut a, "a", vec![file("1")]);
    assert_eq!(a.current().unwrap().op, None);
}

#[test]
fn nothing_is_highlighted_for_an_empty_diff() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![]);
    assert_eq!(a.highlight_effect(), None);
}

#[test]
fn a_reload_keeps_colours_only_while_the_key_is_unchanged() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file("1")]);
    assert!(a.highlight_effect().is_some());
    let key = a.current().unwrap().key.clone().unwrap();
    a.handle_msg(Msg::Highlighted { key: key.clone(), file_index: 0, lines: Default::default() });
    assert_eq!(a.highlight_effect(), None);

    deliver(&mut a, "a", vec![file("1")]);
    assert!(a.current().unwrap().highlights.contains_key(&0));

    let changed = CacheKey { fingerprint: 2, ..key };
    let loaded = Loaded { key: changed, diffs: Arc::new(vec![file("1")]), op: None };
    a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Ok(loaded) });
    assert!(a.current().unwrap().highlights.is_empty());
    assert!(a.highlight_effect().is_some());
}

#[test]
fn a_highlight_for_a_stale_key_changes_nothing() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file("1")]);
    deliver(&mut a, "b", vec![file("1")]);
    let stale = a.states[0].key.clone().unwrap();
    let current = CacheKey { fingerprint: 2, ..stale.clone() };
    let loaded = Loaded { key: current, diffs: Arc::new(vec![file("1")]), op: None };
    a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Ok(loaded) });

    a.handle_msg(Msg::Highlighted { key: stale, file_index: 0, lines: Default::default() });
    assert!(a.states.iter().all(|s| s.highlights.is_empty()));
}

#[test]
fn a_highlight_reaches_its_own_worktree_when_another_is_selected() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file("1")]);
    deliver(&mut a, "b", vec![file("1")]);
    assert_eq!(a.selected, 0);
    let key = a.states[1].key.clone().unwrap();

    a.handle_msg(Msg::Highlighted { key, file_index: 0, lines: Default::default() });
    assert!(a.states[0].highlights.is_empty());
    assert!(a.states[1].highlights.contains_key(&0));
}

#[test]
fn a_failed_load_keeps_the_diff_and_position() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 100), file_with_rows("2", 100)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    let key = a.current().unwrap().key.clone();
    a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::NoMergeBase) });
    let s = a.current().unwrap();
    assert_eq!(s.error, Some(crate::git::GitError::NoMergeBase));
    assert_eq!(s.diffs.as_ref().unwrap().len(), 2);
    assert!(key.is_some() && s.key == key);
    assert_eq!(position(&a), (1, 50, 40));
}

#[test]
fn stale_message_badges_but_keeps_the_diff_and_position() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 100), file_with_rows("2", 100)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    let fx = a.handle_msg(Msg::Stale("/r/a".into()));
    assert!(fx.is_empty());
    assert!(a.current().unwrap().stale);
    assert_eq!(a.current().unwrap().diffs.as_ref().unwrap().len(), 2);
    assert_eq!(position(&a), (1, 50, 40));
}

#[test]
fn r_loads_the_selected_worktree_with_the_current_base() {
    let mut a = app(&["a", "b"]);
    press(&mut a, ']');
    a.base = "main".into();
    let load = Effect::Load { path: "/r/b".into(), base: "main".into() };
    assert_eq!(press(&mut a, 'r'), vec![Effect::ReloadWorktrees, load]);
}

#[test]
fn moving_to_another_file_resets_the_position() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 100), file_with_rows("2", 100)]);
    set_cursor_scroll(&mut a, 50, 40);
    press(&mut a, 'n');
    assert_eq!(position(&a), (1, 0, 0));
    set_cursor_scroll(&mut a, 50, 40);
    press(&mut a, 'p');
    assert_eq!(position(&a), (0, 0, 0));
}

#[test]
fn switching_works_while_help_is_open() {
    let mut a = app(&["a", "b"]);
    press(&mut a, '?');
    press(&mut a, ']');
    assert_eq!(a.selected, 1);
    press(&mut a, '[');
    assert_eq!(a.selected, 0);
    press(&mut a, '2');
    assert_eq!(a.selected, 1);
    press(&mut a, '9');
    assert_eq!(a.selected, 1);
    assert!(a.help_open);
    a.handle_key(KeyEvent::from(KeyCode::Esc));
    assert!(!a.help_open);
}

#[test]
fn other_keys_do_nothing_while_help_is_open() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 100), file_with_rows("2", 100)]);
    press(&mut a, '?');
    for c in ['j', 'k', 'n', 'p', 'v', 'r', 'q'] {
        assert!(press(&mut a, c).is_empty());
    }
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    assert_eq!(a.selected, 0);
    assert_eq!(position(&a), (0, 0, 0));
    assert_eq!((a.focus, a.view), (Focus::Worktrees, DiffView::Unified));
    assert!(a.help_open && !a.should_quit);
}

#[test]
fn removed_selected_worktree_shows_notice_and_moves_on() {
    let mut a = app(&["a", "b", "c"]);
    press(&mut a, ']');
    a.handle_msg(list(&["a", "c"]));
    assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("c"));
    assert!(a.notice.as_deref().unwrap().contains('b'));
}

#[test]
fn a_later_worktree_list_clears_a_failed_one() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Err(crate::git::GitError::Failed("boom".into()))));
    assert!(a.fatal.is_some());
    a.handle_msg(list(&["a"]));
    assert!(a.fatal.is_none());
}

#[test]
fn r_retries_the_worktree_list_when_none_are_listed() {
    let mut a = App::new("develop".into());
    a.handle_msg(Msg::Worktrees(Err(crate::git::GitError::Failed("boom".into()))));
    assert_eq!(press(&mut a, 'r'), vec![Effect::ReloadWorktrees]);
}

#[test]
fn moving_past_either_end_of_the_file_list_keeps_the_diff_position() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 10), file_with_rows("2", 10)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 5, 0);
    press(&mut a, 'n');
    assert_eq!(a.current().unwrap().cursor, 5);
    press(&mut a, 'p');
    assert_eq!(a.current().unwrap().cursor, 0);
    set_cursor_scroll(&mut a, 5, 0);
    press(&mut a, 'p');
    assert_eq!(a.current().unwrap().cursor, 5);
}

#[test]
fn a_reload_keeps_the_position_inside_a_shorter_file() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 100)]);
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("1", 45)]);
    let s = a.current().unwrap();
    assert_eq!((s.cursor, s.scroll), (44, 40));
    deliver(&mut a, "a", vec![file_with_rows("1", 5)]);
    let s = a.current().unwrap();
    assert_eq!((s.cursor, s.scroll), (4, 4));
}

#[test]
fn a_reload_with_fewer_files_or_none_keeps_the_position_valid() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("1", 3), file_with_rows("2", 100)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("1", 3)]);
    let s = a.current().unwrap();
    assert_eq!((s.file, s.cursor, s.scroll), (0, 0, 0));
    deliver(&mut a, "a", vec![]);
    let s = a.current().unwrap();
    assert_eq!((s.file, s.cursor, s.scroll), (0, 0, 0));
}

#[test]
fn a_reload_follows_the_current_file_past_an_inserted_file() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("b", 100), file_with_rows("c", 100)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("a", 5), file_with_rows("b", 100), file_with_rows("c", 100)]);
    assert_eq!(position(&a), (2, 50, 40));
    assert_eq!(a.current_file().unwrap().path, "c");
}

#[test]
fn a_reload_follows_the_current_file_past_a_removed_file() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("a", 5), file_with_rows("b", 100), file_with_rows("c", 100), file_with_rows("d", 100)]);
    press(&mut a, 'n');
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("b", 100), file_with_rows("c", 100), file_with_rows("d", 100)]);
    assert_eq!(position(&a), (1, 50, 40));
    assert_eq!(a.current_file().unwrap().path, "c");
}

#[test]
fn a_followed_file_that_shrank_keeps_its_position_valid() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("b", 100)]);
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("b", 5)]);
    assert_eq!(position(&a), (1, 4, 4));
}

#[test]
fn a_reload_without_the_current_file_keeps_the_index_and_resets_the_position() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("b", 100), file_with_rows("c", 100)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("c", 100)]);
    assert_eq!(position(&a), (1, 0, 0));
    set_cursor_scroll(&mut a, 50, 40);
    deliver(&mut a, "a", vec![file_with_rows("a", 100)]);
    assert_eq!(position(&a), (0, 0, 0));
}

#[test]
fn a_typechange_follows_the_entry_with_the_same_status() {
    let side = |status| FileDiff { status, ..file_with_rows("t", 10) };
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![side(Status::Deleted), side(Status::Added)]);
    press(&mut a, 'n');
    deliver(&mut a, "a", vec![file_with_rows("a", 5), side(Status::Deleted), side(Status::Added)]);
    assert_eq!(a.current().unwrap().file, 2);
}

#[test]
fn a_file_that_changed_status_falls_back_to_the_first_path_match() {
    let side = |status| FileDiff { status, ..file_with_rows("t", 10) };
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_rows("a", 5), file_with_rows("t", 10)]);
    press(&mut a, 'n');
    set_cursor_scroll(&mut a, 5, 2);
    deliver(&mut a, "a", vec![file_with_rows("0", 5), file_with_rows("a", 5), side(Status::Deleted), side(Status::Added)]);
    assert_eq!(position(&a), (2, 5, 2));
}

#[test]
fn ctrl_c_quits() {
    let mut a = app(&["a"]);
    assert!(a.handle_key(with('c', KeyModifiers::CONTROL)).is_empty());
    assert!(a.should_quit);
}

#[test]
fn ctrl_c_quits_while_help_is_open() {
    let mut a = app(&["a"]);
    press(&mut a, '?');
    a.handle_key(with('c', KeyModifiers::CONTROL));
    assert!(a.should_quit);
}

#[test]
fn ctrl_and_alt_keys_change_nothing() {
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        let mut a = app(&["a", "b"]);
        a.notice = Some("kept".into());
        for c in ['j', ']', '2', 'q', 'r'] {
            assert!(a.handle_key(with(c, modifiers)).is_empty());
        }
        assert_eq!(a.selected, 0);
        assert!(!a.should_quit);
        assert_eq!(a.notice.as_deref(), Some("kept"));
    }
}

#[test]
fn altgr_counts_as_text_only_on_windows() {
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    assert!(!is_shortcut(altgr, true));
    assert!(!is_shortcut(altgr | KeyModifiers::SHIFT, true));
    assert!(is_shortcut(altgr, false));
    for windows in [true, false] {
        assert!(is_shortcut(KeyModifiers::CONTROL, windows));
        assert!(is_shortcut(KeyModifiers::ALT, windows));
        assert!(!is_shortcut(KeyModifiers::SHIFT, windows));
        assert!(!is_shortcut(KeyModifiers::NONE, windows));
    }
}

#[test]
fn handle_key_applies_the_altgr_rule_for_this_platform() {
    let mut a = app(&["a", "b"]);
    a.handle_key(with(']', KeyModifiers::CONTROL | KeyModifiers::ALT));
    assert_eq!(a.selected, usize::from(cfg!(windows)));
}

#[test]
fn ctrl_c_needs_control_alone() {
    let mut a = app(&["a"]);
    a.handle_key(with('c', KeyModifiers::CONTROL | KeyModifiers::ALT));
    assert!(!a.should_quit);
}

#[test]
fn shift_still_works() {
    let mut a = app(&["a"]);
    a.handle_key(with('?', KeyModifiers::SHIFT));
    assert!(a.help_open);
}

#[test]
fn j_and_k_stop_at_the_ends_of_the_worktree_list() {
    let mut a = app(&["a", "b"]);
    press(&mut a, 'k');
    assert_eq!(a.selected, 0);
    press(&mut a, 'j');
    press(&mut a, 'j');
    assert_eq!(a.selected, 1);
    a.handle_key(KeyEvent::from(KeyCode::Down));
    assert_eq!(a.selected, 1);
    press(&mut a, ']');
    assert_eq!(a.selected, 0);
}

#[test]
fn moving_keeps_a_notice_and_other_keys_clear_it() {
    let mut a = app(&["a"]);
    a.notice = Some("kept".into());
    press(&mut a, 'j');
    press(&mut a, 'k');
    a.handle_key(KeyEvent::from(KeyCode::Down));
    a.handle_key(KeyEvent::from(KeyCode::Up));
    assert_eq!(a.notice.as_deref(), Some("kept"));
    press(&mut a, 'v');
    assert!(a.notice.is_none());
}

#[test]
fn a_reload_of_an_unselected_worktree_keeps_its_position_valid() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "b", vec![file_with_rows("1", 100)]);
    press(&mut a, ']');
    set_cursor_scroll(&mut a, 50, 0);
    press(&mut a, '[');
    deliver(&mut a, "b", vec![file_with_rows("1", 5)]);
    assert_eq!(a.states[1].cursor, 4);
}

#[test]
fn q_quits() {
    let mut a = app(&["a"]);
    press(&mut a, 'q');
    assert!(a.should_quit);
}

#[test]
fn question_mark_toggles_help_and_esc_closes_it() {
    let mut a = app(&["a"]);
    press(&mut a, '?');
    assert!(a.help_open);
    press(&mut a, '?');
    assert!(!a.help_open);
    press(&mut a, '?');
    a.handle_key(KeyEvent::from(KeyCode::Esc));
    assert!(!a.help_open);
}

#[test]
fn keys_do_nothing_with_no_worktrees_loaded() {
    let mut a = App::new("develop".into());
    for c in [']', '[', 'n', 'p', 'j', 'k', 'c', 'e', 'x', 'y'] {
        assert!(press(&mut a, c).is_empty());
    }
    assert_eq!(a.selected, 0);
    assert!(a.worktrees.is_empty() && a.states.is_empty());
    assert_eq!((a.focus, a.view), (Focus::Worktrees, DiffView::Unified));
    assert!(!a.help_open && !a.should_quit);
    assert!(a.notice.is_none() && a.fatal.is_none());
}

#[test]
fn c_then_enter_saves_a_comment_on_the_current_line() {
    let mut a = app_on_line();
    let fx = type_comment(&mut a, "fix this");
    let c = &a.current().unwrap().review.comments[0];
    assert_eq!((c.line, c.line_text.as_str(), c.body.as_str()), (1, "let x = 1;", "fix this"));
    assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
}

#[test]
fn typing_in_a_comment_does_not_trigger_hotkeys() {
    let mut a = app_on_line();
    type_comment(&mut a, "q?]v1");
    assert!(!a.should_quit && !a.help_open);
    assert_eq!(a.selected, 0);
    assert_eq!(a.view, DiffView::Unified);
    assert_eq!(a.current().unwrap().review.comments[0].body, "q?]v1");
}

#[test]
fn esc_cancels_a_comment() {
    let mut a = app_on_line();
    press(&mut a, 'c');
    press(&mut a, 'x');
    a.handle_key(KeyEvent::from(KeyCode::Esc));
    assert!(a.input.is_none());
    assert!(a.current().unwrap().review.comments.is_empty());
}

#[test]
fn empty_comment_is_not_saved() {
    let mut a = app_on_line();
    type_comment(&mut a, "");
    assert!(a.current().unwrap().review.comments.is_empty());
}

#[test]
fn comment_on_a_hunk_header_is_refused() {
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![file_with_line("x")]);
    a.focus = Focus::Diff;
    press(&mut a, 'c');
    assert!(a.input.is_none());
}

#[test]
fn x_marks_reviewed_and_a_changed_file_resets() {
    let mut a = app_on_line();
    press(&mut a, 'x');
    assert!(a.is_reviewed(0));
    deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
    assert!(!a.is_reviewed(0));
}

#[test]
fn one_x_marks_a_file_whose_old_mark_went_stale() {
    let mut a = app_on_line();
    press(&mut a, 'x');
    deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
    assert!(!a.is_reviewed(0));
    press(&mut a, 'x');
    assert!(a.is_reviewed(0));
}

#[test]
fn a_save_records_the_review_it_was_made_for() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file_with_line("x")]);
    let fx = press(&mut a, 'x');
    press(&mut a, ']');
    assert!(matches!(&fx[0], Effect::SaveReview { id: ReviewId::Branch(b), review }
        if b == "a" && review.reviewed.len() == 1));
}

#[test]
fn only_the_last_save_per_review_is_kept() {
    let save = |name: &str, marks: usize| Effect::SaveReview {
        id: ReviewId::Branch(name.into()),
        review: Review { comments: vec![], reviewed: (0..marks).map(|i| (i.to_string(), 0)).collect() },
    };
    let kept = coalesce_saves(vec![save("a", 1), save("b", 1), Effect::ReloadWorktrees, save("a", 2)]);
    assert_eq!(kept, vec![save("b", 1), Effect::ReloadWorktrees, save("a", 2)]);
}

#[test]
fn d_deletes_only_the_comment_on_the_current_line() {
    let mut a = app_on_line();
    type_comment(&mut a, "one");
    type_comment(&mut a, "two");
    a.states[0].review.comments.push(Comment {
        path: "other.rs".into(), line: 1, side: Side::New, line_text: "x".into(), body: "keep".into(),
    });
    let fx = press(&mut a, 'd');
    let bodies: Vec<_> = a.current().unwrap().review.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(bodies, vec!["one", "keep"]);
    assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
}

#[test]
fn d_with_no_comment_here_changes_nothing_and_says_so() {
    let mut a = app_on_line();
    assert!(press(&mut a, 'd').is_empty());
    assert!(a.notice.is_some());
}

#[test]
fn deleting_a_comment_recounts_outdated() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
    assert_eq!(a.outdated_comments(), 1);
    a.states[0].review.comments[0].line_text = "let x = 2;".into();
    press(&mut a, 'd');
    assert_eq!(a.outdated_comments(), 0);
}

#[test]
fn shift_d_clears_after_y_and_saves() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    press(&mut a, 'D');
    assert!(matches!(a.input, Some(Input::ConfirmClear)));
    assert!(a.current().unwrap().review.comments.len() == 1);
    let fx = press(&mut a, 'y');
    assert!(a.current().unwrap().review.comments.is_empty());
    assert!(matches!(fx.as_slice(), [Effect::SaveReview { .. }]));
}

#[test]
fn any_other_key_cancels_a_clear() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    press(&mut a, 'D');
    assert!(press(&mut a, 'q').is_empty());
    assert!(a.input.is_none() && !a.should_quit);
    assert_eq!(a.current().unwrap().review.comments.len(), 1);
}

#[test]
fn shift_d_with_no_comments_says_so() {
    let mut a = app_on_line();
    press(&mut a, 'D');
    assert!(a.input.is_none() && a.notice.is_some());
}

#[test]
fn x_again_clears_the_mark() {
    let mut a = app_on_line();
    press(&mut a, 'x');
    press(&mut a, 'x');
    assert!(!a.is_reviewed(0));
}

#[test]
fn comment_is_outdated_after_its_line_changes() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    assert_eq!(a.outdated_comments(), 0);
    deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
    assert_eq!(a.outdated_comments(), 1);
}

#[test]
fn comment_on_a_replaced_line_is_not_outdated() {
    let mut replaced = file_with_line("TWO");
    replaced.hunks[0].lines = vec![
        Line { kind: LineKind::Removed, old_no: Some(1), new_no: None, text: "two".into() },
        Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: "TWO".into() },
    ];
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![replaced]);
    focus_first_line(&mut a);
    press(&mut a, 'j');
    type_comment(&mut a, "fix");
    assert_eq!(a.current().unwrap().review.comments[0].side, crate::store::Side::New);
    assert_eq!(a.outdated_comments(), 0);
}

#[test]
fn changing_a_binary_file_resets_its_reviewed_mark() {
    let binary = |id: &str| FileDiff {
        path: "b.dat".into(), old_path: None, status: Status::Modified, binary: true,
        content_id: Some(id.into()), added: 0, removed: 0, hunks: vec![],
    };
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![binary("1..2")]);
    press(&mut a, 'x');
    assert!(a.is_reviewed(0));
    deliver(&mut a, "a", vec![binary("1..3")]);
    assert!(!a.is_reviewed(0));
}

#[test]
fn y_copies_and_shift_y_writes_the_export() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    assert!(matches!(press(&mut a, 'y').as_slice(), [Effect::CopyExport(md)] if md.contains("fix")));
    assert!(matches!(press(&mut a, 'Y').as_slice(), [Effect::WriteExport(md)] if md.contains("fix")));
}

#[test]
fn export_with_no_comments_shows_a_notice() {
    let mut a = app_on_line();
    assert!(press(&mut a, 'y').is_empty());
    assert!(a.notice.is_some());
}

#[test]
fn e_opens_the_editor_at_the_current_line() {
    let mut a = app_on_line();
    assert!(matches!(press(&mut a, 'e').as_slice(), [Effect::OpenEditor { line: 1, .. }]));
}

#[test]
fn removing_the_worktree_cancels_an_open_comment() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file_with_line("x")]);
    focus_first_line(&mut a);
    press(&mut a, 'c');
    press(&mut a, 'h');
    a.handle_msg(list(&["b"]));
    assert!(a.input.is_none());
    assert!(a.handle_key(KeyEvent::from(KeyCode::Enter)).is_empty());
    assert_eq!(a.current().unwrap().review, Review::default());
}

#[test]
fn removing_the_worktree_cancels_an_open_clear() {
    let mut a = app(&["a", "b"]);
    let note = Comment { path: "a.rs".into(), line: 1, side: Side::New, line_text: "x".into(), body: "keep".into() };
    a.states[0].review.comments.push(note.clone());
    a.states[1].review.comments.push(note);
    press(&mut a, 'D');
    a.handle_msg(list(&["b"]));
    assert!(a.input.is_none());
    // Back on hotkeys, `y` copies b's comments rather than clearing them.
    let fx = press(&mut a, 'y');
    assert!(!fx.iter().any(|e| matches!(e, Effect::SaveReview { .. })));
    assert_eq!(a.current().unwrap().review.comments.len(), 1);
}

#[test]
fn e_on_a_removed_line_opens_the_following_line() {
    let f = FileDiff { hunks: vec![hunk("@@ -40,3 +40,2 @@", vec![context(40, 40), removed(41), context(42, 41)])], ..file("m.rs") };
    assert_eq!(editor_line(f, 2), Some(41));
}

#[test]
fn e_on_a_hunk_header_opens_the_hunks_first_new_line() {
    let f = FileDiff { hunks: vec![hunk("@@ -40,2 +42,1 @@", vec![removed(40), context(41, 42)])], ..file("m.rs") };
    assert_eq!(editor_line(f, 0), Some(42));
}

#[test]
fn e_on_a_removed_block_ending_a_hunk_opens_the_line_before_it() {
    let f = FileDiff { hunks: vec![hunk("@@ -40,4 +40,2 @@", vec![context(40, 40), context(41, 41), removed(42), removed(43)])], ..file("m.rs") };
    assert_eq!(editor_line(f, 4), Some(41));
}

#[test]
fn e_in_a_hunk_of_only_removals_opens_its_new_side_start() {
    let f = FileDiff {
        hunks: vec![
            hunk("@@ -1 +1 @@", vec![context(1, 1)]),
            hunk("@@ -40,2 +39,0 @@ fn a(b: u8) -> u8 { b + 1 }", vec![removed(40), removed(41)]),
        ],
        ..file("m.rs")
    };
    assert_eq!(editor_line(f.clone(), 2), Some(39));
    assert_eq!(editor_line(f, 3), Some(39));
}

#[test]
fn e_treats_a_new_side_start_of_zero_as_line_one() {
    let f = FileDiff { hunks: vec![hunk("@@ -1,2 +0,0 @@", vec![removed(1), removed(2)])], ..file("m.rs") };
    assert_eq!(editor_line(f, 1), Some(1));
}

#[test]
fn e_on_a_deleted_file_says_so_and_opens_nothing() {
    let mut a = app(&["a"]);
    let f = FileDiff { status: Status::Deleted, hunks: vec![hunk("@@ -1 +0,0 @@", vec![removed(1)])], ..file("gone.rs") };
    deliver(&mut a, "a", vec![f]);
    assert!(press(&mut a, 'e').is_empty());
    assert!(a.notice.is_some());
}

#[test]
fn e_opens_the_deleted_side_of_a_path_that_still_exists() {
    let mut a = app(&["a"]);
    let gone = FileDiff { status: Status::Deleted, ..file("t") };
    let there = FileDiff { status: Status::Added, ..file("t") };
    deliver(&mut a, "a", vec![gone, there]);
    assert!(matches!(press(&mut a, 'e').as_slice(), [Effect::OpenEditor { line: 1, .. }]));
    assert!(a.notice.is_none());
}

#[test]
fn e_still_opens_untracked_and_binary_files() {
    let untracked = FileDiff { status: Status::Untracked, ..file_with_line("x") };
    assert_eq!(editor_line(untracked, 1), Some(1));
    assert_eq!(editor_line(FileDiff { binary: true, ..file("b.dat") }, 0), Some(1));
}

#[test]
fn a_multi_line_paste_stays_in_one_comment_and_runs_no_hotkeys() {
    let mut a = comment_prompt_with("see: ");
    a.paste("q\nDy\r\nx]2");
    assert!(a.input.is_some() && !a.should_quit && !a.help_open);
    assert_eq!(a.selected, 0);
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    let comments = &a.current().unwrap().review.comments;
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "see: q Dy x]2");
    assert!(a.current().unwrap().review.reviewed.is_empty());
}

#[test]
fn a_paste_turns_line_breaks_and_controls_into_spaces_and_keeps_tabs() {
    assert_eq!(pasted("a\r\nb"), "a b");
    assert_eq!(pasted("a\r\n\r\n\nb"), "a b");
    assert_eq!(pasted("a\rb\nc"), "a b c");
    assert_eq!(pasted("a\u{2028}b\u{2029}c"), "a b c");
    assert_eq!(pasted("a\u{2028}\u{2029}\r\nb"), "a b");
    assert_eq!(pasted("a\u{1b}[2Jb\u{7f}\u{85}"), "a [2Jb  ");
    assert_eq!(pasted("a\tb é日本"), "a\tb é日本");
}

#[test]
fn a_comment_of_only_pasted_newlines_is_not_saved() {
    let mut a = comment_prompt_with("");
    a.paste("\n\r\n");
    assert!(a.handle_key(KeyEvent::from(KeyCode::Enter)).is_empty());
    assert!(a.current().unwrap().review.comments.is_empty());
}

#[test]
fn a_paste_with_no_prompt_open_changes_nothing() {
    let mut a = app_on_line();
    a.paste("q\nDy\nx]2?");
    assert!(a.input.is_none() && !a.should_quit && !a.help_open);
    assert_eq!(a.selected, 0);
    assert_eq!(a.current().unwrap().review, Review::default());
}

#[test]
fn a_paste_does_not_answer_the_clear_prompt() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    press(&mut a, 'D');
    a.paste("y");
    assert!(matches!(a.input, Some(Input::ConfirmClear)));
    assert_eq!(a.current().unwrap().review.comments.len(), 1);
}

#[test]
fn a_save_from_a_later_worktree_holds_its_own_id_and_review() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "a", vec![file_with_line("x")]);
    deliver(&mut a, "b", vec![file_with_line("y")]);
    press(&mut a, 'x');
    press(&mut a, ']');
    focus_first_line(&mut a);
    let fx = type_comment(&mut a, "fix");
    assert_eq!(a.states[1].review.comments.len(), 1);
    assert_ne!(a.states[0].review, a.states[1].review);
    assert_eq!(fx, vec![Effect::SaveReview { id: ReviewId::Branch("b".into()), review: a.states[1].review.clone() }]);
}

#[test]
fn adding_a_comment_recounts_outdated() {
    let mut a = app_on_line();
    press(&mut a, 'c');
    press(&mut a, 'z');
    deliver(&mut a, "a", vec![file_with_line("changed while typing")]);
    assert_eq!(a.outdated_comments(), 0);
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(a.outdated_comments(), 1);
}

#[test]
fn clearing_comments_resets_outdated() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    deliver(&mut a, "a", vec![file_with_line("let x = 2;")]);
    assert_eq!(a.outdated_comments(), 1);
    press(&mut a, 'D');
    press(&mut a, 'y');
    assert_eq!(a.outdated_comments(), 0);
}

#[test]
fn a_comment_on_a_typechanges_second_entry_is_current() {
    let gone = FileDiff { status: Status::Deleted, hunks: vec![hunk("@@ -1 +0,0 @@", vec![removed(1)])], ..file("t") };
    let there = FileDiff { path: "t".into(), status: Status::Added, ..file_with_line("new") };
    let mut a = app(&["a"]);
    deliver(&mut a, "a", vec![gone, there]);
    press(&mut a, 'n');
    focus_first_line(&mut a);
    type_comment(&mut a, "fix");
    assert_eq!(a.outdated_comments(), 0);
}

#[test]
fn a_removed_line_comment_does_not_match_a_line_still_in_the_new_file() {
    let mut a = app(&["a"]);
    a.states[0].review.comments.push(note("a.rs", 5, Side::Old, "x"));
    let f = FileDiff { hunks: vec![hunk("@@ -5 +7 @@", vec![context(5, 7)])], ..file("a.rs") };
    deliver(&mut a, "a", vec![f]);
    assert_eq!(a.outdated_comments(), 1);
}

#[test]
fn capital_y_cancels_a_clear() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    press(&mut a, 'D');
    assert!(a.handle_key(with('Y', KeyModifiers::SHIFT)).is_empty());
    assert!(a.input.is_none());
    assert_eq!(a.current().unwrap().review.comments.len(), 1);
}

#[test]
fn a_clear_leaves_reviewed_marks_alone() {
    let mut a = app_on_line();
    press(&mut a, 'x');
    type_comment(&mut a, "fix");
    press(&mut a, 'D');
    press(&mut a, 'y');
    assert!(a.is_reviewed(0));
}

#[test]
fn a_comment_body_is_kept_exactly() {
    let mut a = app_on_line();
    type_comment(&mut a, "  pad  ");
    assert_eq!(a.current().unwrap().review.comments[0].body, "  pad  ");
}

#[test]
fn commented_lines_covers_only_the_current_file() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    a.states[0].review.comments.push(note("other.rs", 9, Side::New, "x"));
    assert_eq!(a.commented_lines(), HashSet::from([(1, Side::New)]));
}

#[test]
fn comment_at_cursor_respects_the_side() {
    let mut a = app_on_line();
    a.states[0].review.comments.push(note("a.rs", 1, Side::Old, "gone"));
    assert!(a.comment_at_cursor().is_none());
    assert!(press(&mut a, 'd').is_empty());
    assert_eq!(a.current().unwrap().review.comments.len(), 1);
}

#[test]
fn export_names_the_branch_rather_than_the_head() {
    let mut a = app_on_line();
    type_comment(&mut a, "fix");
    assert!(matches!(press(&mut a, 'y').as_slice(),
        [Effect::CopyExport(md)] if md.starts_with("## Review of `a` against `develop`")));
}

#[test]
fn content_hash_differs_when_only_a_lines_kind_differs() {
    let added = numbered(LineKind::Added, None, Some(1));
    let gone = numbered(LineKind::Removed, Some(1), None);
    let with_line = |l| FileDiff { hunks: vec![hunk("@@", vec![l])], ..file("a") };
    assert_ne!(content_hash(&with_line(added)), content_hash(&with_line(gone)));
}

#[test]
fn e_opens_the_full_path_in_the_selected_worktree() {
    let mut a = app(&["a", "b"]);
    deliver(&mut a, "b", vec![file_with_line("x")]);
    press(&mut a, ']');
    focus_first_line(&mut a);
    assert_eq!(press(&mut a, 'e'), vec![Effect::OpenEditor { path: "/r/b/a.rs".into(), line: 1 }]);
}

#[test]
fn w_finds_and_selects_a_worktree() {
    let mut a = app(&["feat-auth", "fix-cache"]);
    press(&mut a, 'w');
    type_text(&mut a, "cache");
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(a.selected, 1);
    assert!(a.input.is_none());
}

#[test]
fn w_opens_the_finder_and_closes_help() {
    let mut a = app(&["a", "b"]);
    press(&mut a, '?');
    press(&mut a, 'w');
    assert!(matches!(a.input, Some(Input::Worktree { .. })));
    assert!(!a.help_open);
}

#[test]
fn b_changes_the_base_and_reloads_every_worktree() {
    let mut a = app(&["a", "b"]);
    let fx = press(&mut a, 'b');
    assert!(matches!(fx.as_slice(), [Effect::ListBranches]));
    a.handle_msg(Msg::Branches(Ok(vec!["develop".into(), "main".into()])));
    type_text(&mut a, "main");
    let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(a.base, "main");
    assert_eq!(fx.iter().filter(|e| matches!(e, Effect::Load { .. })).count(), 2);
}

#[test]
fn a_base_change_reloads_the_selected_worktree_first() {
    let mut a = app(&["a", "b", "c"]);
    press(&mut a, '3');
    press(&mut a, 'b');
    a.handle_msg(Msg::Branches(Ok(vec!["develop".into(), "main".into()])));
    type_text(&mut a, "main");
    let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert!(matches!(&fx[0], Effect::Load { path, .. } if path.ends_with("c")));
}

#[test]
fn a_late_branch_list_keeps_an_open_comment() {
    let mut a = app_on_line();
    press(&mut a, 'c');
    press(&mut a, 'x');
    a.handle_msg(Msg::Branches(Ok(vec!["develop".into()])));
    assert!(matches!(a.input, Some(Input::Comment { .. })));
}

#[test]
fn missing_base_opens_the_base_picker() {
    let mut a = app(&["a"]);
    let path: std::path::PathBuf = "/r/a".into();
    let fx = a.handle_msg(Msg::Loaded { path, result: Err(crate::git::GitError::BaseMissing("develop".into())) });
    assert!(matches!(fx.as_slice(), [Effect::ListBranches]));
}

#[test]
fn a_missing_older_base_does_not_reopen_the_picker() {
    let mut a = app(&["a"]);
    a.base = "main".into();
    let fx = a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::BaseMissing("develop".into())) });
    assert!(fx.is_empty());
}

#[test]
fn one_worktree_failing_leaves_the_others_usable() {
    let mut a = app(&["a", "b"]);
    a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::NoMergeBase) });
    deliver(&mut a, "b", vec![file("1")]);
    assert!(a.states[0].error.is_some());
    press(&mut a, ']');
    assert_eq!(a.file_count(), 1);
}

#[test]
fn a_base_change_flags_each_old_diff_until_its_load_lands() {
    let mut a = prepared();
    press(&mut a, 'b');
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    press(&mut a, 'm');
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert!(a.states.iter().all(|s| s.stale));
    deliver(&mut a, "a", vec![file("1")]);
    assert!(!a.states[0].stale && a.states[1].stale);
}

#[test]
fn enter_on_an_empty_query_keeps_the_selected_worktree() {
    for query in ["", "   "] {
        let (a, fx) = submit_finder(query);
        assert_eq!(a.selected, 2, "{query:?}");
        assert!(a.input.is_none() && fx.is_empty(), "{query:?}");
    }
}

#[test]
fn enter_on_an_empty_query_keeps_the_base_and_reloads_nothing() {
    for query in ["", "   "] {
        let (a, fx) = submit_base(query);
        assert_eq!(a.base, "develop", "{query:?}");
        assert!(a.input.is_none() && fx.is_empty(), "{query:?}");
        assert!(a.states.iter().all(|s| !s.stale), "{query:?}");
    }
}

#[test]
fn enter_on_a_matching_query_still_chooses_it() {
    let (a, _) = submit_finder("a");
    assert_eq!(a.selected, 0);
    let (a, fx) = submit_base("main");
    assert_eq!(a.base, "main");
    assert_eq!(fx.len(), 2);
}

#[test]
fn the_finder_takes_the_top_match() {
    let mut a = app(&["x", "feat-a", "feat-b"]);
    press(&mut a, 'w');
    type_text(&mut a, "feat");
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(a.selected, 1);
}

#[test]
fn the_base_picker_takes_the_top_match() {
    let mut a = app(&["a"]);
    press(&mut a, 'b');
    a.handle_msg(Msg::Branches(Ok(vec!["x".into(), "main-a".into(), "main-b".into()])));
    type_text(&mut a, "main");
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(a.base, "main-a");
}

#[test]
fn a_late_branch_list_keeps_an_open_finder() {
    let mut a = app(&["a"]);
    press(&mut a, 'w');
    press(&mut a, 'x');
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    assert!(matches!(a.input, Some(Input::Worktree { .. })));
    assert_eq!(query(&a), Some("x"));
}

#[test]
fn a_second_branch_list_keeps_an_open_picker() {
    let mut a = app(&["a"]);
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    press(&mut a, 'm');
    a.handle_msg(Msg::Branches(Ok(vec!["other".into()])));
    assert_eq!(query(&a), Some("m"));
    assert!(matches!(&a.input, Some(Input::Base { branches, .. }) if branches == &["main"]));
}

#[test]
fn a_branch_list_closes_help_to_open_the_picker() {
    let mut a = app(&["a"]);
    press(&mut a, '?');
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    assert!(!a.help_open);
    assert!(matches!(a.input, Some(Input::Base { .. })));
}

#[test]
fn a_failed_branch_list_sets_a_notice() {
    let mut a = app(&["a"]);
    a.handle_msg(Msg::Branches(Err(crate::git::GitError::Failed("boom".into()))));
    assert_eq!(a.notice.as_deref(), Some("boom"));
    assert!(a.input.is_none());
}

#[test]
fn a_missing_base_stores_the_error() {
    let mut a = app(&["a"]);
    base_missing(&mut a, "a");
    assert_eq!(a.states[0].error, Some(crate::git::GitError::BaseMissing("develop".into())));
}

#[test]
fn a_missing_base_leaves_an_open_prompt_and_stores_the_error() {
    let mut finder = app(&["a"]);
    press(&mut finder, 'w');
    let mut comment = app_on_line();
    press(&mut comment, 'c');
    for (a, open) in [(&mut finder, "finder"), (&mut comment, "comment")] {
        assert!(base_missing(a, "a").is_empty(), "{open}");
        assert!(a.states[0].error.is_some(), "{open}");
    }
    assert!(matches!(finder.input, Some(Input::Worktree { .. })));
    assert!(matches!(comment.input, Some(Input::Comment { .. })));
}

#[test]
fn every_load_after_a_base_change_uses_the_new_base() {
    let mut a = app(&["a", "b"]);
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    press(&mut a, 'm');
    let fx = a.handle_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(fx, vec![
        Effect::Load { path: "/r/a".into(), base: "main".into() },
        Effect::Load { path: "/r/b".into(), base: "main".into() },
    ]);
}

#[test]
fn a_detached_worktree_is_labelled_with_its_short_hash() {
    let w = Worktree { branch: None, head: "abcdef0123456789".into(), ..wt("a") };
    assert_eq!(worktree_label(&w), "(abcdef01)");
}

#[test]
fn a_failed_load_after_a_base_change_stays_stale() {
    let mut a = prepared();
    a.handle_msg(Msg::Branches(Ok(vec!["main".into()])));
    press(&mut a, 'm');
    a.handle_key(KeyEvent::from(KeyCode::Enter));
    a.handle_msg(Msg::Loaded { path: "/r/a".into(), result: Err(crate::git::GitError::NoMergeBase) });
    assert!(a.states[0].error.is_some());
    assert!(a.states[0].stale);
}

#[test]
fn a_watch_limit_tells_the_user_the_badge_may_be_incomplete() {
    let mut a = app(&["a"]);
    let fx = a.handle_msg(Msg::WatchLimit);
    assert!(fx.is_empty());
    assert_eq!(a.notice.as_deref(), Some("File watching limit reached, the changed badge may be incomplete"));
}
