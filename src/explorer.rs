use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::projects::ProjectList;

/// Unified file-tree model. Rows are a mix of:
///   * project headers  (depth 0, one per project in the list)
///   * dirs / files     (depth ≥ 1, shown only under the active project)
///
/// Exactly one project is "expanded" at a time — the active one — so
/// switching projects automatically collapses the others.
pub struct FileTree {
    pub entries:       Vec<Entry>,
    pub selected:      usize,
    /// Directory paths the user has expanded inside the active project.
    /// Cleared implicitly on project switch: on rebuild we only include
    /// rows under the new active project so anything not under its root
    /// is invisible anyway.
    pub expanded_dirs: HashSet<PathBuf>,
    /// First visible row index. Persisted across frames so cursor motion
    /// only scrolls when the selection would leave the viewport — without
    /// this the List widget recomputes offset from 0 every frame and
    /// anchors the cursor to the bottom of the viewport.
    pub view_offset:   Cell<usize>,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path:     PathBuf,
    pub depth:    u16,
    pub kind:     EntryKind,
    pub expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Non-interactive section label (e.g. "OPEN CELLS", "PROJECTS").
    /// Skipped by cursor movement.
    SectionHeader(&'static str),
    /// One open cell. `idx` indexes into `App::cells`.
    OpenCell { idx: usize },
    /// Project header row. `idx` is the index into
    /// `ProjectList::projects`.
    Project { idx: usize },
    Dir,
    File,
}

impl EntryKind {
    /// True for rows the cursor is allowed to land on. Headers are
    /// visual-only.
    pub fn is_selectable(self) -> bool {
        !matches!(self, EntryKind::SectionHeader(_))
    }
}

/// What happened when the user pressed Enter on the selected row.
pub enum Action {
    None,
    OpenFile(PathBuf),
    SwitchProject(usize),
    /// User activated an "open cell" row — caller should set focus to
    /// `FocusId::Cell(idx)`.
    FocusOpenCell(usize),
}

impl FileTree {
    pub fn new(projects: &ProjectList, open_cell_count: usize) -> Self {
        let expanded_dirs = HashSet::new();
        let entries = build_entries(projects, open_cell_count, &expanded_dirs);
        let selected = first_selectable(&entries);
        Self { entries, selected, expanded_dirs, view_offset: Cell::new(0) }
    }

    /// Re-scan from disk. Preserves selection-by-path and the set of
    /// expanded directories. Called on the 1 s files tick + after any
    /// action that changes the underlying tree.
    pub fn refresh(&mut self, projects: &ProjectList, open_cell_count: usize) {
        let selected_path = self.entries.get(self.selected).map(|e| e.path.clone());
        self.entries = build_entries(projects, open_cell_count, &self.expanded_dirs);
        self.selected = self.relocate_selection(selected_path);
    }

    /// Rebuild after the active project changed. Drops the old
    /// project's expanded dirs (nothing under the new active project
    /// was open yet) and parks selection on the new active header.
    pub fn on_project_switch(&mut self, projects: &ProjectList, open_cell_count: usize) {
        self.expanded_dirs.clear();
        self.entries = build_entries(projects, open_cell_count, &self.expanded_dirs);
        self.selected = self
            .entries
            .iter()
            .position(|e| matches!(e.kind, EntryKind::Project { idx } if idx == projects.active))
            .unwrap_or_else(|| first_selectable(&self.entries));
    }

    pub fn move_up(&mut self) {
        let mut i = self.selected;
        while i > 0 {
            i -= 1;
            if self.entries.get(i).map(|e| e.kind.is_selectable()).unwrap_or(false) {
                self.selected = i;
                return;
            }
        }
    }

    /// Path of the selected row if it's a real file (not a header,
    /// project, dir, or open-cell row). Lets key handlers act on
    /// "selected file" without replicating the entry-kind filter.
    pub fn selected_file(&self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?;
        matches!(entry.kind, EntryKind::File).then(|| entry.path.clone())
    }

    /// Path of the selected row if it's a real file or directory (not
    /// a header, project, or open-cell row). Second tuple element is
    /// `true` for directories.
    pub fn selected_fs_path(&self) -> Option<(PathBuf, bool)> {
        let entry = self.entries.get(self.selected)?;
        match entry.kind {
            EntryKind::File => Some((entry.path.clone(), false)),
            EntryKind::Dir  => Some((entry.path.clone(), true)),
            _ => None,
        }
    }

    /// Project index under the cursor, if the selected row is a
    /// project header. Used by the `c` close-project shortcut.
    pub fn selected_project(&self) -> Option<usize> {
        let entry = self.entries.get(self.selected)?;
        match entry.kind {
            EntryKind::Project { idx } => Some(idx),
            _ => None,
        }
    }

    /// Cell index under the cursor, if the selected row is an open-cell
    /// row. Used by the `c` close-cell shortcut (prefills `:q N`).
    pub fn selected_open_cell(&self) -> Option<usize> {
        let entry = self.entries.get(self.selected)?;
        match entry.kind {
            EntryKind::OpenCell { idx } => Some(idx),
            _ => None,
        }
    }

    /// Where a "new file here" should be anchored for the selected row:
    ///   * File          → its parent dir
    ///   * Dir           → the dir itself
    ///   * Project row   → the project root
    ///   * anything else → `None` (caller falls back to the active
    ///                     project root, if any)
    pub fn selected_new_file_dir(&self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?;
        match entry.kind {
            EntryKind::File    => entry.path.parent().map(Path::to_path_buf),
            EntryKind::Dir     => Some(entry.path.clone()),
            EntryKind::Project { .. } => Some(entry.path.clone()),
            _ => None,
        }
    }

    pub fn move_down(&mut self) {
        let mut i = self.selected + 1;
        while i < self.entries.len() {
            if self.entries[i].kind.is_selectable() {
                self.selected = i;
                return;
            }
            i += 1;
        }
    }

    /// Enter on current row:
    ///   * SectionHeader   → no-op
    ///   * OpenCell        → caller focuses the cell at `idx`
    ///   * Project header  → switch (unless it's already active — no-op)
    ///   * Dir             → toggle expansion in place
    ///   * File            → caller opens it
    pub fn activate(&mut self, projects: &ProjectList, open_cell_count: usize) -> Action {
        let entry = match self.entries.get(self.selected).cloned() {
            Some(e) => e,
            None    => return Action::None,
        };
        match entry.kind {
            EntryKind::SectionHeader(_) => Action::None,
            EntryKind::OpenCell { idx } => Action::FocusOpenCell(idx),
            EntryKind::Project { idx } => {
                if idx == projects.active {
                    Action::None
                } else {
                    Action::SwitchProject(idx)
                }
            }
            EntryKind::Dir => {
                if self.expanded_dirs.contains(&entry.path) {
                    self.expanded_dirs.remove(&entry.path);
                } else {
                    self.expanded_dirs.insert(entry.path.clone());
                }
                let sel_path = Some(entry.path.clone());
                self.entries = build_entries(projects, open_cell_count, &self.expanded_dirs);
                self.selected = self.relocate_selection(sel_path);
                Action::None
            }
            EntryKind::File => Action::OpenFile(entry.path),
        }
    }

    fn relocate_selection(&self, prev_path: Option<PathBuf>) -> usize {
        match prev_path.and_then(|p| self.entries.iter().position(|e| e.path == p)) {
            Some(i) if self.entries[i].kind.is_selectable() => i,
            _ => {
                let fallback = self.selected.min(self.entries.len().saturating_sub(1));
                if self.entries.get(fallback).map(|e| e.kind.is_selectable()).unwrap_or(true) {
                    fallback
                } else {
                    first_selectable(&self.entries)
                }
            }
        }
    }
}

fn first_selectable(entries: &[Entry]) -> usize {
    entries.iter().position(|e| e.kind.is_selectable()).unwrap_or(0)
}

fn build_entries(
    projects:         &ProjectList,
    open_cell_count:  usize,
    expanded_dirs:    &HashSet<PathBuf>,
) -> Vec<Entry> {
    let mut out = Vec::new();

    // ── Open Cells section (shown only when non-empty) ──────────────────
    if open_cell_count > 0 {
        out.push(Entry {
            path:     PathBuf::new(),
            depth:    0,
            kind:     EntryKind::SectionHeader("open cells"),
            expanded: false,
        });
        for i in 0..open_cell_count {
            // Cell rows carry no stable path — titles come from the
            // live cell state in the UI renderer. relocate_selection
            // falls back to index-based when path doesn't match.
            let path = PathBuf::new();
            out.push(Entry {
                path,
                depth:    1,
                kind:     EntryKind::OpenCell { idx: i },
                expanded: false,
            });
        }
    }

    // ── Projects section ────────────────────────────────────────────────
    // The "projects" header is only shown when we also have open cells —
    // otherwise the project rows are the whole panel and a label is noise.
    if !projects.projects.is_empty() && open_cell_count > 0 {
        out.push(Entry {
            path:     PathBuf::new(),
            depth:    0,
            kind:     EntryKind::SectionHeader("projects"),
            expanded: false,
        });
    }
    for (i, p) in projects.projects.iter().enumerate() {
        let is_active = i == projects.active;
        out.push(Entry {
            path:     p.root.clone(),
            depth:    0,
            kind:     EntryKind::Project { idx: i },
            expanded: is_active,
        });
        if is_active {
            collect_tree(&p.root, 1, expanded_dirs, &mut out);
        }
    }
    out
}

fn collect_tree(dir: &Path, depth: u16, expanded_dirs: &HashSet<PathBuf>, out: &mut Vec<Entry>) {
    for mut item in list_dir(dir, depth) {
        let recurse = matches!(item.kind, EntryKind::Dir) && expanded_dirs.contains(&item.path);
        if recurse {
            item.expanded = true;
        }
        let path = item.path.clone();
        out.push(item);
        if recurse {
            collect_tree(&path, depth + 1, expanded_dirs, out);
        }
    }
}

fn list_dir(path: &Path, depth: u16) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut items: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter(|e| {
            // `.acedata` is our per-project session store — internal
            // bookkeeping, never something the user wants to open or
            // stage. Hide it from the tree like `.git/` would be
            // hidden if we had a dotfile convention.
            e.file_name().to_str() != Some(".acedata")
        })
        .map(|e| {
            let p = e.path();
            let is_dir = p.is_dir();
            Entry {
                path:     p,
                depth,
                kind:     if is_dir { EntryKind::Dir } else { EntryKind::File },
                expanded: false,
            }
        })
        .collect();
    // dirs first, then files; case-insensitive alpha within each group.
    items.sort_by(|a, b| {
        let a_dir = matches!(a.kind, EntryKind::Dir);
        let b_dir = matches!(b.kind, EntryKind::Dir);
        b_dir.cmp(&a_dir).then_with(|| {
            let an = a.path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
            let bn = b.path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
            an.cmp(&bn)
        })
    });
    items
}
