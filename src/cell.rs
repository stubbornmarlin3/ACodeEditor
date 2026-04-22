use ratatui::layout::Rect;

use crate::conflict::ConflictView;
use crate::diff::DiffView;
use crate::editor::Editor;
use crate::session::PtySession;

/// Kind of a single session. Claude and Shell are both PTY-backed but
/// render with different titles and conventions (Claude draws its own
/// cursor, Shell relies on the native one). `Diff` is read-only —
/// rendered like an editor but with no insert mode. `Conflict` is the
/// 3-way resolution view for buffer-vs-disk or git-marker files.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionKind {
    Claude,
    Shell,
    Edit,
    Diff,
    Conflict,
}

impl SessionKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" | "c"                     => Some(SessionKind::Claude),
            "shell"  | "sh" | "s"              => Some(SessionKind::Shell),
            "edit"   | "editor" | "e"          => Some(SessionKind::Edit),
            "diff"   | "d"                      => Some(SessionKind::Diff),
            "conflict" | "resolve"              => Some(SessionKind::Conflict),
            _ => None,
        }
    }
}

/// A single session inside a cell. A cell holds one or more of these
/// and shows one at a time (rotated via `<esc>+Tab`).
pub enum Session {
    Claude(PtySession),
    Shell(PtySession),
    Edit(Editor),
    Diff(DiffView),
    Conflict(ConflictView),
}

impl Session {
    pub fn as_pty(&self) -> Option<&PtySession> {
        match self {
            Session::Claude(p) | Session::Shell(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_pty_mut(&mut self) -> Option<&mut PtySession> {
        match self {
            Session::Claude(p) | Session::Shell(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_editor_mut(&mut self) -> Option<&mut Editor> {
        match self {
            Session::Edit(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_diff_mut(&mut self) -> Option<&mut DiffView> {
        match self {
            Session::Diff(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_conflict_mut(&mut self) -> Option<&mut ConflictView> {
        match self {
            Session::Conflict(c) => Some(c),
            _ => None,
        }
    }
}

/// A generic container cell. Invariant: `sessions` is never empty —
/// when the last session is closed, the cell itself is removed.
pub struct Cell {
    pub sessions:  Vec<Session>,
    pub active:    usize,
    /// When `true`, the cell stays alive (sessions keep running, PTYs
    /// keep their output) but is hidden from the layout. Minimized
    /// cells show at the end of the "OPEN CELLS" explorer list, and
    /// are restored via `space+N` / `:restore N` / `Alt+N` (swap).
    pub minimized: bool,
    /// Ephemeral cells are skipped by `.acedata` persistence. Backing
    /// for `:ex <cmd>` one-shot pty cells — they come and go with the
    /// spawned process and shouldn't round-trip across restarts.
    pub ephemeral: bool,
    /// Last output line pushed to the status bar for this ephemeral
    /// cell. Tracked here so the per-tick poll only nudges the status
    /// when the tail actually changes. `None` on non-ephemeral cells.
    pub exec_last_pushed: Option<String>,
}

impl Cell {
    pub fn with_session(s: Session) -> Self {
        Self { sessions: vec![s], active: 0, minimized: false, ephemeral: false, exec_last_pushed: None }
    }

    pub fn active_session(&self) -> &Session {
        &self.sessions[self.active]
    }

    pub fn active_session_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }
}

/// How the main cell area tiles its cells. New algorithms plug in here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    /// Master fills the top ~60%; slaves tile evenly across the bottom 40%.
    /// Default — suits wide terminals where vertical screen real estate is
    /// cheap to hand to a single primary cell.
    MasterBottom,
    /// Master fills the left ~60%; slaves stack vertically on the right.
    MasterRight,
}

impl LayoutMode {
    pub fn rects(self, main: Rect, n: usize) -> Vec<Rect> {
        match self {
            LayoutMode::MasterBottom => master_bottom_rects(main, n),
            LayoutMode::MasterRight  => master_right_rects(main, n),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LayoutMode::MasterBottom => "master-bottom",
            LayoutMode::MasterRight  => "master-right",
        }
    }

    /// Parse a user-facing name (from `:layout` or `.acerc`). Accepts both
    /// the long form and the two-letter shortcut. `master-stack`/`ms` are
    /// kept as aliases for the previous default (`MasterRight`) so old
    /// configs keep working.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "master-bottom" | "mb" => Some(LayoutMode::MasterBottom),
            "master-right"  | "mr" => Some(LayoutMode::MasterRight),
            "master-stack"  | "ms" | "master" => Some(LayoutMode::MasterRight),
            _ => None,
        }
    }
}

fn master_right_rects(main: Rect, n: usize) -> Vec<Rect> {
    if n == 0 || main.width == 0 || main.height == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![main];
    }

    let master_w = (main.width as u32 * 3 / 5).max(1) as u16; // ~60%
    let stack_w  = main.width.saturating_sub(master_w);

    let master = Rect { x: main.x, y: main.y, width: master_w, height: main.height };

    let stack_count = n - 1;
    let mut rects   = Vec::with_capacity(n);
    rects.push(master);

    let row_h = (main.height / stack_count as u16).max(1);
    let mut used = 0u16;
    for i in 0..stack_count {
        let y = main.y + used;
        let h = if i + 1 == stack_count {
            main.height.saturating_sub(used)
        } else {
            row_h
        };
        rects.push(Rect { x: main.x + master_w, y, width: stack_w, height: h });
        used += h;
    }
    rects
}

fn master_bottom_rects(main: Rect, n: usize) -> Vec<Rect> {
    if n == 0 || main.width == 0 || main.height == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![main];
    }

    let master_h = (main.height as u32 * 3 / 5).max(1) as u16; // ~60%
    let stack_h  = main.height.saturating_sub(master_h);

    let master = Rect { x: main.x, y: main.y, width: main.width, height: master_h };

    let stack_count = n - 1;
    let mut rects   = Vec::with_capacity(n);
    rects.push(master);

    let col_w = (main.width / stack_count as u16).max(1);
    let mut used = 0u16;
    for i in 0..stack_count {
        let x = main.x + used;
        let w = if i + 1 == stack_count {
            main.width.saturating_sub(used)
        } else {
            col_w
        };
        rects.push(Rect { x, y: main.y + master_h, width: w, height: stack_h });
        used += w;
    }
    rects
}

