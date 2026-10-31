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

const HELP: &[(&str, &str)] = &[
    ("[ / ]", "previous or next worktree"),
    ("1-9", "jump to a worktree"),
    ("w", "fuzzy find a worktree"),
    ("Tab", "move between areas"),
    ("j / k", "move and scroll"),
    ("n / p", "next or previous file"),
    ("v", "unified or side-by-side diff"),
    ("c", "comment on the current line"),
    ("d / D", "delete a comment, or clear all"),
    ("e", "open in $EDITOR"),
    ("x", "mark file as reviewed"),
    ("y / Y", "copy comments, or write to a file"),
    ("r", "refresh or retry"),
    ("b", "change the base branch"),
    ("?", "open or close this help"),
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

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [left, files, diff] = Layout::horizontal([
        Constraint::Length(26), Constraint::Length(24), Constraint::Min(0),
    ]).areas(main);

    draw_worktrees(f, app, left);
    draw_files(f, app, files);
    draw_diff(f, app, diff);
    draw_status(f, app, status);

    if app.help_open {
        draw_help(f);
    }
    if app.input.is_some() {
        draw_input(f, app);
    }
    if !app.truecolor {
        for cell in &mut f.buffer_mut().content {
            cell.fg = fit_color(cell.fg, false);
            cell.bg = fit_color(cell.bg, false);
        }
    }
}

/// Nearest 256-colour entry, for terminals that don't advertise truecolor.
fn fit_color(color: Color, truecolor: bool) -> Color {
    let Color::Rgb(r, g, b) = color else { return color };
    if truecolor {
        return color;
    }
    let cube = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { (v - 35) / 40 };
    Color::Indexed(16 + 36 * cube(r) + 6 * cube(g) + cube(b))
}

fn block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused { Style::new().fg(Color::Cyan) } else { Style::new() };
    Block::default().borders(Borders::ALL).border_style(style).title(title)
}

fn draw_worktrees(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.worktrees.iter().zip(&app.states).map(|(w, s)| {
        let added: u32 = s.diffs.as_ref().map_or(0, |d| d.iter().map(|x| x.added).sum());
        let mut badges = format!(" +{added}");
        if !s.review.comments.is_empty() {
            badges.push_str(&format!(" c{}", s.review.comments.len()));
        }
        if s.stale { badges.push_str(" ●"); }
        if s.op.is_some() { badges.push_str(" !"); }
        if s.error.is_some() { badges.push_str(" ✗"); }
        // The name gives way, because a badge is what the row is there to show.
        let room = (area.width as usize).saturating_sub(BORDERS + SYMBOL + badges.width());
        ListItem::new(format!("{}{badges}", elide(&worktree_label(w), room)))
    }).collect();
    let mut state = ListState::default().with_selected(Some(app.selected));
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
    // Only the visible rows, since `is_reviewed` can hash a whole file. The
    // selected file sits on the last row, where `List` would scroll it to.
    let visible = (area.height as usize).saturating_sub(BORDERS);
    let first = (s.file + 1).saturating_sub(visible);
    let room = (area.width as usize).saturating_sub(BORDERS + FILE_PREFIX);
    let files = s.diffs.as_ref().map_or(&[][..], |d| d.as_slice());
    let items: Vec<ListItem> = files.iter().enumerate().skip(first).take(visible).map(|(i, x)| {
        let mark = if app.is_reviewed(i) { "✓" } else { " " };
        ListItem::new(format!("{mark}{} {}", x.status.letter(), elide(&x.path, room)))
    }).collect();
    let mut state = ListState::default().with_selected(Some(s.file.saturating_sub(first)));
    let list = List::new(items)
        .block(block("Files", app.focus == Focus::Files))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut state);
}

/// Builds only the visible rows, so frame cost does not grow with diff size.
fn draw_diff(f: &mut Frame, app: &mut App, area: Rect) {
    let visible = area.height.saturating_sub(2) as usize;
    let Some(s) = app.current_mut() else {
        // Git always lists a worktree, so an empty list hasn't arrived yet.
        let text = app.fatal.as_ref().map_or("Loading…".to_string(), |e| format!("Could not list worktrees: {e}"));
        f.render_widget(message(text).block(block("Diff", app.focus == Focus::Diff)), area);
        return;
    };
    if s.cursor < s.scroll {
        s.scroll = s.cursor;
    } else if visible > 0 && s.cursor >= s.scroll + visible {
        s.scroll = s.cursor + 1 - visible;
    }
    let app = &*app;
    let Some(s) = app.current() else { return };
    let focused = app.focus == Focus::Diff;

    let notice = match (&s.error, &s.diffs) {
        (Some(e), _) => Some(e.to_string()),
        (None, None) => Some("Loading…".to_string()),
        (None, Some(d)) if d.is_empty() => Some("No changes against the base branch".to_string()),
        _ => None,
    };
    if let Some(m) = notice {
        f.render_widget(message(m).block(block("Diff", focused)), area);
        return;
    }
    let Some(file) = app.current_file() else { return };
    if file.binary {
        f.render_widget(message("Binary or very large file not shown").block(block("Diff", focused)), area);
        return;
    }
    if file.hunks.is_empty() {
        f.render_widget(message("No text changes to show").block(block("Diff", focused)), area);
        return;
    }
    let highlight = s.highlights.get(&s.file);
    let commented = app.commented_lines();
    let rows = match app.view {
        DiffView::Unified => unified_rows(file, highlight, &commented, s.cursor, s.scroll, visible),
        DiffView::SideBySide => split_rows(file, highlight, &commented, s.cursor, s.scroll, visible, area.width.saturating_sub(2) as usize),
    };
    f.render_widget(Paragraph::new(rows).block(block("Diff", focused)), area);
}

/// Wrapped, because the diff area is only 28 cells wide on an 80-column screen.
fn message<'a>(text: impl Into<ratatui::text::Text<'a>>) -> Paragraph<'a> {
    Paragraph::new(text).wrap(Wrap { trim: false })
}

fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Added => Style::new().bg(Color::Rgb(16, 48, 24)),
        LineKind::Removed => Style::new().bg(Color::Rgb(56, 16, 16)),
        LineKind::Context => Style::new(),
    }
}

/// Gutter mark for a line that has a comment.
fn comment_mark(commented: &HashSet<(u32, Side)>, line: &Line) -> &'static str {
    if line_position(line).is_some_and(|p| commented.contains(&p)) { "*" } else { " " }
}

fn unified_rows<'a>(
    file: &'a FileDiff,
    highlight: Option<&'a crate::highlight::HighlightedFile>,
    commented: &HashSet<(u32, Side)>,
    cursor: usize,
    scroll: usize,
    visible: usize,
) -> Vec<TextLine<'a>> {
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    'outer: for (hi, hunk) in file.hunks.iter().enumerate() {
        if row + 1 + hunk.lines.len() <= scroll {
            row += 1 + hunk.lines.len();
            continue;
        }
        if row >= scroll {
            rows.push(mark(TextLine::from(display_text(&hunk.header)).style(Style::new().fg(Color::Cyan)), row == cursor));
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
                    Some(parts) => spans.extend(parts.iter().map(|(c, t)| Span::styled(display_text(t), Style::new().fg(*c)))),
                    None => spans.push(Span::raw(display_text(&line.text))),
                }
                rows.push(mark(TextLine::from(spans).style(line_style(line.kind)), row == cursor));
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
    let side = width.saturating_sub(4) / 2;
    let mut rows = Vec::with_capacity(visible);
    let mut row = 0;
    for (hi, hunk) in file.hunks.iter().enumerate() {
        if rows.len() >= visible {
            break;
        }
        if row >= scroll {
            rows.push(mark(TextLine::from(display_text(&hunk.header)).style(Style::new().fg(Color::Cyan)), row == cursor));
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
                rows.push(mark(TextLine::from(spans).style(line_style(line.kind)), row == cursor));
            }
            row += 1;
        }
    }
    rows
}

/// Cuts text to `width` cells and pads to it, measuring as drawn so the divider
/// stays aligned. A wide character that would straddle the edge is dropped.
fn fit_spans<'a>(parts: &[(Color, &'a str)], width: usize) -> Vec<Span<'a>> {
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
        out.push(Span::styled(kept, Style::new().fg(*color)));
        if full {
            break;
        }
    }
    out.push(Span::raw(" ".repeat(width - used)));
    out
}

fn mark(line: TextLine<'_>, selected: bool) -> TextLine<'_> {
    if selected { line.patch_style(Style::new().add_modifier(Modifier::REVERSED)) } else { line }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut text = format!(" base: {}", app.base);
    if let Some(e) = &app.fatal {
        text = format!(" Could not list worktrees: {e}");
    } else if let Some(n) = &app.notice {
        text = format!(" {n}");
    } else if let Some(c) = app.comment_at_cursor() {
        text = format!(" * {}", display_text(&c.body));
    } else if let (Some(w), Some(s)) = (app.selected_worktree(), app.current()) {
        let count = s.diffs.as_ref().map_or(0, |d| d.len());
        text.push_str(&format!("   {}   {count} files   {} comments", worktree_label(w), s.review.comments.len()));
        let outdated = app.outdated_comments();
        if outdated > 0 { text.push_str(&format!(" ({outdated} outdated)")); }
        if s.stale { text.push_str("   ● changed"); }
        if let Some(op) = s.op { text.push_str(&format!("   ! {op} in progress")); }
    }
    text.push_str("   ? help");
    f.render_widget(Paragraph::new(text), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let (w, h) = (width.min(area.width), height.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// The end of `text` that fits in `width` cells.
fn tail(text: &str, width: usize) -> &str {
    let mut used = 0;
    for (i, c) in text.char_indices().rev() {
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > width {
            return &text[i + c.len_utf8()..];
        }
    }
    text
}

/// `text` if it fits in `width` cells, otherwise its end after an ellipsis. The
/// end of a path or branch name is usually the part that tells it from others.
fn elide(text: &str, width: usize) -> Cow<'_, str> {
    if text.width() <= width {
        Cow::Borrowed(text)
    } else if width == 0 {
        Cow::Borrowed("")
    } else {
        Cow::Owned(format!("…{}", tail(text, width - 1)))
    }
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 50, HELP.len() as u16 + 2);
    let rows: Vec<TextLine> = HELP.iter().map(|(k, d)| TextLine::from(format!(" {k:<7} {d}"))).collect();
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block("Help", true)), area);
}

fn draw_input(f: &mut Frame, app: &App) {
    let Some(input) = &app.input else { return };
    let (title, text, options) = match input {
        Input::Comment { text, .. } => ("Comment (Enter saves, Esc cancels)", text.as_str(), Vec::new()),
        Input::ConfirmClear => ("Clear all comments for this worktree?", "y to clear, any other key cancels", Vec::new()),
        Input::Worktree { query } => ("Find worktree", query.as_str(), {
            let labels = app.worktree_labels();
            crate::fuzzy::filter(query, &labels).into_iter().take(8).map(|i| labels[i].clone()).collect()
        }),
        Input::Base { query, branches } => ("Base branch", query.as_str(), {
            crate::fuzzy::filter(query, branches).into_iter().take(8).map(|i| branches[i].clone()).collect()
        }),
    };
    let area = centered(f.area(), 60, (options.len().min(8) as u16) + 3);
    let shown = display_text(text);
    // Two for the border and two for "> ".
    let shown = tail(&shown, area.width.saturating_sub(4) as usize);
    let mut rows = vec![TextLine::from(format!("> {shown}"))];
    rows.extend(options.into_iter().take(8).map(TextLine::from));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(rows).block(block(title, true)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elide_keeps_the_end_and_fits_the_width() {
        assert_eq!(elide("src/a.rs", 8), "src/a.rs");
        assert_eq!(elide("src/deep/a.rs", 8), "…ep/a.rs");
        assert_eq!(elide("src/日本語.rs", 7), "…語.rs");
        assert_eq!(elide("a.rs", 0), "");
    }

    #[test]
    fn fit_color_maps_to_the_256_colour_cube() {
        assert_eq!(fit_color(Color::Rgb(0, 0, 0), false), Color::Indexed(16));
        assert_eq!(fit_color(Color::Rgb(255, 255, 255), false), Color::Indexed(231));
        assert_eq!(fit_color(Color::Rgb(135, 95, 175), false), Color::Indexed(97));
    }

    #[test]
    fn fit_color_leaves_truecolor_and_named_colours_alone() {
        assert_eq!(fit_color(Color::Rgb(1, 2, 3), true), Color::Rgb(1, 2, 3));
        assert_eq!(fit_color(Color::Cyan, false), Color::Cyan);
    }
}
