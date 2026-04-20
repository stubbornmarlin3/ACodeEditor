//! Per-project session persistence.
//!
//! Lives at `<project_root>/.acedata`. Stores the open cells and each
//! cell's sessions so that `ace` in a project folder comes back up the
//! way you left it. PTY sessions (shell, claude) can't be "restored" —
//! the child process is gone — so they're spawned fresh in the same
//! slot. Editor sessions come back with their file re-loaded.
//!
//! Format (hand-rolled, matches the style of `projects.toml`):
//!
//!     # .acedata — auto-managed; edit with care.
//!
//!     [[cell]]
//!     active = 0
//!
//!     [[session]]
//!     kind = "edit"
//!     path = "src/main.rs"
//!
//!     [[session]]
//!     kind = "shell"
//!
//!     [[cell]]
//!     active = 0
//!
//!     [[session]]
//!     kind = "claude"
//!
//! `[[session]]` attaches to the most recent `[[cell]]`. Paths are
//! stored relative to the project root so the file round-trips when the
//! project is moved on disk. Unknown keys are ignored; unrecognized
//! `kind` values skip the session with no fuss.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::cell::SessionKind;

#[derive(Debug, Default, Clone)]
pub struct StateSnapshot {
    pub cells: Vec<CellState>,
    /// Last-focused cell index. `None` → explorer was focused. On
    /// restore, an out-of-bounds index falls back to the explorer
    /// (or cell 0 if the explorer is hidden).
    pub focus: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct CellState {
    pub active:   usize,
    pub sessions: Vec<SessionState>,
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub kind: SessionKind,
    /// For `Edit` sessions only: path relative to the project root.
    /// `None` for PTYs and scratch editors.
    pub path: Option<PathBuf>,
}

/// `<root>/.acedata`. Not a fn-path because we want callers to compose
/// it themselves when constructing both load and save paths.
pub fn data_path(project_root: &Path) -> PathBuf {
    project_root.join(".acedata")
}

/// Read `<root>/.acedata` if present. Missing file → `None`; malformed
/// file → best-effort (whatever parsed). Never fails the caller —
/// persistence is a convenience, not a contract.
pub fn load(project_root: &Path) -> Option<StateSnapshot> {
    let content = fs::read_to_string(data_path(project_root)).ok()?;
    Some(parse(&content, project_root))
}

/// Write `<root>/.acedata`. Best-effort: errors are returned for the
/// caller to log but never panic.
pub fn save(project_root: &Path, snap: &StateSnapshot) -> io::Result<()> {
    let path = data_path(project_root);
    fs::write(&path, serialize(snap, project_root))
}

// ── ser/de ──────────────────────────────────────────────────────────────

fn serialize(snap: &StateSnapshot, project_root: &Path) -> String {
    let mut s = String::new();
    s.push_str("# .acedata — ACodeTerm session state, auto-managed.\n\n");
    // Top-level focus state. Written as `focus = N` for a cell index,
    // or omitted when the explorer was focused. Lives above the cell
    // blocks so the parser sees it before the first `[[cell]]` opens
    // a section scope.
    if let Some(i) = snap.focus {
        s.push_str(&format!("focus = {i}\n\n"));
    }
    for cell in &snap.cells {
        s.push_str("[[cell]]\n");
        s.push_str(&format!("active = {}\n\n", cell.active));
        for sess in &cell.sessions {
            s.push_str("[[session]]\n");
            s.push_str(&format!("kind = {}\n", quote(kind_str(sess.kind))));
            if let Some(p) = sess.path.as_ref() {
                let rel = to_relative(p, project_root);
                s.push_str(&format!("path = {}\n", quote(&rel)));
            }
            s.push('\n');
        }
    }
    s
}

fn parse(content: &str, project_root: &Path) -> StateSnapshot {
    let mut snap = StateSnapshot::default();
    let mut current: Current = Current::None;
    let mut pending_cell: Option<CellState> = None;
    let mut pending_session: Option<SessionState> = None;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[cell]]" {
            flush_session(&mut pending_session, &mut pending_cell);
            flush_cell(&mut pending_cell, &mut snap);
            pending_cell = Some(CellState::default());
            current = Current::Cell;
            continue;
        }
        if line == "[[session]]" {
            flush_session(&mut pending_session, &mut pending_cell);
            pending_session = Some(SessionState { kind: SessionKind::Edit, path: None });
            current = Current::Session;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue; };
        let key = key.trim();
        let value = unquote(value.trim());
        match current {
            Current::Cell => {
                if let Some(cell) = pending_cell.as_mut() {
                    if key == "active" {
                        cell.active = value.parse().unwrap_or(0);
                    }
                }
            }
            Current::Session => {
                if let Some(sess) = pending_session.as_mut() {
                    match key {
                        "kind" => {
                            if let Some(k) = SessionKind::parse(&value) {
                                sess.kind = k;
                            }
                        }
                        "path" => {
                            sess.path = Some(resolve_relative(&value, project_root));
                        }
                        _ => {}
                    }
                }
            }
            Current::None => {
                // Top-level keys (outside any section). `focus = N`
                // restores the focused cell; unknown keys are ignored.
                if key == "focus" {
                    snap.focus = value.parse().ok();
                }
            }
        }
    }
    flush_session(&mut pending_session, &mut pending_cell);
    flush_cell(&mut pending_cell, &mut snap);
    snap
}

enum Current { None, Cell, Session }

fn flush_session(pending: &mut Option<SessionState>, cell: &mut Option<CellState>) {
    if let (Some(s), Some(c)) = (pending.take(), cell.as_mut()) {
        c.sessions.push(s);
    }
}

fn flush_cell(pending: &mut Option<CellState>, snap: &mut StateSnapshot) {
    if let Some(c) = pending.take() {
        if !c.sessions.is_empty() {
            snap.cells.push(c);
        }
    }
}

fn kind_str(k: SessionKind) -> &'static str {
    match k {
        SessionKind::Claude   => "claude",
        SessionKind::Shell    => "shell",
        SessionKind::Edit     => "edit",
        SessionKind::Diff     => "diff",
        SessionKind::Conflict => "conflict",
    }
}

fn to_relative(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

fn resolve_relative(s: &str, root: &Path) -> PathBuf {
    let p = PathBuf::from(s);
    if p.is_absolute() { p } else { root.join(p) }
}

fn quote(s: &str) -> String {
    let mut esc = String::with_capacity(s.len() + 2);
    esc.push('"');
    for ch in s.chars() {
        match ch {
            '"'  => esc.push_str("\\\""),
            '\\' => esc.push_str("\\\\"),
            _    => esc.push(ch),
        }
    }
    esc.push('"');
    esc
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && (t.starts_with('"') && t.ends_with('"')
        || t.starts_with('\'') && t.ends_with('\''))
    {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('"')  => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n')  => out.push('\n'),
                    Some('t')  => out.push('\t'),
                    Some(c)    => { out.push('\\'); out.push(c); }
                    None       => out.push('\\'),
                }
            } else {
                out.push(ch);
            }
        }
        out
    } else {
        t.to_string()
    }
}
