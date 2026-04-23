use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

/// Global "a Redraw event is already queued" flag. A chatty PTY reading
/// 4KB at a time can fire hundreds of Redraws a second — the main loop
/// coalesces them on the receive side, but the channel still pays
/// allocation + cross-thread wake for each. This flag lets senders
/// short-circuit enqueuing when one is already in flight. Main loop
/// clears it on pop.
pub static REDRAW_PENDING: AtomicBool = AtomicBool::new(false);

/// Queue a Redraw event only if one isn't already pending. Safe to call
/// from any thread. Uses a CAS so concurrent senders don't both slip
/// past the check. Returns false on channel disconnect so readers can
/// shut down.
pub fn send_redraw_coalesced(tx: &Sender<AppEvent>) -> bool {
    if REDRAW_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return true;
    }
    if tx.send(AppEvent::Redraw).is_err() {
        REDRAW_PENDING.store(false, Ordering::Release);
        return false;
    }
    true
}

/// Main-loop hook: mark the Redraw slot free so the next PTY burst can
/// enqueue one again. Called on pop of `AppEvent::Redraw`.
pub fn notice_redraw_drained() {
    REDRAW_PENDING.store(false, Ordering::Release);
}

use crossterm::event::{self, Event, MouseEventKind};
use notify::{EventKind, RecommendedWatcher, Watcher};

use crate::git::{GitSnapshot, MultiRepo};
use crate::projects::RailRefresh;

pub enum AppEvent {
    Input(Event),
    Redraw,
    /// Fresh status snapshot for the active project's root-cwd repo,
    /// loaded off the UI thread. Cheap (one repo per tick); the app
    /// merges it into whichever repo is at the cwd so nested repos'
    /// cached state isn't clobbered.
    GitRefresh(GitSnapshot),
    /// Startup-time handoff: the full MultiRepo (active project) plus
    /// per-project rail states, all loaded off the UI thread so the
    /// first frame renders instantly with empty dots and fills in when
    /// discovery completes. Project switches can also post this to
    /// avoid blocking the UI for long walks.
    GitBootstrap {
        multi: MultiRepo,
        rail:  Vec<RailRefresh>,
    },
    /// Rail-only refresh: recomputes per-project state dots without
    /// rebuilding the active `MultiRepo`. Cheaper than `GitBootstrap`
    /// because it skips the active-repo walk the passive GitRefresh
    /// already handles.
    RailRefresh(Vec<RailRefresh>),
    /// Result of a backgrounded `git <sub>` shell-out (push/pull/fetch).
    /// Ok is a one-line summary for the statusbar; Err is the same with
    /// stderr's first line.
    GitCmdResult(Result<String, String>),
    /// Timer tick asking the UI thread to re-scan the file tree.
    /// Retained as a slow fallback for paths not under a watched root
    /// (ad-hoc files outside any project).
    ExplorerTick,
    /// Filesystem change reported by the native watcher. One event per
    /// affected path. Callers dedupe/coalesce as they like.
    FsChange(PathBuf),
}

/// Forward crossterm events into the app channel. Thread ends when the
/// receiver drops `tx`.
///
/// Filters out events we don't consume (focus gain/lost, paste, and
/// mouse-drag/move) at the source — some terminals (notably Windows
/// Terminal) flush a burst of focus/mouse events on tab refocus, and
/// every queued event costs a wake + redraw downstream. Keys, resizes,
/// and the mouse events we act on (clicks + scroll) reach the app loop.
pub fn start_input_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(ev) => {
                    let keep = match &ev {
                        Event::Key(_) | Event::Resize(_, _) => true,
                        Event::Mouse(m) => matches!(
                            m.kind,
                            MouseEventKind::Down(_)
                                | MouseEventKind::Up(_)
                                | MouseEventKind::ScrollUp
                                | MouseEventKind::ScrollDown
                                | MouseEventKind::ScrollLeft
                                | MouseEventKind::ScrollRight,
                        ),
                        _ => false,
                    };
                    if !keep {
                        continue;
                    }
                    if tx.send(AppEvent::Input(ev)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// One-shot background bootstrap: walk the active project's tree for
/// nested repos (the pricey part) and compute rail state dots for every
/// open project, off the UI thread. The app starts with empty repos +
/// `None` dots; this fills them in when discovery finishes. Cheap to
/// call again on project switch to avoid blocking the UI.
/// Background rail-only refresh. Complements the passive `GitRefresh`
/// tick: the periodic thread handles the active repo, this handles the
/// project rail dots. Separate so each can be throttled / skipped
/// independently.
pub fn spawn_rail_refresh(tx: Sender<AppEvent>, rail_roots: Vec<std::path::PathBuf>) {
    if rail_roots.is_empty() {
        return;
    }
    thread::spawn(move || {
        let rail = crate::projects::ProjectList::compute_rail(&rail_roots);
        let _ = tx.send(AppEvent::RailRefresh(rail));
    });
}

pub fn spawn_git_bootstrap(
    tx: Sender<AppEvent>,
    active_root: std::path::PathBuf,
    rail_roots: Vec<std::path::PathBuf>,
) {
    thread::spawn(move || {
        let multi = MultiRepo::discover(&active_root);
        let rail = crate::projects::ProjectList::compute_rail(&rail_roots);
        let _ = tx.send(AppEvent::GitBootstrap { multi, rail });
    });
}

/// Periodic background git refresh. Loads a fresh `GitSnapshot` every
/// `interval` and ships it to the app. Runs serially (no debounce
/// needed): if a load takes longer than the interval, the next one just
/// starts later.
pub fn start_git_refresh_thread(tx: Sender<AppEvent>, interval: Duration) {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            // Single-repo passive load — skips branches + stash (the
            // git panel only needs those on user action, which goes
            // through the full `refresh_git`). The expensive nested-
            // repo walk happens on project switch or explicit refresh,
            // not on every tick.
            let snap = GitSnapshot::load_passive(&cwd);
            if tx.send(AppEvent::GitRefresh(snap)).is_err() {
                break;
            }
        }
    });
}

/// Periodic explorer-panel rescan tick. We send a plain signal rather than
/// a preloaded tree — the UI thread owns `FileTree` state (selection,
/// expansion) and does the re-scan itself.
pub fn start_explorer_tick_thread(tx: Sender<AppEvent>, interval: Duration) {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            if tx.send(AppEvent::ExplorerTick).is_err() {
                break;
            }
        }
    });
}

/// Construct a native filesystem watcher whose events fan in to the
/// app channel as `AppEvent::FsChange(path)`. The returned watcher
/// must be kept alive for the lifetime of the app — dropping it stops
/// the internal watcher thread and no more events arrive. Callers
/// attach paths via `watcher.watch(path, mode)`.
pub fn start_fs_watcher(tx: Sender<AppEvent>) -> Option<RecommendedWatcher> {
    let res = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return; };
        // Filter out pure metadata/access noise. Content-relevant kinds
        // are Modify (content or rename), Create, Remove, and Any.
        if !is_relevant(&ev.kind) {
            return;
        }
        for path in ev.paths {
            let _ = tx.send(AppEvent::FsChange(path));
        }
    });
    res.ok()
}

fn is_relevant(kind: &EventKind) -> bool {
    use notify::event::{ModifyKind, MetadataKind};
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Data(_))      => true,
        EventKind::Modify(ModifyKind::Name(_))      => true,
        EventKind::Modify(ModifyKind::Any)          => true,
        // Ignore pure metadata changes (access time, permissions) —
        // they're spammy and don't alter file content.
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))   => false,
        EventKind::Modify(ModifyKind::Metadata(MetadataKind::Permissions))  => false,
        EventKind::Modify(ModifyKind::Metadata(_))                          => false,
        EventKind::Modify(ModifyKind::Other)                                => false,
        EventKind::Any | EventKind::Other | EventKind::Access(_)            => false,
    }
}

/// Convenience wrapper for watching a single path. Errors swallowed —
/// file-watching is best-effort; polling + user action still work if
/// the OS refuses a watch.
pub fn watch_path(watcher: &mut RecommendedWatcher, path: &std::path::Path, recursive: bool) {
    use notify::RecursiveMode;
    let mode = if recursive { RecursiveMode::Recursive } else { RecursiveMode::NonRecursive };
    let _ = watcher.watch(path, mode);
}

#[allow(dead_code)]
pub fn unwatch_path(watcher: &mut RecommendedWatcher, path: &std::path::Path) {
    let _ = watcher.unwatch(path);
}
