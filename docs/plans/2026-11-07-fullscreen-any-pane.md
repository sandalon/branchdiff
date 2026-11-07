# Full-Screen Any Pane Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `f` toggle full screen for whichever pane has focus, and give the full-screen worktree and file lists wider rows with more information.

**Architecture:** `App` keeps one `fullscreen` flag, and the full-screen pane is always `focus`, so `Tab` moves the full-screen view with focus. `ui::draw` gives the main area to the focused pane. `draw_worktrees` and `draw_files` take a `full` flag that picks a wider row builder, and their list, block and highlight code stays shared.

**Tech Stack:** Rust 2021 (MSRV 1.88), ratatui, insta snapshots.

**Design:** `docs/plans/2026-11-07-fullscreen-any-pane-design.md`

**Ground rules:**
- Run everything from the worktree root: `/Users/coreymcclelland/projects/branchdiff/.worktrees/fullscreen-any-pane`.
- CI runs `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`, so every commit must be clean under both. Dead code fails clippy, so each helper lands in the same task as its first caller.
- `cargo-insta` isn't installed. To write a new or changed snapshot, run the test with `INSTA_UPDATE=always`, then **read the `.snap` file** before committing it.
- Comments follow the house style: short doc comments that say why, in plain sentences.

---

### Task 1: `f` makes the focused pane full screen, and `Tab` moves it

**Files:**
- Modify: `src/app.rs` (fields around line 129, `App::new` around line 160, key handling around line 266, `step_focus` and `toggle_diff_fullscreen` around lines 319–342)
- Modify: `src/ui.rs` (`draw` at line 71, `keep_selection_visible` and `keep_diff_selection_visible` around lines 143–170)
- Test: `src/app/tests.rs` (replace `f_toggles_a_diff_only_view_and_restores_focus`, around line 773)
- Test: `tests/ui_snapshots.rs` (`full_screen_diff_replaces_both_navigation_panes`, line 251)
- Modify: `tests/perf.rs` (`full_screen_keypress_to_redraw_is_within_budget`, line 99)

**Step 1: Write the failing app tests**

In `src/app/tests.rs`, replace the whole `f_toggles_a_diff_only_view_and_restores_focus` test with:

```rust
#[test]
fn f_makes_the_focused_pane_full_screen_and_keeps_focus() {
    let mut a = app(&["a"]);
    a.focus = Focus::Files;

    press(&mut a, 'f');
    assert!(a.fullscreen);
    assert_eq!(a.focus, Focus::Files);

    press(&mut a, 'f');
    assert!(!a.fullscreen);
    assert_eq!(a.focus, Focus::Files);
}

#[test]
fn tab_moves_the_full_screen_view_with_focus() {
    let mut a = app(&["a"]);
    press(&mut a, 'f');

    a.handle_key(KeyEvent::from(KeyCode::Tab));
    assert!(a.fullscreen);
    assert_eq!(a.focus, Focus::Files);

    a.handle_key(KeyEvent::from(KeyCode::BackTab));
    a.handle_key(KeyEvent::from(KeyCode::BackTab));
    assert_eq!(a.focus, Focus::Diff);

    press(&mut a, 'f');
    assert!(!a.fullscreen);
    assert_eq!(a.focus, Focus::Diff, "leaving full screen keeps the pane last shown");
}
```

**Step 2: Write the failing UI test**

In `tests/ui_snapshots.rs`, `full_screen_diff_replaces_both_navigation_panes` presses `f` with focus on the worktrees. That will now make the worktree list full screen, so focus the diff first. Add the new test after it:

```rust
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
```

**Step 3: Run the tests to verify they fail**

Run: `cargo test f_makes_the_focused_pane tab_moves_the_full_screen the_full_screen_pane_follows 2>&1 | tail -20`
Expected: a compile error, `no field 'fullscreen' on type 'App'`.

(`cargo test` accepts one filter only, so use `cargo test full_screen` and `cargo test tab_moves` if the combined form complains.)

**Step 4: Change the state and keys in `src/app.rs`**

Replace the two fields:

```rust
    /// Whether the diff pane occupies all of the main area.
    pub diff_fullscreen: bool,
    focus_before_fullscreen: Focus,
```

with:

```rust
    /// Whether the focused pane occupies all of the main area.
    pub fullscreen: bool,
```

In `App::new`, replace `diff_fullscreen: false,` and `focus_before_fullscreen: Focus::Worktrees,` with `fullscreen: false,`.

In `handle_key`, replace `KeyCode::Char('f') => self.toggle_diff_fullscreen(),` with:

```rust
            KeyCode::Char('f') => self.fullscreen = !self.fullscreen,
```

In `step_focus`, delete the early return:

```rust
        if self.diff_fullscreen {
            return;
        }
```

Delete `fn toggle_diff_fullscreen` entirely.

**Step 5: Draw only the focused pane in `src/ui.rs`**

Replace the `if app.diff_fullscreen { ... }` branch of `draw` (keep the `else` branch as it is):

```rust
    if app.fullscreen {
        match app.focus {
            Focus::Worktrees => {
                keep_worktree_selection_visible(app, visible);
                draw_worktrees(f, app, main);
            }
            Focus::Files => {
                keep_file_selection_visible(app, visible);
                draw_files(f, app, main);
            }
            Focus::Diff => {
                let width = (main.width as usize).saturating_sub(BORDERS);
                keep_diff_selection_visible(app, visible, width);
                draw_diff(f, app, main);
            }
        }
    } else {
```

Split `keep_selection_visible` into one helper per pane, so the full-screen branch only does work for the pane it draws:

```rust
/// Moves the stored offsets of the lists and the diff, for panes that show
/// `visible` rows.
fn keep_selection_visible(app: &mut App, visible: usize, diff_width: usize) {
    keep_worktree_selection_visible(app, visible);
    keep_file_selection_visible(app, visible);
    keep_diff_selection_visible(app, visible, diff_width);
}

fn keep_worktree_selection_visible(app: &mut App, visible: usize) {
    app.worktree_offset = list_offset(app.worktree_offset, app.selected, visible, app.worktrees.len());
}

fn keep_file_selection_visible(app: &mut App, visible: usize) {
    let files = app.file_count();
    if let Some(s) = app.current_mut() {
        s.file_offset = list_offset(s.file_offset, s.file, visible, files);
    }
}
```

Change the doc comment on `keep_diff_selection_visible` from "Updates only the state needed by the full-screen pane. In particular, this avoids doing any work for the hidden worktree and file lists." to:

```rust
/// Moves the diff scroll only, so a full-screen diff does no work for the
/// hidden lists.
```

**Step 6: Keep the perf test on the diff**

In `tests/perf.rs`, `full_screen_keypress_to_redraw_is_within_budget` presses `f` with focus on the worktrees. Focus the diff first, as `keypress_to_redraw_is_within_budget` does:

```rust
fn full_screen_keypress_to_redraw_is_within_budget() {
    let mut a = loaded_app();
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Char('f')));
    let d = timed(&mut a, KeyCode::Char('j'));
    assert!(d < BUDGET, "full-screen redraw took {d:?}");
}
```

**Step 7: Run all tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: every `test result: ok`. No existing snapshot should change: the three-pane layout is untouched.

**Step 8: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/app.rs src/ui.rs src/app/tests.rs tests/ui_snapshots.rs tests/perf.rs
git commit -m "feat: make f toggle full screen for the focused pane"
```

---

### Task 2: Full-screen file rows

The full-screen file list shows each whole path with its folders dimmed, both paths of a rename, and the line counts at the right edge.

**Files:**
- Modify: `src/ui.rs` (`draw_files` around line 339, `elide` around line 830, new helpers near `elide`, unit tests at the bottom)
- Test: `tests/ui_snapshots.rs`
- Create: `tests/snapshots/ui_snapshots__full_screen_files.snap` and `tests/snapshots/ui_snapshots__full_screen_files_on_a_narrow_terminal.snap` (generated)

**Step 1: Write the failing unit tests for the helpers**

Add these to `mod tests` at the bottom of `src/ui.rs`:

```rust
    fn texts(spans: &[Span]) -> Vec<String> {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn split_folder_cuts_after_the_last_slash() {
        assert_eq!(split_folder("a.rs"), ("", "a.rs"));
        assert_eq!(split_folder("src/app/tests.rs"), ("src/app/", "tests.rs"));
        assert_eq!(split_folder("dir/"), ("dir/", ""));
        assert_eq!(split_folder("src/日本/語.rs"), ("src/日本/", "語.rs"));
    }

    #[test]
    fn elide_styled_keeps_every_part_that_fits() {
        let dim = Style::new().add_modifier(Modifier::DIM);
        let spans = elide_styled(&[("src/", dim), ("a.rs", Style::new())], 8);
        assert_eq!(texts(&spans), ["src/", "a.rs"]);
        assert_eq!((spans[0].style, spans[1].style), (dim, Style::new()));
    }

    #[test]
    fn elide_styled_cuts_from_the_left_across_parts() {
        let dim = Style::new().add_modifier(Modifier::DIM);
        let spans = elide_styled(&[("src/deep/", dim), ("a.rs", Style::new())], 8);
        assert_eq!(texts(&spans), ["…", "ep/", "a.rs"]);
        assert_eq!((spans[0].style, spans[1].style), (dim, dim));

        // A cut inside a later part gives the ellipsis that part's style.
        let spans = elide_styled(&[("src/", dim), ("long_name.rs", Style::new())], 6);
        assert_eq!(texts(&spans), ["…", "me.rs"]);
        assert_eq!(spans[0].style, Style::new());
    }

    #[test]
    fn elide_styled_makes_one_line_and_fits_no_room() {
        let spans = elide_styled(&[("a\nb/", Style::new()), ("c", Style::new())], 9);
        assert_eq!(texts(&spans), ["a b/", "c"]);
        assert!(elide_styled(&[("a/", Style::new()), ("b", Style::new())], 0).is_empty());
    }
```

**Step 2: Write the failing UI tests**

In `tests/ui_snapshots.rs`, add a buffer helper next to `cell_of`:

```rust
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
```

Then add the tests:

```rust
/// A full-screen file list with a reviewed file, a rename, a binary file and
/// a path too long for the screen.
fn full_screen_files() -> App {
    let deep = format!("{}/deep.rs", ["very_long_folder_name"; 5].join("/"));
    let files = vec![
        FileDiff { path: "src/app/tests.rs".into(), content_id: Some("1".into()), added: 17, removed: 0, ..sample_file() },
        FileDiff { path: "src/ui.rs".into(), added: 333, removed: 33, ..sample_file() },
        FileDiff {
            path: "src/new_name.rs".into(),
            old_path: Some("src/old_name.rs".into()),
            status: Status::Renamed,
            added: 2,
            removed: 1,
            ..sample_file()
        },
        FileDiff { path: "assets/logo.png".into(), status: Status::Added, binary: true, hunks: vec![], ..sample_file() },
        FileDiff { path: deep, ..sample_file() },
    ];
    let mut a = app_with(files);
    a.focus = Focus::Files;
    press(&mut a, "xf");
    a
}

#[test]
fn full_screen_files() {
    insta::assert_snapshot!(render(&mut full_screen_files()));
}

#[test]
fn full_screen_files_on_a_narrow_terminal() {
    insta::assert_snapshot!(render_sized(&mut full_screen_files(), 40, 12));
}

#[test]
fn full_screen_files_dim_folders_but_not_file_names() {
    let buffer = render_buffer(&mut full_screen_files(), 80, 12);
    let (x, y) = position_of(&buffer, "src/ui.rs");
    assert!(buffer[(x, y)].modifier.contains(Modifier::DIM), "the folder is dimmed");
    assert!(!buffer[(x + 4, y)].modifier.contains(Modifier::DIM), "the file name is not");
}

#[test]
fn the_narrow_file_list_is_not_dimmed() {
    let mut a = full_screen_files();
    press(&mut a, "f");
    let buffer = render_buffer(&mut a, 80, 12);
    let (x, y) = position_of(&buffer, "src/ui.rs");
    assert!(!buffer[(x, y)].modifier.contains(Modifier::DIM));
}
```

**Step 3: Run the tests to verify they fail**

Run: `cargo test --lib split_folder 2>&1 | tail -5`
Expected: compile error, `cannot find function 'split_folder'`.

**Step 4: Extract `one_line` from `elide`**

`elide_styled` needs the same control-character cleanup as `elide`, without the shortening. Replace the start of `elide` in `src/ui.rs`:

```rust
fn elide(text: &str, width: usize) -> Cow<'_, str> {
    let line = if text.contains(char::is_control) {
        Cow::Owned(text.replace(char::is_control, " "))
    } else {
        Cow::Borrowed(text)
    };
```

with:

```rust
fn elide(text: &str, width: usize) -> Cow<'_, str> {
    let line = one_line(text);
```

and add above `elide`:

```rust
/// `text` with each control character as a space, since git allows tabs and
/// newlines in paths, and a list row must stay one line.
fn one_line(text: &str) -> Cow<'_, str> {
    if text.contains(char::is_control) {
        Cow::Owned(text.replace(char::is_control, " "))
    } else {
        Cow::Borrowed(text)
    }
}
```

Shorten `elide`'s doc comment to match, since `one_line` now explains the control characters:

```rust
/// `text` as one row. If it doesn't fit in `width` cells, its end after an
/// ellipsis, because the end of a path or branch name is usually the part that
/// tells it from others.
```

**Step 5: Add `split_folder` and `elide_styled`**

Add below `elide`:

```rust
/// `path` split after its last `/` into the folder part and the file name.
/// Git writes `/` on every platform.
fn split_folder(path: &str) -> (&str, &str) {
    path.rfind('/').map_or(("", path), |i| path.split_at(i + 1))
}

/// `parts` as one row of styled spans, shortened from the left like `elide`
/// to fit `width` cells. The ellipsis takes the style of the part it cuts.
fn elide_styled(parts: &[(&str, Style)], width: usize) -> Vec<Span<'static>> {
    let parts: Vec<(Cow<'_, str>, Style)> = parts.iter().map(|(text, style)| (one_line(text), *style)).collect();
    let whole: String = parts.iter().map(|(text, _)| text.as_ref()).collect();
    let shown = elide(&whole, width);
    // `shown` is all of `whole`, nothing, or an ellipsis and the end of `whole`.
    let (ellipsis, kept) = match shown.strip_prefix('…') {
        Some(rest) if shown != whole => (true, rest.len()),
        _ => (false, shown.len()),
    };
    let mut skip = whole.len() - kept;
    let mut spans = Vec::new();
    for (text, style) in &parts {
        if skip >= text.len() {
            skip -= text.len();
            continue;
        }
        if ellipsis && spans.is_empty() {
            spans.push(Span::styled("…", *style));
        }
        spans.push(Span::styled(text[skip..].to_string(), *style));
        skip = 0;
    }
    spans
}
```

`text[skip..]` is always on a character boundary: `elide` keeps whole graphemes from the end of `whole`, and the parts are laid end to end in it.

**Step 6: Add the full row and the `full` flag to `draw_files`**

Add near the other constants at the top of `src/ui.rs`:

```rust
/// Cells between the columns of a full-screen list row.
const COLUMN_GAP: usize = 2;
```

Add below `draw_files`:

```rust
/// A row of the full-screen file list: the whole path with its folders
/// dimmed, both paths of a rename, and the line counts at the right edge.
fn full_file_row(prefix: String, file: &FileDiff, room: usize) -> TextLine<'static> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    let counts = if file.binary { "bin".to_string() } else { format!("+{} −{}", file.added, file.removed) };
    let mut parts = Vec::new();
    if let Some(old) = &file.old_path {
        let (folder, name) = split_folder(old);
        parts.extend([(folder, dim), (name, Style::new()), (" → ", Style::new())]);
    }
    let (folder, name) = split_folder(&file.path);
    parts.extend([(folder, dim), (name, Style::new())]);

    let path_room = room.saturating_sub(COLUMN_GAP + drawn_width(&counts));
    let mut spans = vec![Span::raw(prefix)];
    spans.extend(elide_styled(&parts, path_room));
    let used: usize = spans[1..].iter().map(|s| drawn_width(&s.content)).sum();
    spans.push(Span::raw(" ".repeat(path_room - used + COLUMN_GAP)));
    spans.push(Span::raw(counts));
    TextLine::from(spans)
}
```

Change `draw_files` to take the flag, and build rows with it:

```rust
fn draw_files(f: &mut Frame, app: &App, area: Rect, full: bool) {
```

```rust
        .map(|(i, x)| {
            let mark = if app.is_reviewed(i) { "✓" } else { " " };
            let prefix = format!("{mark}{} ", x.status.letter());
            if full {
                ListItem::new(full_file_row(prefix, x, room))
            } else {
                ListItem::new(format!("{prefix}{}", elide(&x.path, room)))
            }
        })
```

In `draw`, pass `true` in the full-screen `Focus::Files` arm (`draw_files(f, app, main, true)`) and `false` in the three-pane branch (`draw_files(f, app, files, false)`).

**Step 7: Run the unit tests**

Run: `cargo test --lib 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: `test result: ok`.

**Step 8: Write and read the new snapshots**

Run: `INSTA_UPDATE=always cargo test --test ui_snapshots full_screen_files 2>&1 | grep -E "^test result|FAILED"`
Expected: `test result: ok`.

Read `tests/snapshots/ui_snapshots__full_screen_files.snap` and check that:
- the first row reads `✓M src/app/tests.rs` and ends with `+17 −0` at the right border
- the rename row reads `R src/old_name.rs → src/new_name.rs`
- the binary row ends with `bin`
- the deep path starts with `…` and ends with `/deep.rs`
- the counts all end in the same column

Read `ui_snapshots__full_screen_files_on_a_narrow_terminal.snap` and check that each file name is still visible and the counts still end at the right border.

**Step 9: Run all tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all ok. No existing snapshot changed.

**Step 10: Lint and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/ui.rs tests/ui_snapshots.rs tests/snapshots/
git commit -m "feat: show whole paths in the full-screen file list"
```

---

### Task 3: Full-screen worktree rows

The full-screen worktree list shows each name, its dimmed path (with `~` for the home directory), and the line, file and comment counts and warnings at the right edge. When the screen is too narrow for the path, a row falls back to the narrow row.

**Files:**
- Modify: `src/app.rs` (new `home` field in `App` and `App::new`)
- Modify: `src/main.rs:163` (set `home`)
- Modify: `src/ui.rs` (imports, `draw_worktrees` around line 295, new helpers, unit tests)
- Test: `tests/ui_snapshots.rs`
- Create: `tests/snapshots/ui_snapshots__full_screen_worktrees.snap` (generated)

**Step 1: Write the failing unit test**

Add to `mod tests` in `src/ui.rs`:

```rust
    #[test]
    fn display_path_writes_the_home_directory_as_a_tilde() {
        let home = Some(Path::new("/home/me"));
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(display_path(Path::new("/home/me/app"), home), format!("~{sep}app"));
        assert_eq!(display_path(Path::new("/home/me"), home), "~");
        assert_eq!(display_path(Path::new("/home/meg/app"), home), "/home/meg/app");
        assert_eq!(display_path(Path::new("/r/app"), None), "/r/app");
        assert_eq!(display_path(Path::new("/r/app"), Some(Path::new("/"))), "/r/app", "a root home is ignored");
    }
```

**Step 2: Write the failing UI tests**

In `tests/ui_snapshots.rs`:

```rust
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
```

At 30 columns: room is 26, the name column is 9, and `+1 −1  1 file` is 13, so 9 + 2 + 2 + 13 leaves no room for a path.

**Step 3: Run the tests to verify they fail**

Run: `cargo test --lib display_path 2>&1 | tail -5`
Expected: compile error, `cannot find function 'display_path'`.

**Step 4: Store the home directory on `App`**

In `src/app.rs`, add to `App` after `truecolor`:

```rust
    /// Shown as `~` at the start of worktree paths. `main` sets it, so tests
    /// don't depend on the machine's home directory.
    pub home: Option<PathBuf>,
```

and `home: None,` in `App::new`.

In `src/main.rs`, after `let mut app = App::new(base);`:

```rust
    app.home = std::env::home_dir();
```

(`std::env::home_dir` is no longer deprecated as of Rust 1.87, and the MSRV is 1.88. If clippy reports `deprecated`, check `rustc --version` before going further.)

**Step 5: Add `display_path`**

In `src/ui.rs`, add `use std::path::Path;` to the imports, and add below `split_folder`:

```rust
/// `path` with the home directory written as `~`. A home of `/` is ignored,
/// since every path would start with it.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.filter(|h| h.parent().is_some()).and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}
```

**Step 6: Split the narrow worktree row into helpers**

Change the imports to bring in the types the helpers take:

```rust
use crate::app::{line_position, worktree_label, App, DiffView, Focus, Input, WorktreeState};
use crate::git::Worktree;
```

Move the body of the existing row closure in `draw_worktrees` into these functions (the behaviour doesn't change):

```rust
/// A row of the narrow worktree list. The name shortens first, then the counts
/// go, least useful first, so the warnings stay. Only a pane under 10 cells
/// wide can cut those.
fn worktree_row(w: &Worktree, s: &WorktreeState, room: usize) -> String {
    let added: u32 = s.diffs.as_ref().map_or(0, |d| d.iter().map(|x| x.added).sum());
    let comments = comment_badge(s);
    let warnings = warning_badges(s);
    let badges = [format!(" +{added}{comments}{warnings}"), format!("{comments}{warnings}")]
        .into_iter()
        .find(|b| b.width() + MIN_NAME <= room)
        .unwrap_or(warnings);
    let name = room.saturating_sub(badges.width());
    format!("{}{badges}", elide(&worktree_label(w), name))
}

fn comment_badge(s: &WorktreeState) -> String {
    match s.review.comments.len() {
        0 => String::new(),
        n => format!(" c{n}"),
    }
}

/// `●` for a diff that may be out of date, `!` for a git operation in
/// progress and `✗` for an error.
fn warning_badges(s: &WorktreeState) -> String {
    let mut warnings = String::new();
    if s.stale {
        warnings.push_str(" ●");
    }
    if s.op.is_some() {
        warnings.push_str(" !");
    }
    if s.error.is_some() {
        warnings.push_str(" ✗");
    }
    warnings
}
```

**Step 7: Add the full row and the `full` flag to `draw_worktrees`**

Add a constant beside `MIN_NAME`:

```rust
/// Cells a full-screen worktree path keeps before the row falls back to the
/// narrow one.
const MIN_PATH: usize = 8;
```

Add below `worktree_row`:

```rust
/// A row of the full-screen worktree list: the name in a column `name_width`
/// wide, the dimmed path, and the counts and warnings at the right edge.
/// `None` when the path has no room, so the list can use the narrow row.
fn full_worktree_row(
    w: &Worktree,
    s: &WorktreeState,
    room: usize,
    name_width: usize,
    home: Option<&Path>,
) -> Option<TextLine<'static>> {
    let diffs = s.diffs.as_ref().map_or(&[][..], |d| d.as_slice());
    let (added, removed) = diffs.iter().fold((0u32, 0u32), |(a, r), x| (a + x.added, r + x.removed));
    let badges = format!(
        "+{added} −{removed}  {}{}{}",
        plural(diffs.len(), "file"),
        comment_badge(s),
        warning_badges(s)
    );
    let path_room = room.checked_sub(name_width + 2 * COLUMN_GAP + drawn_width(&badges))?;
    if path_room < MIN_PATH {
        return None;
    }
    let label = worktree_label(w);
    let name = elide(&label, name_width);
    let path = display_path(&w.path, home);
    let path = elide(&path, path_room);
    let gap = " ".repeat(COLUMN_GAP);
    Some(TextLine::from(vec![
        Span::raw(format!("{name}{}{gap}", " ".repeat(name_width - drawn_width(&name)))),
        Span::styled(
            format!("{path}{}", " ".repeat(path_room - drawn_width(&path))),
            Style::new().add_modifier(Modifier::DIM),
        ),
        Span::raw(format!("{gap}{badges}")),
    ]))
}
```

Check `s.diffs` is an `Arc<Vec<FileDiff>>` (`crate::cache::Diffs`). `draw_files` already uses `d.as_slice()` on it, so this compiles the same way.

Replace `draw_worktrees` with:

```rust
fn draw_worktrees(f: &mut Frame, app: &App, area: Rect, full: bool) {
    let visible = (area.height as usize).saturating_sub(BORDERS);
    let first = app.worktree_offset;
    let room = (area.width as usize).saturating_sub(BORDERS + SYMBOL);
    // Over the whole list, so the columns stay put as it scrolls.
    let name_width = if full {
        app.worktrees.iter().map(|w| drawn_width(&worktree_label(w))).max().unwrap_or(0).min(room * 2 / 5)
    } else {
        0
    };
    let items: Vec<ListItem> = app
        .worktrees
        .iter()
        .zip(&app.states)
        .skip(first)
        .take(visible)
        .map(|(w, s)| {
            let row = full.then(|| full_worktree_row(w, s, room, name_width, app.home.as_deref())).flatten();
            row.map_or_else(|| ListItem::new(worktree_row(w, s, room)), ListItem::new)
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected.saturating_sub(first)));
    let list = List::new(items)
        .block(block("Worktrees", app.focus == Focus::Worktrees))
        .highlight_symbol("▸ ")
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}
```

In `draw`, pass `true` in the full-screen `Focus::Worktrees` arm and `false` in the three-pane branch.

**Step 8: Run the unit tests and the existing worktree tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: everything passes except `full_screen_worktrees`, which has no snapshot yet. The narrow-row tests, such as `a_long_worktree_name_leaves_room_for_its_badges`, must still pass, which confirms the refactor kept the behaviour.

**Step 9: Write and read the snapshot**

Run: `INSTA_UPDATE=always cargo test --test ui_snapshots full_screen_worktrees 2>&1 | grep -E "^test result|FAILED"`

Read `tests/snapshots/ui_snapshots__full_screen_worktrees.snap` and check that:
- `▸ feat-auth` and `fix-cache` start their paths, `/r/feat-auth` and `/r/fix-cache`, in the same column
- `feat-auth` ends with `+1 −1  1 file c2` at the right border
- `fix-cache`, which isn't loaded, ends with `+0 −0  0 files`

**Step 10: Run all tests, lint and commit**

```bash
cargo test 2>&1 | grep -E "^test result|FAILED|panicked"
cargo fmt && cargo clippy --all-targets -- -D warnings
git add src/app.rs src/main.rs src/ui.rs tests/ui_snapshots.rs tests/snapshots/
git commit -m "feat: show paths and counts in the full-screen worktree list"
```

---

### Task 4: Help, README and perf

**Files:**
- Modify: `src/ui.rs:50` (the `HELP` entry for `f`)
- Modify: `README.md:45`
- Modify: `tests/perf.rs`
- Modify: `tests/snapshots/ui_snapshots__help_overlay_lists_every_hotkey.snap` (regenerated)

**Step 1: Update the help text**

In `HELP`, change `("f", "toggle full-screen diff"),` to:

```rust
    ("f", "toggle full-screen pane"),
```

**Step 2: Update the README**

Change `| `f` | toggle the diff pane full screen |` to:

```markdown
| `f` | toggle the focused pane full screen |
```

**Step 3: Add the perf test**

In `tests/perf.rs`, after `full_screen_keypress_to_redraw_is_within_budget`:

```rust
#[test]
#[ignore]
fn full_screen_file_list_keypress_to_redraw_is_within_budget() {
    let mut a = loaded_app();
    a.handle_key(KeyEvent::from(KeyCode::Tab));
    a.handle_key(KeyEvent::from(KeyCode::Char('f')));
    let d = timed(&mut a, KeyCode::Char('j'));
    assert!(d < BUDGET, "full-screen file list redraw took {d:?}");
}
```

**Step 4: Regenerate the help snapshot and read it**

Run: `INSTA_UPDATE=always cargo test --test ui_snapshots help_overlay 2>&1 | grep -E "^test result|FAILED"`
Then: `git diff tests/snapshots/`
Expected: the only change is `toggle full-screen diff` → `toggle full-screen pane`.

**Step 5: Run the perf tests in release**

Run: `cargo test --release --test perf -- --ignored 2>&1 | grep -E "^test |test result"`
Expected: every perf test passes, including the new one.

**Step 6: Full verification**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test 2>&1 | grep -E "^test result|FAILED|panicked"
git status --short tests/snapshots/   # no stray .snap.new files
```

Expected: all clean, all ok.

**Step 7: Commit**

```bash
git add src/ui.rs README.md tests/perf.rs tests/snapshots/
git commit -m "docs: describe f as full screen for the focused pane"
```

**Step 8: Try it by hand**

Run `cargo run` from the main checkout (it has several worktrees) and check:
- `f` on the worktree list shows names, paths with `~`, and counts
- `Tab` goes to the full-screen file list: dimmed folders, `→` for renames, counts at the right
- `Tab` again goes to the full-screen diff, and `f` returns to three panes with the diff focused
