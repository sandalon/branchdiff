use crate::cache::{CacheKey, Diffs};
use crate::diff::FileDiff;
use crate::git::{GitError, Worktree};
use crate::store::Review;
use crate::worker::Msg;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus { Worktrees, Files, Diff }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView { Unified, SideBySide }

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Load { path: PathBuf, base: String },
    ReloadWorktrees,
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
    pub notice: Option<String>,
    pub fatal: Option<GitError>,
    pub should_quit: bool,
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
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return Vec::new();
        }
        // Holding a movement key would otherwise wipe a notice before it's read.
        if !matches!(key.code, KeyCode::Char('j' | 'k') | KeyCode::Up | KeyCode::Down) {
            self.notice = None;
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
            KeyCode::Char(']') => self.step_worktree(1),
            KeyCode::Char('[') => self.step_worktree(-1),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.worktrees.len() {
                    self.selected = i;
                }
            }
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
            _ => {}
        }
        Vec::new()
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
        let old_selected = self.selected;
        let removed = selected_path.as_ref().filter(|p| !list.iter().any(|w| &w.path == *p)).cloned();

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

        self.selected = match (&selected_path, &removed) {
            (Some(_), Some(gone)) => {
                self.notice = Some(format!("Worktree removed: {}", gone.display()));
                old_selected.min(self.worktrees.len().saturating_sub(1))
            }
            (Some(p), None) => self.index_of(p).unwrap_or(0),
            _ => 0,
        };
        effects
    }
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
        a.handle_msg(Msg::Worktrees(Ok(names.iter().map(|n| wt(n)).collect())));
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

    #[test]
    fn worktree_list_requests_a_load_for_each_worktree() {
        let mut a = App::new("develop".into());
        let fx = a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("b")])));
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
        deliver(&mut a, "a", vec![file("1"), file("2"), file("3")]);
        press(&mut a, 'n');
        press(&mut a, 'n');
        press(&mut a, ']');
        press(&mut a, '[');
        assert_eq!(a.current().unwrap().file, 2);
    }

    #[test]
    fn new_worktree_appears_and_selection_follows_its_path() {
        let mut a = app(&["a", "b"]);
        press(&mut a, ']');
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("aa"), wt("b")])));
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("b"));
    }

    #[test]
    fn removed_selected_worktree_shows_notice_and_moves_on() {
        let mut a = app(&["a", "b", "c"]);
        press(&mut a, ']');
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a"), wt("c")])));
        assert_eq!(a.selected_worktree().unwrap().branch.as_deref(), Some("c"));
        assert!(a.notice.as_deref().unwrap().contains('b'));
    }

    #[test]
    fn stale_message_badges_but_does_not_replace_the_diff() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file("1")]);
        let fx = a.handle_msg(Msg::Stale("/r/a".into()));
        assert!(a.current().unwrap().stale);
        assert_eq!(a.current().unwrap().diffs.as_ref().unwrap().len(), 1);
        assert!(fx.is_empty());
    }

    #[test]
    fn r_requests_a_reload_of_the_selected_worktree() {
        let mut a = app(&["a"]);
        let fx = press(&mut a, 'r');
        assert!(fx.contains(&Effect::ReloadWorktrees));
        assert!(fx.iter().any(|e| matches!(e, Effect::Load { .. })));
    }

    #[test]
    fn a_later_worktree_list_clears_a_failed_one() {
        let mut a = App::new("develop".into());
        a.handle_msg(Msg::Worktrees(Err(crate::git::GitError::Failed("boom".into()))));
        assert!(a.fatal.is_some());
        a.handle_msg(Msg::Worktrees(Ok(vec![wt("a")])));
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
        a.current_mut().unwrap().cursor = 5;
        press(&mut a, 'n');
        assert_eq!(a.current().unwrap().cursor, 5);
        press(&mut a, 'p');
        assert_eq!(a.current().unwrap().cursor, 0);
        a.current_mut().unwrap().cursor = 5;
        press(&mut a, 'p');
        assert_eq!(a.current().unwrap().cursor, 5);
    }

    #[test]
    fn a_reload_keeps_the_position_inside_a_shorter_file() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_rows("1", 100)]);
        let s = a.current_mut().unwrap();
        (s.cursor, s.scroll) = (50, 40);
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
        let s = a.current_mut().unwrap();
        (s.cursor, s.scroll) = (50, 40);
        deliver(&mut a, "a", vec![file_with_rows("1", 3)]);
        let s = a.current().unwrap();
        assert_eq!((s.file, s.cursor, s.scroll), (0, 0, 0));
        deliver(&mut a, "a", vec![]);
        let s = a.current().unwrap();
        assert_eq!((s.file, s.cursor, s.scroll), (0, 0, 0));
    }

    fn at(a: &mut App, cursor: usize, scroll: usize) {
        let s = a.current_mut().unwrap();
        (s.cursor, s.scroll) = (cursor, scroll);
    }

    fn position(a: &App) -> (usize, usize, usize) {
        let s = a.current().unwrap();
        (s.file, s.cursor, s.scroll)
    }

    #[test]
    fn a_reload_follows_the_current_file_past_an_inserted_file() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_rows("b", 100), file_with_rows("c", 100)]);
        press(&mut a, 'n');
        at(&mut a, 50, 40);
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
        at(&mut a, 50, 40);
        deliver(&mut a, "a", vec![file_with_rows("b", 100), file_with_rows("c", 100), file_with_rows("d", 100)]);
        assert_eq!(position(&a), (1, 50, 40));
        assert_eq!(a.current_file().unwrap().path, "c");
    }

    #[test]
    fn a_followed_file_that_shrank_keeps_its_position_valid() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_rows("b", 100)]);
        at(&mut a, 50, 40);
        deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("b", 5)]);
        assert_eq!(position(&a), (1, 4, 4));
    }

    #[test]
    fn a_reload_without_the_current_file_keeps_the_index_and_resets_the_position() {
        let mut a = app(&["a"]);
        deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("b", 100), file_with_rows("c", 100)]);
        press(&mut a, 'n');
        at(&mut a, 50, 40);
        deliver(&mut a, "a", vec![file_with_rows("a", 100), file_with_rows("c", 100)]);
        assert_eq!(position(&a), (1, 0, 0));
        at(&mut a, 50, 40);
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
        at(&mut a, 5, 2);
        deliver(&mut a, "a", vec![file_with_rows("0", 5), file_with_rows("a", 5), side(Status::Deleted), side(Status::Added)]);
        assert_eq!(position(&a), (2, 5, 2));
    }

    fn with(c: char, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), modifiers)
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
        a.current_mut().unwrap().cursor = 50;
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
            press(&mut a, c);
        }
    }
}
