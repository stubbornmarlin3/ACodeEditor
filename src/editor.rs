use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Style};
use tui_textarea::{CursorMove, Input, TextArea};

/// How the current buffer has diverged from disk since we last loaded
/// or saved. Discovered by the FS watcher (or a fallback poll) and
/// reported to the user via the cell title + status bar. Blocks a
/// naive `:w` until the user picks a resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalConflict {
    ModifiedOnDisk,
    Deleted,
}

/// Buffer-word completion popup. Items are full words; the user already
/// typed `prefix`, so accepting splices `items[selected][prefix.len()..]`
/// into the buffer at the cursor (which is parked right after `prefix`).
#[derive(Debug, Clone)]
pub struct CompletionPopup {
    pub items:    Vec<String>,
    pub selected: usize,
    pub prefix:   String,
}

/// Vim operators that compose with a motion (or a text object) to form
/// a change. `Yank` only copies — it never enters insert — so it shares
/// most of the plumbing with `Delete`/`Change` but branches at the
/// apply step.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Operator { Delete, Change, Yank }

/// "Inner" = `iw`, `i(` … skip surrounding whitespace/delimiters.
/// "Around" = `aw`, `a(` … include them. Vim's `i`/`a` prefix.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TextObjScope { Inner, Around }

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TextObjKind {
    Word,
    Paren,
    Bracket,
    Brace,
    DoubleQuote,
    SingleQuote,
}

/// Multi-key state. `None` is the usual case — a single keystroke
/// resolves to a command. A non-`None` value means the editor is
/// waiting on a follow-up key (`g` after `g`, the motion after `d`,
/// the char after `r`, etc.). Any key that doesn't make sense in the
/// pending context clears it.
/// Where to park the cursor's visual row in the viewport for `zt` /
/// `zz` / `zb`.
#[derive(Copy, Clone, Debug)]
pub enum ScrollTo {
    Top,
    Middle,
    Bottom,
}

/// Sticky state kept across consecutive `gj` / `gk` so repeated hops
/// chase the same visual column even as line lengths change. Invalid
/// once the cursor has moved to anywhere other than the remembered
/// (row, col).
#[derive(Copy, Clone, Debug)]
pub struct GjSticky {
    pub screen_col: u16,
    pub last_row:   usize,
    pub last_col:   usize,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Pending {
    None,
    /// After `g` — expecting `g` (top of file) or `e` (word-end back).
    Goto,
    /// After `z` — expecting a view-positioning key (`t`/`z`/`b`, or
    /// `h`/`l` for horizontal scroll in nowrap mode).
    Zoom,
    /// After `d`/`c`/`y` — expecting a motion.
    Operator(Operator),
    /// After `dg`/`cg`/`yg` — expecting `g` (operate to top of file).
    OperatorGoto(Operator),
    /// After `di`/`da`/`ci`/`ca`/`yi`/`ya` — expecting a text-object kind.
    OperatorTextObj(Operator, TextObjScope),
    /// After `r` — expecting the replacement character.
    Replace,
}

/// Motions that compose with an operator. Subset of vim's motion set —
/// the ones we support here plus enough variants to round-trip a `.`
/// replay. Counts are handled separately via [`Editor::count`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Motion {
    CharForward,
    CharBack,
    LineDown,
    LineUp,
    WordForward,
    WordBack,
    WordEnd,
    LineHead,
    LineEnd,
    FirstNonBlank,
    FileTop,
    FileBottom,
    ParagraphForward,
    ParagraphBack,
}

/// The most recent editing action. `.` replays this, so it only stores
/// the subset of actions we know how to rerun without recording a key
/// stream. Insert-mode sequences (`i…<Esc>`, `o…<Esc>`) are recorded
/// with their inserted text so `.` can re-type them verbatim — that's
/// the bulk of everyday `.` usage.
#[derive(Clone, Debug)]
enum LastChange {
    DeleteChar,
    DeleteBack,
    OpMotion(Operator, Motion, u32),
    OpTextObj(Operator, TextObjScope, TextObjKind),
    OpLine(Operator, u32),
    DeleteToEol,
    ChangeToEol,
    JoinLine(u32),
    Paste(bool /* before */),
    Replace(char),
    /// Any change that drops into insert mode and captures what was
    /// typed before `Esc`. Replayed by re-running the insert-entry then
    /// typing the captured text.
    InsertSeq { entry: InsertEntry, text: String },
    /// Shift-< or >> — indent/dedent current line.
    ShiftLine(bool /* indent */),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum InsertEntry {
    /// `i` — insert at cursor.
    AtCursor,
    /// `a` — insert after cursor (cursor moved forward first).
    AfterCursor,
    /// `I` — insert at first non-blank on current line.
    FirstNonBlank,
    /// `A` — append at end of current line.
    LineEnd,
    /// `o` — open new line below.
    OpenBelow,
    /// `O` — open new line above.
    OpenAbove,
    /// `s` — substitute single char (delete then insert).
    SubstituteChar,
    /// `S` / `cc` — substitute whole line.
    SubstituteLine,
    /// `C` — change to end of line.
    ChangeToEol,
    /// `c{motion}` / `ciw` / `caw` / `ci(` / `ca(` … — any change op.
    /// Replaying just deletes the same region again and retypes.
    ChangeOp(ChangeReplay),
}

/// How to re-select the region a `c`-op changed, for `.` replay.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ChangeReplay {
    Motion(Motion, u32),
    TextObj(TextObjScope, TextObjKind),
    Line(u32),
}

/// What the caller should do after [`Editor::handle_normal`] returns —
/// the editor doesn't own the app's mode so the outer loop flips it.
pub enum EditorAction {
    None,
    EnterInsert,
    EnterVisualChar,
    EnterVisualLine,
}

/// One-file editor buffer backed by `tui_textarea::TextArea`.
pub struct Editor {
    pub textarea: TextArea<'static>,
    pub path:     Option<PathBuf>,
    pub dirty:    bool,
    saved_lines:  Vec<String>,
    saved_hash:   u64,
    saved_mtime:  Option<SystemTime>,
    saved_size:   Option<u64>,
    pub external_conflict: Option<ExternalConflict>,
    /// True for help / preview style buffers — insert-mode input and
    /// any mutation op (`x`, `dd`, `p`, …) are swallowed so the content
    /// can't drift.
    pub read_only: bool,
    /// This buffer has a path set but the file doesn't exist on disk
    /// yet — either the user opened a nonexistent path via `:e <path>`
    /// or launched `ace new.txt`. The first `:w` creates the file and
    /// clears this. Drives the `[NEW]` title badge.
    pub is_new: bool,
    /// The default scratch-welcome buffer. Rendered with the welcome
    /// label + ASCII art. Any insert-mode keystroke clears the flag,
    /// wipes the placeholder content, and drops into a regular
    /// unnamed [NEW] scratch.
    pub is_welcome: bool,
    /// Multi-key operator / goto state. Cleared on Esc or on any key
    /// that doesn't compose with it.
    pending: Pending,
    /// Prefix count being composed — `3dw`, `5j`, `12gg`. `0` is
    /// reserved for the "line-start" motion when no count is building,
    /// so we only treat `0` as part of a count if `count > 0` already.
    count: u32,
    /// Last replayable change. `.` re-applies it.
    last_change: Option<LastChange>,
    /// Our parallel yank buffer. tui_textarea's built-in yank is a
    /// single string; we extend it with a linewise flag so `p` vs.
    /// charwise-paste do the right thing. Kept in sync with
    /// `textarea.set_yank_text` when we overwrite it.
    yank_text:     String,
    yank_linewise: bool,
    /// One-shot status message — set by yank / paste so the outer loop
    /// can surface it in the status bar. Drained by `take_status`.
    last_status:   Option<String>,
    /// Soft-wrap toggle. When true (default), long lines reflow onto
    /// multiple visual rows — the buffer is unchanged; only rendering
    /// changes. Logical motions (`j`/`k`/`0`/`$`) still move by
    /// logical line, matching vim's default. Rendering path lives in
    /// `ui::draw_editor_wrapped`; `:set wrap` / `:set nowrap` toggle.
    pub wrap: bool,
    /// Syntax highlighter for this buffer. `None` for unsupported file types.
    pub syntax: Option<crate::syntax::SyntaxHighlighter>,
    /// Set whenever the buffer is mutated; cleared by `sync_syntax` in the
    /// main loop just before the next draw pass.
    pub syntax_stale: bool,
    /// Top-most *visual* row currently on screen. Interior mutability so
    /// the render pass (which holds `&Editor`) can update scroll to keep
    /// the cursor in view without making the whole render signature
    /// take `&mut`.
    pub scroll_top: Cell<usize>,
    /// Content width (in screen columns) the renderer last used. Drives
    /// `gj` / `gk` visual-row motions, which need to rebuild the wrap
    /// layout at motion time. 0 means "no render yet".
    pub last_content_w: Cell<u16>,
    /// Sticky visual column preserved across consecutive `gj` / `gk`
    /// motions (matches vim's intra-`g{jk}` column memory). The tuple
    /// bundles the remembered screen col with the cursor position
    /// after the last `g{jk}` hop — on the next `g{jk}` we check the
    /// cursor is still where we left it; if not, some other motion
    /// fired in between and the sticky column is stale.
    pub gj_sticky: Cell<Option<GjSticky>>,
    /// `:set list` — render tabs / trailing spaces / EOL markers.
    pub list_mode: bool,
    /// `:set autopair` (default on). When typing an opener (`(`, `[`,
    /// `{`, `"`, `'`, `` ` ``) the matching closer is inserted and the
    /// cursor parked between them; typing the closer when it already
    /// sits at the cursor steps over it; Backspace inside an empty pair
    /// deletes both sides.
    pub autopair: bool,
    /// `:set autoindent` (default on). Enter copies the previous line's
    /// leading whitespace; if the char before the cursor is `{`, `(`,
    /// or `[` the new line gets one extra level; if Enter is pressed
    /// between matched pairs (e.g. `{|}`) the closer drops to its own
    /// line and the cursor sits on a blank, indented middle line.
    pub autoindent: bool,
    /// `:set expandtab` (default on). Tab in insert mode inserts four
    /// spaces; with `noexpandtab` it inserts a literal tab. Shift-Tab
    /// always dedents one step regardless of this setting.
    pub expandtab: bool,
    /// `:set completion` (default on). When true, typing a 2+ char
    /// identifier prefix in insert mode auto-arms a popup of matching
    /// words drawn from the current buffer.
    pub completion_enabled: bool,
    /// Active completion popup, if any. Refreshed at the end of every
    /// insert-mode keystroke; `None` when there's no eligible prefix or
    /// no candidates.
    pub completion: Option<CompletionPopup>,
    /// Viewport height (in rows) the renderer last used. Drives `zt` /
    /// `zz` / `zb` so we can reposition the cursor row within the
    /// window without a round-trip through the render pipeline.
    pub last_viewport_h: Cell<u16>,
    /// Insert-mode capture: while in insert after an entry like `i` or
    /// `o`, every keystroke appends to `insert_buffer`. On `Esc`, the
    /// app calls `end_insert()` which rolls it into `last_change`.
    insert_entry:  Option<InsertEntry>,
    insert_buffer: String,
}

impl Editor {
    pub fn empty() -> Self {
        let mut textarea = TextArea::default();
        style_textarea(&mut textarea);
        let saved_lines = textarea.lines().to_vec();
        let saved_hash = hash_lines(&saved_lines);
        Self {
            textarea,
            path: None,
            dirty: false,
            saved_lines,
            saved_hash,
            saved_mtime: None,
            saved_size:  None,
            external_conflict: None,
            read_only: false,
            is_new:     false,
            is_welcome: false,
            pending: Pending::None,
            count: 0,
            last_change: None,
            yank_text: String::new(),
            yank_linewise: false,
            last_status: None,
            wrap: true,
            syntax: None,
            syntax_stale: false,
            scroll_top: Cell::new(0),
            last_content_w: Cell::new(0),
            gj_sticky: Cell::new(None),
            list_mode: false,
            autopair: true,
            autoindent: true,
            expandtab: true,
            completion_enabled: true,
            completion: None,
            last_viewport_h: Cell::new(0),
            insert_entry: None,
            insert_buffer: String::new(),
        }
    }

    /// Default-landing-pad buffer: shows the ACE banner + a couple of
    /// pointers. Writable (so the user can just start typing), but
    /// flagged `is_welcome` — on the first keystroke the placeholder
    /// content is wiped and the flag flips so the buffer becomes a
    /// regular unnamed scratch (`unknown [NEW]`).
    pub fn welcome() -> Self {
        // The banner + hints are drawn by `ui::draw_welcome` — the
        // backing textarea is empty so we can reuse `empty()` and
        // just flip the flag. First keystroke clears is_welcome and
        // the regular textarea takes over.
        let mut ed = Self::empty();
        ed.is_welcome = true;
        ed
    }

    /// Build a read-only buffer prefilled with `content`. Used by
    /// `:help` — the buffer is navigable with every normal-mode motion
    /// but insert/delete are no-ops.
    pub fn read_only_from(content: &str, title: &str) -> Self {
        let lines: Vec<String> = if content.is_empty() {
            vec![String::new()]
        } else {
            content.lines().map(String::from).collect()
        };
        let mut textarea = TextArea::new(lines);
        style_textarea(&mut textarea);
        let saved_lines = textarea.lines().to_vec();
        let saved_hash = hash_lines(&saved_lines);
        Self {
            textarea,
            // Synthetic `[help]`-style path so the tab shows a name
            // without touching disk. The `title` comes in as e.g.
            // "[help]"; we only use it for display via file_name().
            path: Some(PathBuf::from(title)),
            dirty: false,
            saved_lines,
            saved_hash,
            saved_mtime: None,
            saved_size:  None,
            external_conflict: None,
            read_only: true,
            is_new:     false,
            is_welcome: false,
            pending: Pending::None,
            count: 0,
            last_change: None,
            yank_text: String::new(),
            yank_linewise: false,
            last_status: None,
            wrap: true,
            syntax: None,
            syntax_stale: false,
            scroll_top: Cell::new(0),
            last_content_w: Cell::new(0),
            gj_sticky: Cell::new(None),
            list_mode: false,
            autopair: true,
            autoindent: true,
            expandtab: true,
            completion_enabled: true,
            completion: None,
            last_viewport_h: Cell::new(0),
            insert_entry: None,
            insert_buffer: String::new(),
        }
    }

    /// Open `path`. A nonexistent path is NOT an error — we set it as
    /// the buffer's path with `is_new = true` so the first `:w` creates
    /// the file. Matches vim's `:e <nonexistent>` behaviour.
    pub fn load(&mut self, path: &Path) -> io::Result<()> {
        let (lines, is_new) = match std::fs::read_to_string(path) {
            Ok(content) => {
                let lines: Vec<String> = if content.is_empty() {
                    vec![String::new()]
                } else {
                    content.lines().map(String::from).collect()
                };
                (lines, false)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                (vec![String::new()], true)
            }
            Err(e) => return Err(e),
        };
        self.textarea = TextArea::new(lines);
        style_textarea(&mut self.textarea);
        // Store the absolute form. Relative paths would break the
        // `[EXTERNAL]` check (prefix-compared against the absolute
        // project root), git tinting, and any later `:proj add` that
        // changes the cwd underneath us. `absolute` canonicalizes the
        // dotted form without resolving symlinks — important on
        // Windows where OneDrive reparse points would otherwise move
        // the stored path off the git workdir.
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        self.path = Some(abs.clone());
        self.saved_lines = self.textarea.lines().to_vec();
        self.saved_hash  = hash_lines(&self.saved_lines);
        self.capture_disk_stats();
        self.dirty = false;
        self.external_conflict = None;
        self.read_only = false;
        self.is_welcome = false;
        self.is_new = is_new;
        self.pending = Pending::None;
        self.count = 0;
        // Build a fresh syntax highlighter for the new file type.
        self.syntax = crate::syntax::SyntaxHighlighter::new(&abs);
        self.syntax_stale = true;
        Ok(())
    }

    pub fn save(&mut self) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(io::ErrorKind::Other, "read-only buffer"));
        }
        let Some(p) = self.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::Other, "no file name — try :w <path>"));
        };
        self.save_to(&p)
    }

    /// `:w <path>` — write to an explicit path, adopting it as the
    /// buffer's own. Used to give an unnamed/scratch buffer a file.
    /// Vim's `:w path` also does this when the current buffer has no
    /// name; when it does, `:saveas` is the dedicated command.
    pub fn save_as(&mut self, path: &Path) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(io::ErrorKind::Other, "read-only buffer"));
        }
        // Absolutize so the stored path lines up with project prefixes
        // and git workdir paths — same reason as `load`.
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let result = self.save_to(&abs);
        self.path = Some(abs);
        result
    }

    fn save_to(&mut self, p: &Path) -> io::Result<()> {
        let mut text = self.textarea.lines().join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if let Some(dir) = p.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        std::fs::write(p, text)?;
        self.saved_lines = self.textarea.lines().to_vec();
        self.saved_hash  = hash_lines(&self.saved_lines);
        self.capture_disk_stats();
        self.dirty = false;
        self.external_conflict = None;
        // Fresh on-disk file — any "new" / "welcome" placeholder state
        // is now a real named buffer.
        self.is_new = false;
        self.is_welcome = false;
        // Rebuild syntax: a `[NEW]` buffer being saved for the first
        // time under a name like `foo.rs` needs its highlighter created
        // now; a `:w somename` that changes the path may switch
        // languages. Cheap — `SyntaxHighlighter::new` only looks at the
        // extension.
        self.syntax = crate::syntax::SyntaxHighlighter::new(p);
        self.syntax_stale = true;
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

        let Ok(content) = std::fs::read_to_string(&path) else {
            return ReconcileOutcome::NoOp;
        };
        let disk_lines: Vec<String> = if content.is_empty() {
            vec![String::new()]
        } else {
            content.lines().map(String::from).collect()
        };
        let disk_hash = hash_lines(&disk_lines);

        if disk_hash == self.saved_hash {
            self.saved_mtime = disk_mtime;
            self.saved_size  = Some(disk_size);
            return ReconcileOutcome::NoOp;
        }

        if self.dirty {
            self.external_conflict = Some(ExternalConflict::ModifiedOnDisk);
            ReconcileOutcome::ConflictMarked
        } else {
            self.textarea = TextArea::new(disk_lines.clone());
            style_textarea(&mut self.textarea);
            self.saved_lines = disk_lines;
            self.saved_hash  = disk_hash;
            self.saved_mtime = disk_mtime;
            self.saved_size  = Some(disk_size);
            self.external_conflict = None;
            self.syntax_stale = true;
            ReconcileOutcome::AutoReloaded
        }
    }

    pub fn reload_from_disk(&mut self) -> io::Result<()> {
        let Some(p) = self.path.clone() else {
            return Err(io::Error::new(io::ErrorKind::Other, "no file to reload"));
        };
        self.load(&p)
    }

    fn capture_disk_stats(&mut self) {
        let Some(path) = self.path.as_ref() else { return; };
        let Ok(meta) = std::fs::metadata(path) else { return; };
        self.saved_mtime = meta.modified().ok();
        self.saved_size  = Some(meta.len());
    }

    // ── insert mode ──────────────────────────────────────────────────

    /// Called while App.mode == Insert and focus is Edit.
    pub fn handle_insert(&mut self, key: KeyEvent) {
        // Cycle/accept routes don't move the cursor in a way that would
        // change the completion prefix, so they don't need a refresh.
        // All other paths fall through to `dispatch` then refresh.
        let popup_was_open = self.completion.is_some();
        let popup_consumed_key = popup_was_open && matches!(
            key.code,
            KeyCode::Tab | KeyCode::Down |
            KeyCode::BackTab | KeyCode::Up |
            KeyCode::Enter
        );
        self.handle_insert_dispatch(key);
        if !popup_consumed_key {
            if self.completion_enabled {
                self.refresh_completion();
            } else {
                self.completion = None;
            }
        }
    }

    fn handle_insert_dispatch(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc { return; }
        if self.read_only { return; }
        // First keystroke on the welcome page demotes it: wipe the
        // placeholder content and clear the flag so the buffer behaves
        // like a normal unnamed scratch ("unknown [NEW]") from here on.
        if self.is_welcome {
            self.textarea = TextArea::new(vec![String::new()]);
            style_textarea(&mut self.textarea);
            self.saved_lines = self.textarea.lines().to_vec();
            self.saved_hash  = hash_lines(&self.saved_lines);
            self.is_welcome = false;
        }
        // Popup-active key handling. Sits BEFORE capture/autopair so
        // Tab/Enter cycling and accepting take priority over the
        // indent-step Tab and smart-newline Enter behaviours below.
        if self.completion.is_some() {
            match key.code {
                KeyCode::Tab | KeyCode::Down => {
                    self.cycle_completion(1);
                    return;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    self.cycle_completion(-1);
                    return;
                }
                KeyCode::Enter => {
                    self.accept_completion();
                    return;
                }
                _ => {}
            }
        }
        // Capture the raw text that lands in the buffer so `.` can
        // replay the whole "entry + typed text + Esc" sequence.
        if self.insert_entry.is_some() {
            match key.code {
                KeyCode::Char(c) => self.insert_buffer.push(c),
                KeyCode::Enter   => self.insert_buffer.push('\n'),
                KeyCode::Tab     => self.insert_buffer.push('\t'),
                KeyCode::Backspace => { self.insert_buffer.pop(); }
                _ => {}
            }
        }
        // Autopair lane — only intercepts plain (no Ctrl/Alt) key events
        // where it has something to do; everything else falls through to
        // tui-textarea's normal Input handling.
        let plain = !key.modifiers.intersects(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
        );
        if self.autopair && plain {
            match key.code {
                KeyCode::Char(c) if is_autopair_char(c) => {
                    self.type_char_autopair(c);
                    self.recompute_dirty();
                    return;
                }
                KeyCode::Backspace if self.try_autopair_backspace() => {
                    self.recompute_dirty();
                    return;
                }
                _ => {}
            }
        }
        if self.autoindent && plain && key.code == KeyCode::Enter {
            self.smart_newline();
            self.recompute_dirty();
            return;
        }
        // Tab → one indent step. With expandtab (default) that's four
        // spaces; otherwise a literal tab. The capture loop above
        // pushed `'\t'` for `.`-replay, so when we substitute spaces
        // we have to rewrite the last byte of insert_buffer to keep
        // replay aligned with what the buffer actually received.
        if plain && key.code == KeyCode::Tab {
            let step: &str = if self.expandtab { "    " } else { "\t" };
            if self.insert_entry.is_some() && self.expandtab {
                self.insert_buffer.pop(); // drop the '\t' just captured
                self.insert_buffer.push_str(step);
            }
            self.textarea.insert_str(step);
            self.recompute_dirty();
            return;
        }
        // Shift-Tab dedents the current line, regardless of cursor
        // column. Structural, so it doesn't get captured for `.`.
        if key.code == KeyCode::BackTab {
            self.dedent_current_line();
            self.recompute_dirty();
            return;
        }
        let input = Input::from(key);
        if self.textarea.input(input) {
            self.recompute_dirty();
        }
    }

    /// Insert `c` with autopair semantics. Caller is responsible for
    /// `recompute_dirty` afterwards.
    fn type_char_autopair(&mut self, c: char) {
        let (next, prev) = self.surrounding_chars();
        let is_open  = matches!(c, '(' | '[' | '{');
        let is_close = matches!(c, ')' | ']' | '}');
        let is_quote = matches!(c, '"' | '\'' | '`');

        // Skip-over: typing a closer or quote that already sits to the
        // right of the cursor steps past it instead of duplicating.
        // Without this the autopaired closer would force the user to
        // either delete it or arrow past it, which defeats the purpose.
        if (is_close || is_quote) && next == Some(c) {
            self.textarea.move_cursor(CursorMove::Forward);
            return;
        }

        // Quotes only pair when neither neighbour is a word char, so
        // apostrophes mid-word ("don't") and quotes typed inside an
        // identifier don't sprout a stray closer.
        let pair = if is_open {
            true
        } else if is_quote {
            let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
            !prev.is_some_and(is_word) && !next.is_some_and(is_word)
        } else {
            false
        };

        if pair {
            let close = match c { '(' => ')', '[' => ']', '{' => '}', q => q };
            self.textarea.insert_char(c);
            self.textarea.insert_char(close);
            self.textarea.move_cursor(CursorMove::Back);
        } else {
            self.textarea.insert_char(c);
        }
    }

    /// Backspace inside an empty matched pair deletes both halves.
    /// Returns `true` when it consumed the keystroke.
    fn try_autopair_backspace(&mut self) -> bool {
        let (next, prev) = self.surrounding_chars();
        let matched = matches!(
            (prev, next),
            (Some('('), Some(')'))
            | (Some('['), Some(']'))
            | (Some('{'), Some('}'))
            | (Some('"'), Some('"'))
            | (Some('\''), Some('\''))
            | (Some('`'), Some('`'))
        );
        if !matched { return false; }
        self.textarea.delete_next_char();
        self.textarea.delete_char();
        true
    }

    /// Insert a newline that preserves the current line's leading
    /// whitespace, optionally bumping one level when the cursor sits
    /// just after an opener and splitting when it sits between a
    /// matched pair.
    fn smart_newline(&mut self) {
        let (row, col) = self.textarea.cursor();
        let line = &self.textarea.lines()[row];
        let line_chars: Vec<char> = line.chars().collect();
        let prev = if col == 0 { None } else { line_chars.get(col - 1).copied() };
        let next = line_chars.get(col).copied();

        // Use the *current* line's existing indent as the baseline so
        // breaking mid-line stays aligned with the block, matching what
        // most editors do. Tabs vs. spaces is detected from that prefix
        // — we don't try to be smart about mixed indentation.
        let base: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        let step: &str = if base.starts_with('\t') { "\t" } else { "    " };

        let between_pair = matches!(
            (prev, next),
            (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']'))
        );
        let after_opener = matches!(prev, Some('{') | Some('(') | Some('['));

        if between_pair {
            // `{|}` → `{\n    \n}` with cursor on the indented middle.
            // We emit the closer line first then walk the cursor back
            // up so the user lands ready to type the block body.
            self.textarea.insert_newline();
            self.textarea.insert_str(&base);
            self.textarea.move_cursor(CursorMove::Up);
            self.textarea.move_cursor(CursorMove::End);
            self.textarea.insert_newline();
            let mut indent = base.clone();
            indent.push_str(step);
            self.textarea.insert_str(&indent);
        } else if after_opener {
            self.textarea.insert_newline();
            let mut indent = base.clone();
            indent.push_str(step);
            self.textarea.insert_str(&indent);
        } else {
            self.textarea.insert_newline();
            if !base.is_empty() {
                self.textarea.insert_str(&base);
            }
        }
    }

    /// Recompute the completion popup against the current cursor
    /// context. Closes it when the prefix is shorter than two chars,
    /// when there are no buffer-local matches, or when the only match
    /// is the prefix itself.
    fn refresh_completion(&mut self) {
        let (row, col) = self.textarea.cursor();
        let line = match self.textarea.lines().get(row) {
            Some(l) => l,
            None => { self.completion = None; return; }
        };
        let chars: Vec<char> = line.chars().collect();
        let mut start = col.min(chars.len());
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let prefix: String = chars[start..col.min(chars.len())].iter().collect();
        if prefix.chars().count() < 2 {
            self.completion = None;
            return;
        }

        // Buffer-local word index. Cheap enough to rebuild per keystroke
        // for normal-sized files; revisit if it ever shows up in profiles.
        let mut items: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for l in self.textarea.lines() {
            for w in extract_words(l) {
                if w.starts_with(&prefix) && w != prefix && seen.insert(w.to_string()) {
                    items.push(w.to_string());
                }
            }
        }
        if items.is_empty() {
            self.completion = None;
            return;
        }
        items.sort();

        // Try to keep the user's current pick stable across edits — if
        // the previously-selected word still appears in the list, ride
        // it along; otherwise reset to the first candidate.
        let selected = self.completion.as_ref()
            .and_then(|c| c.items.get(c.selected))
            .and_then(|cur| items.iter().position(|w| w == cur))
            .unwrap_or(0);
        self.completion = Some(CompletionPopup { items, selected, prefix });
    }

    fn cycle_completion(&mut self, delta: i32) {
        let Some(comp) = self.completion.as_mut() else { return; };
        let n = comp.items.len() as i32;
        if n == 0 { return; }
        let next = (comp.selected as i32 + delta).rem_euclid(n);
        comp.selected = next as usize;
    }

    fn accept_completion(&mut self) {
        let Some(comp) = self.completion.take() else { return; };
        let item = &comp.items[comp.selected];
        // `starts_with(&prefix)` is enforced when we built the list, so
        // the suffix slice is always at a valid byte boundary.
        let extra = &item[comp.prefix.len()..];
        if extra.is_empty() { return; }
        self.textarea.insert_str(extra);
        if self.insert_entry.is_some() {
            self.insert_buffer.push_str(extra);
        }
        self.recompute_dirty();
    }

    /// Esc dismissal hook called from main.rs's global Esc handler so
    /// the user can close the popup without exiting insert mode.
    /// Returns `true` when a popup was actually closed.
    pub fn try_dismiss_completion(&mut self) -> bool {
        if self.completion.is_some() {
            self.completion = None;
            true
        } else {
            false
        }
    }

    /// Remove up to one indent step from the start of the current
    /// line: a leading tab, else up to four leading spaces. Cursor
    /// column drifts left by however many chars were stripped, which
    /// matches what most editors do for Shift-Tab in insert mode.
    fn dedent_current_line(&mut self) {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines()[row].clone();
        let to_strip = if line.starts_with('\t') {
            1
        } else {
            line.chars().take(4).take_while(|c| *c == ' ').count()
        };
        if to_strip == 0 { return; }
        self.textarea.move_cursor(CursorMove::Jump(row as u16, 0));
        for _ in 0..to_strip {
            self.textarea.delete_next_char();
        }
        // Re-park the cursor where the user logically still is. If the
        // cursor was inside the stripped indent, clamp to column 0.
        let new_col = col.saturating_sub(to_strip);
        self.textarea.move_cursor(CursorMove::Jump(row as u16, new_col as u16));
    }

    /// `(next_char, prev_char)` around the cursor on the current line.
    /// Either side is `None` at the line edge.
    fn surrounding_chars(&self) -> (Option<char>, Option<char>) {
        let (row, col) = self.textarea.cursor();
        let line = &self.textarea.lines()[row];
        let chars: Vec<char> = line.chars().collect();
        let next = chars.get(col).copied();
        let prev = if col == 0 { None } else { chars.get(col - 1).copied() };
        (next, prev)
    }

    /// Called by the app when the user leaves insert mode. Rolls the
    /// captured keystrokes into `last_change` so `.` can replay them.
    pub fn end_insert(&mut self) {
        if let Some(entry) = self.insert_entry.take() {
            let text = std::mem::take(&mut self.insert_buffer);
            // Keep "no-op" sequences out of the dot register — e.g.
            // user pressed `i` then `Esc` with no typing in between;
            // replaying would re-enter insert but there's nothing to
            // do. Record anyway if the entry itself changed the buffer
            // (o/O/s/S/C/cc/c{motion}) because replay still needs to
            // re-create the line / re-delete the region.
            let entry_mutates = !matches!(entry, InsertEntry::AtCursor
                | InsertEntry::AfterCursor
                | InsertEntry::FirstNonBlank
                | InsertEntry::LineEnd);
            if !text.is_empty() || entry_mutates {
                self.last_change = Some(LastChange::InsertSeq { entry, text });
            }
        }
    }

    // ── normal mode ──────────────────────────────────────────────────

    /// Called while App.mode == Normal, focus is Edit, and Space is NOT held.
    /// Returns an [`EditorAction`] telling the outer loop whether to
    /// transition mode (enter Insert, enter Visual). Returning
    /// `EditorAction::None` means the key was either handled locally
    /// or ignored — the caller's Normal-mode fallbacks (`:`, space,
    /// `i`/`a` on non-editor cells) shouldn't run either, because we
    /// already consumed this key on behalf of the editor.
    pub fn handle_normal(&mut self, key: KeyEvent) -> EditorAction {
        use KeyCode::*;

        // Esc cancels any half-typed operator / goto / count.
        if key.code == Esc && key.modifiers == KeyModifiers::NONE {
            self.pending = Pending::None;
            self.count = 0;
            self.textarea.cancel_selection();
            return EditorAction::None;
        }

        // Resolve a pending replace target first — `r{c}` needs exactly
        // one follow-up char no matter what it is.
        if matches!(self.pending, Pending::Replace) {
            if let Char(c) = key.code {
                self.replace_char(c);
                self.last_change = Some(LastChange::Replace(c));
            }
            self.pending = Pending::None;
            self.count = 0;
            return EditorAction::None;
        }

        // Resolve a pending text-object kind — `diw` / `ca(` / …
        if let Pending::OperatorTextObj(op, scope) = self.pending {
            let kind = match key.code {
                Char('w') => Some(TextObjKind::Word),
                Char('(') | Char(')') => Some(TextObjKind::Paren),
                Char('[') | Char(']') => Some(TextObjKind::Bracket),
                Char('{') | Char('}') => Some(TextObjKind::Brace),
                Char('"') => Some(TextObjKind::DoubleQuote),
                Char('\'') => Some(TextObjKind::SingleQuote),
                _ => None,
            };
            self.pending = Pending::None;
            if let Some(k) = kind {
                let enter_insert = op == Operator::Change;
                self.apply_text_object(op, scope, k);
                self.last_change = Some(LastChange::OpTextObj(op, scope, k));
                self.count = 0;
                if enter_insert {
                    self.begin_insert(InsertEntry::ChangeOp(
                        ChangeReplay::TextObj(scope, k),
                    ));
                    return EditorAction::EnterInsert;
                }
            }
            return EditorAction::None;
        }

        // Resolve a pending `z` — cursor/view positioning.
        if matches!(self.pending, Pending::Zoom) {
            match key.code {
                Char('t') => self.scroll_cursor_to(ScrollTo::Top),
                Char('z') => self.scroll_cursor_to(ScrollTo::Middle),
                Char('b') => self.scroll_cursor_to(ScrollTo::Bottom),
                Char('h') | Left  => {
                    let n = self.take_count().max(1) as i16;
                    self.scroll_horizontal(-n);
                }
                Char('l') | Right => {
                    let n = self.take_count().max(1) as i16;
                    self.scroll_horizontal(n);
                }
                _ => {}
            }
            self.pending = Pending::None;
            self.count = 0;
            return EditorAction::None;
        }

        // Resolve a pending `g` — `gg` (top), `ge` (word-end back),
        // `gj`/`gk` (visual-row down/up under soft-wrap), `g0`/`g$`
        // (visual-row head/end).
        if matches!(self.pending, Pending::Goto) {
            match key.code {
                Char('g') => {
                    self.do_motion(Motion::FileTop, 1);
                }
                Char('e') => {
                    // There's no WordEndBack in the crate; approximate
                    // as "word back then one char back" (vim behaviour:
                    // ge lands on the last char of the previous word).
                    let n = self.take_count();
                    for _ in 0..n {
                        self.textarea.move_cursor(CursorMove::WordBack);
                        // Land on the end of that word by jumping
                        // forward to its end.
                        self.textarea.move_cursor(CursorMove::WordEnd);
                    }
                }
                Char('j') | Down => {
                    let n = self.take_count().max(1);
                    for _ in 0..n { self.move_visual(true); }
                }
                Char('k') | Up => {
                    let n = self.take_count().max(1);
                    for _ in 0..n { self.move_visual(false); }
                }
                Char('0') | Home => self.move_visual_home(),
                Char('$') | End  => self.move_visual_end(),
                _ => {}
            }
            self.pending = Pending::None;
            self.count = 0;
            return EditorAction::None;
        }

        // Resolve a pending `{op}g` — `dgg` / `ygg` / `cgg` (operate
        // from cursor to start of file).
        if let Pending::OperatorGoto(op) = self.pending {
            self.pending = Pending::None;
            if matches!(key.code, Char('g')) {
                let n = self.take_count().max(1);
                self.apply_operator_motion(op, Motion::FileTop, n);
                self.last_change = Some(LastChange::OpMotion(op, Motion::FileTop, n));
                if op == Operator::Change {
                    self.begin_insert(InsertEntry::ChangeOp(
                        ChangeReplay::Motion(Motion::FileTop, n),
                    ));
                    return EditorAction::EnterInsert;
                }
            }
            self.count = 0;
            return EditorAction::None;
        }

        // Resolve a pending operator (`d` / `c` / `y`) — expect a motion
        // or text-object prefix (`i`/`a`) or a doubled key for linewise.
        if let Pending::Operator(op) = self.pending {
            match key.code {
                Char('i') if key.modifiers == KeyModifiers::NONE => {
                    self.pending = Pending::OperatorTextObj(op, TextObjScope::Inner);
                    return EditorAction::None;
                }
                Char('a') if key.modifiers == KeyModifiers::NONE => {
                    self.pending = Pending::OperatorTextObj(op, TextObjScope::Around);
                    return EditorAction::None;
                }
                Char('g') if key.modifiers == KeyModifiers::NONE => {
                    self.pending = Pending::OperatorGoto(op);
                    return EditorAction::None;
                }
                // Doubled key → linewise op (`dd`, `yy`, `cc`).
                Char(c) if op == Operator::Delete && c == 'd'
                        || op == Operator::Yank   && c == 'y'
                        || op == Operator::Change && c == 'c' => {
                    self.pending = Pending::None;
                    let n = self.take_count().max(1);
                    self.apply_operator_line(op, n);
                    self.last_change = Some(LastChange::OpLine(op, n));
                    self.count = 0;
                    if op == Operator::Change {
                        self.begin_insert(InsertEntry::ChangeOp(ChangeReplay::Line(n)));
                        return EditorAction::EnterInsert;
                    }
                    return EditorAction::None;
                }
                _ => {}
            }
            // Otherwise try to parse the key as a motion.
            if let Some(m) = self.key_as_motion(&key) {
                self.pending = Pending::None;
                let n = self.take_count().max(1);
                self.apply_operator_motion(op, m, n);
                self.last_change = Some(LastChange::OpMotion(op, m, n));
                self.count = 0;
                if op == Operator::Change {
                    self.begin_insert(InsertEntry::ChangeOp(ChangeReplay::Motion(m, n)));
                    return EditorAction::EnterInsert;
                }
                return EditorAction::None;
            }
            // Unknown key — cancel silently.
            self.pending = Pending::None;
            self.count = 0;
            return EditorAction::None;
        }

        // ── no pending state: raw keys ────────────────────────────────

        // Count prefix. `0` is *only* a count digit if we've already
        // started building a count; otherwise it's the head-of-line
        // motion.
        if let Char(c) = key.code {
            if let Some(d) = c.to_digit(10) {
                if !(d == 0 && self.count == 0) && key.modifiers == KeyModifiers::NONE {
                    self.count = self.count.saturating_mul(10).saturating_add(d);
                    // Cap at a million-ish so a stuck key can't wedge us.
                    if self.count > 1_000_000 { self.count = 1_000_000; }
                    return EditorAction::None;
                }
            }
        }

        // Basic motions + simple ops. Count consumption happens inline.
        match (key.modifiers, key.code) {
            // arrows + hjkl
            (_, Char('h')) | (_, Left)  => { let n = self.take_count(); self.repeat(n, Motion::CharBack); }
            (_, Char('j')) | (_, Down)  => { let n = self.take_count(); self.repeat(n, Motion::LineDown); }
            (_, Char('k')) | (_, Up)    => { let n = self.take_count(); self.repeat(n, Motion::LineUp); }
            (_, Char('l')) | (_, Right) => { let n = self.take_count(); self.repeat(n, Motion::CharForward); }

            // word motions
            (KeyModifiers::NONE, Char('w')) => { let n = self.take_count(); self.repeat(n, Motion::WordForward); }
            (KeyModifiers::NONE, Char('b')) => { let n = self.take_count(); self.repeat(n, Motion::WordBack); }
            (KeyModifiers::NONE, Char('e')) => { let n = self.take_count(); self.repeat(n, Motion::WordEnd); }

            // line motions
            (KeyModifiers::NONE, Char('0')) => { self.count = 0; self.textarea.move_cursor(CursorMove::Head); }
            (KeyModifiers::NONE, Char('$')) => { self.count = 0; self.textarea.move_cursor(CursorMove::End); }
            (KeyModifiers::NONE, Char('^')) => { self.count = 0; self.move_to_first_non_blank(); }

            // paragraph motions
            (KeyModifiers::NONE, Char('{')) => { let n = self.take_count(); self.repeat(n, Motion::ParagraphBack); }
            (KeyModifiers::NONE, Char('}')) => { let n = self.take_count(); self.repeat(n, Motion::ParagraphForward); }

            // file-level
            (m, Char('G')) if m.contains(KeyModifiers::SHIFT) => {
                let n = self.count;
                self.count = 0;
                if n > 0 {
                    let row = (n.saturating_sub(1)).min(u16::MAX as u32) as u16;
                    self.textarea.move_cursor(CursorMove::Jump(row, 0));
                } else {
                    self.textarea.move_cursor(CursorMove::Bottom);
                }
            }
            (KeyModifiers::NONE, Char('g')) => { self.pending = Pending::Goto; }
            (KeyModifiers::NONE, Char('z')) => { self.pending = Pending::Zoom; }

            // insert-mode entries
            (KeyModifiers::NONE, Char('i')) => {
                self.count = 0;
                self.begin_insert(InsertEntry::AtCursor);
                return EditorAction::EnterInsert;
            }
            (KeyModifiers::NONE, Char('a')) => {
                self.count = 0;
                self.textarea.move_cursor(CursorMove::Forward);
                self.begin_insert(InsertEntry::AfterCursor);
                return EditorAction::EnterInsert;
            }
            (m, Char('I')) if m.contains(KeyModifiers::SHIFT) => {
                self.count = 0;
                self.move_to_first_non_blank();
                self.begin_insert(InsertEntry::FirstNonBlank);
                return EditorAction::EnterInsert;
            }
            (m, Char('A')) if m.contains(KeyModifiers::SHIFT) => {
                self.count = 0;
                self.textarea.move_cursor(CursorMove::End);
                self.begin_insert(InsertEntry::LineEnd);
                return EditorAction::EnterInsert;
            }
            (KeyModifiers::NONE, Char('o')) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.open_line_below();
                self.begin_insert(InsertEntry::OpenBelow);
                return EditorAction::EnterInsert;
            }
            (m, Char('O')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.open_line_above();
                self.begin_insert(InsertEntry::OpenAbove);
                return EditorAction::EnterInsert;
            }

            // substitutes
            (KeyModifiers::NONE, Char('s')) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.textarea.delete_next_char();
                self.recompute_dirty();
                self.begin_insert(InsertEntry::SubstituteChar);
                return EditorAction::EnterInsert;
            }
            (m, Char('S')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.delete_line_by_end();
                self.recompute_dirty();
                self.begin_insert(InsertEntry::SubstituteLine);
                return EditorAction::EnterInsert;
            }
            (m, Char('C')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.textarea.delete_line_by_end();
                self.recompute_dirty();
                self.last_change = Some(LastChange::ChangeToEol);
                self.begin_insert(InsertEntry::ChangeToEol);
                return EditorAction::EnterInsert;
            }

            // deletes + line ops
            (KeyModifiers::NONE, Char('x')) => {
                if self.read_only { return EditorAction::None; }
                let n = self.take_count().max(1);
                for _ in 0..n { self.textarea.delete_next_char(); }
                self.recompute_dirty();
                self.last_change = Some(LastChange::DeleteChar);
            }
            (m, Char('X')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                let n = self.take_count().max(1);
                for _ in 0..n { self.textarea.delete_char(); }
                self.recompute_dirty();
                self.last_change = Some(LastChange::DeleteBack);
            }
            (m, Char('D')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.textarea.delete_line_by_end();
                self.recompute_dirty();
                self.last_change = Some(LastChange::DeleteToEol);
            }
            (m, Char('J')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                let n = self.take_count().max(1);
                for _ in 0..n { self.join_line(); }
                self.last_change = Some(LastChange::JoinLine(n));
            }

            // operators — remember intent, wait for motion
            (KeyModifiers::NONE, Char('d')) => {
                if self.read_only { return EditorAction::None; }
                self.pending = Pending::Operator(Operator::Delete);
            }
            (KeyModifiers::NONE, Char('c')) => {
                if self.read_only { return EditorAction::None; }
                self.pending = Pending::Operator(Operator::Change);
            }
            (KeyModifiers::NONE, Char('y')) => {
                self.pending = Pending::Operator(Operator::Yank);
            }

            // pastes
            (KeyModifiers::NONE, Char('p')) => {
                if self.read_only { return EditorAction::None; }
                let n = self.take_count().max(1);
                for _ in 0..n { self.paste_after(); }
                self.report_paste_count(n);
                self.last_change = Some(LastChange::Paste(false));
            }
            (m, Char('P')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                let n = self.take_count().max(1);
                for _ in 0..n { self.paste_before(); }
                self.report_paste_count(n);
                self.last_change = Some(LastChange::Paste(true));
            }

            // undo / redo
            (KeyModifiers::NONE, Char('u')) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                if self.textarea.undo() { self.recompute_dirty(); }
            }
            (KeyModifiers::CONTROL, Char('r')) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                if self.textarea.redo() { self.recompute_dirty(); }
            }

            // replace single char
            (KeyModifiers::NONE, Char('r')) => {
                if self.read_only { return EditorAction::None; }
                self.pending = Pending::Replace;
            }

            // repeat last change
            (KeyModifiers::NONE, Char('.')) => {
                if self.read_only { return EditorAction::None; }
                if let Some(change) = self.last_change.clone() {
                    if let Some(action) = self.replay(change) {
                        return action;
                    }
                }
            }

            // indent / dedent current line
            (m, Char('>')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                // `>>` would need two-key buffering. For now a single
                // `>` indents. Good enough for v1.
                self.shift_line(true);
                self.last_change = Some(LastChange::ShiftLine(true));
            }
            (m, Char('<')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { return EditorAction::None; }
                self.count = 0;
                self.shift_line(false);
                self.last_change = Some(LastChange::ShiftLine(false));
            }

            // visual mode entry
            (KeyModifiers::NONE, Char('v')) => {
                self.count = 0;
                self.textarea.start_selection();
                return EditorAction::EnterVisualChar;
            }
            (m, Char('V')) if m.contains(KeyModifiers::SHIFT) => {
                self.count = 0;
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.start_selection();
                self.textarea.move_cursor(CursorMove::End);
                return EditorAction::EnterVisualLine;
            }

            // scroll
            (KeyModifiers::CONTROL, Char('d')) => {
                self.count = 0;
                self.textarea.scroll((10i16, 0i16));
                for _ in 0..10 { self.textarea.move_cursor(CursorMove::Down); }
            }
            (KeyModifiers::CONTROL, Char('u')) => {
                self.count = 0;
                self.textarea.scroll((-10i16, 0i16));
                for _ in 0..10 { self.textarea.move_cursor(CursorMove::Up); }
            }

            _ => {}
        }
        EditorAction::None
    }

    /// Called while App.mode == Visual and focus is Edit. Motions move
    /// the cursor (extending the selection). `d`/`y`/`c`/`Esc` resolve
    /// to an operator or cancel. Returns what to do with the mode.
    pub fn handle_visual(&mut self, key: KeyEvent, linewise: bool) -> VisualAction {
        use KeyCode::*;
        if key.code == Esc && key.modifiers == KeyModifiers::NONE {
            self.textarea.cancel_selection();
            self.count = 0;
            return VisualAction::Exit;
        }

        // Count prefix.
        if let Char(c) = key.code {
            if let Some(d) = c.to_digit(10) {
                if !(d == 0 && self.count == 0) && key.modifiers == KeyModifiers::NONE {
                    self.count = self.count.saturating_mul(10).saturating_add(d);
                    if self.count > 1_000_000 { self.count = 1_000_000; }
                    return VisualAction::Stay;
                }
            }
        }

        match (key.modifiers, key.code) {
            // motions (extend selection)
            (_, Char('h')) | (_, Left)  => { let n = self.take_count(); self.repeat(n, Motion::CharBack); }
            (_, Char('j')) | (_, Down)  => { let n = self.take_count(); self.repeat(n, Motion::LineDown); }
            (_, Char('k')) | (_, Up)    => { let n = self.take_count(); self.repeat(n, Motion::LineUp); }
            (_, Char('l')) | (_, Right) => { let n = self.take_count(); self.repeat(n, Motion::CharForward); }
            (KeyModifiers::NONE, Char('w')) => { let n = self.take_count(); self.repeat(n, Motion::WordForward); }
            (KeyModifiers::NONE, Char('b')) => { let n = self.take_count(); self.repeat(n, Motion::WordBack); }
            (KeyModifiers::NONE, Char('e')) => { let n = self.take_count(); self.repeat(n, Motion::WordEnd); }
            (KeyModifiers::NONE, Char('0')) => { self.count = 0; self.textarea.move_cursor(CursorMove::Head); }
            (KeyModifiers::NONE, Char('$')) => { self.count = 0; self.textarea.move_cursor(CursorMove::End); }
            (KeyModifiers::NONE, Char('^')) => { self.count = 0; self.move_to_first_non_blank(); }
            (KeyModifiers::NONE, Char('{')) => { let n = self.take_count(); self.repeat(n, Motion::ParagraphBack); }
            (KeyModifiers::NONE, Char('}')) => { let n = self.take_count(); self.repeat(n, Motion::ParagraphForward); }
            (m, Char('G')) if m.contains(KeyModifiers::SHIFT) => {
                self.count = 0;
                self.textarea.move_cursor(CursorMove::Bottom);
            }

            // operators: commit selection → exit visual
            (KeyModifiers::NONE, Char('d')) | (KeyModifiers::NONE, Char('x')) => {
                if self.read_only { self.textarea.cancel_selection(); return VisualAction::Exit; }
                self.apply_visual_op(Operator::Delete, linewise);
                return VisualAction::Exit;
            }
            (KeyModifiers::NONE, Char('c')) => {
                if self.read_only { self.textarea.cancel_selection(); return VisualAction::Exit; }
                self.apply_visual_op(Operator::Change, linewise);
                // `c` from visual drops into insert.
                self.begin_insert(InsertEntry::ChangeOp(ChangeReplay::Motion(Motion::CharForward, 1)));
                return VisualAction::ExitEnterInsert;
            }
            (KeyModifiers::NONE, Char('y')) => {
                self.apply_visual_op(Operator::Yank, linewise);
                return VisualAction::Exit;
            }

            // indent / dedent selection — treat linewise across the
            // selected range.
            (m, Char('>')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { self.textarea.cancel_selection(); return VisualAction::Exit; }
                self.shift_selection(true);
                return VisualAction::Exit;
            }
            (m, Char('<')) if m.contains(KeyModifiers::SHIFT) => {
                if self.read_only { self.textarea.cancel_selection(); return VisualAction::Exit; }
                self.shift_selection(false);
                return VisualAction::Exit;
            }

            _ => {}
        }
        VisualAction::Stay
    }

    // ── helpers: motion primitives ───────────────────────────────────

    fn key_as_motion(&self, key: &KeyEvent) -> Option<Motion> {
        use KeyCode::*;
        let m = match (key.modifiers, key.code) {
            (_, Char('h')) | (_, Left)  => Motion::CharBack,
            (_, Char('j')) | (_, Down)  => Motion::LineDown,
            (_, Char('k')) | (_, Up)    => Motion::LineUp,
            (_, Char('l')) | (_, Right) => Motion::CharForward,
            (KeyModifiers::NONE, Char('w')) => Motion::WordForward,
            (KeyModifiers::NONE, Char('b')) => Motion::WordBack,
            (KeyModifiers::NONE, Char('e')) => Motion::WordEnd,
            (KeyModifiers::NONE, Char('0')) => Motion::LineHead,
            (KeyModifiers::NONE, Char('$')) => Motion::LineEnd,
            (KeyModifiers::NONE, Char('^')) => Motion::FirstNonBlank,
            (KeyModifiers::NONE, Char('{')) => Motion::ParagraphBack,
            (KeyModifiers::NONE, Char('}')) => Motion::ParagraphForward,
            (md, Char('G')) if md.contains(KeyModifiers::SHIFT) => Motion::FileBottom,
            _ => return None,
        };
        Some(m)
    }

    fn do_motion(&mut self, m: Motion, count: u32) {
        let n = count.max(1);
        for _ in 0..n {
            match m {
                Motion::CharForward  => self.textarea.move_cursor(CursorMove::Forward),
                Motion::CharBack     => self.textarea.move_cursor(CursorMove::Back),
                Motion::LineDown     => self.textarea.move_cursor(CursorMove::Down),
                Motion::LineUp       => self.textarea.move_cursor(CursorMove::Up),
                Motion::WordForward  => self.textarea.move_cursor(CursorMove::WordForward),
                Motion::WordBack     => self.textarea.move_cursor(CursorMove::WordBack),
                Motion::WordEnd      => self.textarea.move_cursor(CursorMove::WordEnd),
                Motion::LineHead     => self.textarea.move_cursor(CursorMove::Head),
                Motion::LineEnd      => self.textarea.move_cursor(CursorMove::End),
                Motion::FirstNonBlank=> self.move_to_first_non_blank(),
                Motion::FileTop      => self.textarea.move_cursor(CursorMove::Top),
                Motion::FileBottom   => self.textarea.move_cursor(CursorMove::Bottom),
                Motion::ParagraphForward => self.textarea.move_cursor(CursorMove::ParagraphForward),
                Motion::ParagraphBack    => self.textarea.move_cursor(CursorMove::ParagraphBack),
            }
        }
    }

    fn repeat(&mut self, count: u32, m: Motion) {
        self.do_motion(m, count.max(1));
    }

    fn take_count(&mut self) -> u32 {
        let c = self.count;
        self.count = 0;
        c
    }

    fn move_to_first_non_blank(&mut self) {
        let (row, _) = self.textarea.cursor();
        let Some(line) = self.textarea.lines().get(row) else { return; };
        let col = line.chars().position(|c| !c.is_whitespace()).unwrap_or(0);
        self.textarea.move_cursor(CursorMove::Jump(row as u16, col as u16));
    }

    // ── helpers: operators on regions ────────────────────────────────

    fn apply_operator_motion(&mut self, op: Operator, motion: Motion, count: u32) {
        self.textarea.start_selection();
        self.do_motion(motion, count);
        self.commit_operator(op, false);
    }

    fn apply_operator_line(&mut self, op: Operator, count: u32) {
        let n = count.max(1);
        let (row, _) = self.textarea.cursor();
        let total = self.textarea.lines().len();
        // Aim for N whole lines starting at the current one.
        self.textarea.move_cursor(CursorMove::Head);
        self.textarea.start_selection();
        let last = (row + n as usize).min(total);
        if last < total {
            // Jump to row `last`, col 0 — selects all lines before it.
            self.textarea.move_cursor(CursorMove::Jump(last as u16, 0));
        } else {
            // Selecting through the last line — extend to its end so the
            // operator consumes the whole buffer tail.
            self.textarea.move_cursor(CursorMove::Bottom);
            self.textarea.move_cursor(CursorMove::End);
        }
        self.commit_operator(op, true);
        if op == Operator::Change {
            // `cc` should leave the user on an empty line at the original
            // row, ready to type. The cut above removed content; add an
            // empty line back if we're now on a non-existent row.
            if self.textarea.lines().is_empty() {
                // TextArea::default() already guarantees one line, but
                // be defensive.
            }
        }
    }

    fn commit_operator(&mut self, op: Operator, linewise: bool) {
        match op {
            Operator::Delete => {
                self.capture_yank(linewise);
                self.textarea.cut();
                self.recompute_dirty();
            }
            Operator::Change => {
                self.capture_yank(linewise);
                self.textarea.cut();
                self.recompute_dirty();
            }
            Operator::Yank => {
                self.capture_yank(linewise);
                self.textarea.copy();
                self.textarea.cancel_selection();
                self.report_yank();
            }
        }
    }

    fn capture_yank(&mut self, linewise: bool) {
        // Compute the selected text ourselves (tui_textarea only updates
        // its yank buffer after cut/copy — we want both our buffer and
        // its buffer to end up with the same content so the built-in
        // paste() for charwise pastes still work).
        if let Some(((sr, sc), (er, ec))) = self.textarea.selection_range() {
            let text = extract_range(self.textarea.lines(), sr, sc, er, ec);
            self.yank_text = text.clone();
            self.yank_linewise = linewise;
            self.textarea.set_yank_text(text);
        }
    }

    /// Drain the last editor-level status message (e.g. "yanked 3 lines")
    /// so the outer loop can forward it to `app.status`.
    pub fn take_status(&mut self) -> Option<String> {
        self.last_status.take()
    }

    /// `gj` / `gk` — move the cursor one *visual* row down/up under
    /// soft-wrap, preserving the screen column across steps (sticky
    /// column). Falls back to logical LineDown / LineUp when wrap is
    /// off or the layout hasn't been rendered yet.
    pub fn move_visual(&mut self, down: bool) {
        let width = self.last_content_w.get() as usize;
        if !self.wrap || width == 0 {
            self.textarea.move_cursor(if down { CursorMove::Down } else { CursorMove::Up });
            self.gj_sticky.set(None);
            return;
        }
        let lines = self.textarea.lines();
        let (cur_row, cur_col) = self.textarea.cursor();
        let rows = crate::wrap::build_wrap_rows(lines, width);
        if rows.is_empty() { return; }

        // Current visual row + screen column.
        let mut vrow_idx = 0usize;
        for (i, r) in rows.iter().enumerate() {
            if r.logical_row != cur_row { continue; }
            vrow_idx = i;
            if cur_col >= r.start_char && cur_col < r.end_char.max(r.start_char + 1) {
                break;
            }
        }
        let cur_row_ref = &rows[vrow_idx];
        let cur_line = lines.get(cur_row).map(String::as_str).unwrap_or("");
        let cur_screen = cur_row_ref.prefix_cols as usize
            + crate::wrap::slice_display_width(
                cur_line, cur_row_ref.start_char, cur_col.min(cur_row_ref.end_char),
            );

        // Sticky is valid only if the cursor hasn't moved since we last
        // set it — anything else (h/l, w, dw, typing, mouse, etc.) will
        // have shifted (row, col) and invalidated our remembered column.
        let target_col = match self.gj_sticky.get() {
            Some(s) if s.last_row == cur_row && s.last_col == cur_col => s.screen_col,
            _ => cur_screen as u16,
        };

        let next_idx = if down {
            (vrow_idx + 1).min(rows.len() - 1)
        } else {
            vrow_idx.saturating_sub(1)
        };
        if next_idx == vrow_idx {
            // No movement possible; remember intent so a future hop in
            // the opposite direction still preserves column.
            self.gj_sticky.set(Some(GjSticky {
                screen_col: target_col, last_row: cur_row, last_col: cur_col,
            }));
            return;
        }
        let next = &rows[next_idx];
        let next_line = lines.get(next.logical_row).map(String::as_str).unwrap_or("");
        let new_char = crate::wrap::char_idx_for_screen_col(next_line, next, target_col);
        let new_char = new_char.min(next.end_char);
        self.textarea.move_cursor(CursorMove::Jump(
            next.logical_row as u16,
            new_char as u16,
        ));
        self.gj_sticky.set(Some(GjSticky {
            screen_col: target_col,
            last_row:   next.logical_row,
            last_col:   new_char,
        }));
    }

    /// `g0` — move cursor to the first column of the current visual
    /// row (under breakindent, that means the indent prefix, not the
    /// real line head).
    pub fn move_visual_home(&mut self) {
        let width = self.last_content_w.get() as usize;
        if !self.wrap || width == 0 {
            self.textarea.move_cursor(CursorMove::Head);
            return;
        }
        let (cur_row, cur_col) = self.textarea.cursor();
        let rows = crate::wrap::build_wrap_rows(self.textarea.lines(), width);
        if let Some(r) = rows.iter().find(|r| {
            r.logical_row == cur_row && cur_col >= r.start_char
                && cur_col <= r.end_char.max(r.start_char + 1)
        }) {
            self.textarea.move_cursor(CursorMove::Jump(
                r.logical_row as u16, r.start_char as u16,
            ));
        }
    }

    /// Vertical view scroll for mouse-wheel ticks. Positive `delta`
    /// moves toward the end of the buffer, negative toward the start.
    /// In nowrap mode this defers to tui-textarea's `scroll`; in wrap
    /// mode it nudges `scroll_top` directly (the UI layer clamps to
    /// the document extent and keeps the cursor in view on the next
    /// draw).
    pub fn scroll_lines(&mut self, delta: i16) {
        if !self.wrap {
            self.textarea.scroll((delta, 0));
            return;
        }
        let top = self.scroll_top.get() as isize;
        let new_top = (top + delta as isize).max(0) as usize;
        self.scroll_top.set(new_top);
    }

    /// Horizontal view scroll (vim `zl`/`zh`). Only meaningful in
    /// `nowrap` mode — under soft-wrap there's nothing to scroll
    /// horizontally, so the call is a silent no-op. `delta` is in
    /// screen columns: positive scrolls right, negative left.
    pub fn scroll_horizontal(&mut self, delta: i16) {
        if self.wrap { return; }
        self.textarea.scroll((0, delta));
    }

    /// `zt` / `zz` / `zb` — reposition the cursor's visual row to the
    /// top / middle / bottom of the viewport. Works under both wrap
    /// modes: in wrap mode we adjust `scroll_top` (visual rows); in
    /// nowrap mode we defer to tui-textarea's own scroll.
    pub fn scroll_cursor_to(&mut self, target: ScrollTo) {
        let h = self.last_viewport_h.get() as usize;
        if h == 0 { return; }
        if !self.wrap {
            // Textarea's scroll API is relative, and we don't have its
            // current scroll-top exposed. Simplest reasonable effect:
            // center or park at an extreme by calling scroll() with a
            // big step in the right direction — the textarea clamps
            // automatically. For `zz` we nudge half a page.
            let step = match target {
                ScrollTo::Top    => -(h as i16),
                ScrollTo::Middle => -((h / 2) as i16),
                ScrollTo::Bottom => h as i16,
            };
            self.textarea.scroll((step, 0));
            return;
        }
        let width = self.last_content_w.get() as usize;
        if width == 0 { return; }
        let lines = self.textarea.lines();
        let (cur_row, cur_col) = self.textarea.cursor();
        let rows = crate::wrap::build_wrap_rows(lines, width);
        if rows.is_empty() { return; }
        let mut cur_vrow = 0usize;
        for (i, r) in rows.iter().enumerate() {
            if r.logical_row == cur_row
                && cur_col >= r.start_char
                && cur_col < r.end_char.max(r.start_char + 1)
            {
                cur_vrow = i;
                break;
            }
            if r.logical_row == cur_row { cur_vrow = i; }
        }
        let new_top = match target {
            ScrollTo::Top    => cur_vrow,
            ScrollTo::Middle => cur_vrow.saturating_sub(h / 2),
            ScrollTo::Bottom => cur_vrow + 1 - h.min(cur_vrow + 1),
        };
        self.scroll_top.set(new_top);
    }

    /// `g$` — move cursor to the last char of the current visual row.
    pub fn move_visual_end(&mut self) {
        let width = self.last_content_w.get() as usize;
        if !self.wrap || width == 0 {
            self.textarea.move_cursor(CursorMove::End);
            return;
        }
        let (cur_row, cur_col) = self.textarea.cursor();
        let rows = crate::wrap::build_wrap_rows(self.textarea.lines(), width);
        if let Some(r) = rows.iter().find(|r| {
            r.logical_row == cur_row && cur_col >= r.start_char
                && cur_col < r.end_char.max(r.start_char + 1)
        }) {
            let end = r.end_char.saturating_sub(1).max(r.start_char);
            self.textarea.move_cursor(CursorMove::Jump(
                r.logical_row as u16, end as u16,
            ));
        }
    }


    fn report_yank(&mut self) {
        let msg = format_copy_msg("yanked", &self.yank_text, self.yank_linewise);
        self.last_status = Some(msg);
    }

    fn report_paste(&mut self) {
        let msg = format_copy_msg("pasted", &self.yank_text, self.yank_linewise);
        self.last_status = Some(msg);
    }

    fn report_paste_count(&mut self, count: u32) {
        let base = format_copy_msg("pasted", &self.yank_text, self.yank_linewise);
        self.last_status = Some(if count > 1 {
            format!("{base} ×{count}")
        } else {
            base
        });
    }

    fn apply_visual_op(&mut self, op: Operator, linewise: bool) {
        if linewise {
            // Expand the current selection to full-line boundaries.
            if let Some(((sr, _), (er, _))) = self.textarea.selection_range() {
                self.textarea.cancel_selection();
                self.textarea.move_cursor(CursorMove::Jump(sr as u16, 0));
                self.textarea.start_selection();
                let total = self.textarea.lines().len();
                if er + 1 < total {
                    self.textarea.move_cursor(CursorMove::Jump((er + 1) as u16, 0));
                } else {
                    self.textarea.move_cursor(CursorMove::Jump(er as u16, 0));
                    self.textarea.move_cursor(CursorMove::End);
                }
            }
        }
        self.commit_operator(op, linewise);
    }

    // ── helpers: text objects ────────────────────────────────────────

    fn apply_text_object(&mut self, op: Operator, scope: TextObjScope, kind: TextObjKind) {
        let range = match kind {
            TextObjKind::Word => self.text_obj_word(scope),
            TextObjKind::Paren        => self.text_obj_pair(scope, '(', ')'),
            TextObjKind::Bracket      => self.text_obj_pair(scope, '[', ']'),
            TextObjKind::Brace        => self.text_obj_pair(scope, '{', '}'),
            TextObjKind::DoubleQuote  => self.text_obj_quote(scope, '"'),
            TextObjKind::SingleQuote  => self.text_obj_quote(scope, '\''),
        };
        let Some(((sr, sc), (er, ec))) = range else { return; };
        self.textarea.move_cursor(CursorMove::Jump(sr as u16, sc as u16));
        self.textarea.start_selection();
        self.textarea.move_cursor(CursorMove::Jump(er as u16, ec as u16));
        self.commit_operator(op, false);
    }

    fn text_obj_word(&self, scope: TextObjScope) -> Option<((usize,usize),(usize,usize))> {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines().get(row)?;
        if line.is_empty() { return None; }
        let bytes: Vec<char> = line.chars().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        // Find word start/end around `col`.
        let mut start = col.min(bytes.len().saturating_sub(1));
        while start > 0 && is_word(bytes[start - 1]) { start -= 1; }
        let mut end = col;
        while end < bytes.len() && is_word(bytes[end]) { end += 1; }
        if start == end { return None; }
        match scope {
            TextObjScope::Inner => Some(((row, start), (row, end))),
            TextObjScope::Around => {
                // Extend over trailing (or, if none, leading) whitespace.
                let mut after = end;
                while after < bytes.len() && bytes[after].is_whitespace() { after += 1; }
                if after > end {
                    Some(((row, start), (row, after)))
                } else {
                    let mut before = start;
                    while before > 0 && bytes[before - 1].is_whitespace() { before -= 1; }
                    Some(((row, before), (row, end)))
                }
            }
        }
    }

    fn text_obj_pair(&self, scope: TextObjScope, open: char, close: char) -> Option<((usize,usize),(usize,usize))> {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines().get(row)?;
        let chars: Vec<char> = line.chars().collect();
        // Find the opening delimiter to the left (inclusive of col).
        let mut depth = 0i32;
        let mut open_idx = None;
        let mut i = col.min(chars.len().saturating_sub(1));
        loop {
            let c = *chars.get(i)?;
            if c == close { depth += 1; }
            else if c == open {
                if depth == 0 { open_idx = Some(i); break; }
                depth -= 1;
            }
            if i == 0 { break; }
            i -= 1;
        }
        let open_idx = open_idx?;
        // Find the matching closer to the right.
        let mut depth = 0i32;
        let mut close_idx = None;
        let mut j = open_idx + 1;
        while j < chars.len() {
            let c = chars[j];
            if c == open { depth += 1; }
            else if c == close {
                if depth == 0 { close_idx = Some(j); break; }
                depth -= 1;
            }
            j += 1;
        }
        let close_idx = close_idx?;
        match scope {
            TextObjScope::Inner  => Some(((row, open_idx + 1), (row, close_idx))),
            TextObjScope::Around => Some(((row, open_idx),     (row, close_idx + 1))),
        }
    }

    fn text_obj_quote(&self, scope: TextObjScope, q: char) -> Option<((usize,usize),(usize,usize))> {
        let (row, col) = self.textarea.cursor();
        let line = self.textarea.lines().get(row)?;
        let chars: Vec<char> = line.chars().collect();
        // Find quote on the left, then on the right. No nesting.
        let mut left = None;
        let start = col.min(chars.len().saturating_sub(1));
        let mut i = start;
        loop {
            if chars.get(i) == Some(&q) { left = Some(i); break; }
            if i == 0 { break; }
            i -= 1;
        }
        // If the cursor sits on a quote, treat that one as the opener
        // and look for the next quote after it.
        let left = left?;
        let mut right = None;
        let mut j = left + 1;
        while j < chars.len() {
            if chars[j] == q { right = Some(j); break; }
            j += 1;
        }
        let right = right?;
        match scope {
            TextObjScope::Inner  => Some(((row, left + 1), (row, right))),
            TextObjScope::Around => Some(((row, left),     (row, right + 1))),
        }
    }

    // ── helpers: line-level edits ────────────────────────────────────

    fn open_line_below(&mut self) {
        self.textarea.move_cursor(CursorMove::End);
        if self.autoindent {
            self.smart_newline();
        } else {
            self.textarea.insert_newline();
        }
        self.recompute_dirty();
    }

    fn open_line_above(&mut self) {
        // Take the indent from the current line *before* mutating, so
        // moving up to a still-empty line above doesn't lose it.
        let (row, _) = self.textarea.cursor();
        let indent: String = if self.autoindent {
            self.textarea.lines()[row]
                .chars().take_while(|c| *c == ' ' || *c == '\t').collect()
        } else {
            String::new()
        };
        self.textarea.move_cursor(CursorMove::Head);
        self.textarea.insert_newline();
        self.textarea.move_cursor(CursorMove::Up);
        if !indent.is_empty() {
            self.textarea.insert_str(&indent);
        }
        self.recompute_dirty();
    }

    fn join_line(&mut self) {
        // Vim's J: append next line to current, separated by one space
        // (no leading whitespace on the joined line). If already on the
        // last line, no-op.
        let (row, _) = self.textarea.cursor();
        if row + 1 >= self.textarea.lines().len() { return; }
        self.textarea.move_cursor(CursorMove::End);
        let next_line = self.textarea.lines()[row + 1].clone();
        let trimmed = next_line.trim_start();
        // Delete the newline: end-of-line + delete_next_char collapses
        // it. Then insert " " unless the joined text starts the line.
        self.textarea.delete_next_char();
        // Remove the leading whitespace we just pulled up.
        let pulled = next_line.len() - trimmed.len();
        for _ in 0..pulled { self.textarea.delete_next_char(); }
        // Only add the separator if the original current line was non-
        // empty (vim: `J` on an empty line just pulls up without space).
        let cur = &self.textarea.lines()[row];
        if !cur.is_empty() && !cur.ends_with(' ') && !trimmed.is_empty() {
            self.textarea.insert_char(' ');
            self.textarea.move_cursor(CursorMove::Back);
        }
        self.recompute_dirty();
    }

    fn replace_char(&mut self, c: char) {
        if self.read_only { return; }
        if self.textarea.delete_next_char() {
            self.textarea.insert_char(c);
            self.textarea.move_cursor(CursorMove::Back);
            self.recompute_dirty();
        }
    }

    fn shift_line(&mut self, indent: bool) {
        let (row, _) = self.textarea.cursor();
        self.shift_rows(row, row, indent);
    }

    fn shift_selection(&mut self, indent: bool) {
        let Some(((sr, _), (er, _))) = self.textarea.selection_range() else {
            self.textarea.cancel_selection();
            return;
        };
        self.textarea.cancel_selection();
        self.shift_rows(sr, er, indent);
    }

    fn shift_rows(&mut self, sr: usize, er: usize, indent: bool) {
        const PAD: &str = "    ";
        for row in sr..=er {
            if row >= self.textarea.lines().len() { break; }
            self.textarea.move_cursor(CursorMove::Jump(row as u16, 0));
            if indent {
                self.textarea.insert_str(PAD);
            } else {
                // Remove up to 4 leading spaces (or a tab).
                let line = self.textarea.lines()[row].clone();
                let to_delete = if line.starts_with('\t') { 1 }
                                else { line.chars().take(4).take_while(|c| *c == ' ').count() };
                for _ in 0..to_delete {
                    self.textarea.delete_next_char();
                }
            }
        }
        self.recompute_dirty();
    }

    // ── helpers: paste ───────────────────────────────────────────────

    fn paste_after(&mut self) {
        if self.yank_linewise {
            // Move to end of current line, insert newline, then the text
            // (without trailing newline — we add it ourselves to keep
            // linewise semantics consistent).
            let text = self.yank_text.clone();
            let trimmed = text.trim_end_matches('\n');
            self.textarea.move_cursor(CursorMove::End);
            self.textarea.insert_newline();
            self.textarea.insert_str(trimmed);
            self.recompute_dirty();
        } else {
            // Charwise: step forward one char (vim `p` pastes AFTER the
            // cursor) unless at end of line, then paste.
            self.textarea.move_cursor(CursorMove::Forward);
            self.textarea.paste();
            self.textarea.move_cursor(CursorMove::Back);
            self.recompute_dirty();
        }
    }

    fn paste_before(&mut self) {
        if self.yank_linewise {
            let text = self.yank_text.clone();
            let trimmed = text.trim_end_matches('\n');
            self.textarea.move_cursor(CursorMove::Head);
            self.textarea.insert_str(trimmed);
            self.textarea.insert_newline();
            self.textarea.move_cursor(CursorMove::Up);
            self.textarea.move_cursor(CursorMove::Head);
            self.recompute_dirty();
        } else {
            self.textarea.paste();
            self.textarea.move_cursor(CursorMove::Back);
            self.recompute_dirty();
        }
    }

    // ── helpers: replay ──────────────────────────────────────────────

    fn replay(&mut self, c: LastChange) -> Option<EditorAction> {
        match c {
            LastChange::DeleteChar => {
                self.textarea.delete_next_char();
                self.recompute_dirty();
            }
            LastChange::DeleteBack => {
                self.textarea.delete_char();
                self.recompute_dirty();
            }
            LastChange::OpMotion(op, m, n) => self.apply_operator_motion(op, m, n),
            LastChange::OpTextObj(op, scope, kind) => self.apply_text_object(op, scope, kind),
            LastChange::OpLine(op, n) => self.apply_operator_line(op, n),
            LastChange::DeleteToEol => {
                self.textarea.delete_line_by_end();
                self.recompute_dirty();
            }
            LastChange::ChangeToEol => {
                self.textarea.delete_line_by_end();
                self.recompute_dirty();
            }
            LastChange::JoinLine(n) => { for _ in 0..n { self.join_line(); } }
            LastChange::Paste(before) => {
                if before { self.paste_before(); } else { self.paste_after(); }
                self.report_paste();
            }
            LastChange::Replace(ch) => self.replace_char(ch),
            LastChange::ShiftLine(indent) => self.shift_line(indent),
            LastChange::InsertSeq { entry, text } => {
                // Re-enter the entry point, type the captured text, and
                // let the caller know the editor needs Insert mode so
                // follow-up keystrokes route correctly. For `.` we want
                // to *finish* the insert here (not leave the app in
                // Insert), so we simulate typing then call end_insert-
                // like cleanup inline.
                self.run_insert_entry(entry);
                for ch in text.chars() {
                    if ch == '\n' {
                        if self.autoindent {
                            self.smart_newline();
                        } else {
                            self.textarea.insert_newline();
                        }
                    } else if self.autopair && is_autopair_char(ch) {
                        self.type_char_autopair(ch);
                    } else {
                        self.textarea.insert_char(ch);
                    }
                }
                self.recompute_dirty();
                // Don't re-arm `last_change` — we just re-ran the same one.
            }
        }
        None
    }

    fn run_insert_entry(&mut self, entry: InsertEntry) {
        match entry {
            InsertEntry::AtCursor => {}
            InsertEntry::AfterCursor => self.textarea.move_cursor(CursorMove::Forward),
            InsertEntry::FirstNonBlank => self.move_to_first_non_blank(),
            InsertEntry::LineEnd => self.textarea.move_cursor(CursorMove::End),
            InsertEntry::OpenBelow => self.open_line_below(),
            InsertEntry::OpenAbove => self.open_line_above(),
            InsertEntry::SubstituteChar => { self.textarea.delete_next_char(); }
            InsertEntry::SubstituteLine => {
                self.textarea.move_cursor(CursorMove::Head);
                self.textarea.delete_line_by_end();
            }
            InsertEntry::ChangeToEol => { self.textarea.delete_line_by_end(); }
            InsertEntry::ChangeOp(replay) => {
                match replay {
                    ChangeReplay::Motion(m, n) => self.apply_operator_motion(Operator::Change, m, n),
                    ChangeReplay::Line(n)      => self.apply_operator_line(Operator::Change, n),
                    ChangeReplay::TextObj(scope, kind) => {
                        self.apply_text_object(Operator::Change, scope, kind);
                    }
                }
            }
        }
    }

    fn begin_insert(&mut self, entry: InsertEntry) {
        self.insert_entry = Some(entry);
        self.insert_buffer.clear();
    }

    // ── cross-cutting ────────────────────────────────────────────────

    fn recompute_dirty(&mut self) {
        self.dirty = self.textarea.lines() != self.saved_lines.as_slice();
        self.syntax_stale = true;
    }

    // ── ex-command helpers (called from app::run_command) ────────────

    /// Jump to 1-based line `n`, clamped into range.
    pub fn goto_line(&mut self, n: usize) {
        let last = self.textarea.lines().len().saturating_sub(1);
        let row = n.saturating_sub(1).min(last);
        self.textarea.move_cursor(CursorMove::Jump(row as u16, 0));
        self.move_to_first_non_blank();
    }

    /// `:%s/old/new/g` — whole-file literal substitution. Returns the
    /// number of substitutions. Treats `old`/`new` as plain strings
    /// (no regex) so the user doesn't have to escape metachars.
    pub fn substitute_all(&mut self, old: &str, new: &str) -> usize {
        if self.read_only { return 0; }
        if old.is_empty() { return 0; }
        let mut count = 0;
        let new_lines: Vec<String> = self.textarea.lines().iter().map(|line| {
            let (replaced, n) = replace_count(line, old, new);
            count += n;
            replaced
        }).collect();
        if count > 0 {
            self.textarea = TextArea::new(new_lines);
            style_textarea(&mut self.textarea);
            self.recompute_dirty();
        }
        count
    }

    /// Forward search using the textarea's built-in regex engine.
    /// Returns `true` if the pattern compiled and a match exists.
    pub fn set_search_and_find(&mut self, pattern: &str) -> bool {
        if self.textarea.set_search_pattern(pattern).is_err() {
            return false;
        }
        self.textarea.search_forward(true)
    }

    pub fn search_next(&mut self, backward: bool) -> bool {
        if backward { self.textarea.search_back(false) } else { self.textarea.search_forward(false) }
    }

    // ── existing accessors ───────────────────────────────────────────

    pub fn file_name(&self) -> &str {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    pub fn saved_size(&self) -> Option<u64> { self.saved_size }
    pub fn saved_lines(&self) -> &[String] { &self.saved_lines }

    pub fn retarget_path(&mut self, new_path: PathBuf) {
        self.path = Some(new_path);
        self.external_conflict = None;
        self.capture_disk_stats();
    }
}

/// Return value for [`Editor::handle_visual`] — tells the app whether
/// to stay in visual, return to normal, or jump into insert (after `c`).
pub enum VisualAction {
    Stay,
    Exit,
    ExitEnterInsert,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Pull `[A-Za-z_][A-Za-z0-9_]*`-style identifier tokens out of a line.
/// Produces borrowed slices so the caller can copy only the unique ones
/// into the popup index.
fn extract_words(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Walk forward to the next word start. We use byte indices
        // (cheap) but only break on ASCII word chars; any non-ASCII
        // byte is part of a multi-byte char, which is_word_char handles
        // correctly via the char-iter fallback below.
        let start_byte = match line[i..].char_indices().find(|(_, c)| is_word_char(*c)) {
            Some((rel, _)) => i + rel,
            None => break,
        };
        let mut end_byte = start_byte;
        for (rel, c) in line[start_byte..].char_indices() {
            if is_word_char(c) {
                end_byte = start_byte + rel + c.len_utf8();
            } else {
                break;
            }
        }
        // Skip pure-numeric tokens — `42` is rarely useful as a
        // completion target and would clutter the popup in any file
        // with line numbers, version strings, etc.
        let tok = &line[start_byte..end_byte];
        if !tok.chars().next().unwrap_or(' ').is_ascii_digit() {
            out.push(tok);
        }
        i = end_byte;
    }
    out
}

fn is_autopair_char(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`')
}

pub fn style_textarea(t: &mut TextArea) {
    t.set_line_number_style(Style::default().fg(Color::DarkGray));
    t.set_cursor_line_style(Style::default());
}

/// "yanked 3 lines", "yanked 42 chars". Linewise uses line count, charwise
/// falls back to chars — with a singular/plural flip.
fn format_copy_msg(verb: &str, text: &str, linewise: bool) -> String {
    if linewise {
        let trimmed = text.trim_end_matches('\n');
        let lines = if trimmed.is_empty() { 0 } else { trimmed.matches('\n').count() + 1 };
        let noun = if lines == 1 { "line" } else { "lines" };
        format!("{verb} {lines} {noun}")
    } else {
        let chars = text.chars().count();
        let noun = if chars == 1 { "char" } else { "chars" };
        format!("{verb} {chars} {noun}")
    }
}

fn hash_lines(lines: &[String]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        line.hash(&mut h);
        b'\n'.hash(&mut h);
    }
    h.finish()
}

/// Pull the text between two positions out of the lines — mirrors
/// what tui_textarea's internal yank does, but gives us the raw string
/// we also stash in our own yank buffer for linewise paste semantics.
fn extract_range(lines: &[String], sr: usize, sc: usize, er: usize, ec: usize) -> String {
    if sr == er {
        return lines.get(sr).map(|l| {
            let chars: Vec<char> = l.chars().collect();
            let s = sc.min(chars.len());
            let e = ec.min(chars.len());
            chars[s..e].iter().collect()
        }).unwrap_or_default();
    }
    let mut out = String::new();
    if let Some(first) = lines.get(sr) {
        let chars: Vec<char> = first.chars().collect();
        let s = sc.min(chars.len());
        out.extend(&chars[s..]);
        out.push('\n');
    }
    for row in (sr + 1)..er {
        if let Some(l) = lines.get(row) { out.push_str(l); out.push('\n'); }
    }
    if let Some(last) = lines.get(er) {
        let chars: Vec<char> = last.chars().collect();
        let e = ec.min(chars.len());
        let s: String = chars[..e].iter().collect();
        out.push_str(&s);
    }
    out
}

/// Literal (non-regex) string replace with a count. Avoids pulling in
/// a regex crate for `:%s` when most users want plain text.
fn replace_count(haystack: &str, needle: &str, replacement: &str) -> (String, usize) {
    let mut out = String::with_capacity(haystack.len());
    let mut count = 0;
    let mut i = 0;
    let bytes = haystack.as_bytes();
    let nb = needle.as_bytes();
    while i < bytes.len() {
        if i + nb.len() <= bytes.len() && &bytes[i..i + nb.len()] == nb {
            out.push_str(replacement);
            count += 1;
            i += nb.len();
        } else {
            // Push one char worth (handle multi-byte UTF-8 safely).
            let ch_start = i;
            let mut ch_end = i + 1;
            while ch_end < bytes.len() && (bytes[ch_end] & 0b1100_0000) == 0b1000_0000 {
                ch_end += 1;
            }
            out.push_str(&haystack[ch_start..ch_end]);
            i = ch_end;
        }
    }
    (out, count)
}

/// What `reconcile()` did, so the caller can surface an appropriate
/// status-bar message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    NoOp,
    AutoReloaded,
    ConflictMarked,
    Deleted,
}


