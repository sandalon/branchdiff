use crate::app::{line_position, worktree_label, App, DiffView, Focus, Input, WorktreeState};
use crate::diff::{FileDiff, Line, LineKind};
use crate::git::Worktree;
use crate::store::Side;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

const TAB: &str = "    ";
/// Cells a list loses to its top and bottom, or its left and right, borders.
const BORDERS: usize = 2;
/// Cells the worktree list reserves for its selection symbol.
const SYMBOL: usize = 2;
/// Cells before a path in the file list: reviewed mark, status and a space.
const FILE_PREFIX: usize = 3;
/// Cells a side-by-side row spends on its comment mark and ` │ ` divider.
const SPLIT_CHROME: usize = 4;
/// Cells before source text in a unified row: comment, number, space and sign.
const UNIFIED_GUTTER: usize = 8;
/// Cells a prompt spends on `> `.
const PROMPT_PREFIX: usize = 2;
/// Rows a filtering prompt lists.
const MAX_OPTIONS: usize = 8;
/// Cells a worktree name keeps before anything less important gives way.
const MIN_NAME: usize = 4;
/// Cells a full-screen worktree path keeps before the row falls back to the
/// narrow one.
const MIN_PATH: usize = 8;
/// Cells the launch directory keeps when its full path does not fit.
const MIN_CWD: usize = 8;
/// Cells a base branch keeps, so that a usual name like `develop` stays whole.
const MIN_BASE: usize = 16;
const GAP: &str = "   ";
/// Cells between the columns of a full-screen list row.
const COLUMN_GAP: usize = 2;
const HELP_COLUMN_GAP: usize = 3;
const HELP_HINT: &str = "   ? help";
/// Text on the dark added and removed backgrounds, which must not take a light
/// terminal's dark default. It's the foreground of the highlighting theme.
const ROW_TEXT: Color = Color::Rgb(0xc0, 0xc5, 0xce);

const HELP: &[(&str, &str)] = &[
    ("[ / ]", "previous or next worktree"),
    ("1-9", "jump to a worktree"),
    ("w", "fuzzy find a worktree"),
    ("Tab / Right", "next area"),
    ("Shift+Tab / Left", "previous area"),
    ("j / k", "move and scroll"),
    ("Page Up / Down", "scroll the diff one page"),
    ("n / p", "next or previous file"),
    ("v", "unified or side-by-side diff"),
    ("Shift+W", "toggle word wrap"),
    ("f", "toggle full-screen pane"),
    ("c", "comment on the current line"),
    ("d / D", "delete a comment, or clear all"),
    ("e", "open in $EDITOR"),
    ("x", "mark file as reviewed"),
    ("y / Y", "copy comments, or write to a file"),
    ("r", "refresh or retry"),
    ("b", "change the base branch"),
    ("?", "open or close this help"),
    ("Esc", "close help or a prompt"),
    ("q", "quit"),
];

/// Text as drawn: tabs become spaces and carriage returns are removed.
fn display_text(s: &str) -> Cow<'_, str> {
    if s.contains(['\t', '\r']) {
        Cow::Owned(s.replace('\t', TAB).replace('\r', ""))
    } else {
        Cow::Borrowed(s)
    }
}

/// Draws a frame for the directory the process was launched from. It needs
/// `app` mutably because it also updates stored offsets and scroll positions,
/// which depend on the size of this frame.
pub fn draw(f: &mut Frame, app: &mut App, cwd: &Path) {
    let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let visible = (main.height as usize).saturating_sub(BORDERS);
    app.set_diff_viewport_rows(visible);
    if app.fullscreen {
        match app.focus {
            Focus::Worktrees => {
                keep_worktree_selection_visible(app, visible);
                draw_worktrees(f, app, main, true);
            }
            Focus::Files => {
                keep_file_selection_visible(app, visible);
                draw_files(f, app, main, true);
            }
            Focus::Diff => {
                let width = (main.width as usize).saturating_sub(BORDERS);
                keep_diff_selection_visible(app, visible, width);
                draw_diff(f, app, main);
            }
        }
    } else {
        let side = (f.area().width / 5).clamp(16, 30);
        let [left, files, diff] =
            Layout::horizontal([Constraint::Length(side), Constraint::Length(side), Constraint::Min(0)]).areas(main);

        // The three panes are the same height.
        let diff_width = (diff.width as usize).saturating_sub(BORDERS);
        keep_selection_visible(app, visible, diff_width);
        draw_worktrees(f, app, left, false);
        draw_files(f, app, files, false);
        draw_diff(f, app, diff);
    }
    draw_status(f, app, cwd, status);

    if app.help_open {
        draw_help(f);
    }
    if app.input.is_some() {
        draw_input(f, app);
    }
    if !app.truecolor {
        for cell in &mut f.buffer_mut().content {
            cell.fg = fit_color(cell.fg);
            cell.bg = fit_color(cell.bg);
        }
    }
}

/// Nearest 256-colour entry, for terminals that don't advertise truecolor.
fn fit_color(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    let cube = |v: u8| {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (v - 35) / 40
        }
    };
    Color::Indexed(16 + 36 * cube(r) + 6 * cube(g) + cube(b))
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused { Style::new().fg(Color::Cyan) } else { Style::new() };
    Block::default().borders(Borders::ALL).border_style(style).title(title)
}

/// Moves `offset` just enough to bring `selected` inside `visible` rows of a
/// list of `len`, which also can't leave rows unused below its end after the
/// list shrinks or the terminal grows.
fn list_offset(offset: usize, selected: usize, visible: usize, len: usize) -> usize {
    let offset = if selected < offset {
        selected
    } else if visible > 0 && selected >= offset + visible {
        selected + 1 - visible
    } else {
        offset
    };
    offset.min(len.saturating_sub(visible))
}

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

/// Moves the diff scroll only, so a full-screen diff does no work for the
/// hidden lists.
fn keep_diff_selection_visible(app: &mut App, visible: usize, width: usize) {
    let rows = app.diff_row_count();
    if app.word_wrap {
        let (view, file) = (app.view, app.current_file());
        if let (Some(s), Some(file)) = (app.current(), file) {
            let scroll = wrapped_scroll(file, s.scroll, s.cursor, visible, width, view);
            if let Some(s) = app.current_mut() {
                s.scroll = scroll;
            }
        }
        return;
    }
    if let Some(s) = app.current_mut() {
        s.scroll = list_offset(s.scroll, s.cursor, visible, rows);
    }
}

#[derive(Clone, Copy)]
enum DiffRow<'a> {
    Header(&'a str),
    Line { hunk: usize, line: usize, value: &'a Line },
}

/// Logical diff rows from the model. Keeping this as an iterator means wrapped
/// rendering allocates in proportion to the viewport, never the whole diff.
fn diff_rows(file: &FileDiff) -> impl Iterator<Item = DiffRow<'_>> {
    file.hunks.iter().enumerate().flat_map(|(hi, hunk)| {
        std::iter::once(DiffRow::Header(hunk.header.as_str())).chain(
            hunk.lines.iter().enumerate().map(move |(li, line)| DiffRow::Line { hunk: hi, line: li, value: line }),
        )
    })
}

/// Screen rows needed for one logical row, capped so a megabyte-long source
/// line cannot make a frame expensive.
fn wrapped_row_height(row: DiffRow<'_>, view: DiffView, width: usize, cap: usize) -> usize {
    match row {
        DiffRow::Header(text) => wrap_count(text, width, cap),
        DiffRow::Line { value, .. } => {
            let source_width = match view {
                DiffView::Unified => width.saturating_sub(UNIFIED_GUTTER),
                DiffView::SideBySide => width.saturating_sub(SPLIT_CHROME) / 2,
            };
            wrap_count(&value.text, source_width, cap)
        }
    }
}

fn wrap_count(text: &str, width: usize, cap: usize) -> usize {
    let cap = cap.max(1);
    if width == 0 {
        return 1;
    }
    let shown = display_text(text);
    let mut rows = 1;
    let mut used = 0;
    for grapheme in Span::raw(shown.as_ref()).styled_graphemes(Style::new()) {
        let w = grapheme.symbol.width();
        if used > 0 && used + w > width {
            rows += 1;
            if rows >= cap {
                return rows;
            }
            used = 0;
        }
        used += w;
    }
    rows
}

fn capped_height<'a>(rows: impl Iterator<Item = DiffRow<'a>>, view: DiffView, width: usize, cap: usize) -> usize {
    let mut height = 0usize;
    for row in rows {
        height = height.saturating_add(wrapped_row_height(row, view, width, cap.saturating_sub(height)));
        if height >= cap {
            return cap;
        }
    }
    height
}

/// Keeps a logical-row cursor visible when logical rows occupy different
/// numbers of screen rows. Work is bounded by the viewport, apart from the same
/// walk to the logical offset that unwrapped rendering already performs.
fn wrapped_scroll(
    file: &FileDiff,
    scroll: usize,
    cursor: usize,
    visible: usize,
    width: usize,
    view: DiffView,
) -> usize {
    if visible == 0 {
        return scroll.min(cursor);
    }

    let mut first = scroll.min(cursor);
    let heights: Vec<usize> = diff_rows(file)
        .skip(first)
        .take(cursor.saturating_sub(first) + 1)
        .map(|row| wrapped_row_height(row, view, width, visible + 1))
        .collect();
    if heights.iter().fold(0usize, |sum, h| sum.saturating_add(*h)) > visible {
        let original = scroll.min(cursor);
        let mut used = 0usize;
        first = cursor;
        for (i, height) in heights.iter().enumerate().rev() {
            if used > 0 && used.saturating_add(*height) > visible {
                break;
            }
            used = used.saturating_add(*height);
            first = original + i;
            if used >= visible {
                break;
            }
        }
    }

    // At the end of a diff, pull earlier rows back in so terminal growth does
    // not leave avoidable blank space below the content.
    let after = capped_height(diff_rows(file).skip(first), view, width, visible);
    if after < visible && first > 0 {
        let candidate = first.saturating_sub(visible);
        let before: Vec<usize> = diff_rows(file)
            .skip(candidate)
            .take(first - candidate)
            .map(|row| wrapped_row_height(row, view, width, visible + 1))
            .collect();
        let mut used = after;
        for (i, height) in before.iter().enumerate().rev() {
            if used.saturating_add(*height) > visible {
                break;
            }
            used += *height;
            first = candidate + i;
        }
    }
    first
}

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
    let badges =
        format!("+{added} −{removed}  {}{}{}", plural(diffs.len(), "file"), comment_badge(s), warning_badges(s));
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

fn draw_files(f: &mut Frame, app: &App, area: Rect, full: bool) {
    let Some(s) = app.current() else {
        f.render_widget(block("Files", false), area);
        return;
    };
    // Only the visible rows, since `is_reviewed` can hash a whole file.
    let visible = (area.height as usize).saturating_sub(BORDERS);
    let first = s.file_offset;
    let room = (area.width as usize).saturating_sub(BORDERS + FILE_PREFIX);
    let files = s.diffs.as_ref().map_or(&[][..], |d| d.as_slice());
    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, x)| {
            let mark = if app.is_reviewed(i) { "✓" } else { " " };
            let prefix = format!("{mark}{} ", x.status.letter());
            if full {
                ListItem::new(full_file_row(prefix, x, room))
            } else {
                ListItem::new(format!("{prefix}{}", elide(&x.path, room)))
            }
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.file.saturating_sub(first)));
    let list = List::new(items)
        .block(block("Files", app.focus == Focus::Files))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state);
}

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

/// Builds only the visible rows. Finding them still walks the lines of the
/// hunks before them, so frame cost grows with the size of a hunk, not of a diff.
fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Diff;
    let Some(s) = app.current() else {
        // Git always lists a worktree, so an empty list hasn't arrived yet.
        let text = app.fatal.as_ref().map_or("Loading…".to_string(), |e| format!("Could not list worktrees: {e}"));
        f.render_widget(message(text).block(block("Diff", focused)), area);
        return;
    };
    let metadata = app.current_file().map(file_metadata).unwrap_or_default();
    let diff_block = || {
        let pane = block("Diff", focused);
        if metadata.is_empty() {
            pane
        } else {
            pane.title_bottom(elide(&metadata, (area.width as usize).saturating_sub(BORDERS)).into_owned())
        }
    };
    let notice = match (&s.error, &s.diffs, app.current_file()) {
        (Some(e), ..) => e.to_string(),
        (None, None, _) => "Loading…".to_string(),
        (None, _, None) => "No changes against the base branch".to_string(),
        (None, _, Some(file)) if file.binary => "Binary or very large file not shown".to_string(),
        (None, _, Some(file)) if file.hunks.is_empty() => {
            if metadata.is_empty() {
                "No text changes to show".to_string()
            } else {
                format!("{metadata}\n\nNo text changes to show")
            }
        }
        (None, _, Some(file)) => {
            let visible = (area.height as usize).saturating_sub(BORDERS);
            let width = (area.width as usize).saturating_sub(BORDERS);
            let highlight = s.highlights.get(&s.file);
            let commented = app.commented_lines();
            let rows = match (app.view, app.word_wrap) {
                (DiffView::Unified, false) => {
                    unified_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width)
                }
                (DiffView::SideBySide, false) => {
                    split_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width)
                }
                (DiffView::Unified, true) => {
                    wrapped_unified_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width)
                }
                (DiffView::SideBySide, true) => {
                    wrapped_split_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width)
                }
            };
            f.render_widget(Paragraph::new(rows).block(diff_block()), area);
            return;
        }
    };
    f.render_widget(message(notice).block(diff_block()), area);
}

/// Metadata stays visible even when a rename or mode change has no text hunks.
fn file_metadata(file: &FileDiff) -> String {
    let mut parts = Vec::new();
    if let Some(old) = &file.old_path {
        let clean = |path: &str| path.replace(char::is_control, " ");
        parts.push(format!("Renamed: {} → {}", clean(old), clean(&file.path)));
    }
    match (file.old_mode, file.new_mode) {
        (Some(old), Some(new)) if old != new => parts.push(format!("Mode: {old:06o} → {new:06o}")),
        (None, Some(new)) => parts.push(format!("New mode: {new:06o}")),
        (Some(old), None) => parts.push(format!("Deleted mode: {old:06o}")),
        _ => {}
    }
    parts.join(" · ")
}

/// Wrapped, because git's messages can be far wider than the diff area.
fn message<'a>(text: impl Into<ratatui::text::Text<'a>>) -> Paragraph<'a> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

/// A span's style. `Reset` means no colour of its own, so the row's shows.
fn span_style(color: Color) -> Style {
    if color == Color::Reset {
        Style::new()
    } else {
        Style::new().fg(color)
    }
}

fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Added => Style::new().fg(ROW_TEXT).bg(Color::Rgb(16, 48, 24)),
        LineKind::Removed => Style::new().fg(ROW_TEXT).bg(Color::Rgb(56, 16, 16)),
        LineKind::Context => Style::new(),
    }
}

/// Gutter mark for a line that has a comment.
fn comment_mark(commented: &HashSet<(u32, Side)>, line: &Line) -> &'static str {
    if line_position(line).is_some_and(|p| commented.contains(&p)) {
        "*"
    } else {
        " "
    }
}

fn unified_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
    width: usize,
) -> Vec<TextLine<'a>> {
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    'outer: for (hi, hunk) in file.hunks.iter().enumerate() {
        if row + 1 + hunk.lines.len() <= scroll {
            row += 1 + hunk.lines.len();
            continue;
        }
        if row >= scroll {
            rows.push(finish_row(
                TextLine::from(display_text(&hunk.header)).style(Style::new().fg(Color::Cyan)),
                row == cursor,
                width,
            ));
        }
        row += 1;
        for (li, line) in hunk.lines.iter().enumerate() {
            if rows.len() >= visible {
                break 'outer;
            }
            if row >= scroll {
                let prefix = match line.kind {
                    LineKind::Added => "+",
                    LineKind::Removed => "-",
                    LineKind::Context => " ",
                };
                let number = line.new_no.or(line.old_no).unwrap_or(0);
                let gutter = comment_mark(commented, line);
                let mut spans = vec![Span::raw(format!("{gutter}{number:>5} {prefix}"))];
                match highlight.and_then(|h| h.hunks.get(hi)).and_then(|h| h.get(li)) {
                    Some(parts) => {
                        spans.extend(parts.iter().map(|(c, t)| Span::styled(display_text(t), span_style(*c))))
                    }
                    None => spans.push(Span::raw(display_text(&line.text))),
                }
                rows.push(finish_row(TextLine::from(spans).style(line_style(line.kind)), row == cursor, width));
            }
            row += 1;
        }
        if rows.len() >= visible {
            break;
        }
    }
    rows
}

fn split_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
    width: usize,
) -> Vec<TextLine<'a>> {
    let side = width.saturating_sub(SPLIT_CHROME) / 2;
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    for (hi, hunk) in file.hunks.iter().enumerate() {
        if rows.len() >= visible {
            break;
        }
        if row >= scroll {
            rows.push(finish_row(
                TextLine::from(display_text(&hunk.header)).style(Style::new().fg(Color::Cyan)),
                row == cursor,
                width,
            ));
        }
        row += 1;
        for (li, line) in hunk.lines.iter().enumerate() {
            if rows.len() >= visible {
                break;
            }
            if row >= scroll {
                let parts: Vec<(Color, &str)> = match highlight.and_then(|h| h.hunks.get(hi)).and_then(|h| h.get(li)) {
                    Some(parts) => parts.iter().map(|(c, t)| (*c, t.as_str())).collect(),
                    None => vec![(Color::Reset, line.text.as_str())],
                };
                let left: &[(Color, &str)] = if line.old_no.is_some() { &parts } else { &[] };
                let right: &[(Color, &str)] = if line.new_no.is_some() { &parts } else { &[] };
                let mut spans = vec![Span::raw(comment_mark(commented, line))];
                spans.extend(fit_spans(left, side));
                spans.push(Span::raw(" │ "));
                spans.extend(fit_spans(right, side));
                rows.push(finish_row(TextLine::from(spans).style(line_style(line.kind)), row == cursor, width));
            }
            row += 1;
        }
    }
    rows
}

fn highlighted_parts<'a>(
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    hunk: usize,
    index: usize,
    line: &'a Line,
) -> Vec<(Color, &'a str)> {
    match highlight.and_then(|h| h.hunks.get(hunk)).and_then(|h| h.get(index)) {
        Some(parts) => parts.iter().map(|(color, text)| (*color, text.as_str())).collect(),
        None => vec![(Color::Reset, line.text.as_str())],
    }
}

/// Soft-wraps coloured text by terminal cells. The result is limited to the
/// remaining viewport height, so even an enormous generated line has bounded
/// redraw cost.
fn wrap_spans(parts: &[(Color, &str)], width: usize, limit: usize) -> Vec<Vec<Span<'static>>> {
    if limit == 0 {
        return Vec::new();
    }
    if width == 0 {
        return vec![Vec::new()];
    }

    let mut rows = Vec::new();
    let mut current: Vec<(Color, String)> = Vec::new();
    let mut used = 0usize;
    for (color, text) in parts {
        let shown = display_text(text);
        for grapheme in Span::raw(shown.as_ref()).styled_graphemes(Style::new()) {
            let w = grapheme.symbol.width();
            if used > 0 && used + w > width {
                rows.push(owned_spans(std::mem::take(&mut current)));
                if rows.len() == limit {
                    return rows;
                }
                used = 0;
            }
            if current.last().is_some_and(|(previous, _)| previous == color) {
                current.last_mut().unwrap().1.push_str(grapheme.symbol);
            } else {
                current.push((*color, grapheme.symbol.to_string()));
            }
            used += w;
        }
    }
    rows.push(owned_spans(current));
    rows
}

fn owned_spans(parts: Vec<(Color, String)>) -> Vec<Span<'static>> {
    parts.into_iter().map(|(color, text)| Span::styled(text, span_style(color))).collect()
}

fn pad_spans(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let used: usize =
        spans.iter().flat_map(|span| span.styled_graphemes(Style::new())).map(|grapheme| grapheme.symbol.width()).sum();
    spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    spans
}

fn wrapped_unified_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
    width: usize,
) -> Vec<TextLine<'static>> {
    let mut rows = Vec::with_capacity(visible);
    for (offset, row) in diff_rows(file).skip(scroll).enumerate() {
        let logical = scroll + offset;
        let remaining = visible.saturating_sub(rows.len());
        if remaining == 0 {
            break;
        }
        match row {
            DiffRow::Header(text) => {
                for spans in wrap_spans(&[(Color::Cyan, text)], width, remaining) {
                    rows.push(finish_row(TextLine::from(spans), logical == cursor, width));
                }
            }
            DiffRow::Line { hunk, line: index, value } => {
                let sign = match value.kind {
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                    LineKind::Context => ' ',
                };
                let number = value.new_no.or(value.old_no).unwrap_or(0);
                let first_prefix = format!("{}{number:>5} {sign}", comment_mark(commented, value));
                let continuation = " ".repeat(UNIFIED_GUTTER);
                let parts = highlighted_parts(highlight, hunk, index, value);
                for (i, spans) in
                    wrap_spans(&parts, width.saturating_sub(UNIFIED_GUTTER), remaining).into_iter().enumerate()
                {
                    let prefix = if i == 0 { first_prefix.clone() } else { continuation.clone() };
                    let mut line = vec![Span::raw(prefix)];
                    line.extend(spans);
                    rows.push(finish_row(TextLine::from(line).style(line_style(value.kind)), logical == cursor, width));
                }
            }
        }
    }
    rows
}

fn wrapped_split_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
    width: usize,
) -> Vec<TextLine<'static>> {
    let side = width.saturating_sub(SPLIT_CHROME) / 2;
    let mut rows = Vec::with_capacity(visible);
    for (offset, row) in diff_rows(file).skip(scroll).enumerate() {
        let logical = scroll + offset;
        let remaining = visible.saturating_sub(rows.len());
        if remaining == 0 {
            break;
        }
        match row {
            DiffRow::Header(text) => {
                for spans in wrap_spans(&[(Color::Cyan, text)], width, remaining) {
                    rows.push(finish_row(TextLine::from(spans), logical == cursor, width));
                }
            }
            DiffRow::Line { hunk, line: index, value } => {
                let parts = highlighted_parts(highlight, hunk, index, value);
                let chunks = wrap_spans(&parts, side, remaining);
                let count = if value.old_no.is_some() || value.new_no.is_some() { chunks.len() } else { 1 };
                for i in 0..count {
                    let mut spans = vec![Span::raw(if i == 0 { comment_mark(commented, value) } else { " " })];
                    let left =
                        if value.old_no.is_some() { chunks.get(i).cloned().unwrap_or_default() } else { Vec::new() };
                    spans.extend(pad_spans(left, side));
                    spans.push(Span::raw(" │ "));
                    let right =
                        if value.new_no.is_some() { chunks.get(i).cloned().unwrap_or_default() } else { Vec::new() };
                    spans.extend(pad_spans(right, side));
                    rows.push(finish_row(
                        TextLine::from(spans).style(line_style(value.kind)),
                        logical == cursor,
                        width,
                    ));
                }
            }
        }
    }
    rows
}

/// Cuts text to `width` cells and pads to it, measuring as drawn so the divider
/// stays aligned. A wide character that would straddle the edge is dropped.
fn fit_spans(parts: &[(Color, &str)], width: usize) -> Vec<Span<'static>> {
    let mut used = 0;
    let mut out = Vec::new();
    for (color, text) in parts {
        let mut kept = String::new();
        let mut full = false;
        let text = display_text(text);
        // By grapheme, as ratatui draws: an emoji sequence's width isn't the sum of its characters.
        for grapheme in Span::raw(text.as_ref()).styled_graphemes(Style::new()) {
            let w = grapheme.symbol.width();
            if used + w > width {
                full = true;
                break;
            }
            used += w;
            kept.push_str(grapheme.symbol);
        }
        out.push(Span::styled(kept, span_style(*color)));
        if full {
            break;
        }
    }
    out.push(Span::raw(" ".repeat(width - used)));
    out
}

/// Fills a row to `width` cells, so its background and the cursor reach the
/// pane's edge, and marks the cursor. Stops measuring at the edge, because a
/// minified line can be megabytes long.
fn finish_row(mut line: TextLine<'_>, selected: bool, width: usize) -> TextLine<'_> {
    let graphemes = line.spans.iter().flat_map(|s| s.styled_graphemes(Style::new()));
    let mut used = 0;
    for grapheme in graphemes {
        used += grapheme.symbol.width();
        if used >= width {
            break;
        }
    }
    line.spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    if selected {
        line.patch_style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

fn draw_status(f: &mut Frame, app: &App, cwd: &Path, area: Rect) {
    let width = area.width as usize;
    let mut text = if let Some(e) = &app.fatal {
        status_message(format!(" Could not list worktrees: {e}"), cwd, width)
    } else if let Some(n) = &app.notice {
        status_message(format!(" {n}"), cwd, width)
    } else if let Some(c) = app.comment_at_cursor() {
        status_message(format!(" * {}", display_text(&c.body)), cwd, width)
    } else {
        status_summary(app, cwd, width)
    };
    if drawn_width(&text) + drawn_width(HELP_HINT) <= width {
        text.push_str(HELP_HINT);
    }
    f.render_widget(Paragraph::new(text), area);
}

/// Adds the launch directory to transient status messages when there is room.
/// The message stays first because errors, notices and comments are more urgent.
fn status_message(message: String, cwd: &Path, width: usize) -> String {
    let fixed = drawn_width(&message) + drawn_width(GAP) + drawn_width("cwd: ");
    let room = width.saturating_sub(fixed);
    if room == 0 {
        return message;
    }
    let cwd = cwd.to_string_lossy();
    format!("{message}{GAP}cwd: {}", elide(&cwd, room))
}

/// Warnings first, since the end is what a narrow screen cuts. A long branch
/// name shortens so that the counts after it and the help hint still fit.
fn status_summary(app: &App, cwd: &Path, width: usize) -> String {
    let cwd = cwd.to_string_lossy();
    let (Some(w), Some(s)) = (app.selected_worktree(), app.current()) else {
        let fixed = 1 + drawn_width("cwd: ") + drawn_width(GAP) + drawn_width("base: ");
        let spare = width.saturating_sub(fixed);
        let desired = [drawn_width(&cwd), drawn_width(&app.base), 0];
        let minimum = [MIN_CWD.min(desired[0]), MIN_BASE.min(desired[1]), 0];
        let hint = drawn_width(HELP_HINT);
        let room = if spare >= minimum.iter().sum::<usize>() + hint { spare - hint } else { spare };
        let [cwd_room, base_room, _] = status_name_widths(room, desired, minimum);
        return format!(" cwd: {}{GAP}base: {}", elide(&cwd, cwd_room), elide(&app.base, base_room));
    };
    let mut warnings = String::new();
    if let Some(op) = s.op {
        warnings.push_str(&format!("! {op} in progress{GAP}"));
    }
    if s.stale {
        warnings.push_str(&format!("● changed{GAP}"));
    }
    let outdated = app.outdated_comments();
    if outdated > 0 {
        warnings.push_str(&format!("({outdated} outdated){GAP}"));
    }
    let counts = format!("{}{GAP}{}", plural(app.file_count(), "file"), plural(s.review.comments.len(), "comment"));

    // What's left for the three names, which share it.
    let fixed = 1
        + drawn_width(&warnings)
        + drawn_width("cwd: ")
        + drawn_width("base: ")
        + drawn_width(GAP) * 3
        + drawn_width(&counts);
    let spare = width.saturating_sub(fixed);
    let hint = drawn_width(HELP_HINT);
    let label = worktree_label(w);
    let desired = [drawn_width(&cwd), drawn_width(&app.base), drawn_width(&label)];
    let minimum = [MIN_CWD.min(desired[0]), MIN_BASE.min(desired[1]), MIN_NAME.min(desired[2])];
    let room = if spare >= minimum.iter().sum::<usize>() + hint { spare - hint } else { spare };
    let [cwd_room, base_room, label_room] = if fixed > width && !warnings.is_empty() {
        // Let the terminal clip the less urgent fields after the warning, while
        // still drawing useful values rather than empty field labels.
        desired
    } else {
        status_name_widths(room, desired, minimum)
    };
    format!(
        " {warnings}cwd: {}{GAP}base: {}{GAP}{}{GAP}{counts}",
        elide(&cwd, cwd_room),
        elide(&app.base, base_room),
        elide(&label, label_room)
    )
}

/// Shares the flexible part of the status bar between the launch directory,
/// base branch and selected worktree. Each remains identifiable on narrow
/// screens; extra room completes the short branch names before the long path.
fn status_name_widths(room: usize, desired: [usize; 3], minimum: [usize; 3]) -> [usize; 3] {
    let mut widths = [0; 3];
    let mut left = room;
    for i in 0..3 {
        if desired[i] > 0 && left > 0 {
            widths[i] = 1;
            left -= 1;
        }
    }
    for (i, target) in [(1, minimum[1]), (2, minimum[2]), (0, minimum[0])] {
        let added = target.saturating_sub(widths[i]).min(left);
        widths[i] += added;
        left -= added;
    }
    for (i, target) in [(2, desired[2]), (1, desired[1]), (0, desired[0])] {
        let added = target.saturating_sub(widths[i]).min(left);
        widths[i] += added;
        left -= added;
    }
    widths
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (w, h) = (width.min(area.width), height.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// Cells `text` takes when drawn. By grapheme, as ratatui draws, because an
/// emoji sequence's width isn't the sum of its characters.
fn drawn_width(text: &str) -> usize {
    Span::raw(text).styled_graphemes(Style::new()).map(|g| g.symbol.width()).sum()
}

/// The end of `text` that fits in `width` cells.
fn tail(text: &str, width: usize) -> String {
    let span = Span::raw(text);
    let graphemes: Vec<_> = span.styled_graphemes(Style::new()).collect();
    let mut used = 0;
    let mut start = graphemes.len();
    for (i, g) in graphemes.iter().enumerate().rev() {
        used += g.symbol.width();
        if used > width {
            break;
        }
        start = i;
    }
    graphemes[start..].iter().map(|g| g.symbol).collect()
}

/// `text` with each control character as a space, since git allows tabs and
/// newlines in paths, and a list row must stay one line.
fn one_line(text: &str) -> Cow<'_, str> {
    if text.contains(char::is_control) {
        Cow::Owned(text.replace(char::is_control, " "))
    } else {
        Cow::Borrowed(text)
    }
}

/// `text` as one row. If it doesn't fit in `width` cells, its end after an
/// ellipsis, because the end of a path or branch name is usually the part that
/// tells it from others.
fn elide(text: &str, width: usize) -> Cow<'_, str> {
    let line = one_line(text);
    if drawn_width(&line) <= width {
        line
    } else if width == 0 {
        Cow::Borrowed("")
    } else {
        Cow::Owned(format!("…{}", tail(&line, width - 1)))
    }
}

/// `path` split after its last `/` into the folder part and the file name.
/// Git writes `/` on every platform.
fn split_folder(path: &str) -> (&str, &str) {
    path.rfind('/').map_or(("", path), |i| path.split_at(i + 1))
}

/// `path` with the home directory written as `~`. A home of `/` is ignored,
/// since every path would start with it.
fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home.filter(|h| h.parent().is_some()).and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
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

fn draw_help(f: &mut Frame) {
    let key_width = HELP.iter().map(|(key, _)| drawn_width(key)).max().unwrap_or(0);
    let description_width = HELP.iter().map(|(_, description)| drawn_width(description)).max().unwrap_or(0);
    // One cell of breathing room on both sides of the two text columns.
    let width = 2 + key_width + HELP_COLUMN_GAP + description_width + BORDERS;
    let rows: Vec<TextLine> = HELP
        .iter()
        .map(|(key, description)| {
            let padding = " ".repeat(key_width.saturating_sub(drawn_width(key)) + HELP_COLUMN_GAP);
            TextLine::from(format!(" {key}{padding}{description}"))
        })
        .collect();
    let area = centered(f.area(), width as u16, (HELP.len() + BORDERS) as u16);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block("Help", true)), area);
}

/// The items a filtering prompt lists for `query`, best first.
fn best_matches(query: &str, items: &[String]) -> Vec<String> {
    crate::fuzzy::filter(query, items).into_iter().take(MAX_OPTIONS).map(|i| items[i].clone()).collect()
}

fn draw_input(f: &mut Frame, app: &App) {
    let Some(input) = &app.input else { return };
    let (title, text, options) = match input {
        Input::Comment { text, .. } => ("Comment (Enter saves, Esc cancels)", text.as_str(), Vec::new()),
        Input::ConfirmClear => {
            ("Clear all comments for this worktree?", "y to clear, any other key cancels", Vec::new())
        }
        Input::Worktree { query } => ("Find worktree", query.as_str(), best_matches(query, &app.worktree_labels())),
        Input::Base { query, branches } => ("Base branch", query.as_str(), best_matches(query, branches)),
    };
    // One row for the text being typed.
    let area = centered(f.area(), 60, (options.len() + 1 + BORDERS) as u16);
    let room = (area.width as usize).saturating_sub(BORDERS);
    let shown = display_text(text);
    let shown = tail(&shown, room.saturating_sub(PROMPT_PREFIX));
    let mut rows = vec![TextLine::from(format!("> {shown}"))];
    rows.extend(options.iter().map(|o| TextLine::from(elide(o, room).into_owned())));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block(title, true)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_text_expands_tabs_and_removes_carriage_returns() {
        assert_eq!(display_text("\ta\r"), "    a");
        assert!(matches!(display_text("plain"), Cow::Borrowed("plain")));
    }

    #[test]
    fn list_offset_moves_only_as_far_as_it_must() {
        assert_eq!(list_offset(5, 7, 5, 20), 5, "already inside");
        assert_eq!(list_offset(5, 3, 5, 20), 3, "above the window");
        assert_eq!(list_offset(5, 12, 5, 20), 8, "below the window");
        assert_eq!(list_offset(15, 4, 5, 6), 1, "list shrank");
        assert_eq!(list_offset(9, 19, 30, 20), 0, "window grew");
        assert_eq!(list_offset(0, 0, 5, 0), 0, "empty list");
        assert_eq!(list_offset(2, 3, 0, 5), 2, "no rows");
    }

    #[test]
    fn tail_and_elide_measure_as_drawn() {
        let heart = "❤\u{fe0f}";
        assert_eq!(tail(&format!("{heart}a"), 2), "a");
        assert_eq!(tail(&format!("{heart}a"), 3), format!("{heart}a"));
        assert_eq!(elide(&heart.repeat(4), 7), format!("…{}", heart.repeat(3)));
    }

    #[test]
    fn elide_makes_one_line_of_control_characters() {
        assert_eq!(elide("a\nb\tc", 9), "a b c");
        assert_eq!(elide("long\nname", 6), "… name");
    }

    #[test]
    fn elide_keeps_the_end_and_fits_the_width() {
        assert_eq!(elide("src/a.rs", 8), "src/a.rs");
        assert_eq!(elide("src/deep/a.rs", 8), "…ep/a.rs");
        assert_eq!(elide("src/日本語.rs", 7), "…語.rs");
        assert_eq!(elide("a.rs", 0), "");
    }

    #[test]
    fn fit_color_maps_to_the_256_colour_cube() {
        assert_eq!(fit_color(Color::Rgb(0, 0, 0)), Color::Indexed(16));
        assert_eq!(fit_color(Color::Rgb(255, 255, 255)), Color::Indexed(231));
        assert_eq!(fit_color(Color::Rgb(135, 95, 175)), Color::Indexed(97));
    }

    #[test]
    fn fit_color_leaves_named_colours_alone() {
        assert_eq!(fit_color(Color::Cyan), Color::Cyan);
    }
    fn texts(spans: &[Span]) -> Vec<String> {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

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
}
