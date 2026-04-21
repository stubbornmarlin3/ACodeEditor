use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event};
use notify::{EventKind, RecommendedWatcher, Watcher};

use crate::git::GitSnapshot;

pub enum AppEvent {
    Input(Event),
    Redraw,
    /// Fresh git snapshot loaded off the UI thread.
    GitRefresh(GitSnapshot),
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
/// Filters out events we don't consume (mouse, focus gain/lost, paste)
/// at the source — some terminals (notably Windows Terminal) flush a
/// burst of focus/mouse events on tab refocus, and every queued event
/// costs a wake + redraw downstream. Only keys and resizes reach the
/// app loop.
pub fn start_input_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(ev) => {
                    let keep = matches!(ev, Event::Key(_) | Event::Resize(_, _));
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

/// Periodic background git refresh. Loads a fresh `GitSnapshot` every
/// `interval` and ships it to the app. Runs serially (no debounce
/// needed): if a load takes longer than the interval, the next one just
/// starts later.
pub fn start_git_refresh_thread(tx: Sender<AppEvent>, interval: Duration) {
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let snap = GitSnapshot::load(&cwd);
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
