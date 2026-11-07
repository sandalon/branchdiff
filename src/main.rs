use anyhow::{bail, Context, Result};
use arboard::Clipboard;
use branchdiff::app::{coalesce_saves, App, Effect};
use branchdiff::cache::CacheKey;
use branchdiff::git::{self, GitError, RepoInfo};
use branchdiff::store::{ReviewId, Store};
use branchdiff::worker::{Job, Lists, Loads, Msg, Pool, Probes, Reaction, WORKER_THREAD};
use branchdiff::{config, editor, signals, ui, watch};
use clap::Parser;
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{enable_raw_mode, EnterAlternateScreen};
use ratatui::DefaultTerminal;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Base branch to compare against
    #[arg(long)]
    base: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let info = match git::discover(&cwd) {
        Ok(i) => i,
        Err(e @ (GitError::GitMissing | GitError::NotARepo)) => bail!("{e}"),
        Err(e) => return Err(e).context("could not read the repository"),
    };
    let (configured, config_problem) = match config::configured_base(&info.common_dir) {
        Ok(base) => (base, None),
        Err(notice) => (None, Some(notice)),
    };
    let base = cli.base.or(configured).unwrap_or_else(|| "develop".into());

    signals::catch_quit_signals();
    let mut terminal = ratatui::try_init().context("could not start the terminal")?;
    keep_terminal_on_worker_panic();
    set_bracketed_paste(true);
    let result = run(&mut terminal, info, base, config_problem);
    let _ = terminal.show_cursor();
    set_bracketed_paste(false);
    // Not `ratatui::restore` or the terminal's drop, which both panic writing
    // their own error to a terminal that has hung up.
    let _ = ratatui::try_restore();
    std::mem::forget(terminal);
    result
}

const MAX_HIGHLIGHTS: usize = 2;

/// Terminals advertise this in `COLORTERM`. Windows Terminal doesn't, but sets
/// `WT_SESSION`.
fn supports_truecolor() -> bool {
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    colorterm.contains("truecolor") || colorterm.contains("24bit") || std::env::var_os("WT_SESSION").is_some()
}

/// Without this, a pasted newline arrives as Enter and the rest of the paste
/// runs as hotkeys. Best effort, because some terminals don't support it.
fn set_bracketed_paste(on: bool) {
    let mut out = std::io::stdout();
    let _ = if on { execute!(out, EnableBracketedPaste) } else { execute!(out, DisableBracketedPaste) };
}

/// ratatui's hook restores the terminal on a panic from any thread. Workers
/// catch their own panics and carry on, so the app must keep its screen.
fn keep_terminal_on_worker_panic() {
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some(WORKER_THREAD) {
            set_bracketed_paste(false);
            ratatui_hook(info);
        }
    }));
}

struct Runtime {
    pool: Pool,
    loads: Loads,
    probes: Probes,
    lists: Lists,
    store: Store,
    common_dir: PathBuf,
    /// Opened on first copy, then kept alive because on Linux this process
    /// hosts the copied text.
    clipboard: Option<Clipboard>,
    paused: Arc<AtomicBool>,
    repo: PathBuf,
    watch: Option<Sender<watch::Command>>,
    highlighting: HashSet<(CacheKey, usize)>,
}

impl Runtime {
    fn repo_dir(&self, app: &App) -> PathBuf {
        app.repo_dir(&self.repo, &self.common_dir)
    }
}

/// Polls rather than blocking in `read`, so it can stop consuming keys while
/// an editor owns the terminal.
fn spawn_input(tx: Sender<Event>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || loop {
        if paused.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if !event::poll(Duration::from_millis(20)).unwrap_or(false) {
            continue;
        }
        let Ok(ev) = event::read() else { break };
        if tx.send(ev).is_err() {
            break;
        }
    });
}

/// Sends Ctrl-C after a quit signal, because that quits from every state, so
/// `run` ends the normal way once any editor has finished. Separate from
/// `spawn_input` because crossterm's poll never returns once the terminal hangs up.
fn spawn_quit_watch(tx: Sender<Event>) {
    std::thread::spawn(move || {
        while !signals::quit_requested() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = tx.send(Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    });
}

fn run(terminal: &mut DefaultTerminal, info: RepoInfo, base: String, notice: Option<String>) -> Result<()> {
    let (msg_tx, msg_rx) = unbounded::<Msg>();
    let (key_tx, key_rx) = unbounded::<Event>();
    let paused = Arc::new(AtomicBool::new(false));
    spawn_quit_watch(key_tx.clone());
    spawn_input(key_tx, paused.clone());

    // Workers mostly wait on git, so few CPUs is no reason for few threads.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().clamp(4, 8));
    let watcher = watch::spawn(info.common_dir.clone(), msg_tx.clone());
    let watch_failed = watcher.is_err();
    let mut rt = Runtime {
        pool: Pool::new(threads, msg_tx.clone()),
        loads: Loads::default(),
        probes: Probes::default(),
        lists: Lists::default(),
        store: Store::new(&info.common_dir),
        common_dir: info.common_dir.clone(),
        clipboard: None,
        paused,
        repo: info.toplevel.clone(),
        watch: watcher.ok(),
        highlighting: HashSet::new(),
    };
    let mut app = App::new(base);
    app.home = std::env::home_dir();
    app.truecolor = supports_truecolor();
    app.notice = notice;
    if watch_failed {
        app.notice = Some("File watching is unavailable, so the changed badge will not appear".into());
    }
    request_list(&app, &mut rt);

    loop {
        terminal.draw(|f| ui::draw(f, &mut app))?;
        if app.should_quit {
            return Ok(());
        }
        let effects = next_effects(&mut app, &mut rt, &key_rx, &msg_rx)?;
        for effect in coalesce_saves(effects) {
            perform(effect, &mut app, &mut rt, terminal)?;
        }
        if let Some(effect) = app.highlight_effect() {
            perform(effect, &mut app, &mut rt, terminal)?;
        }
    }
}

/// Blocks for one event, then drains anything else queued, so a burst of key
/// repeats or results produces one redraw rather than many.
fn next_effects(app: &mut App, rt: &mut Runtime, keys: &Receiver<Event>, msgs: &Receiver<Msg>) -> Result<Vec<Effect>> {
    let mut effects = Vec::new();
    select! {
        recv(keys) -> ev => effects.extend(on_event(app, ev?)),
        recv(msgs) -> msg => effects.extend(on_msg(app, rt, msg?)),
        default(Duration::from_secs(60)) => {}
    }
    while let Ok(ev) = keys.try_recv() {
        effects.extend(on_event(app, ev));
    }
    while let Ok(msg) = msgs.try_recv() {
        effects.extend(on_msg(app, rt, msg));
    }
    Ok(effects)
}

fn on_event(app: &mut App, ev: Event) -> Vec<Effect> {
    match ev {
        Event::Key(k) if k.kind != KeyEventKind::Release => app.handle_key(k),
        Event::Paste(text) => {
            app.paste(&text);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn request_list(app: &App, rt: &mut Runtime) {
    if rt.lists.request() {
        rt.pool.submit(Job::ListWorktrees { cwd: rt.repo_dir(app) });
    }
}

/// Finds out whether a worktree really changed before showing it.
fn report_change(app: &mut App, rt: &mut Runtime, path: &Path) {
    match rt.probes.on_change(rt.loads.is_running(path), app.key_for(path)) {
        Reaction::Submit(job) => rt.pool.submit(job),
        Reaction::Badge => drop(app.handle_msg(Msg::Stale(path.to_path_buf()))),
        Reaction::Nothing => {}
    }
}

fn on_msg(app: &mut App, rt: &mut Runtime, msg: Msg) -> Vec<Effect> {
    if let Msg::Stale(path) = &msg {
        rt.loads.mark_changed(path);
        report_change(app, rt, path);
        return Vec::new();
    }
    if let Msg::Probed { path, changed } = &msg {
        let done = rt.probes.finished(path, *changed, app.key_for(path));
        if let Some(job) = done.next {
            rt.pool.submit(job);
        }
        if done.raise {
            app.handle_msg(Msg::Stale(path.clone()));
        }
        return Vec::new();
    }
    if matches!(msg, Msg::Worktrees(_)) && rt.lists.finished() {
        rt.pool.submit(Job::ListWorktrees { cwd: rt.repo_dir(app) });
    }
    let mut changed_during_load = None;
    if let Msg::Loaded { path, .. } = &msg {
        let done = rt.loads.finished(path);
        if let Some(job) = done.next {
            rt.pool.submit(job);
        }
        if done.superseded {
            return Vec::new();
        }
        if done.changed {
            changed_during_load = Some(path.clone());
        }
    }
    let effects = match msg {
        Msg::Worktrees(Ok(list)) => {
            if let Some(tx) = &rt.watch {
                let _ = tx.send(watch::Command::SetWorktrees(list.iter().map(|w| w.path.clone()).collect()));
            }
            // A branch switch keeps the path but changes the review, and saving
            // the old one would overwrite the new branch's comments.
            let reload: Vec<bool> = list
                .iter()
                .map(|w| !app.worktrees.iter().any(|o| o.path == w.path && ReviewId::of(o) == ReviewId::of(w)))
                .collect();
            let effects = app.handle_msg(Msg::Worktrees(Ok(list)));
            for (i, needed) in reload.into_iter().enumerate() {
                if needed {
                    let id = ReviewId::of(&app.worktrees[i]);
                    let loaded = rt.store.load(&id);
                    if let Some(path) = loaded.moved_aside {
                        app.notice = Some(format!("Review could not be read, kept at {}", path.display()));
                    } else if let Some(path) = loaded.left_in_place {
                        app.notice = Some(format!("Review could not be read or moved, still at {}", path.display()));
                    }
                    app.set_review(i, loaded.review);
                }
            }
            effects
        }
        Msg::Highlighted { ref key, file_index, .. } => {
            // A reload with an unchanged key must be able to highlight again.
            rt.highlighting.remove(&(key.clone(), file_index));
            app.handle_msg(msg)
        }
        other => app.handle_msg(other),
    };
    // A loaded result clears the badge, so check for edits it can't include.
    if let Some(path) = changed_during_load {
        report_change(app, rt, &path);
    }
    effects
}

fn perform(effect: Effect, app: &mut App, rt: &mut Runtime, terminal: &mut DefaultTerminal) -> Result<()> {
    match effect {
        Effect::Load { path, base } => {
            if let Some(job) = rt.loads.request(path, base) {
                rt.pool.submit(job);
            }
        }
        Effect::ReloadWorktrees => request_list(app, rt),
        Effect::ListBranches => rt.pool.submit(Job::ListBranches { cwd: rt.repo_dir(app) }),
        Effect::Highlight { key, file_index, diffs } => {
            // Holding `n` would otherwise queue a job per file passed and
            // bury refreshes. A skipped file is requested again on return.
            if rt.highlighting.len() < MAX_HIGHLIGHTS && rt.highlighting.insert((key.clone(), file_index)) {
                rt.pool.submit(Job::Highlight { key, file_index, diffs });
            }
        }
        Effect::SaveReview { id, review } => {
            if let Err(e) = rt.store.save(&id, &review) {
                app.notice = Some(format!("Could not save review: {e}"));
            }
        }
        Effect::CopyExport(md) => {
            if rt.clipboard.is_none() {
                rt.clipboard = Clipboard::new().ok();
            }
            let copied = rt.clipboard.as_mut().is_some_and(|c| c.set_text(md.clone()).is_ok());
            app.notice = Some(if copied {
                "Copied review comments".to_string()
            } else {
                format!("Clipboard unavailable. {}", write_export(rt, &md))
            });
        }
        Effect::WriteExport(md) => app.notice = Some(write_export(rt, &md)),
        Effect::OpenEditor { path, line } => open_editor(terminal, &path, line, app, rt)?,
    }
    Ok(())
}

/// Writes under the git common dir, so an export never appears as a change
/// in the worktree being reviewed.
fn write_export(rt: &Runtime, md: &str) -> String {
    let dir = rt.common_dir.join("branchdiff/exports");
    let path = dir.join("latest.md");
    match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, md)) {
        Ok(()) => format!("Wrote {}", path.display()),
        Err(e) => format!("Could not write export: {e}"),
    }
}

fn open_editor(terminal: &mut DefaultTerminal, path: &Path, line: u32, app: &mut App, rt: &Runtime) -> Result<()> {
    let chosen = editor::default_editor(git::core_editor(&rt.repo_dir(app)));
    let (prog, args) = editor::command(&chosen, path, line);
    rt.paused.store(true, Ordering::SeqCst);
    let status = editor::without_interrupts(|| -> Result<_> {
        terminal.show_cursor()?;
        set_bracketed_paste(false);
        let _ = ratatui::try_restore();
        let status = std::process::Command::new(&prog).args(&args).status();
        // Not `ratatui::init`, which would stack a new panic hook over ours.
        enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        set_bracketed_paste(true);
        Ok(status)
    })?;
    terminal.clear()?;
    rt.paused.store(false, Ordering::SeqCst);
    match status {
        Ok(s) if !s.success() => app.notice = Some(format!("{prog} exited with {s}")),
        Err(e) => app.notice = Some(format!("Could not start {prog}: {e}")),
        Ok(_) => {}
    }
    Ok(())
}
