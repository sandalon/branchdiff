use crate::cache::{CacheKey, Diffs};
use crate::diff::{FileDiff, Hunk, Status};
use crate::git::{GitError, Worktree};
use crate::highlight::HighlightedFile;
use crate::store::{fnv1a, Comment, Review, ReviewId, Side};
use crate::worker::Msg;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus { Worktrees, Files, Diff }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView { Unified, SideBySide }

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Load { path: PathBuf, base: String },
    ReloadWorktrees,
    ListBranches,
    SaveReview { id: ReviewId, review: Review },
    CopyExport(String),
    WriteExport(String),
    OpenEditor { path: PathBuf, line: u32 },
    Highlight { key: CacheKey, file_index: usize, diffs: Diffs },
}

/// A text prompt. While set, keys go here rather than to hotkeys.
pub enum Input {
    Comment { path: String, line: u32, side: Side, line_text: String, text: String },
    Worktree { query: String },
    Base { query: String, branches: Vec<String> },
    ConfirmClear,
}

impl Input {
    /// The text that keys and pastes edit, if this prompt has any.
    fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Comment { text, .. } => Some(text),
            Self::Worktree { query } | Self::Base { query, .. } => Some(query),
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
    pub highlights: HashMap<usize, HighlightedFile>,
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
            KeyCode::Char('b') => return vec![Effect::ListBranches],
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
            KeyCode::Char('w') => {
                self.help_open = false;
                self.input = Some(Input::Worktree { query: String::new() });
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

    /// Requests colour for the current file if it does not have it yet.
    pub fn highlight_effect(&self) -> Option<Effect> {
        let s = self.current()?;
        // A worktree with no changes has no file to highlight.
        s.diffs.as_ref()?.get(s.file)?;
        if s.highlights.contains_key(&s.file) {
            return None;
        }
        Some(Effect::Highlight { key: s.key.clone()?, file_index: s.file, diffs: s.diffs.clone()? })
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
                            if s.key.as_ref() != Some(&l.key) {
                                s.highlights.clear();
                            }
                            s.key = Some(l.key);
                            s.replace_diffs(l.diffs);
                            s.outdated = count_outdated(s);
                            s.op = l.op;
                            s.error = None;
                            s.stale = false;
                        }
                        Err(e) => {
                            // A result for an older base can arrive after the base has changed.
                            let missing = matches!(&e, GitError::BaseMissing(b) if *b == self.base);
                            s.error = Some(e);
                            if missing && self.input.is_none() {
                                return vec![Effect::ListBranches];
                            }
                        }
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
            Msg::Branches(Ok(branches)) => {
                // A late reply must not wipe a half-written comment.
                if self.input.is_none() {
                    self.help_open = false;
                    self.input = Some(Input::Base { query: String::new(), branches });
                }
                Vec::new()
            }
            Msg::Branches(Err(e)) => {
                self.notice = Some(e.to_string());
                Vec::new()
            }
            Msg::Highlighted { key, file_index, lines } => {
                if let Some(s) = self.states.iter_mut().find(|s| s.key.as_ref() == Some(&key)) {
                    s.highlights.insert(file_index, lines);
                }
                Vec::new()
            }
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
            KeyCode::Enter => return self.submit_input(),
            _ => {}
        }
        Vec::new()
    }

    pub fn worktree_labels(&self) -> Vec<String> {
        self.worktrees.iter().map(worktree_label).collect()
    }

    fn submit_input(&mut self) -> Vec<Effect> {
        match self.input.take() {
            Some(Input::Comment { path, line, side, line_text, text }) if !text.trim().is_empty() => {
                if let Some(s) = self.current_mut() {
                    s.review.comments.push(Comment { path, line, side, line_text, body: text });
                    s.outdated = count_outdated(s);
                    return self.save_effect();
                }
            }
            Some(Input::Worktree { query }) => {
                if let Some(i) = top_match(&query, &self.worktree_labels()) {
                    self.selected = i;
                }
            }
            Some(Input::Base { query, mut branches }) => {
                if let Some(i) = top_match(&query, &branches) {
                    self.base = branches.swap_remove(i);
                    return self.reload_all();
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn reload_all(&mut self) -> Vec<Effect> {
        // Each diff is against the old base until its load lands.
        for s in &mut self.states {
            s.stale = true;
        }
        let selected = self.selected_worktree().map(|w| w.path.clone());
        let mut paths: Vec<PathBuf> = self.worktrees.iter().map(|w| w.path.clone()).collect();
        // The worktree on screen first, since the pool runs jobs in order.
        paths.sort_by_key(|p| Some(p) != selected.as_ref());
        paths.into_iter().map(|path| Effect::Load { path, base: self.base.clone() }).collect()
    }

    /// Adds pasted text to the open prompt. Pastes arrive whole, so a newline
    /// in one can't submit the prompt or run the rest as hotkeys.
    pub fn paste(&mut self, text: &str) {
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

/// What Enter takes in a filtering prompt. A blank query cancels.
fn top_match(query: &str, items: &[String]) -> Option<usize> {
    if query.trim().is_empty() {
        return None;
    }
    crate::fuzzy::filter(query, items).first().copied()
}

pub fn worktree_label(w: &Worktree) -> String {
    w.branch.clone().unwrap_or_else(|| {
        let short: String = w.head.chars().take(8).collect();
        format!("({short})")
    })
}

/// Makes pasted text fit a one-line prompt: a run of line breaks becomes one
/// space, and so does every other control character but tab. Export writes
/// comment bodies as they are, so a kept newline could start a Markdown heading.
fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut after_break = false;
    for c in text.chars() {
        let is_break = matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}');
        if !(is_break && after_break) {
            out.push(if is_break || (c.is_control() && c != '\t') { ' ' } else { c });
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
mod tests;
