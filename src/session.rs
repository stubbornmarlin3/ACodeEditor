use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use crate::events::AppEvent;

/// Pre-rendered PTY screen: a ratatui `Buffer` with styles already
/// translated from vt100. Built on the reader thread right after
/// `parser.process(buf)` while it still holds the parser lock, stored
/// in `PtySession.snapshot`, and blitted into the frame buffer by the
/// UI. Decouples the render walk from the UI thread — which means the
/// UI never contends with the reader for the parser lock, and a full
/// redraw costs one `Buffer` memcpy per cell instead of thousands of
/// `String`/`Span` allocations.
///
/// Overlays (virtual cursor, Visual-mode selection) are NOT baked in
/// — the UI applies them after blitting by mutating a small number of
/// cells in the destination buffer, since they depend on focus/mode
/// state the reader doesn't know about.
pub struct PtySnapshot {
    pub rows:       u16,
    pub cols:       u16,
    pub scrollback: usize,
    /// Cell grid sized `cols × rows`, area = `Rect::new(0, 0, cols, rows)`.
    pub buf:        Buffer,
}

type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Scrollback ring capacity passed to `vt100::Parser::new`. Kept as a
/// constant so the Normal/Visual layer can reason about the oldest
/// still-addressable row (vt100 doesn't expose the ring length on
/// Screen's public API, and we don't want to keep probing it just for
/// the cap).
pub const SCROLLBACK_CAP: usize = 2000;

/// A single end of a Visual-mode selection in a PTY cell. `abs` is an
/// **absolute row index** in the conceptual infinite buffer (0 = the
/// first row ever written). `col` is a column index within the row.
///
/// Absolute indexing means new PTY output does NOT drift the position
/// — the anchor stays pinned to its original content. If the scrollback
/// ring evicts rows older than `abs`, the position is clamped to the
/// oldest surviving row at read time.
#[derive(Copy, Clone, Debug)]
pub struct VPos {
    pub abs: u64,
    pub col: u16,
}

pub struct PtySession {
    pub parser:  Arc<Mutex<vt100::Parser>>,
    pub program: String,
    writer:      SharedWriter,
    master:      Box<dyn MasterPty + Send>,
    pub rows:    u16,
    pub cols:    u16,
    /// Virtual cursor for Normal / Visual mode — a separate cursor from
    /// the PTY child's own cursor. Stored in absolute-row space so
    /// scrollback churn and new output don't drift the position.
    pub vcursor: VPos,
    /// Set while Visual mode is active; cleared on Esc / exit.
    pub visual_anchor: Option<VPos>,
    /// Mid-chord flag for `gg` → jump-to-top. Cleared by any other key.
    pub pending_g: bool,
    /// Monotonic count of rows pushed off the top of the live area into
    /// scrollback. Bumped by `tick_rows_emitted` based on observed
    /// growth of the ring's occupancy. Used to convert `VPos::abs` to /
    /// from scrollback-relative coords.
    ///
    /// Note: once the ring fills to `SCROLLBACK_CAP` we lose the signal
    /// (the ring length stops growing). We compensate by watching the
    /// vt100 auto-shift of `scrollback_offset` — when the user is
    /// scrolled back and ring is full, each push increments `offset`
    /// by 1, which we detect here.
    rows_emitted: AtomicU64,
    /// Last observed ring occupancy — delta against the new reading
    /// tells us how many new rows entered scrollback this tick.
    last_ring_len: AtomicUsize,
    /// Last observed scrollback offset. Used in the ring-full case to
    /// detect row pushes via vt100's auto-shift of the offset.
    last_scrollback_offset: AtomicUsize,
    /// Child process exited. Flipped by the waiter thread on reap; the
    /// main loop polls this to reap the cell so a dead shell/claude
    /// doesn't leave a zombie pane.
    exited: Arc<AtomicBool>,
    /// Epoch-millis of the last byte received from the PTY child.
    /// Bumped by the reader thread on every read. `is_busy()` uses this
    /// to decide whether `:q` should refuse (running command / Claude
    /// spinner) and require `:q!` instead.
    last_output_ms: Arc<AtomicU64>,
    /// OS pid of the shell/claude process we spawned. Used by
    /// `is_busy()` to detect silent long-running children (e.g. a dev
    /// server waiting for file changes): any non-shell, non-helper
    /// descendant means something is running even if no output is
    /// flowing. `None` when portable-pty couldn't hand us a pid (rare
    /// — usually a spawn race).
    child_pid: Option<u32>,
    /// Pre-rendered `Buffer` snapshot of the current screen. Built by
    /// the reader thread right after each `parser.process`, and also
    /// rebuilt synchronously on scroll / resize (operations that
    /// change what the UI should draw but don't fire a reader event).
    /// `None` only before the very first render. UI thread reads via
    /// `Arc::clone` — a pointer bump, no per-cell work.
    pub snapshot: Arc<Mutex<Option<Arc<PtySnapshot>>>>,
}

impl PtySession {
    pub fn spawn_shell(rows: u16, cols: u16, cwd: Option<&Path>, tx: Sender<AppEvent>) -> Result<Self> {
        let (program, mut cmd) = default_shell_command();
        apply_cwd(&mut cmd, cwd);
        Self::spawn(rows, cols, program, cmd, tx)
    }

    /// Spawn a shell from an explicit argv (bypasses the config /
    /// platform default). First element is the executable; remaining
    /// elements are its args. Returns an error if the argv is empty.
    pub fn spawn_shell_custom(
        argv: Vec<String>,
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
        tx: Sender<AppEvent>,
    ) -> Result<Self> {
        let (program, mut cmd) = build_from_argv(argv)
            .ok_or_else(|| anyhow::anyhow!("empty shell argv"))?;
        apply_cwd(&mut cmd, cwd);
        Self::spawn(rows, cols, program, cmd, tx)
    }

    /// Spawn a one-shot command line through the user's configured
    /// shell (`.acerc` `shell = ...`), falling back to `powershell` on
    /// Windows and `sh` elsewhere. The right "run this string" flag
    /// (`-c`, `-Command`, `/C`) is inferred from the program's basename.
    /// The PTY exits as soon as the command does, which lets the main
    /// loop reap the cell automatically. Backing for `:ex <cmd>`.
    pub fn spawn_exec(
        cmdline: &str,
        rows: u16,
        cols: u16,
        cwd: Option<&Path>,
        tx: Sender<AppEvent>,
    ) -> Result<Self> {
        let mut argv = crate::config::Config::load()
            .shell
            .filter(|v| !v.is_empty())
            .unwrap_or_else(exec_fallback_argv);
        argv.push(exec_flag_for(&argv[0]).to_string());
        argv.push(cmdline.to_string());
        Self::spawn_shell_custom(argv, rows, cols, cwd, tx)
    }

    pub fn spawn_claude(rows: u16, cols: u16, cwd: Option<&Path>, tx: Sender<AppEvent>) -> Result<Self> {
        let (program, mut cmd) = default_claude_command();
        apply_cwd(&mut cmd, cwd);
        Self::spawn(rows, cols, program, cmd, tx)
    }

    /// True once the child process has exited. The waiter thread sets
    /// this after `child.wait()` returns; the main loop reaps the cell
    /// on the next iteration.
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Relaxed)
    }

    /// Rough "is something actively running in this PTY" heuristic.
    /// True iff the child hasn't exited AND either:
    ///   * the child produced output in the last 500ms (Claude
    ///     spinner, streaming build, prompt redraw), or
    ///   * the shell has a user-launched descendant alive (a silent
    ///     dev server like `npm run dev`, a `sleep`, a blocked
    ///     command) — i.e. a descendant process whose name isn't a
    ///     known shell or pty-host helper.
    ///
    /// Used by `:q` / `:Q` to refuse closing a cell while work is in
    /// flight (forcing `:q!`). False negatives are preferable to
    /// false positives — a stuck `:q` is worse than closing a truly
    /// idle shell.
    pub fn is_busy(&self) -> bool {
        if self.has_exited() { return false; }
        let last = self.last_output_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms().saturating_sub(last) < 500 {
            return true;
        }
        let Some(root) = self.child_pid else { return false; };
        count_user_descendants(root) > 0
    }

    fn spawn(
        rows: u16,
        cols: u16,
        program: String,
        cmd: CommandBuilder,
        tx: Sender<AppEvent>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let child_pid = child.process_id();

        // `SCROLLBACK_CAP` lines of scrollback — plenty for normal shell
        // usage while keeping memory bounded per cell.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_CAP)));
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer()?));

        // Reader thread: PTY output → parser → redraw signal,
        // also handles the ConPTY DSR/handshake replies.
        let reader          = pair.master.try_clone_reader()?;
        let parser_clone    = Arc::clone(&parser);
        let tx_clone        = tx.clone();
        let writer_clone    = Arc::clone(&writer);
        let last_output_ms  = Arc::new(AtomicU64::new(0));
        let last_output_rd  = Arc::clone(&last_output_ms);
        let snapshot        = Arc::new(Mutex::new(None::<Arc<PtySnapshot>>));
        let snapshot_rd     = Arc::clone(&snapshot);
        thread::spawn(move || {
            pty_reader_loop(reader, parser_clone, tx_clone, writer_clone, last_output_rd, snapshot_rd);
        });

        // Waiter thread: owns the Child so it doesn't become a zombie.
        // On reap, flip `exited` and kick a redraw so the main loop
        // notices and closes the cell on its next tick.
        let exited = Arc::new(AtomicBool::new(false));
        let exited_clone = Arc::clone(&exited);
        let tx_exit = tx.clone();
        thread::spawn(move || {
            let _ = child.wait();
            exited_clone.store(true, Ordering::Relaxed);
            crate::events::send_redraw_coalesced(&tx_exit);
        });

        let sess = Self {
            parser,
            program,
            writer,
            master: pair.master,
            rows,
            cols,
            // Start the virtual cursor at the (soon-to-be) live-bottom.
            // With rows_emitted = 0, live-bottom abs = rows - 1.
            vcursor: VPos { abs: rows.saturating_sub(1) as u64, col: 0 },
            visual_anchor: None,
            pending_g: false,
            rows_emitted: AtomicU64::new(0),
            last_ring_len: AtomicUsize::new(0),
            last_scrollback_offset: AtomicUsize::new(0),
            exited,
            last_output_ms,
            child_pid,
            snapshot,
        };
        // Seed the snapshot so the first frame has something to blit
        // even if no PTY output has arrived yet. Cheap — an empty vt100
        // Screen is all blanks and the build just walks the grid once.
        sess.rebuild_snapshot_locked();
        Ok(sess)
    }

    /// Rebuild the rendered `PtySnapshot` from the current parser state
    /// and publish it. Call on any state change that the reader thread
    /// won't fire on its own — scroll offset changes, resize, and the
    /// initial seed in `spawn()`. Holds the parser lock for the
    /// duration of the walk, same as the reader's post-`process` path,
    /// so the two are mutually exclusive by construction.
    pub fn rebuild_snapshot_locked(&self) {
        let Ok(p) = self.parser.lock() else { return; };
        let snap = build_snapshot(p.screen());
        drop(p);
        if let Ok(mut guard) = self.snapshot.lock() {
            *guard = Some(Arc::new(snap));
        }
    }

    /// Read the current snapshot (pointer-bump clone). `None` only
    /// before the first `rebuild_snapshot_locked` — should never happen
    /// in practice because `spawn()` seeds it.
    pub fn snapshot(&self) -> Option<Arc<PtySnapshot>> {
        self.snapshot.lock().ok().and_then(|g| g.clone())
    }

    pub fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
        // Any user input snaps the view back to live — matches every
        // other terminal emulator.
        self.scroll_reset();
        // Recover from poison: a panic in a PTY helper thread (reader,
        // handshake responder) leaves the writer mutex poisoned, but the
        // underlying `Write` is still intact — we can keep serving user
        // input instead of crashing the editor.
        let mut w = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        w.write_all(bytes)?;
        w.flush()
    }

    /// Current scrollback offset in lines (0 = live, N = viewing N lines
    /// back). Used by the UI to render an indicator in the cell title.
    /// Reads from the published snapshot, not the parser — same source
    /// of truth the UI is about to blit, and no lock contention with
    /// the reader thread. Falls back to the atomic mirror kept in sync
    /// by every `set_scrollback` path (including `tick_rows_emitted`)
    /// for the pre-first-snapshot window.
    pub fn scrollback(&self) -> usize {
        if let Some(s) = self.snapshot() {
            return s.scrollback;
        }
        self.last_scrollback_offset.load(Ordering::Relaxed)
    }

    pub fn scroll_by(&self, delta: isize) {
        let changed = if let Ok(mut p) = self.parser.lock() {
            let cur = p.screen().scrollback() as isize;
            let next = (cur + delta).max(0) as usize;
            if next != cur as usize {
                p.set_scrollback(next);
                self.last_scrollback_offset.store(next, Ordering::Relaxed);
                true
            } else { false }
        } else { false };
        if changed { self.rebuild_snapshot_locked(); }
    }

    /// Terminal title set by the child process via OSC 0/2. Empty when
    /// the child hasn't set one. Claude Code updates this with the cell
    /// name it picks; shells typically leave it empty unless `PS1` or
    /// `PROMPT_COMMAND` writes one.
    pub fn title(&self) -> String {
        self.parser
            .lock()
            .map(|p| p.screen().title().to_string())
            .unwrap_or_default()
    }

    /// Last non-empty line currently on screen, trimmed. Walks from the
    /// bottom row upward and returns the first row with visible content.
    /// Used by `:ex` to surface command output to the status bar.
    pub fn last_nonempty_line(&self) -> Option<String> {
        let p = self.parser.lock().ok()?;
        let screen = p.screen();
        let (rows, cols) = screen.size();
        for r in (0..rows).rev() {
            let mut line = String::new();
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    line.push_str(&cell.contents());
                }
            }
            let trimmed = line.trim_end();
            if !trimmed.is_empty() {
                return Some(trimmed.trim_start().to_string());
            }
        }
        None
    }

    pub fn scroll_reset(&self) {
        let changed = if let Ok(mut p) = self.parser.lock() {
            if p.screen().scrollback() != 0 {
                p.set_scrollback(0);
                self.last_scrollback_offset.store(0, Ordering::Relaxed);
                true
            } else { false }
        } else { false };
        if changed { self.rebuild_snapshot_locked(); }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        // Poison recovery: the parser is still structurally valid after
        // a panicked reader thread — just resize it anyway.
        if let Ok(mut p) = self.parser.lock().or_else(|e| Ok::<_, ()>(e.into_inner())) {
            p.set_size(rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
        // Clamp cursor + anchor to the new column range. `abs` is
        // unaffected by a width change.
        self.vcursor.col = self.vcursor.col.min(cols.saturating_sub(1));
        if let Some(a) = self.visual_anchor.as_mut() {
            a.col = a.col.min(cols.saturating_sub(1));
        }
        // Snapshot is sized to the old rows/cols — rebuild at the new
        // geometry before the next draw tries to blit a mis-sized grid.
        self.rebuild_snapshot_locked();
        Ok(())
    }

    // ── absolute-row bookkeeping ───────────────────────────────────────

    pub fn rows_emitted(&self) -> u64 {
        self.rows_emitted.load(Ordering::Relaxed)
    }

    /// Live-bottom (the most recent row in the live area, inclusive).
    pub fn live_bottom_abs(&self) -> u64 {
        self.rows_emitted() + self.rows.saturating_sub(1) as u64
    }

    /// Oldest row that's still addressable (either in scrollback ring
    /// or in the live area). Anything with a smaller `abs` has been
    /// evicted from the ring and is unrecoverable.
    pub fn oldest_addressable_abs(&self) -> u64 {
        let ring_len = self.last_ring_len.load(Ordering::Relaxed) as u64;
        self.rows_emitted().saturating_sub(ring_len)
    }

    /// Probe vt100 for how many rows have entered scrollback since the
    /// previous tick, and update `rows_emitted`. Call before reading
    /// any `VPos`-valued state so `abs` → viewport conversions use a
    /// current `rows_emitted` value.
    ///
    /// Two signals are combined:
    ///   * Growth of the scrollback ring's occupancy (works until the
    ///     ring fills).
    ///   * vt100's auto-increment of `scrollback_offset` when a new row
    ///     is pushed while the user is scrolled back (catches the
    ///     ring-full case, provided the user is actually scrolled).
    ///
    /// If the ring is full AND the user is at live (offset=0), we can't
    /// observe new pushes — this is a rare gap acknowledged in the
    /// feature docs. In practice Visual-mode motion scrolls the user
    /// back immediately, closing the gap.
    pub fn tick_rows_emitted(&self) {
        let (ring_len_now, offset_now) = {
            let Ok(mut p) = self.parser.lock() else { return; };
            let saved = p.screen().scrollback();
            // Probing with MAX clamps to ring occupancy — that's the
            // number we want. Restore immediately so the user's view
            // isn't disturbed mid-frame.
            p.set_scrollback(usize::MAX / 2);
            let ring_len = p.screen().scrollback();
            p.set_scrollback(saved);
            (ring_len, saved)
        };

        let last_ring = self.last_ring_len.load(Ordering::Relaxed);
        let last_offset = self.last_scrollback_offset.load(Ordering::Relaxed);

        // Signal 1 — ring grew.
        let mut delta: u64 = 0;
        if ring_len_now > last_ring {
            delta = (ring_len_now - last_ring) as u64;
        } else if ring_len_now == last_ring && ring_len_now == SCROLLBACK_CAP {
            // Signal 2 — ring is full, so growth stalls. vt100
            // auto-shifts scrollback_offset when the ring is full and
            // new rows arrive *while the user is scrolled back*. If
            // the user didn't touch set_scrollback between ticks, an
            // offset bump means N rows were pushed.
            if offset_now > last_offset {
                delta = (offset_now - last_offset) as u64;
            }
            // If the user is at live (offset_now == 0 == last_offset)
            // and the ring is full, we have no signal. This is the
            // documented gap.
        }

        if delta > 0 {
            self.rows_emitted.fetch_add(delta, Ordering::Relaxed);
        }
        self.last_ring_len.store(ring_len_now, Ordering::Relaxed);
        self.last_scrollback_offset.store(offset_now, Ordering::Relaxed);
    }

    // ── virtual cursor / selection helpers ─────────────────────────────

    /// Viewport row currently occupied by `p`, or `None` if above or
    /// below the visible window. Caller is responsible for calling
    /// `tick_rows_emitted` first when precision matters.
    ///
    /// Formula: at scrollback offset `s`, viewport row 0 shows
    /// `abs = rows_emitted - s`, row `rows-1` shows
    /// `abs = rows_emitted - s + rows - 1`. So
    /// `viewport_row = abs - (rows_emitted - s)`.
    pub fn vpos_viewport_row(&self, p: VPos) -> Option<u16> {
        let n = self.rows_emitted();
        let s = self.scrollback() as u64;
        let rows = self.rows as u64;
        let top = n.saturating_sub(s);
        if p.abs < top { return None; }
        let r = p.abs - top;
        if r >= rows { return None; }
        Some(r as u16)
    }

    /// Move virtual cursor horizontally. Negative moves left, positive
    /// right. Clamped to `[0, cols-1]`.
    pub fn vcursor_move_col(&mut self, delta: i32) {
        let new = (self.vcursor.col as i32 + delta)
            .clamp(0, self.cols as i32 - 1) as u16;
        self.vcursor.col = new;
    }

    /// Move virtual cursor vertically in absolute-row space. Negative
    /// = up (older content), positive = down (newer). Clamped to the
    /// addressable range `[oldest_addressable_abs, live_bottom_abs]`.
    /// Auto-scrolls the viewport if the cursor leaves it.
    pub fn vcursor_move_row(&mut self, delta: i32) {
        if delta == 0 { return; }
        self.tick_rows_emitted();
        let oldest = self.oldest_addressable_abs();
        let live_bottom = self.live_bottom_abs();
        let cur = self.vcursor.abs as i64;
        let target = (cur + delta as i64).max(oldest as i64).min(live_bottom as i64) as u64;
        self.vcursor.abs = target;
        self.ensure_vcursor_in_view();
    }

    /// Adjust the screen's scrollback offset so `vcursor.abs` lies
    /// within the visible viewport. A no-op when the cursor is already
    /// visible.
    pub fn ensure_vcursor_in_view(&mut self) {
        self.tick_rows_emitted();
        let rows = self.rows as u64;
        let n = self.rows_emitted();
        let abs = self.vcursor.abs;
        let s = self.scrollback() as u64;
        // viewport window is abs in [n - s, n - s + rows - 1].
        // Above viewport: abs < n - s.
        // Below viewport: abs > n - s + rows - 1  →  s < n - abs - rows + 1... let me re-derive.
        //   abs > n - s + rows - 1  ⟺  s > n - abs + rows - 1  ⟺  s > n + rows - 1 - abs.
        //   But `s` should be a non-negative offset; if abs > n + rows - 1 - 0, cursor is below even at s=0.
        //   That means abs > live_bottom_abs — impossible since we clamped.
        //
        // So only the "above viewport" case can happen here, or the
        // "in viewport" case (no-op).
        let top = n.saturating_sub(s);
        let mut changed = false;
        if abs < top {
            // Need to scroll UP (increase offset) so viewport top
            // equals abs. new_s = n - abs.
            let new_s = n.saturating_sub(abs) as usize;
            if let Ok(mut p) = self.parser.lock() {
                p.set_scrollback(new_s);
                changed = true;
            }
        } else if abs >= top + rows {
            // Below viewport (shouldn't happen after the clamp in
            // vcursor_move_row, but guard for safety). Scroll DOWN.
            // Put cursor at viewport bottom: abs = n - s + rows - 1
            //   → s = n + rows - 1 - abs.
            let new_s = (n + rows - 1).saturating_sub(abs) as usize;
            if let Ok(mut p) = self.parser.lock() {
                p.set_scrollback(new_s);
                changed = true;
            }
        }
        // Update last_scrollback_offset so the next tick doesn't
        // misinterpret our manual scroll as an auto-shift.
        let actual = self.parser.lock().map(|p| p.screen().scrollback()).unwrap_or(0);
        self.last_scrollback_offset.store(actual, Ordering::Relaxed);
        if changed { self.rebuild_snapshot_locked(); }
    }

    pub fn vcursor_jump_bottom(&mut self) {
        self.tick_rows_emitted();
        self.vcursor.abs = self.live_bottom_abs();
        let changed = if let Ok(mut p) = self.parser.lock() {
            if p.screen().scrollback() != 0 {
                p.set_scrollback(0);
                true
            } else { false }
        } else { false };
        self.last_scrollback_offset.store(0, Ordering::Relaxed);
        if changed { self.rebuild_snapshot_locked(); }
    }

    /// Snap the virtual cursor to the PTY child's real cursor — used on
    /// Insert → Normal transitions so Normal starts where the user was
    /// typing. Child cursor is always in the live area, so conversion
    /// is simple.
    pub fn sync_vcursor_to_real(&mut self) {
        self.scroll_reset();
        self.tick_rows_emitted();
        let (cy, cx) = match self.parser.lock() {
            Ok(p) => p.screen().cursor_position(),
            Err(_) => return,
        };
        let n = self.rows_emitted();
        // At scroll=0, viewport row r shows abs = n + r (live area).
        let abs = n + cy as u64;
        self.vcursor = VPos { abs, col: cx.min(self.cols.saturating_sub(1)) };
    }

    /// Next word-boundary to the right on the current row. "Word" =
    /// run of non-whitespace. Doesn't cross rows.
    pub fn vcursor_word_next(&mut self) {
        let row_text = self.row_text_for_vcursor();
        let chars: Vec<char> = row_text.chars().collect();
        let start = self.vcursor.col as usize;
        let n = chars.len();
        let mut i = start;
        while i < n && !chars[i].is_whitespace() { i += 1; }
        while i < n && chars[i].is_whitespace() { i += 1; }
        if i >= n { i = (self.cols as usize).saturating_sub(1); }
        self.vcursor.col = (i as u16).min(self.cols.saturating_sub(1));
    }

    pub fn vcursor_word_prev(&mut self) {
        let row_text = self.row_text_for_vcursor();
        let chars: Vec<char> = row_text.chars().collect();
        let start = self.vcursor.col as usize;
        let mut i = start;
        if i == 0 { return; }
        i -= 1;
        while i > 0 && chars.get(i).map_or(true, |c| c.is_whitespace()) { i -= 1; }
        while i > 0 && chars.get(i - 1).map_or(false, |c| !c.is_whitespace()) { i -= 1; }
        self.vcursor.col = i as u16;
    }

    fn row_text_for_vcursor(&self) -> String {
        let cols = self.cols;
        self.with_row_visible(self.vcursor.abs, |screen, vrow| {
            screen.contents_between(vrow, 0, vrow, cols)
        }).unwrap_or_default()
    }

    /// Jump to the oldest addressable row (top of scrollback).
    pub fn vcursor_jump_top(&mut self) {
        self.tick_rows_emitted();
        self.vcursor.abs = self.oldest_addressable_abs();
        self.ensure_vcursor_in_view();
    }

    /// Half-screen scroll (keeps cursor at the same relative viewport
    /// row after the scroll).
    pub fn vcursor_page(&mut self, direction: i32) {
        let half = (self.rows / 2).max(1) as i32;
        self.vcursor_move_row(direction * half);
    }

    pub fn start_visual(&mut self) {
        self.tick_rows_emitted();
        self.visual_anchor = Some(self.vcursor);
    }

    pub fn clear_visual(&mut self) {
        self.visual_anchor = None;
    }

    /// Run `f` with the screen temporarily scrolled so that the row at
    /// absolute index `abs` is visible. `f` receives the screen and
    /// the viewport row that `abs` lands on. Restores scrollback on
    /// return. Returns `None` if `abs` can't be brought into view (row
    /// evicted from the ring).
    fn with_row_visible<F, T>(&self, abs: u64, f: F) -> Option<T>
    where
        F: FnOnce(&vt100::Screen, u16) -> T,
    {
        self.tick_rows_emitted();
        let n = self.rows_emitted();
        let rows = self.rows as u64;
        let oldest = self.oldest_addressable_abs();
        if abs < oldest { return None; }
        if abs > n + rows - 1 { return None; }
        // Put the target row at viewport row 0: scroll = n - abs.
        let target_scroll = n.saturating_sub(abs) as usize;
        let saved_scroll = self.scrollback();
        let result = {
            let mut p = self.parser.lock().ok()?;
            p.set_scrollback(target_scroll);
            let actual = p.screen().scrollback();
            // After clamp, viewport row for `abs` = abs - (n - actual)
            //                                      = abs + actual - n.
            // (Mathematically the same as "rows-1 - (abs_sfb - actual)".)
            let n_now = self.rows_emitted();
            if abs + (actual as u64) < n_now {
                p.set_scrollback(saved_scroll);
                return None;
            }
            let vrow_u64 = abs + actual as u64 - n_now;
            if vrow_u64 >= rows {
                p.set_scrollback(saved_scroll);
                return None;
            }
            let vrow = vrow_u64 as u16;
            let out = f(p.screen(), vrow);
            p.set_scrollback(saved_scroll);
            out
        };
        // Sync our last-seen offset so the next tick doesn't see our
        // manual scroll as a new row push.
        self.last_scrollback_offset
            .store(self.scrollback(), Ordering::Relaxed);
        Some(result)
    }

    /// Extract text between `visual_anchor` and `vcursor`, inclusive,
    /// returned oldest-to-newest. Walks the selection in `rows`-sized
    /// chunks, setting the scrollback offset so each chunk becomes
    /// visible, then restoring. Returns `""` if no anchor.
    pub fn visual_selection_text(&self) -> String {
        let Some(anchor) = self.visual_anchor else { return String::new(); };
        self.tick_rows_emitted();
        let cursor = self.vcursor;

        // Normalize: `start` has the smaller abs (older), `end` the
        // larger abs (newer). Matches the "oldest first" yank order.
        let (start, end) = if anchor.abs < cursor.abs
            || (anchor.abs == cursor.abs && anchor.col <= cursor.col)
        {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };

        // Clamp to the addressable range so evicted rows don't ruin
        // the extraction. Silently truncates — the user can still yank
        // whatever survives.
        let oldest = self.oldest_addressable_abs();
        let start_abs = start.abs.max(oldest);
        let end_abs   = end.abs;
        let start_col = if start.abs < oldest { 0 } else { start.col };

        let rows = self.rows as u64;
        let cols = self.cols;
        let saved_scroll = self.scrollback();
        let mut out = String::new();

        // Walk top-down in chunks of `rows`. First chunk starts at
        // `start_abs`; each chunk of size K spans abs [chunk_top .. chunk_top + K - 1].
        let mut chunk_top = start_abs;
        while chunk_top <= end_abs {
            let chunk_len = (end_abs - chunk_top + 1).min(rows);
            // Scroll so `chunk_top` sits at viewport row 0.
            let n = self.rows_emitted();
            let scroll = n.saturating_sub(chunk_top) as usize;
            if let Ok(mut p) = self.parser.lock() {
                p.set_scrollback(scroll);
            }
            let actual = self.scrollback() as u64;
            let top_vrow_u64 = chunk_top + actual - n;
            if top_vrow_u64 >= rows { break; }
            let top_vrow = top_vrow_u64 as u16;
            let bot_vrow = top_vrow + (chunk_len as u16).saturating_sub(1);

            let chunk_bot_abs = chunk_top + chunk_len - 1;
            let is_first_chunk = chunk_top == start_abs;
            let is_last_chunk  = chunk_bot_abs == end_abs;
            let sc = if is_first_chunk { start_col } else { 0 };
            let ec = if is_last_chunk  { end.col.saturating_add(1).min(cols) } else { cols };

            if let Ok(p) = self.parser.lock() {
                let screen = p.screen();
                out.push_str(&screen.contents_between(top_vrow, sc, bot_vrow, ec));
                if !is_last_chunk && !out.ends_with('\n') {
                    out.push('\n');
                }
            }

            chunk_top = chunk_bot_abs + 1;
        }

        if let Ok(mut p) = self.parser.lock() {
            p.set_scrollback(saved_scroll);
        }
        self.last_scrollback_offset
            .store(self.scrollback(), Ordering::Relaxed);
        out
    }
}

fn pty_reader_loop(
    mut reader: Box<dyn Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    tx: Sender<AppEvent>,
    writer: SharedWriter,
    last_output_ms: Arc<AtomicU64>,
    snapshot: Arc<Mutex<Option<Arc<PtySnapshot>>>>,
) {
    let mut buf = [0u8; 4096];
    // Throttle the redraw-wake rate under bulk streams. Small reads
    // (typical for keystroke echo) always wake immediately so the user
    // sees their keystroke; large-buffer bulk reads (a build log, a
    // large paste) batch to ~8 ms. The snapshot itself is still
    // rebuilt every read — this only caps how often we *ask* the UI
    // thread to redraw. Pairs with `send_redraw_coalesced`'s in-flight
    // dedup for a layered rate limit: coalescing stops pointless
    // enqueues, throttling stops pointless wake-ups.
    let mut last_wake = Instant::now()
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    const BULK_THROTTLE: std::time::Duration = std::time::Duration::from_millis(8);
    const SMALL_READ_BYPASS: usize = 256;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // ConPTY / VT handshake: reply to queries so the child
                // (esp. cmd.exe / pwsh) doesn't stall waiting for us.
                let replies = build_replies(&buf[..n]);
                if !replies.is_empty() {
                    if let Ok(mut w) = writer.lock() {
                        let _ = w.write_all(&replies);
                        let _ = w.flush();
                    }
                }

                // Hold the parser lock for both the process and the
                // snapshot walk: this is the one place where we
                // guarantee the snapshot matches a consistent parser
                // state, and it keeps the lock window short (one
                // process + one grid walk) vs. acquiring twice.
                if let Ok(mut p) = parser.lock() {
                    p.process(&buf[..n]);
                    let snap = build_snapshot(p.screen());
                    drop(p);
                    if let Ok(mut guard) = snapshot.lock() {
                        *guard = Some(Arc::new(snap));
                    }
                }
                last_output_ms.store(now_ms(), Ordering::Relaxed);

                // Wake the UI. Small reads bypass the throttle so
                // keystroke echo feels instant; bulk reads wait out
                // the 8 ms window.
                let now = Instant::now();
                let bypass = n < SMALL_READ_BYPASS;
                if bypass || now.duration_since(last_wake) >= BULK_THROTTLE {
                    last_wake = now;
                    if !crate::events::send_redraw_coalesced(&tx) {
                        break;
                    }
                }
                // When throttled, the coalesced redraw that's already
                // in flight (or the next small read) will pick up the
                // newest snapshot — no correctness cost.
            }
            Err(_) => break,
        }
    }
    crate::events::send_redraw_coalesced(&tx);
}

/// Walk a `vt100::Screen` into a ratatui `Buffer`. Runs on the reader
/// thread under the parser lock. All per-cell style allocation happens
/// here, off the UI thread; the UI side just blits and applies
/// overlays. Interns styles by packed (fg, bg, attrs) so a screen with
/// repeated colour runs (nearly all of them) avoids rebuilding the same
/// `Style` thousands of times.
fn build_snapshot(screen: &vt100::Screen) -> PtySnapshot {
    let (rows, cols) = screen.size();
    let scrollback = screen.scrollback();
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    // Small style-intern cache. A typical terminal screen uses <10
    // distinct (fg,bg,attrs) combos, so a HashMap is overkill but
    // trivially cheap and keeps the hot loop free of `Style` construction.
    let mut style_cache: HashMap<u64, Style> = HashMap::with_capacity(16);

    for r in 0..rows {
        for c in 0..cols {
            let Some(src) = screen.cell(r, c) else { continue; };
            // `Buffer` indexing is (x, y); vt100 is (row, col).
            let dst = &mut buf[(c, r)];
            let key = cell_style_key(src);
            let style = *style_cache
                .entry(key)
                .or_insert_with(|| cell_style_from_vt100(src));
            dst.set_style(style);
            // Skip the String allocation for empty cells — most cells
            // on a typical screen are blank. `Buffer::empty` already
            // filled the symbol with " ".
            if src.has_contents() {
                let s = src.contents();
                if !s.is_empty() {
                    dst.set_symbol(&s);
                }
            }
        }
    }

    PtySnapshot { rows, cols, scrollback, buf }
}

/// Pack a `vt100::Cell`'s style into a `u64` key for interning. Layout:
/// 3 bytes fg + 3 bytes bg + 1 byte attrs + 1 byte tag distinguishing
/// "default / indexed / rgb" for each color. Uniqueness per visual
/// style is all that matters.
fn cell_style_key(cell: &vt100::Cell) -> u64 {
    fn color_bits(c: vt100::Color) -> (u32, u8) {
        match c {
            vt100::Color::Default      => (0, 0),
            vt100::Color::Idx(i)       => (i as u32, 1),
            vt100::Color::Rgb(r, g, b) => ((r as u32) << 16 | (g as u32) << 8 | b as u32, 2),
        }
    }
    let (fg, fg_tag) = color_bits(cell.fgcolor());
    let (bg, bg_tag) = color_bits(cell.bgcolor());
    let attrs = (cell.bold()      as u8)
              | ((cell.italic()    as u8) << 1)
              | ((cell.underline() as u8) << 2)
              | ((cell.inverse()   as u8) << 3);
    (fg as u64)
        | ((bg as u64) << 24)
        | ((attrs as u64) << 48)
        | ((fg_tag as u64) << 56)
        | ((bg_tag as u64) << 60)
}

fn cell_style_from_vt100(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    match cell.fgcolor() {
        vt100::Color::Default      => {}
        vt100::Color::Idx(i)       => style = style.fg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style = style.fg(Color::Rgb(r, g, b)),
    }
    match cell.bgcolor() {
        vt100::Color::Default      => {}
        vt100::Color::Idx(i)       => style = style.bg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style = style.bg(Color::Rgb(r, g, b)),
    }
    let mut mods = Modifier::empty();
    if cell.bold()      { mods |= Modifier::BOLD; }
    if cell.italic()    { mods |= Modifier::ITALIC; }
    if cell.underline() { mods |= Modifier::UNDERLINED; }
    if cell.inverse()   { mods |= Modifier::REVERSED; }
    style.add_modifier(mods)
}

/// Executables that belong to the PTY infrastructure itself — they
/// appear in every pty's process tree regardless of what the user is
/// doing, so the busy heuristic must ignore them. Matched
/// case-insensitively with the `.exe` suffix stripped.
const PTY_HOST_HELPERS: &[&str] = &["conhost", "openconsole"];

/// Interactive shells we treat as "transparent" containers: a shell
/// sitting at its prompt isn't user-launched work. This filters out
/// Git Bash's inner-bash wrapper (bash.exe launcher → bin/bash.exe)
/// and any subshell the user might have `exec`'d. Trade-off: if the
/// user manually opens a subshell and walks away, we'll call the pty
/// idle — acceptable vs. the alternative of Git Bash reading busy
/// forever.
const SHELL_NAMES: &[&str] = &[
    "bash", "sh", "zsh", "fish", "dash", "ksh",
    "cmd", "powershell", "pwsh", "nu", "xonsh",
];

fn name_stem(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_ascii_lowercase()
}

fn is_idle_process(stem: &str) -> bool {
    PTY_HOST_HELPERS.iter().any(|n| *n == stem)
        || SHELL_NAMES.iter().any(|n| *n == stem)
}

/// Count descendants of `root` that look like user-launched work —
/// i.e. any process in the tree whose executable name isn't a known
/// shell or pty-host helper. Walks the full process table once and
/// expands the "ours" set transitively by parent pid, so double-
/// forked children are included. Returns 0 on any sysinfo failure
/// (safer to under-report than to over-report busy).
fn count_user_descendants(root: u32) -> usize {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::new()),
    );
    let root_pid = Pid::from_u32(root);
    let mut ours: std::collections::HashSet<Pid> = std::collections::HashSet::new();
    ours.insert(root_pid);
    loop {
        let mut added = false;
        for (pid, proc) in sys.processes() {
            if ours.contains(pid) { continue; }
            if let Some(ppid) = proc.parent() {
                if ours.contains(&ppid) {
                    ours.insert(*pid);
                    added = true;
                }
            }
        }
        if !added { break; }
    }
    ours.iter().filter(|pid| {
        if **pid == root_pid { return false; }
        let Some(proc) = sys.process(**pid) else { return false; };
        !is_idle_process(&name_stem(proc.name().to_string_lossy().as_ref()))
    }).count()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Scan incoming bytes for terminal queries that expect a host reply
/// (mostly DSR-6 cursor-position-report). Returns the concatenated replies
/// to write back to the PTY master.
fn build_replies(bytes: &[u8]) -> Vec<u8> {
    // ESC [ 6 n  → DSR-6; reply ESC [ <row> ; <col> R  (we claim 1;1).
    const DSR_6:   &[u8] = b"\x1b[6n";
    const DSR_RPL: &[u8] = b"\x1b[1;1R";

    let mut out = Vec::new();
    let mut i = 0;
    while i + DSR_6.len() <= bytes.len() {
        if &bytes[i..i + DSR_6.len()] == DSR_6 {
            out.extend_from_slice(DSR_RPL);
            i += DSR_6.len();
        } else {
            i += 1;
        }
    }
    out
}

fn default_shell_command() -> (String, CommandBuilder) {
    // Precedence:
    //   1. ACE_SHELL env var      (highest — useful for testing)
    //   2. `shell = "..."` in .acerc  (user- or project-level config)
    //   3. Platform default
    if let Ok(cmdline) = std::env::var("ACE_SHELL") {
        if let Some(argv) = crate::config::parse_argv(&cmdline) {
            if let Some(built) = build_from_argv(argv) {
                return built;
            }
        }
    }
    if let Some(argv) = crate::config::Config::load().shell {
        if let Some(built) = build_from_argv(argv) {
            return built;
        }
    }
    platform_default_shell()
}

fn build_from_argv(argv: Vec<String>) -> Option<(String, CommandBuilder)> {
    let mut iter = argv.into_iter();
    let program = iter.next()?;
    if program.is_empty() {
        return None;
    }
    let mut cmd = CommandBuilder::new(&program);
    for arg in iter {
        cmd.arg(arg);
    }
    apply_common_env(&mut cmd);
    Some((program, cmd))
}

#[cfg(windows)]
fn platform_default_shell() -> (String, CommandBuilder) {
    // Windows PowerShell is preinstalled. pwsh (7+) often isn't on PATH.
    let prog = "powershell".to_string();
    let mut cmd = CommandBuilder::new(&prog);
    cmd.arg("-NoLogo");
    apply_common_env(&mut cmd);
    (prog, cmd)
}

#[cfg(not(windows))]
fn platform_default_shell() -> (String, CommandBuilder) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = CommandBuilder::new(&shell);
    apply_common_env(&mut cmd);
    (shell, cmd)
}

/// Fallback argv for `:ex` when the user hasn't configured a shell:
/// `powershell` on Windows, `sh` elsewhere. Deliberately distinct from
/// `platform_default_shell` — `:ex` wants a minimal "run this string"
/// shell, not necessarily the user's interactive one.
fn exec_fallback_argv() -> Vec<String> {
    #[cfg(windows)]
    { vec!["powershell".into(), "-NoLogo".into()] }
    #[cfg(not(windows))]
    { vec!["sh".into()] }
}

/// Pick the "run this string" flag appropriate to the shell's basename.
/// powershell/pwsh use `-Command`; cmd uses `/C`; everything else gets
/// the POSIX `-c`.
fn exec_flag_for(program: &str) -> &'static str {
    let base = std::path::Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    match base.as_str() {
        "powershell" | "pwsh" => "-Command",
        "cmd"                 => "/C",
        _                     => "-c",
    }
}

fn apply_common_env(cmd: &mut CommandBuilder) {
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
}

/// Pin the child's working directory. portable-pty doesn't inherit the
/// parent's cwd on Windows/ConPTY — without this, shells land wherever
/// the launching exe sits (e.g. `target/debug` under `cargo run`) rather
/// than the project root we just `set_current_dir`-ed to.
fn apply_cwd(cmd: &mut CommandBuilder, cwd: Option<&Path>) {
    if let Some(dir) = cwd {
        if dir.is_dir() {
            cmd.cwd(dir);
        }
    }
}

fn default_claude_command() -> (String, CommandBuilder) {
    // Precedence:
    //   1. ACE_CLAUDE env var
    //   2. `claude = "..."` in .acerc
    //   3. bare `claude` on PATH
    let cfg = crate::config::Config::load();
    let env_argv = std::env::var("ACE_CLAUDE")
        .ok()
        .and_then(|c| crate::config::parse_argv(&c));
    let (program, mut cmd) = env_argv
        .and_then(build_from_argv)
        .or_else(|| cfg.claude.clone().and_then(build_from_argv))
        .unwrap_or_else(|| {
            let prog = "claude".to_string();
            let mut cmd = CommandBuilder::new(&prog);
            apply_common_env(&mut cmd);
            (prog, cmd)
        });

    if cfg.claude_skip_permissions.unwrap_or(false) {
        cmd.arg("--dangerously-skip-permissions");
    }

    (program, cmd)
}
