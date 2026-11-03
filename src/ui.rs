use crate::app::{line_position, worktree_label, App, DiffView, Focus, Input};
use crate::diff::{FileDiff, Line, LineKind};
use crate::store::Side;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::borrow::Cow;
use std::collections::HashSet;
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
/// Cells a prompt spends on `> `.
const PROMPT_PREFIX: usize = 2;
/// Rows a filtering prompt lists.
const MAX_OPTIONS: usize = 8;
/// Cells a worktree name keeps before anything less important gives way.
const MIN_NAME: usize = 4;
/// Cells a base branch keeps, so that a usual name like `develop` stays whole.
const MIN_BASE: usize = 16;
const GAP: &str = "   ";
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
    ("n / p", "next or previous file"),
    ("v", "unified or side-by-side diff"),
    ("f", "toggle full-screen diff"),
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

/// Draws a frame. It needs `app` mutably because it also updates the stored
/// list offsets and diff scroll, which depend on the size of this frame.
pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let visible = (main.height as usize).saturating_sub(BORDERS);
    if app.diff_fullscreen {
        keep_diff_selection_visible(app, visible);
        draw_diff(f, app, main);
    } else {
        let side = (f.area().width / 5).clamp(16, 30);
        let [left, files, diff] =
            Layout::horizontal([Constraint::Length(side), Constraint::Length(side), Constraint::Min(0)]).areas(main);

        // The three panes are the same height.
        keep_selection_visible(app, visible);
        draw_worktrees(f, app, left);
        draw_files(f, app, files);
        draw_diff(f, app, diff);
    }
    draw_status(f, app, status);

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
fn keep_selection_visible(app: &mut App, visible: usize) {
    app.worktree_offset = list_offset(app.worktree_offset, app.selected, visible, app.worktrees.len());
    let files = app.file_count();
    if let Some(s) = app.current_mut() {
        s.file_offset = list_offset(s.file_offset, s.file, visible, files);
    }
    keep_diff_selection_visible(app, visible);
}

/// Updates only the state needed by the full-screen pane. In particular, this
/// avoids doing any work for the hidden worktree and file lists.
fn keep_diff_selection_visible(app: &mut App, visible: usize) {
    let rows = app.diff_row_count();
    if let Some(s) = app.current_mut() {
        s.scroll = list_offset(s.scroll, s.cursor, visible, rows);
    }
}

fn draw_worktrees(f: &mut Frame, app: &App, area: Rect) {
    let visible = (area.height as usize).saturating_sub(BORDERS);
    let first = app.worktree_offset;
    let items: Vec<ListItem> = app
        .worktrees
        .iter()
        .zip(&app.states)
        .skip(first)
        .take(visible)
        .map(|(w, s)| {
            let added: u32 = s.diffs.as_ref().map_or(0, |d| d.iter().map(|x| x.added).sum());
            let comments = match s.review.comments.len() {
                0 => String::new(),
                n => format!(" c{n}"),
            };
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
            // The name shortens first, then the counts go, least useful first, so
            // the warnings stay. Only a pane under 10 cells wide can cut those.
            let room = (area.width as usize).saturating_sub(BORDERS + SYMBOL);
            let badges = [format!(" +{added}{comments}{warnings}"), format!("{comments}{warnings}")]
                .into_iter()
                .find(|b| b.width() + MIN_NAME <= room)
                .unwrap_or(warnings);
            let name = room.saturating_sub(badges.width());
            ListItem::new(format!("{}{badges}", elide(&worktree_label(w), name)))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(app.selected.saturating_sub(first)));
    let list = List::new(items)
        .block(block("Worktrees", app.focus == Focus::Worktrees))
        .highlight_symbol("▸ ")
        .highlight_style(Style::new().add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_files(f: &mut Frame, app: &App, area: Rect) {
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
            ListItem::new(format!("{mark}{} {}", x.status.letter(), elide(&x.path, room)))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(s.file.saturating_sub(first)));
    let list = List::new(items)
        .block(block("Files", app.focus == Focus::Files))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state);
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
    let notice = match (&s.error, &s.diffs, app.current_file()) {
        (Some(e), ..) => e.to_string(),
        (None, None, _) => "Loading…".to_string(),
        (None, _, None) => "No changes against the base branch".to_string(),
        (None, _, Some(file)) if file.binary => "Binary or very large file not shown".to_string(),
        (None, _, Some(file)) if file.hunks.is_empty() => "No text changes to show".to_string(),
        (None, _, Some(file)) => {
            let visible = (area.height as usize).saturating_sub(BORDERS);
            let width = (area.width as usize).saturating_sub(BORDERS);
            let highlight = s.highlights.get(&s.file);
            let commented = app.commented_lines();
            let rows = match app.view {
                DiffView::Unified => unified_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width),
                DiffView::SideBySide => split_rows(file, highlight, &commented, s.cursor, s.scroll, visible, width),
            };
            f.render_widget(Paragraph::new(rows).block(block("Diff", focused)), area);
            return;
        }
    };
    f.render_widget(message(notice).block(block("Diff", focused)), area);
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

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    let mut text = if let Some(e) = &app.fatal {
        format!(" Could not list worktrees: {e}")
    } else if let Some(n) = &app.notice {
        format!(" {n}")
    } else if let Some(c) = app.comment_at_cursor() {
        format!(" * {}", display_text(&c.body))
    } else {
        status_summary(app, width)
    };
    if drawn_width(&text) + drawn_width(HELP_HINT) <= width {
        text.push_str(HELP_HINT);
    }
    f.render_widget(Paragraph::new(text), area);
}

/// Warnings first, since the end is what a narrow screen cuts. A long branch
/// name shortens so that the counts after it and the help hint still fit.
fn status_summary(app: &App, width: usize) -> String {
    let (Some(w), Some(s)) = (app.selected_worktree(), app.current()) else {
        return format!(" base: {}", app.base);
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

    // What's left for the two names, which share it.
    let fixed = 1 + drawn_width(&warnings) + drawn_width("base: ") + drawn_width(GAP) * 2 + drawn_width(&counts);
    let spare = width.saturating_sub(fixed);
    let hint = drawn_width(HELP_HINT);
    let label = worktree_label(w);
    let names = MIN_BASE.min(drawn_width(&app.base)) + MIN_NAME.min(drawn_width(&label));
    let room = if spare >= names + hint { spare - hint } else { spare };
    let base_room = room.saturating_sub(drawn_width(&label).min(room / 2)).max(MIN_BASE);
    let base = elide(&app.base, base_room);
    let label_room = room.saturating_sub(drawn_width(&base)).max(MIN_NAME);
    format!(" {warnings}base: {base}{GAP}{}{GAP}{counts}", elide(&label, label_room))
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

/// `text` as one row: control characters become spaces, since git allows tabs
/// and newlines in paths, and a list row must stay one line. If it doesn't fit
/// in `width` cells, its end after an ellipsis, because the end of a path or
/// branch name is usually the part that tells it from others.
fn elide(text: &str, width: usize) -> Cow<'_, str> {
    let line = if text.contains(char::is_control) {
        Cow::Owned(text.replace(char::is_control, " "))
    } else {
        Cow::Borrowed(text)
    };
    if drawn_width(&line) <= width {
        line
    } else if width == 0 {
        Cow::Borrowed("")
    } else {
        Cow::Owned(format!("…{}", tail(&line, width - 1)))
    }
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 50, (HELP.len() + BORDERS) as u16);
    let rows: Vec<TextLine> = HELP.iter().map(|(k, d)| TextLine::from(format!(" {k:<7} {d}"))).collect();
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
}
