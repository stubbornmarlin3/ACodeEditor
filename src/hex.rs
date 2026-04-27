//! Hex editor view. A peer of `Editor` — same role, different storage:
//! a flat `Vec<u8>` instead of a line-based `TextArea`. Rendering is
//! handled by `ui::draw_hex`; key handling lives here and mirrors
//! `Editor`'s `handle_normal`/`handle_insert`/`handle_visual` shape so
//! the outer mode machinery in `main.rs` can route through it the same
//! way.
//!
//! v1 is overwrite-only: every byte offset stays put for the lifetime
//! of the view. Insert/delete is deferred. The ASCII pane to the right
//! is a read-only mirror of `bytes`.

use std::cell::Cell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExternalConflict {
    ModifiedOnDisk,
    Deleted,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    NoOp,
    AutoReloaded,
    ConflictMarked,
    Deleted,
}

/// What the outer normal-mode dispatcher should do after the key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HexAction {
    None,
    EnterInsert,
    EnterVisual,
}

/// What the outer visual-mode dispatcher should do after the key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HexVisualAction {
    Stay,
    Exit,
}

pub struct HexView {
    pub path:    Option<PathBuf>,
    pub bytes:   Vec<u8>,
    pub cursor:  usize,
    /// Hex side has two nibbles per byte. `true` = high nibble (next
    /// keystroke fills the upper 4 bits). Reset to `true` on any
    /// non-insert motion or after a low-nibble write completes.
    pub nibble_high: bool,
    pub dirty:   bool,
    pub is_new:  bool,
    saved_bytes: Vec<u8>,
    saved_mtime: Option<SystemTime>,
    saved_size:  Option<u64>,
    pub external_conflict: Option<ExternalConflict>,
    /// Visual selection anchor (byte offset). Other end is `cursor`.
    pub anchor:  Option<usize>,
    /// Top-row byte offset visible on screen. Interior mutability so
    /// the renderer (which holds `&HexView`) can adjust scroll to keep
    /// the cursor in view.
    pub scroll:  Cell<usize>,
    /// Bytes per row picked by the renderer based on cell width. The
    /// renderer overwrites this each frame.
    pub bytes_per_row: Cell<u16>,
    /// Last viewport height (rows of bytes) the renderer used. Drives
    /// PgUp/PgDn paging.
    pub viewport_rows: Cell<u16>,
    last_status: Option<String>,
    /// Multi-key pending (`g` waiting for `g` to make `gg`). Clears on
    /// any non-composing key.
    pending_g: bool,
    /// `r` waiting for two hex chars. `Some(None)` after `r`; advances
    /// to `Some(Some(hi))` once the high nibble lands; the low nibble
    /// commits the byte and clears the state.
    pending_r: Option<Option<u8>>,
    /// Last-searched byte sequence — `n`/`N` repeat against this.
    /// Empty vec / `None` means no prior search.
    pub search_pat: Option<Vec<u8>>,
}

impl HexView {
    pub fn empty() -> Self {
        Self {
            path: None,
            bytes: Vec::new(),
            cursor: 0,
            nibble_high: true,
            dirty: false,
            is_new: false,
            saved_bytes: Vec::new(),
            saved_mtime: None,
            saved_size:  None,
            external_conflict: None,
            anchor: None,
            scroll: Cell::new(0),
            bytes_per_row: Cell::new(16),
            viewport_rows: Cell::new(0),
            last_status: None,
            pending_g: false,
            pending_r: None,
            search_pat: None,
        }
    }

    /// Set the search pattern (interpreted as raw ASCII bytes) and jump
    /// to the next match starting from the byte after the cursor.
    /// Returns `true` on a match, `false` on no-match.
    pub fn set_search_and_find(&mut self, pat: &str) -> bool {
        if pat.is_empty() {
            self.search_pat = None;
            return false;
        }
        self.search_pat = Some(pat.as_bytes().to_vec());
        self.search_next(false)
    }

    /// Move cursor to the next (or previous, if `backward`) match of the
    /// stored search pattern. Wraps around. `false` when no pattern set
    /// or no occurrence anywhere.
    pub fn search_next(&mut self, backward: bool) -> bool {
        let Some(pat) = self.search_pat.as_ref() else { return false; };
        if pat.is_empty() || self.bytes.is_empty() || pat.len() > self.bytes.len() {
            return false;
        }
        let n = self.bytes.len();
        if !backward {
            // Search forward starting one byte past cursor; wrap.
            let start = (self.cursor + 1).min(n);
            for i in start..=n.saturating_sub(pat.len()) {
                if self.bytes[i..].starts_with(pat) {
                    self.cursor = i;
                    self.nibble_high = true;
                    return true;
                }
            }
            for i in 0..start {
                if i + pat.len() <= n && self.bytes[i..].starts_with(pat) {
                    self.cursor = i;
                    self.nibble_high = true;
                    return true;
                }
            }
        } else {
            // Backward: scan from the byte just before cursor down, then wrap.
            let cursor = self.cursor;
            let mut i = cursor;
            loop {
                if i == 0 { break; }
                i -= 1;
                if i + pat.len() <= n && self.bytes[i..].starts_with(pat) {
                    self.cursor = i;
                    self.nibble_high = true;
                    return true;
                }
            }
            let mut i = n.saturating_sub(pat.len());
            while i > cursor {
                if self.bytes[i..].starts_with(pat) {
                    self.cursor = i;
                    self.nibble_high = true;
                    return true;
                }
                if i == 0 { break; }
                i -= 1;
            }
        }
        false
    }

    /// Open `path` as bytes. Missing file → empty buffer with `is_new`.
    pub fn load(&mut self, path: &Path) -> io::Result<()> {
        let (bytes, is_new) = match std::fs::read(path) {
            Ok(b)  => (b, false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), true),
            Err(e) => return Err(e),
        };
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.path = Some(abs);
        self.saved_bytes = bytes.clone();
        self.bytes = bytes;
        self.cursor = 0;
        self.nibble_high = true;
        self.scroll.set(0);
        self.dirty = false;
        self.is_new = is_new;
        self.external_conflict = None;
        self.anchor = None;
        self.capture_disk_stats();
        Ok(())
    }

    /// Build from raw bytes already in memory — used when swapping an
    /// `Editor` into a `HexView` (we don't re-read from disk so a dirty
    /// buffer carries across the swap).
    pub fn from_bytes(path: Option<PathBuf>, bytes: Vec<u8>, dirty: bool, is_new: bool) -> Self {
        let mut v = Self::empty();
        v.path = path;
        // `saved_bytes` represents what's on disk. If the source buffer
        // was dirty, we don't actually know the on-disk content — leave
        // saved_bytes empty so the dirty flag stays correct. If it was
        // clean, the in-memory bytes match disk.
        v.saved_bytes = if dirty { Vec::new() } else { bytes.clone() };
        v.bytes = bytes;
        v.dirty = dirty;
        v.is_new = is_new;
        v.capture_disk_stats();
        v
    }

    pub fn save(&mut self) -> io::Result<()> {
        let Some(p) = self.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::Other, "no file name — try :w <path>"));
        };
        self.save_to(&p)
    }

    pub fn save_as(&mut self, path: &Path) -> io::Result<()> {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let r = self.save_to(&abs);
        self.path = Some(abs);
        r
    }

    fn save_to(&mut self, p: &Path) -> io::Result<()> {
        if let Some(dir) = p.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        std::fs::write(p, &self.bytes)?;
        self.saved_bytes = self.bytes.clone();
        self.dirty = false;
        self.is_new = false;
        self.external_conflict = None;
        self.capture_disk_stats();
        Ok(())
    }

    pub fn reconcile(&mut self) -> ReconcileOutcome {
        let Some(path) = self.path.clone() else {
            return ReconcileOutcome::NoOp;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            self.external_conflict = Some(ExternalConflict::Deleted);
            return ReconcileOutcome::Deleted;
        };
        let disk_mtime = meta.modified().ok();
        let disk_size  = meta.len();
        if disk_mtime.is_some()
            && disk_mtime == self.saved_mtime
            && Some(disk_size) == self.saved_size
        {
            if self.external_conflict == Some(ExternalConflict::Deleted) {
                self.external_conflict = None;
            }
            return ReconcileOutcome::NoOp;
        }
        let Ok(disk) = std::fs::read(&path) else {
            return ReconcileOutcome::NoOp;
        };
        if disk == self.saved_bytes {
            self.saved_mtime = disk_mtime;
            self.saved_size  = Some(disk_size);
            return ReconcileOutcome::NoOp;
        }
        if self.dirty {
            self.external_conflict = Some(ExternalConflict::ModifiedOnDisk);
            ReconcileOutcome::ConflictMarked
        } else {
            self.bytes = disk.clone();
            self.saved_bytes = disk;
            self.saved_mtime = disk_mtime;
            self.saved_size  = Some(disk_size);
            self.cursor = self.cursor.min(self.bytes.len().saturating_sub(1));
            self.external_conflict = None;
            ReconcileOutcome::AutoReloaded
        }
    }

    fn capture_disk_stats(&mut self) {
        let Some(path) = self.path.as_ref() else { return; };
        let Ok(meta) = std::fs::metadata(path) else { return; };
        self.saved_mtime = meta.modified().ok();
        self.saved_size  = Some(meta.len());
    }

    pub fn file_name(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    pub fn take_status(&mut self) -> Option<String> {
        self.last_status.take()
    }

    /// Try converting the current byte buffer back to UTF-8 text. `Ok`
    /// returns the textarea-ready string; `Err` carries an error on
    /// invalid UTF-8 (caller can offer `:edit!` for lossy conversion).
    pub fn to_text(&self) -> Result<String, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes).map(|s| s.to_string())
    }

    pub fn to_text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    /// Currently selected byte range (inclusive low, exclusive high)
    /// when in visual; `None` outside visual.
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        let lo = a.min(self.cursor);
        let hi = a.max(self.cursor) + 1;
        Some((lo, hi.min(self.bytes.len().max(1))))
    }

    // ── motions ──────────────────────────────────────────────────────

    fn move_by(&mut self, delta: isize) {
        let n = self.bytes.len();
        if n == 0 { self.cursor = 0; return; }
        let cur = self.cursor as isize;
        let nx = (cur + delta).clamp(0, (n - 1) as isize);
        self.cursor = nx as usize;
        self.nibble_high = true;
    }

    fn move_to(&mut self, off: usize) {
        let n = self.bytes.len();
        self.cursor = if n == 0 { 0 } else { off.min(n - 1) };
        self.nibble_high = true;
    }

    fn row_start(&self) -> usize {
        let bpr = self.bytes_per_row.get().max(1) as usize;
        (self.cursor / bpr) * bpr
    }

    fn row_end(&self) -> usize {
        let bpr = self.bytes_per_row.get().max(1) as usize;
        let n   = self.bytes.len();
        if n == 0 { return 0; }
        ((self.cursor / bpr) * bpr + bpr - 1).min(n - 1)
    }

    // ── normal mode ──────────────────────────────────────────────────

    pub fn handle_normal(&mut self, key: KeyEvent) -> HexAction {
        use KeyCode::*;
        let m = key.modifiers;
        let was_g = std::mem::take(&mut self.pending_g);

        // `r<hex><hex>` replace — overwrite the byte under the cursor
        // without entering Insert mode. Esc or any non-hex key cancels.
        if let Some(rstate) = self.pending_r {
            if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::ALT) {
                self.pending_r = None;
                return HexAction::None;
            }
            if let Char(c) = key.code {
                if let Some(nib) = hex_nibble(c) {
                    match rstate {
                        None => { self.pending_r = Some(Some(nib)); }
                        Some(hi) => {
                            if let Some(b) = self.bytes.get_mut(self.cursor) {
                                let nv = (hi << 4) | nib;
                                if *b != nv { *b = nv; self.recompute_dirty(); }
                            }
                            self.pending_r = None;
                        }
                    }
                    return HexAction::None;
                }
            }
            // Any non-hex key cancels the pending replace.
            self.pending_r = None;
            return HexAction::None;
        }

        // Composed motions first.
        if m == KeyModifiers::NONE && key.code == Char('g') {
            if was_g {
                self.move_to(0);
                self.scroll.set(0);
            } else {
                self.pending_g = true;
            }
            return HexAction::None;
        }

        let bpr = self.bytes_per_row.get().max(1) as isize;
        match (m, key.code) {
            (KeyModifiers::NONE, Char('h')) | (_, Left)  => { self.move_by(-1); }
            (KeyModifiers::NONE, Char('l')) | (_, Right) => { self.move_by( 1); }
            (KeyModifiers::NONE, Char('j')) | (_, Down)  => { self.move_by( bpr); }
            (KeyModifiers::NONE, Char('k')) | (_, Up)    => { self.move_by(-bpr); }
            (KeyModifiers::NONE, Char('w'))               => { self.move_by( bpr); }
            (KeyModifiers::NONE, Char('b'))               => { self.move_by(-bpr); }
            (KeyModifiers::NONE, Char('0')) | (_, Home)   => {
                let r = self.row_start(); self.move_to(r);
            }
            (_, Char('$')) | (_, End)                     => {
                let r = self.row_end(); self.move_to(r);
            }
            (_, Char('G'))                                => {
                let n = self.bytes.len();
                self.move_to(n.saturating_sub(1));
            }
            (KeyModifiers::NONE, PageDown)                => {
                let step = (self.viewport_rows.get() as isize) * bpr;
                self.move_by(step.max(bpr));
            }
            (KeyModifiers::NONE, PageUp)                  => {
                let step = (self.viewport_rows.get() as isize) * bpr;
                self.move_by(-step.max(bpr));
            }
            (KeyModifiers::NONE, Char('r')) => {
                self.pending_r = Some(None);
                return HexAction::None;
            }
            (KeyModifiers::NONE, Char('x')) => {
                if let Some(b) = self.bytes.get_mut(self.cursor) {
                    if *b != 0 {
                        *b = 0;
                        self.dirty = true;
                    }
                }
            }
            (KeyModifiers::NONE, Char('i')) => {
                self.nibble_high = true;
                return HexAction::EnterInsert;
            }
            (KeyModifiers::NONE, Char('a')) => {
                // `a` parks on the current byte's high nibble too —
                // overwrite-only means there's no "after the byte"
                // gap to land in like vim's append.
                self.nibble_high = true;
                return HexAction::EnterInsert;
            }
            (KeyModifiers::NONE, Char('v')) => {
                self.anchor = Some(self.cursor);
                return HexAction::EnterVisual;
            }
            _ => {}
        }
        HexAction::None
    }

    // ── insert mode ──────────────────────────────────────────────────

    pub fn handle_insert(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            return; // App handles the mode flip; we just stop accepting nibbles.
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let bpr = self.bytes_per_row.get().max(1) as isize;
        match key.code {
            KeyCode::Char(c) => {
                if let Some(nib) = hex_nibble(c) {
                    self.write_nibble(nib);
                }
            }
            KeyCode::Backspace => {
                if !self.nibble_high {
                    // Half-written byte → undo the high nibble we just
                    // typed by restoring the saved value's high half.
                    let saved_hi = self.saved_bytes.get(self.cursor).copied().unwrap_or(0) & 0xF0;
                    if let Some(b) = self.bytes.get_mut(self.cursor) {
                        *b = saved_hi | (*b & 0x0F);
                    }
                    self.nibble_high = true;
                    self.recompute_dirty();
                } else if self.cursor > 0 {
                    self.cursor -= 1;
                    self.nibble_high = true;
                }
            }
            KeyCode::Left  => { self.move_by(-1); }
            KeyCode::Right => { self.move_by( 1); }
            KeyCode::Down  => { self.move_by( bpr); }
            KeyCode::Up    => { self.move_by(-bpr); }
            _ => {}
        }
    }

    fn write_nibble(&mut self, nib: u8) {
        if self.bytes.is_empty() {
            return;
        }
        let n = self.bytes.len();
        if self.cursor >= n {
            self.cursor = n - 1;
        }
        let b = &mut self.bytes[self.cursor];
        if self.nibble_high {
            *b = (nib << 4) | (*b & 0x0F);
            self.nibble_high = false;
        } else {
            *b = (*b & 0xF0) | nib;
            self.nibble_high = true;
            if self.cursor + 1 < n {
                self.cursor += 1;
            }
        }
        self.recompute_dirty();
    }

    fn recompute_dirty(&mut self) {
        self.dirty = self.bytes != self.saved_bytes;
    }

    // ── visual mode ──────────────────────────────────────────────────

    pub fn handle_visual(&mut self, key: KeyEvent) -> HexVisualAction {
        use KeyCode::*;
        let m = key.modifiers;
        // Movement extends the selection (the anchor stays put; only
        // `cursor` moves, and `selection_range` recomputes from both).
        let bpr = self.bytes_per_row.get().max(1) as isize;
        match (m, key.code) {
            (KeyModifiers::NONE, Char('h')) | (_, Left)  => { self.move_by(-1); }
            (KeyModifiers::NONE, Char('l')) | (_, Right) => { self.move_by( 1); }
            (KeyModifiers::NONE, Char('j')) | (_, Down)  => { self.move_by( bpr); }
            (KeyModifiers::NONE, Char('k')) | (_, Up)    => { self.move_by(-bpr); }
            (KeyModifiers::NONE, Char('0')) | (_, Home)  => {
                let r = self.row_start(); self.move_to(r);
            }
            (_, Char('$')) | (_, End)                    => {
                let r = self.row_end(); self.move_to(r);
            }
            (_, Char('G')) => { let n = self.bytes.len(); self.move_to(n.saturating_sub(1)); }
            (KeyModifiers::NONE, Char('g')) => {
                // gg in visual goes to start.
                self.move_to(0);
            }
            (KeyModifiers::NONE, Char('y')) => {
                // Yank itself is handled by the outer dispatcher (it
                // owns clipboard access). We just signal exit; main.rs
                // pulls the selection bytes via `selection_range` before
                // calling handle_visual, so the data is already in hand.
                self.anchor = None;
                return HexVisualAction::Exit;
            }
            (KeyModifiers::NONE, Char('x')) | (KeyModifiers::NONE, Char('d')) => {
                if let Some((lo, hi)) = self.selection_range() {
                    let mut changed = false;
                    for b in &mut self.bytes[lo..hi] {
                        if *b != 0 { *b = 0; changed = true; }
                    }
                    if changed { self.recompute_dirty(); }
                    self.cursor = lo;
                    self.last_status = Some(format!("zeroed {} bytes", hi - lo));
                }
                self.anchor = None;
                return HexVisualAction::Exit;
            }
            (KeyModifiers::NONE, Char('v')) => {
                self.anchor = None;
                return HexVisualAction::Exit;
            }
            _ => {}
        }
        HexVisualAction::Stay
    }

    /// Called by App on `enter_normal` — clear visual selection so the
    /// highlight goes away on the same Esc that flips the mode.
    pub fn cancel_selection(&mut self) {
        self.anchor = None;
    }
}

fn hex_nibble(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some((c as u8) - b'0'),
        'a'..='f' => Some((c as u8) - b'a' + 10),
        'A'..='F' => Some((c as u8) - b'A' + 10),
        _ => None,
    }
}
