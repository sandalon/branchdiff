# Full-Screen Any Pane Design

**Date:** 2026-11-07
**Status:** Approved

## Purpose

`f` makes the diff pane full screen. The worktree and file panes are clamped to 16–30 columns, so they shorten names and paths and leave out data they already hold. This change makes `f` work on whichever pane has focus, and gives the two list panes a wider row with more information when they fill the screen.

The goals are to see each worktree's path next to its name, and to see the files' relative paths so the project structure is easy to read.

## State and key handling

The full-screen pane is always the focused one, so the state is one flag:

- `App::diff_fullscreen` is renamed to `fullscreen`.
- `focus_before_fullscreen` is removed. The full-screen pane is `focus`.

Keys:

- `f` toggles `fullscreen` and leaves `focus` alone. Turning it off returns to three panes with the same pane focused.
- `Tab`, `Shift+Tab`, `Left` and `Right` no longer return early in `step_focus`. Focus moves as usual and the full-screen view follows it, so a reviewer can pick a worktree, pick a file and read the diff without leaving full screen.
- Every other key acts on `focus` as before.

`step_focus` is the only runtime change to focus, so the full-screen pane can't fall out of step with it.

The `f` entry in `HELP` and in the README key table becomes "toggle full-screen pane".

Out of scope: a key per pane, remembering the full-screen state per pane, and any change to `f` while the help overlay or an input prompt is open.

## Rendering

`draw` switches on `fullscreen` and `focus` and gives `main` to one pane. Only the drawn pane's scroll offset is updated, in the way `keep_diff_selection_visible` works today. That makes one small helper per pane, and the three-pane layout calls all three.

`draw_worktrees` and `draw_files` take a `full: bool` that picks a short or a full row builder. The list, block, focus border and highlight code stays shared.

### Full-screen worktree row

```
▸ feat/word-wrap     ~/projects/branchdiff-wt/word-wrap      +333 −33   7 files  c2  ●
  master             ~/projects/branchdiff                      +0 −0   0 files
  (a99b3b43)         ~/projects/branchdiff-wt/detached          +20 −14  3 files      ✗
```

- Columns are aligned, and the widest label sets the name column, capped at about 40% of the width.
- The path is dimmed, the home directory shows as `~`, and `elide` shortens it from the left.
- The removed-line count and the file count are new. Both come from `s.diffs`.
- The warning glyphs are unchanged. The status bar already spells out the selected worktree's warnings.

### Full-screen file row

```
✓M src/app/tests.rs                    +17 −0
 M src/ui.rs                          +333 −33
 R src/old_name.rs → src/new_name.rs     +2 −1
 A assets/logo.png                        bin
```

- The folder part is dimmed and the filename is drawn normally. A rename shows `old → new`, with each side styled the same way.
- `+added −removed` sits in a right-aligned column. Binary files show `bin`.
- The selected row stays reversed, which hides the dimming on that one row.
- The dimming is for full screen only. The narrow file pane is unchanged.

## Edge cases

- **Nothing loaded or nothing selected:** each full-screen pane shows what the narrow pane shows today.
- **Narrow terminal:** the full worktree row drops columns in the narrow row's order, so the path goes first, then the file count and the line counts, and the warnings stay.
- **Long paths:** `elide` shortens from the left so the filename stays visible. The folder part that's left is dimmed.
- **Resize:** each frame lays out again, so nothing extra is needed.
- **Control characters:** names and paths still go through `elide`, and the folder/filename split works on that cleaned text.

## Testing

- `src/app/tests.rs`: `f` keeps focus; `Tab` moves focus and stays full screen; `f` leaves full screen on the current pane. The test that focus is restored is removed.
- `tests/ui_snapshots.rs`: new snapshots for full-screen worktrees, full-screen files (reviewed mark, rename, binary file, long path) and full-screen files on a narrow terminal. The full-screen diff snapshot should stay the same. The help overlay snapshot is regenerated.
- `tests/perf.rs`: keep the full-screen diff case and add a full-screen files case with a large file list.
- `src/ui.rs`: a unit test for the folder/filename split covering no folder, nested folders, a trailing slash and wide characters.
