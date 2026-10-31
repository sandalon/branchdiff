use crate::cache::{CacheKey, Diffs};
use crate::diff::{FileDiff, Hunk, Status};
use crate::git::{GitError, Worktree};
use crate::store::{fnv1a, Comment, Review, ReviewId, Side};
use crate::worker::Msg;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus { Worktrees, Files, Diff }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView { Unified, SideBySide }

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Load { path: PathBuf, base: String },
    ReloadWorktrees,
    SaveReview { id: ReviewId, review: Review },
    CopyExport(String),
    WriteExport(String),
    OpenEditor { path: PathBuf, line: u32 },
}

/// A text prompt. While set, keys go here rather than to hotkeys.
pub enum Input {
    Comment { path: String, line: u32, side: Side, line_text: String, text: String },
    ConfirmClear,
}

impl Input {
    /// The text that keys and pastes edit, if this prompt has any.
    fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Comment { text, .. } => Some(text),
            Self::ConfirmClear => None,
        }
    }
}

/// Everything remembered for one worktree across switches.
#[derive(Default)]
pub struct WorktreeState {
    pub key: Option<CacheKey>,
    pub diffs: Option<Diffs>,
    pub error: Option<GitError>,
    pub op: Option<&'static str>,
    pub stale: bool,
    pub file: usize,
    pub cursor: usize,
    pub scroll: usize,
    pub review: Review,
    pub outdated: usize,
}

impl WorktreeState {
    fn current_file(&self) -> Option<&FileDiff> {
        self.diffs.as_ref()?.get(self.file)
    }

    /// Rows in the current file: one per hunk header plus one per line.
    fn row_count(&self) -> usize {
        self.current_file().map_or(0, |f| f.hunks.iter().map(|h| 1 + h.lines.len()).sum())
    }

    /// Installs a reloaded diff, staying on the same file. If that file has
    /// gone, the index stays and the position starts at the top of whatever
    /// file is there now.
    fn replace_diffs(&mut self, diffs: Diffs) {
        let old = self.diffs.replace(diffs);
        if let Some(previous) = old.as_ref().and_then(|d| d.get(self.file)) {
            match self.find_file(previous) {
                Some(i) => self.file = i,
                None => {
                    self.cursor = 0;
                    self.scroll = 0;
                }
            }
        }
        self.clamp_position();
    }

    /// A typechange gives two entries with one path, so the status decides
    /// between them. A file whose status changed still matches by path.
    fn find_file(&self, wanted: &FileDiff) -> Option<usize> {
        let diffs = self.diffs.as_ref()?;
        diffs.iter().position(|f| f.path == wanted.path && f.status == wanted.status)
            .or_else(|| diffs.iter().position(|f| f.path == wanted.path))
    }

    /// Pulls the position back inside a diff that has just been replaced.
    fn clamp_position(&mut self) {
        self.file = self.file.min(self.diffs.as_ref().map_or(0, |d| d.len().saturating_sub(1)));
        self.cursor = self.cursor.min(self.row_count().saturating_sub(1));
        self.scroll = self.scroll.min(self.cursor);
    }
}

pub struct App {
    pub base: String,
    pub worktrees: Vec<Worktree>,
    pub states: Vec<WorktreeState>,
    pub selected: usize,
    pub focus: Focus,
    pub view: DiffView,
    pub help_open: bool,
    pub input: Option<Input>,
    pub notice: Option<String>,
    pub fatal: Option<GitError>,
    pub should_quit: bool,
}

/// Answers whether `modifiers` mark a shortcut rather than typed text.
/// crossterm's Windows parser reports AltGr as Ctrl+Alt and keeps the printed
/// character, and layouts such as German need AltGr for `[` and `]`.
fn is_shortcut(modifiers: KeyModifiers, windows: bool) -> bool {
    let shortcut = KeyModifiers::CONTROL | KeyModifiers::ALT;
    modifiers.intersects(shortcut) && !(windows && modifiers.contains(shortcut))
}

impl App {
    pub fn new(base: String) -> Self {
        Self {
            base,
            worktrees: Vec::new(),
            states: Vec::new(),
            selected: 0,
            focus: Focus::Worktrees,
            view: DiffView::Unified,
            help_open: false,
            input: None,
            notice: None,
            fatal: None,
            should_quit: false,
        }
    }

    pub fn selected_worktree(&self) -> Option<&Worktree> {
        self.worktrees.get(self.selected)
    }

    pub fn current(&self) -> Option<&WorktreeState> {
        self.states.get(self.selected)
    }

    pub fn current_mut(&mut self) -> Option<&mut WorktreeState> {
        self.states.get_mut(self.selected)
    }

    pub fn file_count(&self) -> usize {
        self.current().and_then(|s| s.diffs.as_ref()).map_or(0, |d| d.len())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Vec::new();
        }
        if is_shortcut(key.modifiers, cfg!(windows)) {
            return Vec::new();
        }
        // Holding a movement key would otherwise wipe a notice before it's read.
        if !matches!(key.code, KeyCode::Char('j' | 'k') | KeyCode::Up | KeyCode::Down) {
            self.notice = None;
        }
        // Checked before switching, because `]` and digits are text here.
        if self.input.is_some() {
            return self.handle_input_key(key);
        }
        // Switching works from any screen.
        if self.switch_worktree(key.code) {
            return Vec::new();
        }
        if self.help_open {
            if matches!(key.code, KeyCode::Char('?') | KeyCode::Esc) {
                self.help_open = false;
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.help_open = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Worktrees => Focus::Files,
                    Focus::Files => Focus::Diff,
                    Focus::Diff => Focus::Worktrees,
                };
            }
            KeyCode::Char('n') => self.step_file(1),
            KeyCode::Char('p') => self.step_file(-1),
            KeyCode::Char('j') | KeyCode::Down => self.move_vertical(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_vertical(-1),
            KeyCode::Char('v') => {
                self.view = match self.view {
                    DiffView::Unified => DiffView::SideBySide,
                    DiffView::SideBySide => DiffView::Unified,
                };
            }
            KeyCode::Char('r') => return self.reload_selected(),
            KeyCode::Char('c') => self.start_comment(),
            KeyCode::Char('d') => return self.delete_comment(),
            KeyCode::Char('D') => self.start_clear(),
            KeyCode::Char('x') => return self.toggle_reviewed(),
            KeyCode::Char('e') => return self.open_editor(),
            KeyCode::Char('y') => return self.export(Effect::CopyExport),
            KeyCode::Char('Y') => return self.export(Effect::WriteExport),
            _ => {}
        }
        Vec::new()
    }

    /// Answers whether `code` is a worktree switching key.
    fn switch_worktree(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char(']') => self.step_worktree(1),
            KeyCode::Char('[') => self.step_worktree(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.worktrees.len() {
                    self.selected = i;
                }
            }
            _ => return false,
        }
        true
    }

    fn reload_selected(&mut self) -> Vec<Effect> {
        let base = self.base.clone();
        // The list too, since a branch switch inside a worktree changes its row.
        // With nothing selected, this is the only way to retry a failed list.
        let mut effects = vec![Effect::ReloadWorktrees];
        if let Some(path) = self.selected_worktree().map(|w| w.path.clone()) {
            effects.push(Effect::Load { path, base });
        }
        effects
    }

    fn step_worktree(&mut self, delta: isize) {
        let n = self.worktrees.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(n) as usize;
        }
    }

    fn step_file(&mut self, delta: isize) {
        let n = self.file_count() as isize;
        if let Some(s) = self.current_mut() {
            if n > 0 {
                let file = (s.file as isize + delta).clamp(0, n - 1) as usize;
                if file != s.file {
                    s.file = file;
                    s.cursor = 0;
                    s.scroll = 0;
                }
            }
        }
    }

    fn move_vertical(&mut self, delta: isize) {
        match self.focus {
            Focus::Worktrees => {
                let max = self.worktrees.len().saturating_sub(1);
                self.selected = (self.selected as isize + delta).clamp(0, max as isize) as usize;
            }
            Focus::Files => self.step_file(delta),
            Focus::Diff => {
                let max = self.diff_row_count().saturating_sub(1);
                if let Some(s) = self.current_mut() {
                    s.cursor = (s.cursor as isize + delta).clamp(0, max as isize) as usize;
                }
            }
        }
    }

    /// Rows in the current file: one per hunk header plus one per line.
    pub fn diff_row_count(&self) -> usize {
        self.current().map_or(0, WorktreeState::row_count)
    }

    pub fn current_file(&self) -> Option<&FileDiff> {
        self.current()?.current_file()
    }

    pub fn handle_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Worktrees(Ok(list)) => {
                self.fatal = None;
                self.apply_worktrees(list)
            }
            Msg::Worktrees(Err(e)) => {
                self.fatal = Some(e);
                Vec::new()
            }
            Msg::Loaded { path, result } => {
                if let Some(i) = self.index_of(&path) {
                    let s = &mut self.states[i];
                    match result {
                        Ok(l) => {
                            s.key = Some(l.key);
                            s.replace_diffs(l.diffs);
                            s.outdated = count_outdated(s);
                            s.op = l.op;
                            s.error = None;
                            s.stale = false;
                        }
                        Err(e) => s.error = Some(e),
                    }
                }
                Vec::new()
            }
            Msg::Stale(path) => {
                if let Some(i) = self.index_of(&path) {
                    self.states[i].stale = true;
                }
                Vec::new()
            }
            Msg::WorktreesChanged => vec![Effect::ReloadWorktrees],
            Msg::Branches(_) | Msg::Highlighted { .. } => Vec::new(),
        }
    }

    fn index_of(&self, path: &std::path::Path) -> Option<usize> {
        self.worktrees.iter().position(|w| w.path == path)
    }

    /// Replaces the list, keeping per-worktree state by path and following
    /// the selected worktree to its new position.
    fn apply_worktrees(&mut self, list: Vec<Worktree>) -> Vec<Effect> {
        let selected_path = self.selected_worktree().map(|w| w.path.clone());

        let mut old: Vec<(PathBuf, WorktreeState)> = self.worktrees.drain(..)
            .map(|w| w.path)
            .zip(self.states.drain(..))
            .collect();

        let mut effects = Vec::new();
        for w in &list {
            let state = match old.iter().position(|(p, _)| *p == w.path) {
                Some(i) => old.swap_remove(i).1,
                None => {
                    effects.push(Effect::Load { path: w.path.clone(), base: self.base.clone() });
                    WorktreeState::default()
                }
            };
            self.states.push(state);
        }
        self.worktrees = list;

        self.selected = match selected_path {
            Some(path) => match self.index_of(&path) {
                Some(i) => i,
                None => {
                    self.notice = Some(format!("Worktree removed: {}", path.display()));
                    // It has no target now, and would act on the next worktree.
                    if matches!(self.input, Some(Input::Comment { .. } | Input::ConfirmClear)) {
                        self.input = None;
                    }
                    self.selected.min(self.worktrees.len().saturating_sub(1))
                }
            },
            None => 0,
        };
        effects
    }

    /// The diff line under the cursor. `None` on a hunk header.
    pub fn line_at_cursor(&self) -> Option<&crate::diff::Line> {
        let (hunk, index) = self.hunk_at_cursor()?;
        hunk.lines.get(index?)
    }

    /// The hunk under the cursor and the line's index in it, `None` on its header.
    fn hunk_at_cursor(&self) -> Option<(&Hunk, Option<usize>)> {
        let mut row = self.current()?.cursor;
        for hunk in &self.current_file()?.hunks {
            if row == 0 {
                return Some((hunk, None));
            }
            row -= 1;
            if row < hunk.lines.len() {
                return Some((hunk, Some(row)));
            }
            row -= hunk.lines.len();
        }
        None
    }

    fn start_comment(&mut self) {
        let Some(path) = self.current_file().map(|f| f.path.clone()) else { return };
        let Some(line) = self.line_at_cursor() else {
            self.notice = Some("Move to a diff line to comment".into());
            return;
        };
        let Some((no, side)) = line_position(line) else { return };
        let line_text = line.text.clone();
        self.input = Some(Input::Comment { path, line: no, side, line_text, text: String::new() });
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if matches!(self.input, Some(Input::ConfirmClear)) {
            self.input = None;
            return if key.code == KeyCode::Char('y') { self.clear_comments() } else { Vec::new() };
        }
        let Some(text) = self.input.as_mut().and_then(Input::text_mut) else { return Vec::new() };
        match key.code {
            KeyCode::Char(c) => text.push(c),
            KeyCode::Backspace => { text.pop(); }
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                if let Some(Input::Comment { path, line, side, line_text, text }) = self.input.take() {
                    if !text.trim().is_empty() {
                        if let Some(s) = self.current_mut() {
                            s.review.comments.push(Comment { path, line, side, line_text, body: text });
                            s.outdated = count_outdated(s);
                            return self.save_effect();
                        }
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Adds pasted text to the open prompt. Pastes arrive whole, so a newline
    /// in one can't submit the prompt or run the rest as hotkeys.
    pub fn paste(&mut self, text: &str) {
        if self.help_open {
            return;
        }
        if let Some(open) = self.input.as_mut().and_then(Input::text_mut) {
            open.push_str(&single_line(text));
        }
    }

    /// The comment on the line under the cursor, newest first when there are several.
    pub fn comment_at_cursor(&self) -> Option<&Comment> {
        let path = &self.current_file()?.path;
        let (line, side) = line_position(self.line_at_cursor()?)?;
        self.current()?.review.comments.iter().rev()
            .find(|c| &c.path == path && c.line == line && c.side == side)
    }

    /// Positions in the current file that have comments, for the diff's markers.
    pub fn commented_lines(&self) -> HashSet<(u32, Side)> {
        let (Some(s), Some(file)) = (self.current(), self.current_file()) else { return HashSet::new() };
        s.review.comments.iter().filter(|c| c.path == file.path).map(|c| (c.line, c.side)).collect()
    }

    fn delete_comment(&mut self) -> Vec<Effect> {
        let Some(comment) = self.comment_at_cursor().cloned() else {
            self.notice = Some("No comment on this line".into());
            return Vec::new();
        };
        let Some(s) = self.current_mut() else { return Vec::new() };
        if let Some(i) = s.review.comments.iter().rposition(|c| *c == comment) {
            s.review.comments.remove(i);
        }
        s.outdated = count_outdated(s);
        self.save_effect()
    }

    fn start_clear(&mut self) {
        if self.current().is_some_and(|s| !s.review.comments.is_empty()) {
            self.input = Some(Input::ConfirmClear);
        } else {
            self.notice = Some("No comments to clear".into());
        }
    }

    fn clear_comments(&mut self) -> Vec<Effect> {
        let Some(s) = self.current_mut() else { return Vec::new() };
        s.review.comments.clear();
        s.outdated = 0;
        self.save_effect()
    }

    /// Built when the edit happens, because later keys in the same batch can
    /// change the selection before `main` performs this.
    fn save_effect(&self) -> Vec<Effect> {
        match (self.selected_worktree(), self.current()) {
            (Some(w), Some(s)) => vec![Effect::SaveReview { id: ReviewId::of(w), review: s.review.clone() }],
            _ => Vec::new(),
        }
    }

    fn toggle_reviewed(&mut self) -> Vec<Effect> {
        let Some((path, hash)) = self.current_file().map(|f| (f.path.clone(), content_hash(f))) else {
            return Vec::new();
        };
        let Some(s) = self.current_mut() else { return Vec::new() };
        // Compare hashes: a mark left over from older content shows as unreviewed.
        if s.review.reviewed.get(&path) == Some(&hash) {
            s.review.reviewed.remove(&path);
        } else {
            s.review.reviewed.insert(path, hash);
        }
        self.save_effect()
    }

    pub fn is_reviewed(&self, index: usize) -> bool {
        let Some(s) = self.current() else { return false };
        let Some(f) = s.diffs.as_ref().and_then(|d| d.get(index)) else { return false };
        s.review.reviewed.get(&f.path) == Some(&content_hash(f))
    }

    /// Counted on load and when comments change, because this runs every frame.
    pub fn outdated_comments(&self) -> usize {
        self.current().map_or(0, |s| s.outdated)
    }

    fn export(&mut self, effect: fn(String) -> Effect) -> Vec<Effect> {
        let Some(w) = self.selected_worktree() else { return Vec::new() };
        let branch = w.branch.clone().unwrap_or_else(|| w.head.clone());
        let Some(s) = self.current() else { return Vec::new() };
        let md = crate::export::render(&branch, &self.base, &s.review.comments);
        if md.is_empty() {
            self.notice = Some("No comments to export".into());
            return Vec::new();
        }
        vec![effect(md)]
    }

    fn open_editor(&mut self) -> Vec<Effect> {
        let Some(root) = self.selected_worktree().map(|w| w.path.clone()) else { return Vec::new() };
        let Some(file) = self.current_file() else { return Vec::new() };
        let path = root.join(&file.path);
        if self.is_gone(file) {
            self.notice = Some("File was deleted on this branch".into());
            return Vec::new();
        }
        let line = self.hunk_at_cursor().map_or(1, |(hunk, index)| nearest_new_line(hunk, index));
        vec![Effect::OpenEditor { path, line }]
    }

    /// Answers whether opening `file` would create it. A typechange, or a
    /// deleted file recreated as untracked, also has an entry for what's there.
    fn is_gone(&self, file: &FileDiff) -> bool {
        let diffs = self.current().and_then(|s| s.diffs.as_ref());
        file.status == Status::Deleted
            && !diffs.is_some_and(|d| d.iter().any(|f| f.path == file.path && f.status != Status::Deleted))
    }
}

/// Makes pasted text fit a one-line prompt: a run of line breaks becomes one
/// space, and so does every other control character but tab. Export writes
/// comment bodies as they are, so a kept newline could start a Markdown heading.
fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_break = false;
    for c in text.chars() {
        let is_break = matches!(c, '\r' | '\n');
        if !(is_break && after_break) {
            out.push(if c.is_control() && c != '\t' { ' ' } else { c });
        }
        after_break = is_break;
    }
    out
}

/// The line to open for a position in `hunk`, since a removed line or a header
/// has no line of its own in the new file. `index` is `None` on the header.
fn nearest_new_line(hunk: &Hunk, index: Option<usize>) -> u32 {
    let (before, after) = hunk.lines.split_at(index.unwrap_or(0));
    after.iter().find_map(|l| l.new_no)
        .or_else(|| before.iter().rev().find_map(|l| l.new_no))
        .unwrap_or_else(|| new_side_start(&hunk.header))
}

/// The `c` in `@@ -a,b +c,d @@`. It's 0 when the new file has no lines at all.
fn new_side_start(header: &str) -> u32 {
    let start = header.split_once(" +").and_then(|(_, rest)| rest.split([',', ' ']).next());
    start.and_then(|n| n.parse().ok()).unwrap_or(1).max(1)
}

/// Identifies a file's content cheaply, since this runs for every file on
/// every frame. Files with no `content_id`, such as mode-only changes, pure
/// renames and dirty submodules, fall back to hashing hunks.
///
/// Saved in reviews, so it uses `fnv1a`, which is stable across Rust releases.
fn content_hash(file: &crate::diff::FileDiff) -> u64 {
    if let Some(id) = &file.content_id {
        return fnv1a(id.as_bytes());
    }
    let mut bytes = Vec::new();
    for line in file.hunks.iter().flat_map(|h| &h.lines) {
        bytes.push(line.kind as u8);
        bytes.extend_from_slice(line.text.as_bytes());
        bytes.push(b'\n');
    }
    fnv1a(&bytes)
}

/// Keeps the last save per review, since each one costs an fsync and holding
/// `x` would otherwise queue saves faster than they finish.
pub fn coalesce_saves(effects: Vec<Effect>) -> Vec<Effect> {
    let mut seen = HashSet::new();
    let mut kept: Vec<Effect> = effects.into_iter().rev().filter(|e| match e {
        Effect::SaveReview { id, .. } => seen.insert(id.clone()),
        _ => true,
    }).collect();
    kept.reverse();
    kept
}

/// Where a diff line sits for a comment: the new file's number, or the old
/// file's for a removed line.
pub fn line_position(line: &crate::diff::Line) -> Option<(u32, Side)> {
    match (line.new_no, line.old_no) {
        (Some(n), _) => Some((n, Side::New)),
        (None, Some(o)) => Some((o, Side::Old)),
        (None, None) => None,
    }
}

/// Counts comments whose line text is no longer at that line in the diff.
fn count_outdated(s: &WorktreeState) -> usize {
    let Some(diffs) = s.diffs.as_ref() else { return 0 };
    s.review.comments.iter().filter(|c| {
        // Filter rather than find: a typechange gives two entries with one path.
        let current = diffs.iter()
            .filter(|f| f.path == c.path)
            .flat_map(|f| f.hunks.iter().flat_map(|h| &h.lines))
            .find(|l| match c.side {
                Side::New => l.new_no == Some(c.line),
                Side::Old => l.new_no.is_none() && l.old_no == Some(c.line),
            })
            .map(|l| l.text.as_str());
        c.is_outdated(current)
    }).count()
}

#[cfg(test)]
mod tests {
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
        a.focus = Focus::Diff;
        press(&mut a, 'j'); // Row 0 is the hunk header, so row 1 is the line.
        a
    }

    fn type_comment(a: &mut App, text: &str) -> Vec<Effect> {
        press(a, 'c');
        for ch in text.chars() {
            press(a, ch);
        }
        a.handle_key(KeyEvent::from(KeyCode::Enter))
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
        a.focus = Focus::Diff;
        press(&mut a, 'j');
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
        a.focus = Focus::Diff;
        press(&mut a, 'j');
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

    fn comment_prompt_with(typed: &str) -> App {
        let mut a = app_on_line();
        press(&mut a, 'c');
        for c in typed.chars() {
            press(&mut a, c);
        }
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
    fn a_paste_while_help_is_open_changes_nothing() {
        let mut a = comment_prompt_with("kept");
        a.help_open = true;
        a.paste("more");
        assert!(matches!(&a.input, Some(Input::Comment { text, .. }) if text == "kept"));
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
}
