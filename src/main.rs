mod app;
mod cell;
mod completion;
mod config;
mod conflict;
mod diff;
mod editor;
mod events;
mod explorer;
mod git;
mod projects;
mod session;
mod session_state;
mod status;
mod syntax;
mod theme;
mod ui;
mod wrap;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use app::{App, ExplorerMode, FocusId, Mode, Startup, StartupKind};
use cell::{Cell, Session};
use editor::Editor;
use events::AppEvent;

fn main() -> Result<()> {
    let startup = parse_argv();

    // Panic hook: restore the terminal before the default hook prints
    // the backtrace, so the user actually sees it instead of raw-mode
    // garbage. Must be installed before `ratatui::init()` so a panic
    // anywhere in setup is covered.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        reset_terminal_title();
        default_hook(info);
    }));

    let mut terminal = ratatui::init();

    let (tx, rx) = mpsc::channel::<AppEvent>();
    events::start_input_thread(tx.clone());
    events::start_git_refresh_thread(tx.clone(), std::time::Duration::from_secs(3));
    // FS watcher drives real-time explorer/editor refresh. This tick
    // stays as a slow safety net for paths outside any watched root
    // (the OS occasionally drops events too — rare but real).
    events::start_explorer_tick_thread(tx.clone(), std::time::Duration::from_secs(5));

    let cli_files = startup.files.clone();
    let mut app = App::new(tx.clone(), startup);
    spawn_initial_cells(&mut app, &cli_files);
    app.explorer.refresh(&app.projects, app.cells.len());
    // Hook the FS watcher to every project root + ad-hoc file parent.
    app.refresh_watchers();

    // Catch panics from `run()` so `persist_cells()` still fires —
    // otherwise unwinding blows past the save below and the last
    // in-flight mutation never reaches `.acedata`. `App` only lends
    // the closure a `&mut` borrow, so on unwind it stays valid in
    // this frame and we can persist from it safely.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut terminal, &mut app, &rx)
    }));
    ratatui::restore();
    reset_terminal_title();
    // Final persistence runs regardless of how `run()` returned — clean
    // quit, channel hangup, I/O error, or panic. Skips silently for
    // rootless (files-only) sessions.
    app.persist_cells();
    match result {
        Ok(r)  => r,
        // The panic hook already printed the trace. Exit with the
        // conventional panic code so shell scripts can detect it.
        Err(_) => std::process::exit(101),
    }
}

/// Parse `ace [paths…]`:
///   - zero args                 → GlobalList, or CwdOnly if `.acerc` has cwd_only=true
///   - any existing dir arg      → Explicit { dirs }
///   - any file arg (existing or not) → files list
///   - mix of dirs and files is supported
/// Non-existent paths are classified as files (they'll be created on
/// first save, matching normal editor behaviour).
fn parse_argv() -> Startup {
    let mut dirs:  Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "--version" || arg == "-V" {
            println!("ace {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        if arg == "--update" || arg == "-u" {
            // Optional next arg is a tag. Anything starting with `-`
            // (another flag) or `.`/`/` or an existing path doesn't
            // count — treat the update as "latest" in that case.
            let tag = match args.peek() {
                Some(s) if !s.starts_with('-') && !s.contains(std::path::MAIN_SEPARATOR) && !s.contains('/') => {
                    Some(args.next().unwrap())
                }
                _ => None,
            };
            run_update(tag.as_deref());
            std::process::exit(0);
        }
        let p = PathBuf::from(&arg);
        match std::fs::metadata(&p) {
            Ok(md) if md.is_dir() => dirs.push(p),
            _                     => files.push(p),
        }
    }

    let kind = if dirs.is_empty() && files.is_empty() {
        // No args — `.acerc on_launch` chooses between the three
        // behaviours. Default is `welcome` so new users land somewhere
        // that explains how to proceed; `cwd` and `global` are opt-in
        // for people who want their last-opened setup.
        let cfg = config::Config::load();
        match cfg.on_launch.as_deref().map(str::trim) {
            Some("cwd") | Some("cwd_only") => StartupKind::CwdOnly,
            Some("global")                  => StartupKind::GlobalList,
            _                               => StartupKind::Welcome,
        }
    } else {
        StartupKind::Explicit(dirs)
    };
    Startup { kind, files }
}

/// First launch: pick the right initial-cells story based on startup:
///   1. CLI file args → one editor cell per file (no .acedata).
///   2. Active project with `.acedata` → restore cells from the
///      snapshot (PTYs respawn fresh in their recorded slots).
///   3. Otherwise → a single scratch editor, as before.
fn spawn_initial_cells(app: &mut App, cli_files: &[PathBuf]) {
    if !cli_files.is_empty() {
        for path in cli_files {
            // Absolutize so paths survive a later `cd` (e.g. on
            // `:proj add`). Otherwise relative paths would resolve
            // against the new cwd.
            let path = std::path::absolute(path).unwrap_or_else(|_| path.clone());
            let mut ed = Editor::empty();
            // `ed.load` now tolerates a nonexistent path by flipping
            // is_new on — the buffer comes up empty with the `[NEW]`
            // badge, and `:w` creates the file. A real load failure
            // (permission, IO) still errors silently here; users will
            // see the buffer empty and can retry with `:e` later.
            let _ = ed.load(&path);
            app.cells.push(Cell::with_session(Session::Edit(ed)));
            if app.cells.len() >= app::MAX_CELLS {
                break;
            }
        }
        app.set_focus(FocusId::Cell(0));
        return;
    }

    if let Some(root) = app.current_project_root() {
        if let Some(snap) = App::load_acedata(&root) {
            if app.restore_cells_from_snapshot(&snap) > 0 {
                apply_initial_focus(app, snap.focus);
                return;
            }
        }
    }

    // No files, no restored session — land on the welcome page. First
    // keystroke demotes it to a regular `unknown [NEW]` scratch.
    app.cells.push(Cell::with_session(Session::Edit(Editor::welcome())));
    apply_initial_focus(app, None);
}

/// Restore the last-focused target on startup. If the saved index
/// still points at a live cell, land there. Otherwise prefer the
/// explorer — unless it's hidden, in which case any first cell is a
/// safer landing spot than an invisible panel.
fn apply_initial_focus(app: &mut App, saved: Option<usize>) {
    let target = match saved {
        Some(i) if i < app.cells.len() => FocusId::Cell(i),
        _ if app.explorer_hidden && !app.cells.is_empty() => FocusId::Cell(0),
        _ => FocusId::Explorer,
    };
    app.set_focus(target);
}

/// Invoke the cargo-dist installer for the requested tag (or latest).
/// On Windows: `iwr | iex` the PowerShell installer. On Unix: pipe the
/// shell installer through `sh`. The installer drops a fresh `ace`
/// binary into the standard location and upgrades this one in place.
fn run_update(tag: Option<&str>) {
    const OWNER: &str = "stubbornmarlin3";
    const REPO:  &str = "ACodeEditor";
    // Package name (installer asset prefix) comes from Cargo — matches
    // whatever cargo-dist uploads.
    const PKG: &str = "acodeeditor";

    let (os_name, installer_ext, installer_name) = if cfg!(target_os = "windows") {
        ("windows", "ps1", format!("{PKG}-installer.ps1"))
    } else {
        ("unix", "sh", format!("{PKG}-installer.sh"))
    };

    // Normalize tag: users can pass `v0.2.0` or `0.2.0` — GitHub release
    // tags in this repo are the version with or without a `v` prefix;
    // fall back to `latest` when no tag is supplied.
    let url = match tag {
        Some(t) => {
            let t = t.trim();
            // `latest` keyword resolves to the latest release via the
            // aliased `releases/latest` URL, which the installer script
            // redirects through correctly.
            if t.eq_ignore_ascii_case("latest") || t.is_empty() {
                format!("https://github.com/{OWNER}/{REPO}/releases/latest/download/{installer_name}")
            } else {
                format!("https://github.com/{OWNER}/{REPO}/releases/download/{t}/{installer_name}")
            }
        }
        None => format!("https://github.com/{OWNER}/{REPO}/releases/latest/download/{installer_name}"),
    };

    let label = tag.unwrap_or("latest");
    eprintln!("ace: updating to {label} via {os_name} installer…");
    eprintln!("     {url}");

    // The installer overwrites this very binary. If we `status()` and
    // wait, the running ace.exe holds a lock on its own image and the
    // installer's rename fails on Windows. Spawn the installer detached
    // with a small leading delay, then return so main() exits and the
    // OS releases the file handle before the installer writes.
    let spawn_result = if cfg!(target_os = "windows") {
        // Launch in a new PowerShell window so the installer's output
        // isn't mashed into the shell prompt that'll reappear as soon
        // as ace exits. The outer PowerShell just fires Start-Process
        // and returns — the inner one does the work and pauses at the
        // end so the user sees the result.
        let inner = format!(
            "Start-Sleep -Seconds 2; try {{ irm {url} | iex }} \
             catch {{ Write-Host \"ace: installer failed: $_\" -ForegroundColor Red }}; \
             Write-Host ''; Write-Host 'Press Enter to close.'; \
             [void][System.Console]::ReadLine()"
        );
        // Base64-encode the inner command to avoid quoting pain when
        // passing it through Start-Process -ArgumentList.
        let utf16: Vec<u8> = inner.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        let encoded = base64_encode(&utf16);
        let outer = format!(
            "Start-Process powershell -ArgumentList \
             '-NoProfile','-ExecutionPolicy','Bypass','-EncodedCommand','{encoded}'"
        );
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &outer])
            .spawn()
    } else {
        // sh -c composes the pipeline; the leading sleep gives ace time
        // to exit before the installer's rename. Prefer curl, fall back
        // to wget. setsid detaches from our controlling terminal so the
        // installer survives even if this process' tty goes away.
        let cmd = format!(
            "sleep 1; curl --proto '=https' --tlsv1.2 -LsSf {url} | sh \
             || wget -qO- {url} | sh"
        );
        let mut c = std::process::Command::new("sh");
        c.args(["-c", &cmd]);
        c.spawn()
    };

    match spawn_result {
        Ok(_) => {
            eprintln!("ace: installer launched in a detached process.");
            eprintln!("ace: exiting now so the binary can be replaced.");
        }
        Err(e) => {
            eprintln!("ace: update failed to launch installer: {e}");
            let _ = installer_ext;
            std::process::exit(1);
        }
    }
}

/// Minimal base64 (standard alphabet, padded) — used only for packing
/// the PowerShell `-EncodedCommand` payload. Avoids pulling in a crate
/// for a handful of bytes.
fn base64_encode(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(A[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(A[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn current_term_rect() -> Rect {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    Rect { x: 0, y: 0, width: cols, height: rows }
}

fn resize_ptys_if_needed(app: &mut App, term: Rect) {
    let areas = ui::layout(term, app);
    for (i, cell) in app.cells.iter_mut().enumerate() {
        // Minimized cells get a zero rect from the layout. Resizing the
        // PTY down to (3, 20) makes the child reformat its buffer for a
        // tiny pane; on restore it's stuck with that narrow layout until
        // the next full redraw. Skip the resize and keep the PTY at its
        // last live size so claude/shell output stays readable when the
        // cell comes back.
        if cell.minimized {
            continue;
        }
        let rect = match areas.cells.get(i).copied() {
            Some(r) => r,
            None => continue,
        };
        let (rows, cols) = ui::inner_size(rect);
        for s in cell.sessions.iter_mut() {
            if let Some(pty) = s.as_pty_mut() {
                if pty.rows != rows || pty.cols != cols {
                    let _ = pty.resize(rows.max(3), cols.max(20));
                }
            }
        }
    }
}

fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<AppEvent>,
) -> Result<()> {
    resize_ptys_if_needed(app, current_term_rect());
    let mut last_title = String::new();
    update_terminal_title(app, &mut last_title);
    terminal.draw(|f| ui::draw(f, app))?;

    while !app.should_quit {
        // Block only as long as the next status message needs to stay
        // visible. While a confirm prompt is armed the message IS the
        // prompt — keep it pinned until the user answers. When the
        // queue is empty there's nothing to tick, so fall back to a
        // long wait (the input/fs/git threads will wake us before it
        // elapses).
        let timeout = if app.pending_confirm.is_some() {
            Duration::from_secs(3600)
        } else {
            app.status
                .next_tick_in(Instant::now())
                .unwrap_or(Duration::from_secs(3600))
        };
        match rx.recv_timeout(timeout) {
            Ok(AppEvent::Input(Event::Key(k))) => handle_key(app, k),
            Ok(AppEvent::Input(Event::Resize(w, h))) => {
                let r = Rect { x: 0, y: 0, width: w, height: h };
                resize_ptys_if_needed(app, r);
            }
            Ok(AppEvent::Input(_)) => {}
            Ok(AppEvent::Redraw)   => {}
            Ok(AppEvent::GitRefresh(snap)) => {
                app.set_git_snapshot(snap);
                // Piggyback on the git tick to refresh other projects'
                // state dots too — cheap (at our scale) and keeps the
                // rail honest without a second timer thread.
                app.projects.refresh_states();
            }
            Ok(AppEvent::GitCmdResult(r)) => {
                match r {
                    Ok(msg)  => app.status.push_auto(msg),
                    Err(msg) => app.status.push_auto(msg),
                }
                app.refresh_git();
            }
            Ok(AppEvent::ExplorerTick) => {
                // Slow fallback — FS events handle the real-time path.
                // State persistence is handled by the end-of-loop
                // hash-diff check, so nothing to do here for .acedata.
                app.pending_explorer_refresh = true;
            }
            Ok(AppEvent::FsChange(path)) => {
                // Real-time: a file changed under a watched root. Reconcile
                // the affected editors immediately (cheap and path-scoped)
                // but coalesce the explorer re-scan: a single save can emit
                // several FS events and a full tree walk per event adds up.
                app.handle_fs_change(&path);
                app.pending_explorer_refresh = true;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Wake for a status-bar tick. The tick itself runs
                // below (shared with every other loop iteration).
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Advance past any status messages whose 3-second window has
        // elapsed; this runs every loop iteration so expiry fires even
        // if the wake came from input/fs/git rather than a timeout. A
        // pending confirm pins its prompt until answered.
        if app.pending_confirm.is_none() {
            app.status.tick(Instant::now());
        }
        // Reap any shell/claude PTYs whose child has exited. Runs every
        // tick so a `exit`/`Ctrl-D` in a shell (or Claude quitting on
        // its own) closes the cell promptly instead of leaving a dead
        // pane the user has to `:q`.
        let reaped = app.reap_exited_ptys();
        if reaped > 0 {
            app.status.push_auto(format!(
                "closed {} exited session{}",
                reaped,
                if reaped == 1 { "" } else { "s" },
            ));
        }
        // One coalesced explorer refresh per loop iteration, no matter
        // how many FS events fed the flag. Cheap when the flag is clear.
        if std::mem::take(&mut app.pending_explorer_refresh) {
            app.explorer.refresh(&app.projects, app.cells.len());
        }
        // Keep all PTYs in sync with current layout — explorer panel width,
        // cell count, and active layout algorithm all affect cell geometry.
        resize_ptys_if_needed(app, current_term_rect());
        // Realtime state save: any structural change (cell add/remove,
        // session rotation, path swap) is hashed and persisted now.
        // Content-only edits leave the hash unchanged and skip the write.
        app.persist_cells_if_dirty();
        // No cells left → nothing to edit/run. Quit on the next loop
        // iteration. Doing this after the tick lets the final "closed
        // last cell" status message get drawn before we exit.
        if app.cells.is_empty() {
            app.should_quit = true;
        }
        sync_syntax(app);
        update_terminal_title(app, &mut last_title);
        terminal.draw(|f| ui::draw(f, app))?;
    }
    Ok(())
}

/// Push an OSC 2 title to the outer terminal reflecting the focused
/// cell: `Ace | {title}`. Skips the write when the title hasn't changed
/// so we don't spam the tty with escape codes every frame.
fn update_terminal_title(app: &App, last: &mut String) {
    use std::io::Write;
    let label = current_cell_title(app);
    let desired = if label.is_empty() { "Ace".to_string() } else { format!("Ace | {label}") };
    if desired == *last {
        return;
    }
    // OSC 2 — set window title. BEL terminator is more widely supported
    // than ST (some terminals, including older Windows Terminal, don't
    // honour ESC \). Errors are swallowed — title is a nicety, never a
    // blocker.
    let seq = format!("\x1b]2;{desired}\x07");
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(seq.as_bytes());
    let _ = stdout.flush();
    *last = desired;
}

/// Clear the OSC 2 window title we set while running. Most terminals
/// fall back to their default title (shell / tab profile) when they
/// receive an empty title string — without this, the terminal would
/// stay stamped with "Ace | …" after ace exits.
fn reset_terminal_title() {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b]2;\x07");
    let _ = stdout.flush();
}

/// Human-readable label for the currently-focused element: editor file
/// name, PTY program or child-provided title, diff/conflict title, or
/// `explorer` when the sidebar has focus.
fn current_cell_title(app: &App) -> String {
    match app.focus {
        FocusId::Explorer => "explorer".to_string(),
        FocusId::Cell(i) => {
            let Some(cell) = app.cells.get(i) else { return String::new(); };
            use cell::Session as S;
            match cell.active_session() {
                S::Edit(ed)      => ed.file_name().to_string(),
                S::Shell(p)      => {
                    let t = p.title();
                    if !t.trim().is_empty() { t } else {
                        std::path::Path::new(&p.program)
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("shell")
                            .to_string()
                    }
                }
                S::Claude(p)     => {
                    let t = p.title();
                    if !t.trim().is_empty() { t } else { "claude".to_string() }
                }
                S::Diff(v)       => format!("diff · {}", v.title),
                S::Conflict(v)   => format!("conflict · {}", v.title),
            }
        }
    }
}

/// Re-highlight any editor buffers whose content changed since the last draw.
fn sync_syntax(app: &mut App) {
    for cell in &mut app.cells {
        if let cell::Session::Edit(ed) = cell.active_session_mut() {
            if ed.syntax_stale {
                if let Some(sh) = ed.syntax.as_mut() {
                    sh.rehighlight(ed.textarea.lines());
                }
                ed.syntax_stale = false;
            }
        }
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }

    // Ctrl-C is routed to the focused cell — never quits the app.
    //   * PTY cell (claude / shell) → forward the byte (SIGINT etc.)
    //     regardless of mode so a hung command can always be cancelled.
    //   * editor in Insert → drop back to Normal (vim-like, safe).
    //   * anywhere else → swallow the key so a stray Ctrl-C can't
    //     lose work. Users quit with `:q` / `:Q`.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if let Some(pty) = app.focused_pty_mut() {
            let _ = pty.write(&[0x03]);
            return;
        }
        if matches!(app.mode, Mode::Insert) && app.focused_session_is_editor() {
            app.enter_normal();
        }
        return;
    }

    // Destructive action awaiting y/N. Intercepts before anything
    // else so a pending confirm can't be silently bypassed by a mode
    // transition or other dispatch.
    if app.pending_confirm.is_some() {
        if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT {
            let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
            app.resolve_confirm(yes);
        } else {
            // Modified keys (Ctrl-*) shouldn't silently cancel a
            // confirm — that'd be surprising. Drop the key but keep
            // the prompt armed so the user can still answer.
        }
        return;
    }

    // Esc in Insert / Visual → Normal, globally. Applies to editors
    // and PTY cells alike; PTY users can re-enter Insert with `i`/`a`
    // (the natural mode for a shell/claude cell is Insert, so
    // set_focus will also re-arm it automatically on re-entry). For
    // Visual, the editor's cancel_selection fires inside its own Esc
    // branch, so we just flip the mode here.
    if key.code == KeyCode::Esc
        && key.modifiers == KeyModifiers::NONE
        && matches!(app.mode, Mode::Insert | Mode::Visual { .. })
    {
        app.enter_normal();
        return;
    }

    app.status.clear();

    match app.mode {
        Mode::Normal        => handle_normal(app, key),
        Mode::Insert        => handle_insert(app, key),
        Mode::Visual {..}   => handle_visual(app, key),
        Mode::Command {..}  => handle_command(app, key),
        Mode::Password {..} => handle_password(app, key),
    }
}

/// Hidden-input prompt driven by :sudo / :w! / :x!. Chars append to the
/// password buffer without echoing to the screen; Enter submits (spawn
/// sudo); Esc cancels. Ctrl-C was already handled upstream (routed to a
/// focused PTY) so we don't need to treat it specially here.
fn handle_password(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc)       => app.password_cancel(),
        (_, KeyCode::Enter)     => app.password_submit(),
        (_, KeyCode::Backspace) => app.password_backspace(),
        (m, KeyCode::Char(c))
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            app.password_push(c);
        }
        _ => {}
    }
}

fn handle_visual(app: &mut App, key: KeyEvent) {
    // `:` from visual mode goes to command mode, same as Normal. The
    // selection stays on the editor until a `d`/`y`/`c`/motion key
    // lands, so the command runs with the selection still live —
    // handy for `:w` while visual is open.
    if key.code == KeyCode::Char(':') {
        app.enter_command();
        return;
    }

    // PTY cells have their own Visual-mode handler — motions + `y`
    // yank to clipboard. Editors follow the existing tui-textarea path
    // below.
    if app.focused_pty_mut().is_some() {
        match handle_pty_visual(app, key) {
            PtyAction::Consumed   => {}
            PtyAction::ExitNormal => { app.enter_normal(); }
            PtyAction::None       => {}
        }
        return;
    }

    let linewise = matches!(app.mode, Mode::Visual { linewise: true });
    let Some(ed) = app.focused_editor_mut() else {
        // No editor focus — visual doesn't apply here. Bail to Normal.
        app.mode = Mode::Normal;
        return;
    };
    use crate::editor::VisualAction;
    let act = ed.handle_visual(key, linewise);
    let msg = ed.take_status();
    match act {
        VisualAction::Stay => {}
        VisualAction::Exit => { app.mode = Mode::Normal; }
        VisualAction::ExitEnterInsert => { app.mode = Mode::Insert; }
    }
    if let Some(m) = msg { app.status.push_auto(m); }
}

/// Outcome of routing a key into the PTY's virtual-cursor layer.
#[derive(Copy, Clone, Debug)]
enum PtyAction {
    /// Key consumed, no mode change needed.
    Consumed,
    /// Consumed and the caller should transition to Normal.
    ExitNormal,
    /// Not consumed — caller should fall through to the default path.
    None,
}

/// Motion keys for PTY cells in Normal mode. Returns `Consumed` when
/// the key drove a virtual-cursor change or similar read-only action,
/// `None` when the key should fall through to the default PTY normal
/// handling (e.g. `i`/`a` → Insert). Visual-mode entry is signalled by
/// a mode transition written back to `app`.
fn handle_pty_normal(app: &mut App, key: KeyEvent) -> PtyAction {
    use KeyCode::*;
    use KeyModifiers as M;

    // Paste is tricky to do with the pty borrow held because it
    // reaches into the OS clipboard. Resolve it up front before
    // taking the &mut borrow.
    if key.modifiers == M::NONE && key.code == Char('p') {
        pty_paste_from_clipboard(app);
        return PtyAction::Consumed;
    }

    let Some(pty) = app.focused_pty_mut() else { return PtyAction::None; };
    let was_pending_g = std::mem::take(&mut pty.pending_g);

    match (key.modifiers, key.code) {
        (M::NONE, Char('h')) | (_, Left)  => { pty.vcursor_move_col(-1); PtyAction::Consumed }
        (M::NONE, Char('l')) | (_, Right) => { pty.vcursor_move_col( 1); PtyAction::Consumed }
        (M::NONE, Char('j')) | (_, Down)  => { pty.vcursor_move_row( 1); PtyAction::Consumed }
        (M::NONE, Char('k')) | (_, Up)    => { pty.vcursor_move_row(-1); PtyAction::Consumed }
        (M::NONE, Char('0')) | (_, Home)  => { pty.vcursor.col = 0; PtyAction::Consumed }
        (_, Char('$')) | (_, End)         => {
            let c = pty.cols.saturating_sub(1);
            pty.vcursor.col = c;
            PtyAction::Consumed
        }
        (_, Char('G'))                    => { pty.vcursor_jump_bottom(); PtyAction::Consumed }
        (M::NONE, Char('g'))              => {
            if was_pending_g { pty.vcursor_jump_top(); }
            else             { pty.pending_g = true; }
            PtyAction::Consumed
        }
        (M::CONTROL, Char('u'))           => { pty.vcursor_page(-1); PtyAction::Consumed }
        (M::CONTROL, Char('d'))           => { pty.vcursor_page( 1); PtyAction::Consumed }
        (M::NONE, Char('w'))              => { pty.vcursor_word_next(); PtyAction::Consumed }
        (M::NONE, Char('b'))              => { pty.vcursor_word_prev(); PtyAction::Consumed }
        (M::NONE, Char('v'))              => {
            pty.start_visual();
            app.mode = Mode::Visual { linewise: false };
            PtyAction::Consumed
        }
        _ => PtyAction::None,
    }
}

/// Visual-mode keys for PTY cells. Motions extend the selection; `y`
/// copies the selected span to the OS clipboard and exits to Normal;
/// `v` and Esc exit without copying (Esc is actually intercepted
/// upstream in `handle_key`).
fn handle_pty_visual(app: &mut App, key: KeyEvent) -> PtyAction {
    use KeyCode::*;
    use KeyModifiers as M;

    // `y` → yank + clipboard + exit. Do it before the &mut borrow so
    // we can report status via `app.status`.
    if key.modifiers == M::NONE && key.code == Char('y') {
        let text = app.focused_pty_mut()
            .map(|pty| pty.visual_selection_text())
            .unwrap_or_default();
        if text.is_empty() {
            app.status.push_auto("nothing to yank".into());
        } else {
            match clipboard_set(&text) {
                Ok(())  => app.status.push_auto(format!("yanked {} chars", text.chars().count())),
                Err(e)  => app.status.push_auto(format!("clipboard: {e}")),
            }
        }
        if let Some(pty) = app.focused_pty_mut() { pty.clear_visual(); }
        return PtyAction::ExitNormal;
    }

    // `v` from Visual exits without yanking.
    if key.modifiers == M::NONE && key.code == Char('v') {
        if let Some(pty) = app.focused_pty_mut() { pty.clear_visual(); }
        return PtyAction::ExitNormal;
    }

    // Motion keys reuse the Normal-mode handler — selection is
    // implicit (visual_anchor stays put, vcursor is what moves).
    // `p`, `i`, `a` are NOT valid in Visual; they fall through to
    // no-op (None).
    match (key.modifiers, key.code) {
        (M::NONE, Char('h')) | (_, Left)
        | (M::NONE, Char('l')) | (_, Right)
        | (M::NONE, Char('j')) | (_, Down)
        | (M::NONE, Char('k')) | (_, Up)
        | (M::NONE, Char('0')) | (_, Home)
        | (_, Char('$')) | (_, End)
        | (_, Char('G'))
        | (M::NONE, Char('g'))
        | (M::CONTROL, Char('u'))
        | (M::CONTROL, Char('d'))
        | (M::NONE, Char('w'))
        | (M::NONE, Char('b')) => {
            handle_pty_normal(app, key)
        }
        _ => PtyAction::None,
    }
}

/// Process-wide clipboard handle. `arboard::Clipboard` must be kept
/// alive across copy/paste ops so the OS clipboard state persists
/// between cells. Creating a fresh handle per operation can drop
/// copied content on some platforms (e.g. X11 without a running owner,
/// or arboard backends that tie lifetime to the handle).
///
/// Initialized lazily: the first yank or paste creates the handle. If
/// creation fails (headless Linux, no X/Wayland) we cache the error
/// and every subsequent op reports the same failure.
static CLIPBOARD: std::sync::OnceLock<std::sync::Mutex<Option<arboard::Clipboard>>> =
    std::sync::OnceLock::new();

fn with_clipboard<F, T>(f: F) -> Result<T, String>
where
    F: FnOnce(&mut arboard::Clipboard) -> Result<T, String>,
{
    let slot = CLIPBOARD.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = slot.lock().map_err(|e| e.to_string())?;
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    let clip = guard.as_mut().ok_or("clipboard unavailable")?;
    f(clip)
}

fn clipboard_set(text: &str) -> Result<(), String> {
    with_clipboard(|clip| clip.set_text(text.to_string()).map_err(|e| e.to_string()))
}

fn clipboard_get() -> Result<String, String> {
    with_clipboard(|clip| clip.get_text().map_err(|e| e.to_string()))
}

/// Hard cap on clipboard paste size. Above this we refuse the paste
/// rather than shove the whole blob into the PTY: a 1 GB clipboard
/// (common on Windows after a copy from a huge file) would otherwise
/// allocate a second copy here, then another inside vt100 as the child
/// echoes back, and stall the UI for seconds. 16 MB is comfortably more
/// than any realistic paste and well under "my editor froze" territory.
const CLIPBOARD_PASTE_MAX: usize = 16 * 1024 * 1024;

/// Paste clipboard text to the focused PTY. Uses the child's declared
/// bracketed-paste mode when set (so `vim`-like TUIs see a paste event
/// rather than characters typed one by one).
fn pty_paste_from_clipboard(app: &mut App) {
    let text = match clipboard_get() {
        Ok(t) if !t.is_empty() => t,
        Ok(_)                  => { app.status.push_auto("clipboard empty".into()); return; }
        Err(e)                 => { app.status.push_auto(format!("clipboard: {e}")); return; }
    };
    if text.len() > CLIPBOARD_PASTE_MAX {
        app.status.push_auto(format!(
            "clipboard too large ({} MB) — max {} MB",
            text.len() / (1024 * 1024),
            CLIPBOARD_PASTE_MAX / (1024 * 1024),
        ));
        return;
    }
    let Some(pty) = app.focused_pty_mut() else { return; };
    let bracketed = pty.parser.lock().map(|p| p.screen().bracketed_paste()).unwrap_or(false);
    let bytes = if bracketed {
        let mut v: Vec<u8> = Vec::with_capacity(text.len() + 12);
        v.extend_from_slice(b"\x1b[200~");
        v.extend_from_slice(text.as_bytes());
        v.extend_from_slice(b"\x1b[201~");
        v
    } else {
        text.into_bytes()
    };
    let _ = pty.write(&bytes);
    app.status.push_auto("pasted".into());
}

fn handle_normal(app: &mut App, key: KeyEvent) {
    // Space-armed jump mode. Three exits:
    //   * digit (0 → Explorer, 1..9 → Cell)      → jump + leave jump mode
    //   * Tab / Shift-Tab                         → cycle tabs in the
    //                                               focused cell, STAY in
    //                                               jump mode so repeated
    //                                               Tab rolls through all
    //                                               sessions.
    //   * Space again                             → leave jump mode.
    //   * anything else                           → leave jump mode, drop
    //                                               the key.
    if app.pending_jump {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(' ')) => {
                app.pending_jump = false;
                return;
            }
            (KeyModifiers::NONE, KeyCode::Char(c)) if c.is_ascii_digit() => {
                let d = c.to_digit(10).unwrap_or(0);
                app.pending_jump = false;
                app.jump_to_cell_by_digit(d);
                return;
            }
            (KeyModifiers::NONE, KeyCode::Tab) => {
                app.cycle_active_session(false);
                // stay armed
                return;
            }
            (m, KeyCode::BackTab) | (m, KeyCode::Tab) if m.contains(KeyModifiers::SHIFT) => {
                app.cycle_active_session(true);
                // stay armed
                return;
            }
            // Space → s → N  : swap content, focus stays at original slot.
            (KeyModifiers::NONE, KeyCode::Char('s')) => {
                app.pending_jump = false;
                app.arm_swap(false);
                return;
            }
            // Space → m → N  : move focus+content to target slot. Space
            // → m followed by any non-digit minimizes the focused cell
            // (see pending_swap handler below).
            (KeyModifiers::NONE, KeyCode::Char('m')) => {
                app.pending_jump = false;
                app.arm_swap(true);
                return;
            }
            // Space → q : quit the focused cell (shorthand for `:q`).
            (KeyModifiers::NONE, KeyCode::Char('q')) => {
                app.pending_jump = false;
                app.cmd_close(false);
                return;
            }
            _ => {
                app.pending_jump = false;
                return;
            }
        }
    }

    // Swap / move arm — primed by Space → s (pending_swap) or
    // Space → m (pending_swap_follow). The next digit picks the target:
    //   * 0        → minimize focused cell
    //   * 1..9     → swap / move with that cell
    // For `m` (pending_swap_follow) a non-digit key minimizes instead
    // of cancelling — so `Space m <anything>` is a one-chord minimize.
    // For `s` (pending_swap), non-digit cancels (original behaviour).
    if app.pending_swap || app.pending_swap_follow {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char(c)) if c.is_ascii_digit() => {
                let d = c.to_digit(10).unwrap_or(0);
                let follow = app.pending_swap_follow;
                app.pending_swap = false;
                app.pending_swap_follow = false;
                app.swap_focused_with_digit(d, follow);
                return;
            }
            _ => {
                let was_follow = app.pending_swap_follow;
                app.pending_swap = false;
                app.pending_swap_follow = false;
                if was_follow {
                    app.minimize_focused();
                }
                return;
            }
        }
    }

    // `:` always enters command mode regardless of focus / sub-mode.
    // Claimed up here so it can't be accidentally swallowed by a
    // future per-mode handler that matches on `Char(':')`.
    if key.code == KeyCode::Char(':') {
        app.enter_command();
        return;
    }

    // Esc in Normal on a PTY cell → pass a literal ESC byte to the
    // child. This is how Claude / shell users cancel their TUI input
    // without leaving Normal mode. Editors ignore it (editor Normal
    // already is the "out of insert" state).
    if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
        if let Some(pty) = app.focused_pty_mut() {
            let _ = pty.write(&[0x1b]);
            return;
        }
    }


    // Panel-local navigation — j/k/Enter and git keys when focused on
    // Explorer. Explorer owns j/k in Normal, so this has to run before the
    // editor motion handler below.
    if app.focus == FocusId::Explorer {
        if handle_explorer_key(app, key) {
            return;
        }
    }

    // Diff cell: scroll-only, owns j/k. Runs before the editor motion
    // handler so j/k go to the diff view rather than a hypothetical
    // editor scroll. Diff sessions never host an editor, so there's
    // no conflict with the `focused_editor_mut` branch.
    if handle_diff_cell(app, key) {
        return;
    }

    // Conflict cell: owns j/k (hunk nav) and o/t/b (resolution).
    if handle_conflict_cell(app, key) {
        return;
    }

    // Space-arm for `<leader>+digit` jumps. Matched up here so the
    // editor's normal-mode handler below doesn't swallow the space
    // (it's not a vim motion but we also don't want to leak it into
    // `insert a space`).
    if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Char(' ') {
        app.pending_jump = true;
        return;
    }

    // Editor-focused? Search keys have first claim — `n`/`N` in vim are
    // pure search-nav (not motions), and `/` / `?` open the search
    // prompt. Route them before the editor's normal-mode handler so
    // its `n`-as-motion-nothing path can't conflict.
    if app.focused_session_is_editor() {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('/')) => {
                app.enter_command_with("/"); return;
            }
            (KeyModifiers::NONE, KeyCode::Char('?')) => {
                app.enter_command_with("?"); return;
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => {
                if let Some(ed) = app.focused_editor_mut() { ed.search_next(false); }
                return;
            }
            (m, KeyCode::Char('N')) if m.contains(KeyModifiers::SHIFT) => {
                if let Some(ed) = app.focused_editor_mut() { ed.search_next(true); }
                return;
            }
            _ => {}
        }
    }

    // Editor normal-mode motions + operators. Dispatch to the editor
    // and honor its return to drive mode transitions (Insert / Visual).
    if let Some(ed) = app.focused_editor_mut() {
        use crate::editor::EditorAction;
        let act = ed.handle_normal(key);
        let msg = ed.take_status();
        match act {
            EditorAction::None             => { /* consumed locally */ }
            EditorAction::EnterInsert      => { app.mode = Mode::Insert; }
            EditorAction::EnterVisualChar  => { app.mode = Mode::Visual { linewise: false }; }
            EditorAction::EnterVisualLine  => { app.mode = Mode::Visual { linewise: true  }; }
        }
        if let Some(m) = msg { app.status.push_auto(m); }
        return;
    }

    // PTY-focused cells have their own Normal-mode layer: virtual
    // cursor motions + `v` to enter Visual + `p` to paste. Consumed
    // keys return here so `i`/`a` below only catches plain inserts.
    if app.focused_pty_mut().is_some() {
        match handle_pty_normal(app, key) {
            PtyAction::Consumed | PtyAction::ExitNormal => return,
            PtyAction::None => {}
        }
    }

    // Non-editor cells (PTYs, diff, conflict) — `i`/`a` still enters
    // Insert, which is how users drop keystrokes into a shell / claude
    // pty. Editors handle their own `i`/`a` above so this only fires
    // for PTY focus.
    match (key.modifiers, key.code) {
        (KeyModifiers::NONE, KeyCode::Char('i')) => app.enter_insert(),
        (KeyModifiers::NONE, KeyCode::Char('a')) => app.enter_insert(),
        _ => {}
    }
}

/// Keys handled when the unified Explorer panel is focused. Returns true
/// if the key was consumed. Dispatches by `ExplorerMode` — the state
/// machine is:
///
///     Normal ── g ──▶ GitOverview ── b ──▶ GitBranches
///        ▲              │         └─ c ──▶ GitChanges
///        │              │                    │
///        └── esc ──── esc ◀─── esc ─────────┘
///
/// Git-global actions (commit, stage-all, push, …) are available in
/// every git sub-mode; section-specific actions (switch branch, stage
/// file, …) only in the relevant cursor sub-mode.
fn handle_explorer_key(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
        return false;
    }
    // Global-to-Explorer keys — available in every sub-mode so quick
    // project-hopping works whether you're browsing files or inside a
    // git sub-view.
    match key.code {
        KeyCode::PageUp   => { app.project_jump(false); return true; }
        KeyCode::PageDown => { app.project_jump(true);  return true; }
        _ => {}
    }
    match app.explorer_mode {
        ExplorerMode::Normal       => handle_explorer_normal(app, key),
        ExplorerMode::GitOverview  => handle_git_overview(app, key),
        ExplorerMode::GitBranches  => handle_git_branches(app, key),
        ExplorerMode::GitChanges   => handle_git_changes(app, key),
        ExplorerMode::GitLog       => handle_git_log(app, key),
    }
}

/// Keys while a Conflict cell is focused in Normal mode.
/// Hunk nav: j/k (or Down/Up).
/// Resolution: o (ours), t (theirs), b (both), e (hand-edit).
/// Scroll: PgUp/PgDn for long files.
/// Returns true if consumed.
fn handle_conflict_cell(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
        return false;
    }
    let mut start_edit = false;
    let consumed = {
        let Some(cell) = app.focused_cell_mut() else { return false; };
        let Some(c) = cell.active_session_mut().as_conflict_mut() else { return false; };
        use crate::conflict::Resolution as R;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => { c.next_hunk(); true }
            KeyCode::Char('k') | KeyCode::Up   => { c.prev_hunk(); true }
            KeyCode::Char('o') => { c.resolve_selected(R::KeepOurs);   c.next_hunk(); true }
            KeyCode::Char('t') => { c.resolve_selected(R::KeepTheirs); c.next_hunk(); true }
            KeyCode::Char('b') => { c.resolve_selected(R::KeepBoth);   c.next_hunk(); true }
            KeyCode::Char('e') => { start_edit = c.start_edit(); true }
            KeyCode::PageDown  => { c.scroll = c.scroll.saturating_add(10); true }
            KeyCode::PageUp    => { c.scroll = c.scroll.saturating_sub(10); true }
            _ => false,
        }
    };
    if start_edit {
        // Drop into Insert so keystrokes route to the hunk's textarea.
        app.enter_insert();
    }
    consumed
}

/// Keys while a Diff cell is focused in Normal mode. Scroll-only
/// (read-only view). Returns true if consumed.
fn handle_diff_cell(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
        return false;
    }
    let Some(cell) = app.focused_cell_mut() else { return false; };
    let Some(d) = cell.active_session_mut().as_diff_mut() else { return false; };
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => { d.scroll( 1); true }
        KeyCode::Char('k') | KeyCode::Up   => { d.scroll(-1); true }
        KeyCode::PageDown                   => { d.scroll_page(10, true);  true }
        KeyCode::PageUp                     => { d.scroll_page(10, false); true }
        KeyCode::Char('g') | KeyCode::Home  => { d.scroll = 0; true }
        KeyCode::Char('G') | KeyCode::End   => {
            d.scroll = d.lines.len().saturating_sub(1);
            true
        }
        _ => false,
    }
}

fn handle_explorer_normal(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => { app.explorer.move_down(); true }
        KeyCode::Char('k') | KeyCode::Up   => { app.explorer.move_up();   true }
        KeyCode::Enter                      => { activate_explorer_row(app); true }
        // `e` — open selected file in a fresh cell and focus it
        // (distinct from Enter, which previews without leaving the
        // explorer). No-op on non-file rows.
        KeyCode::Char('e') => {
            if let Some(path) = app.explorer.selected_file() {
                open_file_in_cell(app, path);
            }
            true
        }
        KeyCode::Char('g')                  => { app.enter_git_overview(); true }
        KeyCode::Char('i') if !app.git.is_repo() => { app.git_init_here(); true }
        // Prefilled command prompts so common explorer actions are one
        // key + <path> + Enter. `a` adds a project, `o` opens a file,
        // `n` creates a new file anchored at the row under the cursor.
        KeyCode::Char('a') => { app.enter_command_with("proj add "); true }
        KeyCode::Char('o') => { app.enter_command_with("e ");        true }
        KeyCode::Char('n') => {
            // Anchor the path at the best-guess directory — the dir
            // under the cursor, the parent of the selected file, or the
            // active project root as a last resort. The user just types
            // the new filename and hits Enter to open a [NEW] buffer.
            let base = app.explorer.selected_new_file_dir()
                .or_else(|| app.projects.projects
                    .get(app.projects.active)
                    .map(|p| p.root.clone()));
            let prefix = match base {
                Some(dir) => {
                    // Use forward slashes in the prompt so paths stay
                    // readable on Windows (ace's cmd line accepts both,
                    // but backslashes are ugly in a mini-textarea).
                    let s = dir.to_string_lossy().replace('\\', "/");
                    format!("e {s}/")
                }
                None => "e ".to_string(),
            };
            app.enter_command_with(&prefix);
            true
        }
        // `c` — close the thing under the cursor via a prefilled
        // command you confirm with Enter:
        //   * open-cell row → `:q <N>` (closes that cell)
        //   * project row   → `:proj rm <name>` (closes that project)
        //   * anything else → `:proj rm <active-project>` as a fallback
        KeyCode::Char('c') => {
            if let Some(cell_idx) = app.explorer.selected_open_cell() {
                app.enter_command_with(&format!("q {}", cell_idx + 1));
            } else {
                let idx = app.explorer.selected_project().unwrap_or(app.projects.active);
                if let Some(p) = app.projects.projects.get(idx) {
                    let name = p.name.clone();
                    app.enter_command_with(&format!("proj rm {name}"));
                }
            }
            true
        }
        _ => false,
    }
}

/// Git globals available in every git sub-mode. Returns true if
/// consumed.
fn handle_git_global(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('m') => { app.git_begin_commit(); true }
        KeyCode::Char('a') => { app.git_stage_all();    true }
        KeyCode::Char('A') => { app.git_unstage_all();  true }
        KeyCode::Char('p') => { app.git_push();         true }
        KeyCode::Char('P') => { app.git_pull();         true }
        KeyCode::Char('f') => { app.git_fetch();        true }
        _ => false,
    }
}

fn handle_git_overview(app: &mut App, key: KeyEvent) -> bool {
    if handle_git_global(app, key) { return true; }
    match key.code {
        KeyCode::Esc       => { app.exit_git_submode(); true }
        KeyCode::Char('b') => { app.enter_git_branches(); true }
        KeyCode::Char('c') => { app.enter_git_changes();  true }
        KeyCode::Char('l') => { app.enter_git_log();      true }
        _ => false,
    }
}

fn handle_git_branches(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => { app.git_branch_move( 1); true }
        KeyCode::Char('k') | KeyCode::Up   => { app.git_branch_move(-1); true }
        KeyCode::Enter                      => { app.git_switch_selected_branch(); true }
        KeyCode::Char('n') => { app.git_begin_new_branch(); true }
        KeyCode::Char('d') => { app.git_delete_selected_branch();        true }
        KeyCode::Char('D') => { app.git_force_delete_selected_branch();  true }
        KeyCode::Char('c') => { app.enter_git_changes(); true }
        KeyCode::Char('l') => { app.enter_git_log();     true }
        KeyCode::Esc       => { app.exit_git_submode(); true }
        _ => handle_git_global(app, key),
    }
}

fn handle_git_changes(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => { app.git_change_move( 1); true }
        KeyCode::Char('k') | KeyCode::Up   => { app.git_change_move(-1); true }
        KeyCode::Enter | KeyCode::Char('s') => {
            app.git_toggle_selected_change(); true
        }
        KeyCode::Char('d') => { app.git_discard_selected_change(); true }
        // Conflict resolution — only fire when the cursor is on a
        // conflicted row; otherwise fall through (e.g. `o` might mean
        // something else in a future sub-mode).
        KeyCode::Char('o') => { app.git_resolve_selected_ours();   true }
        KeyCode::Char('t') => { app.git_resolve_selected_theirs(); true }
        KeyCode::Char('e') => { app.git_open_selected_in_editor(); true }
        KeyCode::Char('v') => { app.git_open_diff_for_selected();  true }
        KeyCode::Char('b') => { app.enter_git_branches(); true }
        KeyCode::Char('l') => { app.enter_git_log();      true }
        KeyCode::Esc       => { app.exit_git_submode(); true }
        _ => handle_git_global(app, key),
    }
}

fn handle_git_log(app: &mut App, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => { app.git_log_move( 1); true }
        KeyCode::Char('k') | KeyCode::Up   => { app.git_log_move(-1); true }
        KeyCode::Enter | KeyCode::Char('v') => { app.git_log_open_diff(); true }
        KeyCode::Char('c')                  => { app.git_log_copy_sha();  true }
        KeyCode::Char('b') => { app.enter_git_branches(); true }
        KeyCode::Esc       => { app.exit_git_submode(); true }
        _ => handle_git_global(app, key),
    }
}

/// Enter on the selected row in the Explorer panel. A row can be a
/// project header (switch projects), a dir (toggle expand), or a file
/// (open in an editor cell).
fn activate_explorer_row(app: &mut App) {
    match app.explorer.activate(&app.projects, app.cells.len()) {
        explorer::Action::None => {}
        explorer::Action::SwitchProject(i) => app.project_switch_idx_keep_focus(i),
        // Enter on a file previews: load into a reusable slot, keep
        // explorer focus so the user can keep skimming. `e` (handled
        // separately) is the full-open that commits + focuses.
        explorer::Action::OpenFile(p)      => preview_file(app, p),
        explorer::Action::FocusOpenCell(i) => {
            if i < app.cells.len() {
                app.set_focus(FocusId::Cell(i));
            }
        }
    }
}

/// Find a cell we can load a fresh file into without creating a new
/// one. The active preview cell wins (its whole purpose), otherwise
/// any scratch editor (no path, not dirty) is fair game — startup
/// spawns a scratch cell precisely so the first file opened reuses
/// it instead of piling up tabs. Dirty buffers are never clobbered.
fn reusable_cell_idx(app: &App) -> Option<usize> {
    if let Some(idx) = app.preview_cell_idx {
        if app.cells.get(idx)
            .and_then(|c| match c.active_session() {
                Session::Edit(_) => Some(()),
                _ => None,
            })
            .is_some()
        {
            return Some(idx);
        }
    }
    app.cells.iter().position(|c| match c.active_session() {
        Session::Edit(ed) => ed.path.is_none() && !ed.dirty,
        _ => false,
    })
}

/// Short-form name for status-bar messages: the file's basename rather
/// than the full path, so long absolute paths don't blow out the line.
/// Falls back to the full display if the path has no basename (root or
/// bare drive letter).
fn short_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Open `path` for editing — focused, committed. Prefers an empty
/// scratch cell or the current preview slot before spinning up a new
/// cell. Used by `e` on a file in the explorer and by `:e <path>`.
fn open_file_in_cell(app: &mut App, path: std::path::PathBuf) {
    let name = short_name(&path);
    if let Some(idx) = reusable_cell_idx(app) {
        let loaded = app.cells[idx].active_session_mut()
            .as_editor_mut()
            .map(|ed| ed.load(&path));
        match loaded {
            Some(Ok(())) => {
                app.set_focus(FocusId::Cell(idx));   // commits the preview
                app.status.push_auto(format!("opened {name}"));
                app.on_sessions_changed();
            }
            Some(Err(e)) => app.status.push_auto(format!("open failed: {e}")),
            None         => app.status.push_auto("open failed: no editor".into()),
        }
        return;
    }

    if app.cells.len() >= app::MAX_CELLS {
        app.status.push_auto(format!("max {} cells — close one first", app::MAX_CELLS));
        return;
    }
    let mut ed = Editor::empty();
    match ed.load(&path) {
        Ok(()) => {
            app.insert_cell_at_top(Cell::with_session(Session::Edit(ed)));
            app.preview_cell_idx = None;
            app.status.push_auto(format!("opened {name}"));
            app.persist_cells();
        }
        Err(e) => app.status.push_auto(format!("open failed: {e}")),
    }
}

/// Load `path` into the preview slot without stealing focus from the
/// explorer. Reuses the current preview cell or any non-dirty scratch
/// editor; otherwise spins up a fresh cell at the top and remembers
/// its index as the new preview.
fn preview_file(app: &mut App, path: std::path::PathBuf) {
    let name = short_name(&path);
    if let Some(idx) = reusable_cell_idx(app) {
        let loaded = app.cells[idx].active_session_mut()
            .as_editor_mut()
            .map(|ed| ed.load(&path));
        match loaded {
            Some(Ok(())) => {
                app.preview_cell_idx = Some(idx);
                app.status.push_auto(format!("preview {name}"));
                app.on_sessions_changed();
            }
            Some(Err(e)) => app.status.push_auto(format!("preview failed: {e}")),
            None         => app.status.push_auto("preview failed: no editor".into()),
        }
        return;
    }

    if app.cells.len() >= app::MAX_CELLS {
        app.status.push_auto(format!("max {} cells — close one first", app::MAX_CELLS));
        return;
    }
    let mut ed = Editor::empty();
    match ed.load(&path) {
        Ok(()) => {
            app.insert_cell_at_top_raw(Cell::with_session(Session::Edit(ed)));
            app.preview_cell_idx = Some(0);
            app.status.push_auto(format!("preview {name}"));
            app.persist_cells();
        }
        Err(e) => app.status.push_auto(format!("preview failed: {e}")),
    }
}

fn handle_insert(app: &mut App, key: KeyEvent) {
    // Esc is intercepted in handle_key and never reaches us here.

    // Conflict hunk hand-edit: if a conflict cell has an active
    // HunkEdit, all Insert keystrokes belong to its textarea.
    if let Some(cell) = app.focused_cell_mut() {
        if let Some(cv) = cell.active_session_mut().as_conflict_mut() {
            if let Some(edit) = cv.editing.as_mut() {
                edit.textarea.input(tui_textarea::Input::from(key));
                return;
            }
        }
    }

    if let Some(ed) = app.focused_editor_mut() {
        ed.handle_insert(key);
        return;
    }

    // PTY scrollback: Shift+PageUp/PageDown scrolls by ~half a page.
    // Regular PageUp/PageDown still pass through to the child program.
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        let delta = pty_page_height(app) as isize / 2;
        match key.code {
            KeyCode::PageUp   => { pty_scroll_focused(app,  delta); return; }
            KeyCode::PageDown => { pty_scroll_focused(app, -delta); return; }
            _ => {}
        }
    }

    let bytes = match key_to_bytes(key) {
        Some(b) => b,
        None => return,
    };

    if let Some(pty) = app.focused_pty_mut() {
        let _ = pty.write(&bytes);
    }
}

/// Inner rows of the focused cell — the "one page" unit for scrollback.
fn pty_page_height(app: &App) -> u16 {
    let FocusId::Cell(i) = app.focus else { return 12; };
    let areas = ui::layout(current_term_rect(), app);
    let rect = match areas.cells.get(i) {
        Some(r) => *r,
        None => return 12,
    };
    let (rows, _) = ui::inner_size(rect);
    rows.max(4)
}

fn pty_scroll_focused(app: &mut App, delta: isize) {
    if let Some(pty) = app.focused_pty_mut() {
        pty.scroll_by(delta);
    }
}

fn handle_command(app: &mut App, key: KeyEvent) {
    match (key.modifiers, key.code) {
        (_, KeyCode::Esc)       => app.command_cancel(),
        (_, KeyCode::Enter)     => app.command_submit(),
        (_, KeyCode::Backspace) => app.command_backspace(),
        (m, KeyCode::Tab)       => app.command_complete(m.contains(KeyModifiers::SHIFT)),
        (_, KeyCode::BackTab)   => app.command_complete(true),
        (m, KeyCode::Char(c))
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            app.command_push(c);
        }
        _ => {}
    }
}

/// Translate a key event into the bytes a PTY expects.
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;

    match key.code {
        Char(c) => {
            // Ctrl-<letter> produces the corresponding control byte.
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let lower = c.to_ascii_lowercase();
                if ('a'..='z').contains(&lower) {
                    return Some(vec![(lower as u8) - b'a' + 1]);
                }
                if c == ' ' {
                    return Some(vec![0]);
                }
            }
            Some(c.to_string().into_bytes())
        }
        Enter     => Some(b"\r".to_vec()),
        Tab       => Some(b"\t".to_vec()),
        BackTab   => Some(b"\x1b[Z".to_vec()),
        Backspace => Some(b"\x7f".to_vec()),
        Up        => Some(b"\x1b[A".to_vec()),
        Down      => Some(b"\x1b[B".to_vec()),
        Right     => Some(b"\x1b[C".to_vec()),
        Left      => Some(b"\x1b[D".to_vec()),
        Home      => Some(b"\x1b[H".to_vec()),
        End       => Some(b"\x1b[F".to_vec()),
        PageUp    => Some(b"\x1b[5~".to_vec()),
        PageDown  => Some(b"\x1b[6~".to_vec()),
        Delete    => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}
