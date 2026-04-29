use std::sync::mpsc;

use ratatui::layout::Rect;

use crate::cell::{Cell, LayoutMode, Session, SessionKind};
use crate::editor::Editor;
use crate::events::AppEvent;
use crate::hex::HexView;
use crate::explorer::FileTree;
use crate::git::{self, ChangeRow, MultiRepo};
use crate::projects::ProjectList;
use crate::session_state::{CellState, SessionState, StateSnapshot};
use crate::status::StatusBar;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use notify::RecommendedWatcher;

struct RenameDetected {
    old: PathBuf,
    new: PathBuf,
}

/// User's home directory. Checks `HOME` first (Unix + Git Bash) then
/// `USERPROFILE` (Windows native). Returns `None` only if neither is set,
/// in which case the caller keeps whatever cwd it already had.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Ensure `.gitignore` at `project_root` contains the ace session
/// artefacts (`.acedata`, `.acerc`). Only acts if the project has a
/// `.git/` directory (anything else is either a non-repo folder or a
/// worktree we don't own). No-op when `.acerc auto_gitignore = false`.
/// Best-effort: IO errors are swallowed so a read-only filesystem doesn't
/// break session startup.
pub fn ensure_gitignore_entries(project_root: &std::path::Path) {
    // Opt-out via user-level or project-level `.acerc`. Unset / true →
    // inject; false → skip.
    let cfg = crate::config::Config::load();
    if matches!(cfg.auto_gitignore, Some(false)) {
        return;
    }
    if !project_root.join(".git").exists() {
        return;
    }
    let gi = project_root.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    let wanted = [".acedata", ".acerc"];
    let has_entry = |content: &str, pat: &str| -> bool {
        content
            .lines()
            .map(str::trim)
            .any(|l| l == pat || l == format!("/{pat}"))
    };
    let missing: Vec<&str> = wanted.iter().copied().filter(|p| !has_entry(&existing, p)).collect();
    if missing.is_empty() {
        return;
    }
    let mut out = existing.clone();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push_str("\n# ace session data (auto-added; disable via .acerc auto_gitignore=false)\n");
    } else {
        out.push_str("# ace session data (auto-added; disable via .acerc auto_gitignore=false)\n");
    }
    for m in &missing {
        out.push_str(m);
        out.push('\n');
    }
    let _ = std::fs::write(&gi, out);
}

/// Expand a leading `~` (or `~/…`) to the user's home directory. Any
/// non-tilde input and paths without a leading tilde are returned
/// unchanged. Used at every command-bar path entry point so users don't
/// have to type the full home path.
pub fn expand_tilde(input: &str) -> String {
    if input == "~" {
        if let Some(home) = home_dir() {
            return home.to_string_lossy().into_owned();
        }
        return input.to_string();
    }
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    input.to_string()
}

/// Remap an index when a `Vec::remove(from); insert(to, _)` happens.
/// - The removed element at `from` ends up at `to`
/// - Elements between `from` and `to` shift by one in the opposite
///   direction
/// - Elements outside that range are untouched
fn remap_move_index(i: usize, from: usize, to: usize) -> usize {
    if i == from { return to; }
    if from < to {
        if i > from && i <= to { i - 1 } else { i }
    } else {
        if i >= to && i < from { i + 1 } else { i }
    }
}

fn hash_snapshot(snap: &StateSnapshot) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    snap.cells.len().hash(&mut h);
    for cell in &snap.cells {
        cell.active.hash(&mut h);
        cell.sessions.len().hash(&mut h);
        for sess in &cell.sessions {
            (sess.kind as u8).hash(&mut h);
            if let Some(p) = sess.path.as_ref() {
                p.hash(&mut h);
            }
        }
    }
    snap.focus.hash(&mut h);
    h.finish()
}

/// Startup intent from the CLI. `ace` with no args uses `GlobalList`;
/// `.acerc` cwd_only=true flips to `CwdOnly`; `ace <paths…>` is `Explicit`.
/// `files` are ad-hoc editor targets to open at launch (one cell each).
pub struct Startup {
    pub kind:  StartupKind,
    pub files: Vec<PathBuf>,
}

pub enum StartupKind {
    /// Global saved list, cwd NOT auto-added.
    GlobalList,
    /// Session-only: just the cwd.
    CwdOnly,
    /// Session-only: list of dirs passed on argv. Empty = no project rail
    /// (files-only session if `files` is non-empty).
    Explicit(Vec<PathBuf>),
    /// Session-only: no projects at all, welcome page front-and-centre.
    /// Triggered by `.acerc welcome = true` when `ace` is run with no
    /// args. The user can `:proj add <dir>` to add a project at will.
    Welcome,
}
use crate::session::PtySession;
use crate::theme::Theme;

/// Maximum number of main-area cells. With `<space>+<digit>` digit
/// jumps (0 → Explorer, 1..9 → cells), one digit can address one
/// Explorer pane plus nine cells.
pub const MAX_CELLS: usize = 9;

/// Something that can be focused. The main cell area is addressed by
/// index; the left sidebar is a single unified panel (projects + files
/// + git tinting, all in one).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FocusId {
    Explorer,
    Cell(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Visual selection — entered with `v` (charwise) or `V` (linewise)
    /// from Normal while an editor is focused. Motions extend the
    /// selection; `d`/`c`/`y` resolve it and return to Normal (or to
    /// Insert, for `c`). Only meaningful on editor cells — other focus
    /// types never land here.
    Visual { linewise: bool },
    Command { buffer: String },
    /// Hidden-input prompt for a sudo password. Entered via `:sudo …`,
    /// `:w!`, `:w!q`, or `:x!`; the buffer never renders as cleartext.
    /// Submit runs `action`; Esc cancels without spawning sudo.
    Password { buffer: String, action: SudoAction },
}

/// What a pending sudo prompt should do once the password is submitted.
/// Captured before entering `Mode::Password` so the action runs with the
/// focus state the user had when they typed the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SudoAction {
    /// `:sudo w` / `:w!` — write via `sudo tee`, stay open.
    Write,
    /// `:sudo wq` / `:sudo x` / `:w!q` / `:x!` — write, then close the
    /// focused cell.
    WriteClose,
    /// `:sudo wQ` — write, then quit the whole app.
    WriteQuitApp,
}

impl Mode {
    pub fn badge(&self) -> &'static str {
        match self {
            Mode::Normal              => "NOR",
            Mode::Insert              => "INS",
            Mode::Visual { linewise } => if *linewise { "V-L" } else { "VIS" },
            Mode::Command {..}        => "CMD",
            Mode::Password {..}       => "PWD",
        }
    }
}

/// Sub-modes of the Explorer panel. Only meaningful while the Explorer
/// panel is focused and the outer [`Mode`] is `Normal`.
///
/// * `Normal`      — standard file-tree navigation; git section shows a
///                   compact 2-line footer at the bottom of the panel.
/// * `GitOverview` — pressing `g` collapses projects to headers and
///                   expands the git section (branches + changes lists
///                   visible but no cursor in either — globals only).
/// * `GitBranches` — from `GitOverview`, pressing `b` puts the cursor on
///                   the branches list; branch-specific keybinds engage.
/// * `GitChanges`  — `c` moves cursor into the changes list; file-specific
///                   keybinds engage. Globals remain available in both
///                   sub-modes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExplorerMode {
    Normal,
    GitOverview,
    GitBranches,
    GitChanges,
    GitLog,
}

impl ExplorerMode {
    pub fn badge(self) -> &'static str {
        match self {
            ExplorerMode::Normal       => "NOR",
            ExplorerMode::GitOverview  => "GIT",
            ExplorerMode::GitBranches  => "BCH",
            ExplorerMode::GitChanges   => "CHG",
            ExplorerMode::GitLog       => "LOG",
        }
    }

    pub fn is_git(self) -> bool {
        !matches!(self, ExplorerMode::Normal)
    }
}

/// Pending destructive action awaiting a `y`/`N` keystroke. Kept as
/// owned data so we don't borrow anything across the user's next
/// keypress.
#[derive(Clone, Debug)]
pub enum PendingConfirm {
    /// `d` on an unstaged or untracked change — restores (or
    /// deletes, for untracked) the path unconditionally.
    DiscardChange  { path: String },
    /// `D` on a branch with unmerged commits — drops the branch
    /// anyway. Data loss if no other ref holds those commits.
    ForceDeleteBranch { name: String },
    /// `o` / `t` on a conflicted path — overwrites the worktree with
    /// the chosen side's blob; the other side is discarded.
    ResolveConflict { path: String, side: crate::git::ConflictSide },
    /// `:git stash drop [n]` — can't be recovered from the stash ref
    /// after it's dropped (reflog sticks around for a while, but
    /// relying on that isn't obvious to most users).
    StashDrop { idx: usize },
    /// `d` on a file/dir in the explorer — removes it from disk.
    /// Directories are deleted recursively.
    DeletePath { path: PathBuf, is_dir: bool },
}

impl PendingConfirm {
    /// Text shown to the user in the status bar while waiting on
    /// `y`/`N`. Always ends with the `[y/N]` hint so the user isn't
    /// guessing what keys to press.
    pub fn prompt(&self) -> String {
        match self {
            PendingConfirm::DiscardChange { path } =>
                format!("discard {path}? [y/N]"),
            PendingConfirm::ForceDeleteBranch { name } =>
                format!("force-delete branch {name}? (unmerged commits will be lost) [y/N]"),
            PendingConfirm::ResolveConflict { path, side } => {
                let lbl = match side {
                    crate::git::ConflictSide::Ours   => "ours",
                    crate::git::ConflictSide::Theirs => "theirs",
                };
                format!("resolve {path} with {lbl}? (other side discarded) [y/N]")
            }
            PendingConfirm::StashDrop { idx } =>
                format!("drop stash@{{{idx}}}? [y/N]"),
            PendingConfirm::DeletePath { path, is_dir } => {
                let name = path.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                let kind = if *is_dir { "directory" } else { "file" };
                format!("delete {kind} {name}? [y/N]")
            }
        }
    }
}

pub struct App {
    pub focus: FocusId,
    pub last_cell_focus: usize,     // remembered so digit jumps feel stable
    /// True after Space in Normal mode arms a jump. The next key press
    /// dispatches: `0` → Explorer, `1..9` → Cell(n-1); anything else
    /// cancels silently.
    pub pending_jump: bool,
    /// True after Ctrl+Space arms swap mode. The next digit picks the
    /// target cell to swap content with. Focus stays at the *original*
    /// slot — content swaps but the user's screen position doesn't.
    /// `pending_swap_follow` is the same arm with focus moving to the
    /// target's slot instead. Set together they're mutually exclusive —
    /// whichever primed last wins.
    pub pending_swap: bool,
    pub pending_swap_follow: bool,
    pub mode: Mode,
    pub theme: Theme,
    pub git: MultiRepo,
    pub tray_count: u32,
    pub should_quit: bool,
    pub status: StatusBar,

    pub cells:       Vec<Cell>,
    pub layout_mode: LayoutMode,

    pub explorer: FileTree,
    pub projects: ProjectList,
    pub tx:       mpsc::Sender<AppEvent>,

    /// `true` hides the explorer sidebar — its column drops to 0 width
    /// and cell layout expands to fill the terminal. Re-shown by a
    /// `space+0` jump or another `e` toggle.
    pub explorer_hidden: bool,

    /// `true` expands the explorer to the full terminal width (cells
    /// column collapses to 0). Mutually exclusive with `explorer_hidden`
    /// — setting one clears the other. Toggled with `F` on the focused
    /// explorer.
    pub explorer_fullscreen: bool,

    /// Native filesystem watcher for real-time reconciliation. `None`
    /// if the OS refused to create one — the slow poll tick still
    /// functions as a fallback.
    pub fs_watcher: Option<RecommendedWatcher>,
    /// Paths we've asked the watcher to follow. Tracked here so `watch`
    /// is idempotent and so we can `unwatch` on cleanup.
    pub watched: HashSet<PathBuf>,
    /// Hash of the last `.acedata` snapshot we wrote to disk. End-of-
    /// loop persistence compares the current snapshot against this and
    /// only writes when they diverge — keeps state save effectively
    /// realtime without re-hashing more than needed.
    pub last_saved_hash: u64,
    /// Coarse "something happened this iteration that might have changed
    /// the structural snapshot" flag. Set by the event dispatch path and
    /// cleared by `persist_cells_if_dirty`. When clear, the dirty-check
    /// skips building+hashing a snapshot — cheap iteration when the
    /// loop only woke for a status tick.
    pub persist_dirty_hint: bool,

    /// Sub-mode of the Explorer panel. Purely a UI concern — cell focus
    /// elsewhere forces us back to `Normal` on re-entry.
    pub explorer_mode:     ExplorerMode,
    /// Cursor index into `app.git.branches` while in `GitBranches`.
    pub git_branch_sel: usize,
    /// Cursor index into `app.git.change_rows()` while in `GitChanges`.
    pub git_change_sel: usize,
    /// Cached commit log for `GitLog` view. Loaded on-demand.
    pub git_log:        Vec<crate::git::LogEntry>,
    /// Cursor index into `git_log` while in `GitLog`.
    pub git_log_sel:    usize,
    /// Destructive action queued for a y/N confirmation. While this is
    /// `Some`, the outer key handler routes the next keystroke to
    /// `resolve_confirm` instead of the normal dispatch.
    pub pending_confirm: Option<PendingConfirm>,

    /// Cell currently acting as the Explorer "preview" slot. Enter on
    /// a file row loads into this cell without stealing focus, and a
    /// second Enter on a different file overwrites the same slot —
    /// so the user can quickly skim many files without cluttering the
    /// cell list. As soon as the cell is focused (committed), this
    /// clears and the next preview opens a fresh cell.
    pub preview_cell_idx: Option<usize>,

    /// Live tab-completion cycle for the `:`-command line. `None`
    /// when no cycle is active; set by the first `Tab` and cleared
    /// by any non-Tab edit. `sel` indexes `options`; the current
    /// option is always what sits in the command buffer at
    /// `buffer[start..]` — so the buffer alone is authoritative for
    /// what will run on Enter.
    pub completion: Option<CompletionState>,

    /// Coalesced explorer-refresh flag. FS events fire in bursts (a save
    /// can easily produce 3-5 events on Windows); instead of doing a full
    /// tree re-scan per event we set this flag and let the main loop do
    /// one refresh per iteration.
    pub pending_explorer_refresh: bool,

    /// Cells parked by project root on switch-out. Preserves PTY state
    /// (shell/claude children keep running in the background) so
    /// returning to a project brings back the exact same running
    /// sessions instead of respawning from `.acedata`. Keyed by the
    /// project's absolute root path.
    pub parked_cells: HashMap<PathBuf, ParkedProject>,
}

/// A project's cells parked while the user is in another project.
pub struct ParkedProject {
    pub cells: Vec<Cell>,
    pub focus: Option<usize>,
}

/// In-flight state for a `:`-command Tab-completion cycle.
#[derive(Debug, Clone)]
pub struct CompletionState {
    pub start:   usize,
    pub options: Vec<String>,
    pub sel:     usize,
    /// `true` when the options were auto-computed from typing and have
    /// NOT yet been spliced into the buffer — a hint preview. The first
    /// Tab promotes this state into an active cycle (splices `sel` and
    /// flips the flag). `false` once cycling is underway.
    pub preview: bool,
}

impl App {
    /// Attach or re-attach the FS watcher to every project root
    /// (recursive), every ad-hoc CLI file's parent dir, and every
    /// currently-open editor's parent dir. Safe to call repeatedly —
    /// duplicates are skipped via `self.watched`. Call after any
    /// mutation that adds a new file path (new cell, `:e`, etc.).
    pub fn refresh_watchers(&mut self) {
        let Some(w) = self.fs_watcher.as_mut() else { return; };
        // 1. Project roots — recursive covers every file in the tree.
        for p in &self.projects.projects {
            if p.root.exists() && self.watched.insert(p.root.clone()) {
                crate::events::watch_path(w, &p.root, true);
            }
        }
        // 2. Currently-open editors — picks up files opened mid-session
        //    via `:e <path>` that don't live under any project root.
        let editor_parents: Vec<PathBuf> = self
            .cells
            .iter()
            .flat_map(|c| c.sessions.iter())
            .filter_map(|s| match s {
                Session::Edit(ed) => ed.path.as_ref().and_then(|p| p.parent()).map(PathBuf::from),
                _ => None,
            })
            .collect();
        for parent in editor_parents {
            if parent.exists() && self.watched.insert(parent.clone()) {
                crate::events::watch_path(w, &parent, false);
            }
        }
    }

    pub fn new(tx: mpsc::Sender<AppEvent>, startup: Startup) -> Self {
        let tx_for_watcher = tx.clone();
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

        let projects = match startup.kind {
            StartupKind::GlobalList        => ProjectList::global(&cwd),
            StartupKind::CwdOnly           => ProjectList::cwd_only(&cwd),
            StartupKind::Explicit(dirs) if dirs.is_empty() => ProjectList::empty(),
            StartupKind::Explicit(dirs)    => ProjectList::explicit(dirs, &cwd),
            StartupKind::Welcome           => ProjectList::empty(),
        };

        // If `ace` (no args) ran with an empty global list, fall back to
        // the welcome page (no project rail) — matches the explicit
        // `Welcome` startup. Users who want a cwd-only session should
        // set `.acerc cwd_only = true`.
        let projects = if projects.projects.is_empty() && startup.files.is_empty() {
            ProjectList::empty()
        } else {
            projects
        };

        // Active-project cwd: when `projects` is non-empty, `cd` to the
        // active project root so git + new PTYs start there. Otherwise
        // keep the launch cwd — running `ace` in a directory should
        // stay in that directory.
        if let Some(dir) = projects.projects.get(projects.active).map(|p| p.root.clone()) {
            let _ = std::env::set_current_dir(&dir);
        }
        let git_cwd = std::env::current_dir().unwrap_or_else(|_| cwd.clone());
        // Git discovery (nested-repo walk + per-repo status load) is
        // the dominant cost of cold startup. Do it off-thread — the
        // first frame renders with empty repos + `None` rail dots, and
        // `AppEvent::GitBootstrap` fills them in when discovery finishes.
        let mut git = MultiRepo::empty();
        git.project_root = Some(git_cwd.clone());
        let rail_roots: Vec<PathBuf> = projects.projects.iter().map(|p| p.root.clone()).collect();
        crate::events::spawn_git_bootstrap(tx.clone(), git_cwd, rail_roots);
        // Cells aren't populated yet — spawn_initial_cells runs after
        // App::new. The caller refreshes the explorer with real cell
        // counts once that's done.
        let explorer = FileTree::new(&projects, 0);

        // Explorer visibility rule: hidden when there's no project rail
        // *and* the session has at most one ad-hoc file — i.e. either the
        // welcome page (0 files) or a single-file launch. Any other
        // combination (multiple files, single/multi projects, mixed)
        // shows it.
        let explorer_hidden =
            projects.projects.is_empty() && startup.files.len() <= 1;

        Self {
            focus: FocusId::Explorer,
            last_cell_focus: 0,
            pending_jump: false,
            pending_swap: false,
            pending_swap_follow: false,
            mode: Mode::Normal,

            theme: Theme::dark(),
            git,
            tray_count: 0,
            should_quit: false,
            status: StatusBar::new(),

            cells:       Vec::new(),
            layout_mode: crate::config::Config::load()
                .layout
                .as_deref()
                .and_then(LayoutMode::parse)
                .unwrap_or(LayoutMode::MasterBottom),

            explorer,
            projects,
            tx,

            explorer_hidden,
            explorer_fullscreen: false,
            fs_watcher:        crate::events::start_fs_watcher(tx_for_watcher),
            watched:           HashSet::new(),
            last_saved_hash:   0,
            persist_dirty_hint: false,
            explorer_mode:     ExplorerMode::Normal,
            git_branch_sel: 0,
            git_change_sel: 0,
            git_log:        Vec::new(),
            git_log_sel:    0,
            pending_confirm: None,
            preview_cell_idx: None,
            completion: None,
            pending_explorer_refresh: false,
            parked_cells: HashMap::new(),
        }
    }

    /// Re-read git state from the current working directory. Synchronous —
    /// used after any action the user expects immediate feedback from
    /// (`:w`, stage, commit, etc.). The periodic background refresh in
    /// `events::start_git_refresh_thread` is the passive counterpart.
    pub fn refresh_git(&mut self) {
        let cwd = self.current_project_root()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
        // Preserve the user's active-repo cursor across refreshes so a
        // background reload mid-session doesn't snap back to repo 0.
        // Stays valid as long as the repo set is stable; otherwise
        // `clamp_git_cursors` below trims it.
        let prev_active = self.git.active;
        self.git = MultiRepo::discover(&cwd);
        if prev_active < self.git.repos.len() {
            self.git.active = prev_active;
        }
        self.clamp_git_cursors();
    }

    /// Install the result of a background `GitBootstrap`: swap in the
    /// active project's freshly walked `MultiRepo` and merge per-project
    /// rail state dots. Preserves the user's `active` repo cursor if
    /// the new MultiRepo is for the same project root.
    pub fn apply_git_bootstrap(
        &mut self,
        multi: MultiRepo,
        rail: Vec<crate::projects::RailRefresh>,
    ) {
        let same_project = match (&self.git.project_root, &multi.project_root) {
            (Some(a), Some(b)) => crate::git::paths_equal(a, b),
            _ => false,
        };
        let prev_active = self.git.active;
        self.git = multi;
        if same_project && prev_active < self.git.repos.len() {
            self.git.active = prev_active;
        }
        self.clamp_git_cursors();
        self.projects.apply_rail_refresh(rail);
    }

    /// Apply a fresh single-repo snapshot delivered by the background
    /// refresh thread. Matches the snapshot's workdir against our
    /// existing `MultiRepo.repos` and replaces that slot so nested-repo
    /// state stays stable between ticks. If no match, drops the update
    /// — that means the active project or the repo set shifted and
    /// an explicit `refresh_git` will catch up.
    pub fn set_git_snapshot(&mut self, mut snap: crate::git::GitSnapshot) {
        let Some(wd) = snap.workdir.clone() else { return; };
        for r in self.git.repos.iter_mut() {
            let matches = r.workdir.as_ref()
                .map(|existing| crate::git::paths_equal(existing, &wd))
                .unwrap_or(false);
            if matches {
                // Passive loads (periodic tick) skip branches + stash
                // to keep the 3s refresh cheap — preserve whatever the
                // previous full load cached for those fields so the
                // git panel doesn't flicker empty between user actions.
                if snap.branches.is_empty() && !r.branches.is_empty() {
                    snap.branches = std::mem::take(&mut r.branches);
                }
                if snap.stash_count == 0 && r.stash_count != 0 {
                    snap.stash_count = r.stash_count;
                }
                *r = snap;
                self.clamp_git_cursors();
                return;
            }
        }
    }

    /// Clamp branch / change cursors after a snapshot swap so stale
    /// indices can't point past the end of the newly loaded lists.
    pub fn clamp_git_cursors(&mut self) {
        let n_branches = self.git.branches.len();
        if self.git_branch_sel >= n_branches {
            self.git_branch_sel = n_branches.saturating_sub(1);
        }
        let n_changes = self.git.change_rows().len();
        if self.git_change_sel >= n_changes {
            self.git_change_sel = n_changes.saturating_sub(1);
        }
        // If a git sub-mode refers to a section that's now empty, drop
        // back to overview so the UI doesn't render a dead cursor.
        match self.explorer_mode {
            ExplorerMode::GitBranches if n_branches == 0 => self.explorer_mode = ExplorerMode::GitOverview,
            ExplorerMode::GitChanges  if n_changes  == 0 => self.explorer_mode = ExplorerMode::GitOverview,
            _ => {}
        }
        if self.explorer_mode.is_git() && !self.git.is_repo() {
            self.explorer_mode = ExplorerMode::Normal;
        }
        if self.git_log_sel >= self.git_log.len() {
            self.git_log_sel = self.git_log.len().saturating_sub(1);
        }
    }

    // ── accessors ────────────────────────────────────────────────────────

    pub fn focused_cell(&self) -> Option<&Cell> {
        match self.focus {
            FocusId::Cell(i) => self.cells.get(i),
            _                => None,
        }
    }

    pub fn focused_cell_mut(&mut self) -> Option<&mut Cell> {
        match self.focus {
            FocusId::Cell(i) => self.cells.get_mut(i),
            _                => None,
        }
    }

    pub fn focused_pty_mut(&mut self) -> Option<&mut PtySession> {
        self.focused_cell_mut()?.active_session_mut().as_pty_mut()
    }

    pub fn focused_editor_mut(&mut self) -> Option<&mut Editor> {
        self.focused_cell_mut()?.active_session_mut().as_editor_mut()
    }

    pub fn focused_hex_mut(&mut self) -> Option<&mut HexView> {
        self.focused_cell_mut()?.active_session_mut().as_hex_mut()
    }

    pub fn focused_session_is_hex(&self) -> bool {
        self.focused_cell()
            .map(|c| matches!(c.active_session(), Session::Hex(_)))
            .unwrap_or(false)
    }

    pub fn focused_session_is_editor(&self) -> bool {
        self.focused_cell()
            .map(|c| matches!(c.active_session(), Session::Edit(_)))
            .unwrap_or(false)
    }

    pub fn focused_session_is_diff(&self) -> bool {
        self.focused_cell()
            .map(|c| matches!(c.active_session(), Session::Diff(_)))
            .unwrap_or(false)
    }

    // ── focus / mode ─────────────────────────────────────────────────────

    pub fn set_focus(&mut self, next: FocusId) {
        let same = self.focus == next;
        self.focus = next;
        if let FocusId::Cell(i) = next {
            if i < self.cells.len() {
                self.last_cell_focus = i;
            }
            // Focusing the preview cell "commits" it — the user has
            // engaged with that buffer, so the next explorer preview
            // should open its own cell instead of overwriting this one.
            if self.preview_cell_idx == Some(i) {
                self.preview_cell_idx = None;
            }
        }
        // Explorer remembers its sub-mode across focus changes — if you
        // leave in GitChanges and come back, you land back in
        // GitChanges. `clamp_git_cursors` corrects stale cursors when
        // a snapshot refresh changes the underlying lists.
        // Don't blow away Command or Password mode (user is composing
        // a command / typing a sudo password — focus changes shouldn't
        // silently discard that).
        if matches!(self.mode, Mode::Command { .. } | Mode::Password { .. }) {
            return;
        }
        if same {
            return;
        }
        self.mode = self.natural_mode_for_focus();
    }

    /// Everything lands in Normal on focus change. Users press `i`/`a`
    /// to enter Insert; the editor uses it for text edits, shell/claude
    /// PTY cells use it to forward keystrokes to the child. Normal
    /// mode on a PTY cell is still useful — it lets `<Space>+<digit>`
    /// jumps reach the navigator, and `<Esc>` pass-through literal
    /// ESC bytes to the child when needed.
    fn natural_mode_for_focus(&self) -> Mode {
        Mode::Normal
    }

    /// Space-armed digit jump — `0` targets the Explorer panel, `1..9`
    /// target cell indices `0..8`. Out-of-range digits no-op with a
    /// status-bar message.
    ///
    /// Explorer toggle: jumping to `0` while the Explorer is already
    /// focused and visible hides it — the same key that shows it also
    /// dismisses it. Hidden or unfocused → reveal + focus.
    pub fn jump_to_cell_by_digit(&mut self, d: u32) {
        if d == 0 {
            if self.focus == FocusId::Explorer && !self.explorer_hidden {
                self.toggle_explorer_hidden();
            } else {
                self.explorer_hidden = false;
                self.set_focus(FocusId::Explorer);
            }
            return;
        }
        let idx = (d - 1) as usize;
        if idx >= self.cells.len() {
            self.status.push_auto(format!("no cell {d}"));
            return;
        }
        // Jumping to a minimized cell restores it first — same logic as
        // `:restore N` so `space+N` doubles as the restore shortcut.
        if self.cells[idx].minimized {
            self.cmd_restore(d);
        } else {
            self.set_focus(FocusId::Cell(idx));
        }
    }

    /// Arm swap mode so the next digit picks the swap target.
    /// `follow = false`: focus stays at the original slot (the user's
    /// screen position doesn't move, only the cell contents shuffle).
    /// `follow = true`: focus moves to the target slot so the user
    /// follows the content they were on (old behaviour).
    pub fn arm_swap(&mut self, follow: bool) {
        self.pending_jump = false;
        if !matches!(self.focus, FocusId::Cell(_)) {
            self.status.push_auto("swap: focus a cell first".into());
            return;
        }
        self.pending_swap = !follow;
        self.pending_swap_follow = follow;
    }

    /// Move cells[from] to position `to`, shifting the intervening cells
    /// by one. Keeps `App.focus` pointing at the same cell's new index.
    fn move_cell(&mut self, from: usize, to: usize) {
        if from == to || from >= self.cells.len() || to >= self.cells.len() {
            return;
        }
        let c = self.cells.remove(from);
        self.cells.insert(to, c);
        // Remap focus: the moved cell's index shifted; others between
        // `from` and `to` shifted by one in the opposite direction.
        if let FocusId::Cell(i) = self.focus {
            let new_i = remap_move_index(i, from, to);
            self.focus = FocusId::Cell(new_i);
            self.last_cell_focus = new_i;
        }
    }

    /// Minimize the focused cell: mark it minimized and move it to the
    /// tail of the cells vec so the invariant `visible-first, then
    /// minimized` holds. Focus follows the cell to its new index, then
    /// the caller can decide to move focus elsewhere. Currently focus
    /// stays on the now-hidden cell; the user can jump with space+N.
    pub fn minimize_focused(&mut self) {
        let FocusId::Cell(i) = self.focus else {
            self.status.push_auto("minimize: focus a cell first".into());
            return;
        };
        if self.cells[i].minimized {
            return;
        }
        let to = self.cells.len() - 1;
        self.move_cell(i, to);
        // `move_cell` also migrated focus to `to` via the remap. Mark
        // minimized after the move so the layout reshuffles correctly.
        self.cells[to].minimized = true;
        // Shift focus to the first visible cell so the layout isn't
        // pointing at a hidden one (which would leave no visible focus
        // border). Falls back to Explorer if everything is minimized.
        let next_visible = self.cells.iter().position(|c| !c.minimized);
        match next_visible {
            Some(vi) => self.set_focus(FocusId::Cell(vi)),
            None     => self.set_focus(FocusId::Explorer),
        }
        self.status.push_auto("minimized".into());
    }

    /// Minimize a specific cell by index (0-based). Like
    /// `minimize_focused` but targets any cell, not just the focused one.
    /// Focus is preserved if possible; if the focused cell itself is
    /// minimized, falls back to the first visible cell (or Explorer).
    pub fn minimize_idx(&mut self, i: usize) {
        if i >= self.cells.len() {
            self.status.push_auto(format!("no cell {}", i + 1));
            return;
        }
        if self.cells[i].minimized {
            return;
        }
        let focused = matches!(self.focus, FocusId::Cell(f) if f == i);
        let to = self.cells.len() - 1;
        // Stash focus index before the move so we can restore it if we
        // weren't targeting the focused cell.
        let saved_focus = if let FocusId::Cell(f) = self.focus { Some(f) } else { None };
        self.move_cell(i, to);
        self.cells[to].minimized = true;
        if focused {
            let next_visible = self.cells.iter().position(|c| !c.minimized);
            match next_visible {
                Some(vi) => self.set_focus(FocusId::Cell(vi)),
                None     => self.set_focus(FocusId::Explorer),
            }
        } else if let Some(f) = saved_focus {
            // `move_cell` already remapped focus; re-clamp in case the
            // now-minimized cell is ahead of visible focus.
            let remapped = if let FocusId::Cell(rf) = self.focus { rf } else { f };
            self.set_focus(FocusId::Cell(remapped));
        }
        self.status.push_auto(format!("minimized cell {}", i + 1));
    }

    /// `:min N` / `:min *` — minimize specific cell(s). `None` →
    /// focused cell (same as bare `:min`).
    pub fn cmd_minimize_target(&mut self, target: Option<CellTarget>) {
        match target {
            None                       => self.minimize_focused(),
            Some(CellTarget::Idx(i))   => self.minimize_idx(i),
            Some(CellTarget::All)      => {
                // Leave the focused cell visible so there's always one
                // live pane and the user keeps whatever they were
                // working on. Falls back to cell 0 when the explorer is
                // focused (no focused cell to protect).
                let keep = match self.focus {
                    FocusId::Cell(i) => i,
                    _                => 0,
                };
                let n = self.cells.len();
                // `minimize_idx` moves the cell to the tail, which
                // renumbers indices as we go. Collect the set of cells
                // to minimize before we start mutating so `keep` stays
                // meaningful for the whole pass.
                let mut todo: Vec<usize> = (0..n).filter(|&i| i != keep && !self.cells[i].minimized).collect();
                // Highest index first so each minimize doesn't disturb
                // the still-queued indices (move_cell shifts elements in
                // (i, to] down by one; working from the tail avoids
                // hitting any of them).
                todo.sort_unstable_by(|a, b| b.cmp(a));
                for i in todo {
                    if i < self.cells.len() && !self.cells[i].minimized {
                        self.minimize_idx(i);
                    }
                }
            }
        }
    }

    /// Restore a minimized cell by moving it to position 0 (the
    /// master slot — cell 1 in 1-based digit terms) and clearing its
    /// minimized flag. `i` is the current index in `App.cells`.
    fn restore_cell(&mut self, i: usize) {
        if i >= self.cells.len() || !self.cells[i].minimized {
            return;
        }
        self.cells[i].minimized = false;
        self.move_cell(i, 0);
    }

    /// `:restore N` — restores cell N (1..9) if minimized, then focuses
    /// it. If cell N is already visible, just focus it (same as jump).
    pub fn cmd_restore(&mut self, d: u32) {
        if d < 1 || d > 9 {
            self.status.push_auto(format!("restore: no cell {d}"));
            return;
        }
        let idx = (d - 1) as usize;
        if idx >= self.cells.len() {
            self.status.push_auto(format!("restore: no cell {d}"));
            return;
        }
        let new_idx = if self.cells[idx].minimized {
            // Restore moves the cell to position 0 (master slot).
            self.restore_cell(idx);
            0
        } else {
            idx
        };
        self.set_focus(FocusId::Cell(new_idx));
        self.last_cell_focus = new_idx;
    }

    /// Swap the focused cell with cell `d` (1..9). The swap exchanges
    /// both position AND minimized state: if the target is minimized,
    /// the focused cell becomes minimized at the target's old tail
    /// position, and the target becomes visible at the focused cell's
    /// old slot. `d == 0` minimizes the focused cell instead of
    /// swapping — shorthand for `:minimize`.
    pub fn swap_focused_with_digit(&mut self, d: u32, follow: bool) {
        if d == 0 {
            self.minimize_focused();
            return;
        }
        let FocusId::Cell(from) = self.focus else {
            self.status.push_auto("swap: focus a cell first".into());
            return;
        };
        if d > 9 {
            self.status.push_auto(format!("swap: no cell {d}"));
            return;
        }
        let to = (d - 1) as usize;
        if to >= self.cells.len() || to == from {
            if to >= self.cells.len() {
                self.status.push_auto(format!("swap: no cell {d}"));
            }
            return;
        }
        // Exchange minimized flags so the visual status swaps alongside
        // the positional swap: visible↔minimized becomes
        // minimized↔visible.
        let (a_min, b_min) = (self.cells[from].minimized, self.cells[to].minimized);
        self.cells[from].minimized = b_min;
        self.cells[to].minimized = a_min;
        // Positional swap.
        self.cells.swap(from, to);
        // Focus placement depends on `follow`:
        //   * follow=false (default Ctrl+Space): stay in the original
        //     slot (`from`). The user's screen position doesn't move;
        //     only the content under their cursor changes.
        //   * follow=true  (Ctrl+Shift+Space): focus moves with the
        //     content to the target slot (`to`).
        // In either case, if the chosen slot ended up minimized, fall
        // back to a visible cell so the layout has a focused border.
        let preferred = if follow { to } else { from };
        let alternate = if follow { from } else { to };
        let target_focus = if !self.cells[preferred].minimized {
            FocusId::Cell(preferred)
        } else if !self.cells[alternate].minimized {
            FocusId::Cell(alternate)
        } else {
            match self.cells.iter().position(|c| !c.minimized) {
                Some(v) => FocusId::Cell(v),
                None    => FocusId::Explorer,
            }
        };
        self.set_focus(target_focus);
        // Re-normalize invariant: move all minimized cells to the tail
        // in case the swap left a minimized cell interleaved with
        // visible ones.
        self.normalize_cell_order();
        self.status.push_auto(format!("swapped {} ↔ {}", from + 1, to + 1));
    }

    /// Stable partition: visible cells first, minimized cells last.
    /// Preserves relative order within each group. Called after any
    /// operation that could have left minimized cells interleaved.
    fn normalize_cell_order(&mut self) {
        let focused_cell_idx = if let FocusId::Cell(i) = self.focus { Some(i) } else { None };
        // Build new order with a stable partition.
        let n = self.cells.len();
        let mut new_order: Vec<usize> = (0..n).filter(|&i| !self.cells[i].minimized).collect();
        new_order.extend((0..n).filter(|&i| self.cells[i].minimized));
        if new_order.iter().enumerate().all(|(new_i, &old_i)| new_i == old_i) {
            return; // already sorted
        }
        // Take out the cells and reinsert in the new order. Using Option
        // sentinels so we can move without cloning `Cell`.
        let mut slots: Vec<Option<Cell>> = self.cells.drain(..).map(Some).collect();
        for old_i in new_order.iter().copied() {
            // new_order is a permutation of 0..n so each slot is taken
            // exactly once — but if that invariant ever breaks (future
            // edit), silently skip rather than panic mid-relayout.
            if let Some(cell) = slots.get_mut(old_i).and_then(Option::take) {
                self.cells.push(cell);
            }
        }
        // Remap focus.
        if let Some(old_i) = focused_cell_idx {
            let new_i = new_order.iter().position(|&oi| oi == old_i).unwrap_or(0);
            self.focus = FocusId::Cell(new_i);
            self.last_cell_focus = new_i;
        }
    }

    /// Cycle the active session inside the focused cell. Forward
    /// (`Tab`) steps to the next session; backward (`Shift+Tab`) steps
    /// to the previous. No-op when the focused cell has only one
    /// session or when focus isn't on a cell at all.
    pub fn cycle_active_session(&mut self, backward: bool) {
        let FocusId::Cell(i) = self.focus else { return; };
        let Some(cell) = self.cells.get_mut(i) else { return; };
        let n = cell.sessions.len();
        if n <= 1 { return; }
        cell.active = if backward {
            (cell.active + n - 1) % n
        } else {
            (cell.active + 1) % n
        };
    }

    /// Toggle the Explorer sidebar's visibility. Bound to `e` from any
    /// `ExplorerMode`. When hiding: if the Explorer was focused, focus
    /// drops to the most recently focused cell so the user isn't
    /// stranded on an invisible panel. Refuses to hide when no cells
    /// exist — we'd have nothing to focus on.
    pub fn toggle_explorer_hidden(&mut self) {
        if !self.explorer_hidden && self.cells.is_empty() {
            self.status.push_auto("can't hide explorer: no cells to focus".into());
            return;
        }
        self.explorer_hidden = !self.explorer_hidden;
        if self.explorer_hidden {
            // Hidden and fullscreen are mutually exclusive — clear one
            // when the other wins so state can't drift.
            self.explorer_fullscreen = false;
            if self.focus == FocusId::Explorer {
                let idx = self.last_cell_focus.min(self.cells.len() - 1);
                self.set_focus(FocusId::Cell(idx));
            }
        }
        self.status.push_auto(if self.explorer_hidden {
            "explorer hidden (space+0 to show)".into()
        } else {
            "explorer shown".into()
        });
    }

    /// Expand/collapse the explorer to/from full-terminal width. Cells
    /// aren't rendered while fullscreen — their column collapses to 0.
    /// Bound to `F` on the focused explorer. Reveals + focuses the
    /// explorer if it was hidden, so the toggle is always meaningful.
    pub fn toggle_explorer_fullscreen(&mut self) {
        self.explorer_fullscreen = !self.explorer_fullscreen;
        if self.explorer_fullscreen {
            self.explorer_hidden = false;
            self.set_focus(FocusId::Explorer);
            self.status.push_auto("explorer fullscreen (F to exit)".into());
        } else {
            self.status.push_auto("explorer windowed".into());
        }
    }

    /// PageUp/PageDown in the Explorer — switch to the previous/next
    /// project. Works in every `ExplorerMode` (git sub-modes included)
    /// so quick project-hopping doesn't require first collapsing.
    pub fn project_jump(&mut self, forward: bool) {
        let n = self.projects.projects.len();
        if n == 0 { return; }
        let cur = self.projects.active;
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        if next != cur {
            self.project_switch_idx_keep_focus(next);
        }
    }

    /// Cycle through every repo discovered across every project. Used
    /// by PgUp/PgDn while inside a git sub-mode — the REPOSITORIES
    /// list shows all of them, so this matches. When the next repo
    /// lives in a different project, switches that project active as
    /// a side-effect, then points `app.git.active` at the right repo.
    pub fn repo_jump_global(&mut self, forward: bool) {
        // Flat list of (proj_idx, repo_idx) — the same order the
        // REPOSITORIES header renders in.
        let flat: Vec<(usize, usize)> = self.projects.projects.iter().enumerate()
            .flat_map(|(pi, p)| (0..p.repos.len()).map(move |ri| (pi, ri)))
            .collect();
        if flat.is_empty() { return; }

        let cur = self.projects.active;
        let cur_repo = self.git.active;
        let cur_pos = flat.iter()
            .position(|&(pi, ri)| pi == cur && ri == cur_repo)
            .unwrap_or(0);
        let next_pos = if forward {
            (cur_pos + 1) % flat.len()
        } else {
            (cur_pos + flat.len() - 1) % flat.len()
        };
        if next_pos == cur_pos { return; }
        let (next_proj, next_repo) = flat[next_pos];
        if next_proj != cur {
            // Crosses a project boundary — do a full project switch so
            // the file tree, footer, and parked-cells machinery all
            // catch up. `project_switch_idx_keep_focus` rebuilds
            // `app.git` via `refresh_git`, so we set `active` after.
            self.project_switch_idx_keep_focus(next_proj);
        }
        if next_repo < self.git.repos.len() {
            self.git.active = next_repo;
            self.clamp_git_cursors();
        }
    }

    // ── mode entry points ────────────────────────────────────────────────

    pub fn enter_insert(&mut self) {
        let can_insert = match self.focus {
            FocusId::Cell(_) => true,  // any cell can go to Insert (editor goes to raw insert, pty stays there)
            _ => false,
        };
        if can_insert {
            // PTY cells: snap the virtual cursor to the child's real
            // cursor on the way IN to Insert, so the cursor's identity
            // is anchored to a current live-area row. Without this, an
            // earlier Normal-mode position can stay pinned to its abs
            // — and on the way back out (Esc → Normal) any
            // staleness in `rows_emitted` would show up as the cursor
            // jumping into history. Doing it here closes the gap before
            // it has a chance to grow.
            if let Some(cell) = self.focused_cell_mut() {
                if let Some(pty) = cell.active_session_mut().as_pty_mut() {
                    pty.sync_vcursor_to_real();
                    pty.clear_visual();
                    pty.pending_g = false;
                }
            }
            self.mode = Mode::Insert;
        }
    }

    pub fn enter_command(&mut self) {
        self.mode = Mode::Command { buffer: String::new() };
        self.status.clear();
        self.pending_jump = false;
        self.completion = None;
    }

    /// Enter command mode with `prefix` already typed into the buffer,
    /// cursor sitting at the end. Used for one-key shortcuts like the
    /// explorer's `a` (→ `:proj add `) and `o` (→ `:e `) so the user
    /// just types the path and hits Enter.
    pub fn enter_command_with(&mut self, prefix: &str) {
        self.mode = Mode::Command { buffer: prefix.to_string() };
        self.status.clear();
        self.pending_jump = false;
        self.completion = None;
        self.refresh_completion_preview();
    }

    pub fn enter_normal(&mut self) {
        // If a conflict hunk is mid-edit, Esc commits it as a Custom
        // resolution. The user entered Insert via `e` on a hunk — the
        // symmetric exit should keep the work, not discard it.
        // If an editor is mid-insert (with an insert entry captured for
        // `.` replay), flush its captured buffer into `last_change` so
        // the next `.` can replay the whole "entry + typed text" sequence.
        // PTY cells: sync the virtual cursor to the child's real cursor
        // (so Normal-mode motions start where the user was typing) and
        // clear any stale Visual anchor left over from a prior session.
        let from_insert = matches!(self.mode, Mode::Insert);
        if let Some(cell) = self.focused_cell_mut() {
            match cell.active_session_mut() {
                Session::Conflict(cv) => {
                    if cv.is_editing() {
                        cv.commit_edit();
                    }
                }
                Session::Edit(ed) => {
                    ed.end_insert();
                    // Visual→Normal: cancel the textarea selection so
                    // the highlight clears on the same Esc that flipped
                    // the mode. Without this the user would have to
                    // press Esc again — first Esc lands here and only
                    // changes mode; second Esc reaches handle_normal
                    // which clears the selection.
                    ed.textarea.cancel_selection();
                }
                Session::Hex(h) => {
                    h.cancel_selection();
                    h.nibble_high = true;
                }
                Session::Claude(pty) | Session::Shell(pty) => {
                    pty.clear_visual();
                    pty.pending_g = false;
                    // Only re-sync the virtual cursor on Insert→Normal —
                    // that's the path where the user was typing and the
                    // child cursor has presumably moved. On Visual→Normal
                    // the user explicitly placed the vcursor, so snapping
                    // it back to the live cursor would make it jump to
                    // the bottom of the screen on every Esc. Insert mode
                    // already syncs on the way IN, so the value here is
                    // already up to date for that path too.
                    if from_insert {
                        pty.sync_vcursor_to_real();
                    }
                }
                _ => {}
            }
        }
        self.mode = Mode::Normal;
    }

    pub fn command_push(&mut self, c: char) {
        if let Mode::Command { buffer } = &mut self.mode {
            buffer.push(c);
        }
        self.refresh_completion_preview();
    }

    // ── sudo / password prompt ──────────────────────────────────────────
    //
    // `:sudo w` (and aliases `:w!`, `:w!q`, `:x!`) flip the mode to
    // `Password`. Keystrokes from main.rs's `handle_password` land on
    // `password_push`/`password_backspace`; Enter calls
    // `password_submit` which spawns `sudo -S tee <path>` with the
    // password on stdin and the buffer as the file content. Esc cancels
    // without spawning anything.

    pub fn begin_sudo(&mut self, action: SudoAction) {
        let Some(ed) = self.focused_editor_mut() else {
            self.status.push_auto("sudo: needs editor focus".into());
            return;
        };
        if ed.path.is_none() {
            self.status.push_auto("sudo: buffer has no file — try :w <path> first".into());
            return;
        }
        self.mode = Mode::Password { buffer: String::new(), action };
        self.status.push_auto("sudo: enter password (Enter to submit, Esc to cancel)".into());
        self.completion = None;
    }

    pub fn password_push(&mut self, c: char) {
        if let Mode::Password { buffer, .. } = &mut self.mode {
            buffer.push(c);
        }
    }

    pub fn password_backspace(&mut self) {
        if let Mode::Password { buffer, .. } = &mut self.mode {
            buffer.pop();
        }
    }

    pub fn password_cancel(&mut self) {
        if matches!(self.mode, Mode::Password { .. }) {
            self.mode = self.natural_mode_for_focus();
            self.status.push_auto("sudo cancelled".into());
        }
    }

    pub fn password_submit(&mut self) {
        // Take the mode out so we own `buffer` and `action` while we
        // mutate the editor below. Stays in Normal on the failure path
        // — re-running the command is the user's next step.
        let (password, action) = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Password { buffer, action } => (buffer, action),
            other => { self.mode = other; return; }
        };
        let Some(ed) = self.focused_editor_mut() else {
            self.status.push_auto("sudo: no focused editor".into());
            return;
        };
        let Some(path) = ed.path.clone() else {
            self.status.push_auto("sudo: no file".into());
            return;
        };
        let mut content = ed.textarea.lines().join("\n");
        if !content.ends_with('\n') {
            content.push('\n');
        }
        let result = exec_sudo_write(&path, content.as_bytes(), &password);
        // Zeroize the copy we hold so the password doesn't sit around in
        // the heap any longer than necessary. (The kernel still has
        // copies via the pipe; this is best-effort.)
        drop(password);
        match result {
            Ok(()) => {
                if let Some(ed) = self.focused_editor_mut() {
                    // `sudo tee` has overwritten the file; reload to
                    // resync saved_hash / mtime / size and clear dirty.
                    let _ = ed.reload_from_disk();
                }
                self.refresh_git();
                self.status.push_auto(format!("sudo wrote {}", path.display()));
                match action {
                    SudoAction::Write        => {}
                    SudoAction::WriteClose   => self.cmd_close(false),
                    SudoAction::WriteQuitApp => { self.should_quit = true; }
                }
            }
            Err(e) => {
                self.status.push_auto(format!("sudo: {e}"));
            }
        }
    }

    pub fn command_backspace(&mut self) {
        let exit = matches!(&self.mode, Mode::Command { buffer } if buffer.is_empty());
        if exit {
            self.mode = self.natural_mode_for_focus();
            self.completion = None;
            return;
        }
        if let Mode::Command { buffer } = &mut self.mode {
            buffer.pop();
        }
        self.refresh_completion_preview();
    }

    /// Recompute completion options for the current buffer without
    /// touching the buffer itself. Populates `self.completion` with
    /// `preview = true` so the UI can render a live hint line beneath
    /// the command; the first Tab promotes it into a real cycle.
    fn refresh_completion_preview(&mut self) {
        let buffer = match &self.mode {
            Mode::Command { buffer } => buffer.clone(),
            _ => { self.completion = None; return; }
        };
        if buffer.is_empty() {
            self.completion = None;
            return;
        }
        let comp = self.compute_completion(&buffer);
        if comp.options.is_empty() {
            self.completion = None;
            return;
        }
        self.completion = Some(CompletionState {
            start:   comp.start,
            options: comp.options,
            sel:     0,
            preview: true,
        });
    }

    fn compute_completion(&self, buffer: &str) -> crate::completion::Completion {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let changed: Vec<String> = self.git.change_rows()
            .into_iter()
            .map(|r| r.path)
            .collect();
        let project_names: Vec<String> = self.projects.projects.iter()
            .map(|p| p.name.clone())
            .collect();
        let ctx = crate::completion::CompletionCtx {
            cwd: &cwd,
            projects: &project_names,
            branches: &self.git.branches,
            changed_paths: &changed,
        };
        crate::completion::complete(buffer, &ctx)
    }

    pub fn command_submit(&mut self) {
        let cmd = match &self.mode {
            Mode::Command { buffer } => buffer.clone(),
            _ => return,
        };
        self.mode = self.natural_mode_for_focus();
        self.completion = None;
        self.run_command(cmd.trim());
    }

    pub fn command_cancel(&mut self) {
        self.mode = self.natural_mode_for_focus();
        self.completion = None;
    }

    /// `Tab` in command mode. Either advances the running completion
    /// cycle or, if no cycle is active, computes one. `backward=true`
    /// is Shift-Tab. No-op outside Command mode.
    pub fn command_complete(&mut self, backward: bool) {
        if !matches!(self.mode, Mode::Command { .. }) { return; }

        // Already have options (either a live preview from typing or a
        // running cycle from a prior Tab). Promote/step and splice.
        if let Some(cs) = self.completion.as_mut() {
            if cs.options.is_empty() { self.completion = None; return; }
            let n = cs.options.len();
            // Preview state: first Tab commits the currently-previewed
            // option (sel already points at it — usually 0) without
            // advancing. Subsequent Tabs cycle.
            if !cs.preview {
                cs.sel = if backward {
                    (cs.sel + n - 1) % n
                } else {
                    (cs.sel + 1) % n
                };
            }
            cs.preview = false;
            let new_tail = cs.options[cs.sel].clone();
            let start = cs.start;
            if let Mode::Command { buffer } = &mut self.mode {
                buffer.truncate(start);
                buffer.push_str(&new_tail);
            }
            return;
        }

        // No preview available (e.g. buffer empty of matches). Try once
        // more in case the caller wants a fresh compute — matches prior
        // behaviour of surfacing "no completions".
        let buffer = match &self.mode {
            Mode::Command { buffer } => buffer.clone(),
            _ => return,
        };
        let comp = self.compute_completion(&buffer);
        if comp.options.is_empty() {
            self.status.push_auto("no completions".into());
            return;
        }
        let start = comp.start;
        let first = comp.options[0].clone();
        if let Mode::Command { buffer } = &mut self.mode {
            buffer.truncate(start);
            buffer.push_str(&first);
        }
        self.completion = Some(CompletionState {
            start,
            options: comp.options,
            sel: 0,
            preview: false,
        });
    }


    // ── commands ─────────────────────────────────────────────────────────

    fn run_command(&mut self, cmd: &str) {
        if cmd.is_empty() {
            return;
        }
        // Sudo shortcuts — any of these open the password prompt before
        // any disk write happens. Keep this before cell-target parsing so
        // `:w!` doesn't get eaten as "force-write focused cell".
        //
        //   :sudo w   :w!         → sudo write
        //   :sudo wq  :w!q  :x!   → sudo write + close focused cell
        //   :sudo x               → alias for :sudo wq
        //   :sudo wQ              → sudo write + quit app
        //
        // Earlier `:w!` meant "force over external conflict"; sudo
        // supersedes it (the kernel writes regardless of local-editor
        // conflict state). Users who want the old behaviour can `:e!` to
        // reload then `:w`, or `:conflict` to merge.
        if let Some(action) = parse_sudo_command(cmd) {
            self.begin_sudo(action);
            return;
        }
        // Cell-target commands: a trailing 1-based index or `*`
        // wildcard operates on that cell (or every cell) instead of
        // the focused one. Handle them up-front so the match below
        // keeps its simple bare-command arms.
        if let Some((base, target)) = parse_cell_target(cmd) {
            match base {
                "q" | "quit" | "close" | "bd" | "bdelete"
                                                  => { self.cmd_close_target(false, Some(target)); return; }
                "q!" | "quit!" | "close!" | "bd!" | "bdelete!"
                                                  => { self.cmd_close_target(true,  Some(target)); return; }
                "w"  | "write"                    => { self.cmd_write_target(false, Some(target)); return; }
                "w!" | "write!"                   => { self.cmd_write_target(true,  Some(target)); return; }
                "wq" | "x"                        => { self.cmd_write_quit_target(Some(target)); return; }
                "min" | "minimize"                => { self.cmd_minimize_target(Some(target)); return; }
                _ => {} // fall through — not a cell-target command
            }
        }
        // Numeric-only command `:42` — jump to line 42 in the focused
        // editor. Handled before the match so the parse doesn't have to
        // try every integer combination.
        if cmd.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = cmd.parse::<usize>() {
                self.cmd_goto_line(n);
                return;
            }
        }
        // `:%s/old/new/` or `:%s/old/new/g` — whole-file substitute.
        if cmd.starts_with("%s/") || cmd.starts_with("%s ") {
            self.cmd_substitute(cmd);
            return;
        }
        // Leading `/` from the status-line search prompt reuses the
        // command buffer to keep a single UI. The submit path forwards
        // it here instead of treating it as an ex command.
        if let Some(pat) = cmd.strip_prefix('/') {
            self.cmd_search(pat, false);
            return;
        }
        if let Some(pat) = cmd.strip_prefix('?') {
            self.cmd_search(pat, true);
            return;
        }
        match cmd {
            // `:q` / `:quit` close the focused cell, not the app. The
            // app itself quits naturally when the last cell is gone.
            // `:Q` quits the whole app; refuses if any editor is
            // dirty. `:Q!` force-quits regardless.
            "q" | "quit"                        => self.cmd_close(false),
            "q!" | "quit!"                      => self.cmd_close(true),
            "Q" | "Quit"                        => self.cmd_quit_app(false),
            "Q!" | "Quit!"                      => self.cmd_quit_app(true),
            "w" | "write"                       => self.cmd_write(false),
            "w!" | "write!"                     => self.cmd_write(true),
            _ if cmd.starts_with("w ") || cmd.starts_with("write ")
                                         || cmd.starts_with("w! ") || cmd.starts_with("write! ") => {
                let force = cmd.starts_with("w! ") || cmd.starts_with("write! ");
                let path = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_write_as(path, force);
            }
            "wq" | "x"                          => self.cmd_write_quit(),
            "wQ"                                => self.cmd_write_quit_app(false),
            "wQ!"                               => self.cmd_write_quit_app(true),
            "e!" | "edit!"                      => self.cmd_edit_force(false),
            "edit" | "e"                        => self.cmd_edit_toggle(false),
            "hex"                               => self.cmd_hex_toggle(false),
            "hex!"                              => self.cmd_hex_toggle(true),
            "conflict" | "resolve"              => self.cmd_conflict(),
            "close" | "bd" | "bdelete"          => self.cmd_close(false),
            "close!" | "bd!" | "bdelete!"       => self.cmd_close(true),
            "split"                             => self.cmd_split(),
            "help" | "h"                        => self.cmd_help(),
            "nohl" | "nohlsearch" | "noh"       => self.cmd_clear_search(),
            "set wrap"                          => self.cmd_set_wrap(true),
            "set nowrap"                        => self.cmd_set_wrap(false),
            "set list"                          => self.cmd_set_list(true),
            "set nolist"                        => self.cmd_set_list(false),
            "set autopair"                      => self.cmd_set_autopair(true),
            "set noautopair"                    => self.cmd_set_autopair(false),
            "set autoindent"                    => self.cmd_set_autoindent(true),
            "set noautoindent"                  => self.cmd_set_autoindent(false),
            "set expandtab"                     => self.cmd_set_expandtab(true),
            "set noexpandtab"                   => self.cmd_set_expandtab(false),
            "set completion"                    => self.cmd_set_completion(true),
            "set nocompletion"                  => self.cmd_set_completion(false),
            "layout"                           => self.status.push_auto("usage: :layout master".into()),
            "swap"                              => self.status.push_auto("usage: :swap <1..9> | 0 (minimize)".into()),
            "min" | "minimize"                  => self.minimize_focused(),
            "restore"                           => self.status.push_auto("usage: :restore <1..9>".into()),
            _ if cmd.starts_with("swap ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                match rest.parse::<u32>() {
                    Ok(n) => self.swap_focused_with_digit(n, false),
                    Err(_) => self.status.push_auto(format!("swap: not a number: {rest}")),
                }
            }
            _ if cmd.starts_with("restore ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                match rest.parse::<u32>() {
                    Ok(n) => self.cmd_restore(n),
                    Err(_) => self.status.push_auto(format!("restore: not a number: {rest}")),
                }
            }
            _ if cmd.starts_with("e ") || cmd.starts_with("edit ") => {
                let path = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_edit(path);
            }
            _ if cmd.starts_with("hex ") => {
                let path = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_hex_path(path);
            }
            _ if cmd.starts_with("new ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_new(rest);
            }
            // Shorthands for `:new claude` / `:new shell [exec]`. `:s`
            // and `:c` save a keystroke for the most common spawns;
            // `:shell zsh` / `:shell bash -i` override the default shell
            // for one cell.
            "c" | "claude"                      => self.cmd_new("claude"),
            _ if cmd.starts_with("c ") || cmd.starts_with("claude ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                // Claude doesn't take arbitrary args here — everything
                // past `:claude` is ignored for now, matching `:new
                // claude`'s existing shape. Kept as a future-extension
                // hook.
                let _ = rest;
                self.cmd_new("claude");
            }
            "s" | "shell"                       => self.cmd_new("shell"),
            _ if cmd.starts_with("s ") || cmd.starts_with("shell ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                if rest.is_empty() {
                    self.cmd_new("shell");
                } else {
                    self.cmd_new(&format!("shell {rest}"));
                }
            }
            "ex" | "execute" => self.status.push_auto("usage: :ex <cmd>".into()),
            _ if cmd.starts_with("ex ") || cmd.starts_with("execute ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_execute(rest);
            }
            _ if cmd.starts_with("tab ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_tab(rest);
            }
            _ if cmd.starts_with("layout ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_layout(rest);
            }
            "git" => self.cmd_git(""),
            _ if cmd.starts_with("git ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_git(rest);
            }
            "proj" | "project" | "projects" => self.cmd_proj(""),
            _ if cmd.starts_with("proj ") || cmd.starts_with("project ") => {
                let rest = cmd.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                self.cmd_proj(rest);
            }
            _ => self.status.push_auto(format!("unknown: :{cmd}")),
        }
    }

    fn cmd_write(&mut self, force: bool) {
        // Hex cell focus: route the save through HexView. Same conflict
        // guard as the Edit branch below — refuse a plain `:w` if the
        // file changed on disk and the user hasn't acked it with `:w!`.
        if self.focused_session_is_hex() {
            if !force {
                if let Some(c) = self.focused_hex_mut().and_then(|h| h.external_conflict) {
                    use crate::hex::ExternalConflict as C;
                    let hint = match c {
                        C::ModifiedOnDisk => "disk changed — :w! to overwrite, :e! to reload",
                        C::Deleted        => "disk file gone — :w! to recreate",
                    };
                    self.status.push_auto(hint.into());
                    return;
                }
            }
            let out = self.focused_hex_mut().map(|h| (h.save(), h.file_name().to_string()));
            match out {
                Some((Ok(()), name)) => {
                    self.refresh_git();
                    let prefix = if force { "forced " } else { "" };
                    self.status.push_auto(format!("{prefix}wrote {name}"));
                }
                Some((Err(e), _)) => self.status.push_auto(format!("write failed: {e}")),
                None              => self.status.push_auto(":w needs editor focus".into()),
            }
            return;
        }

        // If a Conflict cell is focused, `:w` writes the resolved
        // output to disk. Partially-resolved files write with the
        // remaining hunks defaulting to "keep ours" — we warn but
        // don't block, so the user can iterate.
        if let Some(cell) = self.focused_cell_mut() {
            if let Some(cv) = cell.active_session_mut().as_conflict_mut() {
                let unresolved = cv.unresolved_count();
                let path_clone = cv.path.clone();
                match cv.save() {
                    Ok(()) => {
                        let msg = if unresolved == 0 {
                            format!("resolved {}", path_clone.display())
                        } else {
                            format!("wrote {} ({unresolved} hunks defaulted to ours)", path_clone.display())
                        };
                        self.refresh_git();
                        // Nudge any editor on the same file so it picks
                        // up the resolved content instead of flagging a
                        // conflict from the very write we just made.
                        // Remember the first matching editor cell — we
                        // return focus there once the conflict cell is
                        // gone, so the user lands back where they came
                        // from.
                        let mut editor_cell_idx: Option<usize> = None;
                        for (ci, c) in self.cells.iter_mut().enumerate() {
                            for s in c.sessions.iter_mut() {
                                if let Session::Edit(ed) = s {
                                    if ed.path.as_deref() == Some(path_clone.as_path()) {
                                        let _ = ed.reload_from_disk();
                                        if editor_cell_idx.is_none() {
                                            editor_cell_idx = Some(ci);
                                        }
                                    }
                                }
                            }
                        }
                        self.status.push_auto(msg);
                        // Close the conflict cell and hand focus back to
                        // the originating editor cell (if any). Resolving
                        // a conflict is a one-way trip — keeping the
                        // conflict view open after save just forces the
                        // user to `:q` it themselves.
                        if let FocusId::Cell(conflict_idx) = self.focus {
                            self.cmd_close_target(true, Some(CellTarget::Idx(conflict_idx)));
                            if let Some(ei) = editor_cell_idx {
                                // The close shifted indices above it
                                // down by one — re-align before focusing.
                                let target = if ei > conflict_idx { ei - 1 } else { ei };
                                if target < self.cells.len() {
                                    self.set_focus(FocusId::Cell(target));
                                }
                            }
                        }
                    }
                    Err(e) => self.status.push_auto(format!("write failed: {e}")),
                }
                return;
            }
        }

        // Conflict guard: if disk changed out from under us, a plain
        // `:w` would blow away those external edits. Refuse and point
        // the user at `:w!` (overwrite) or `:e!` (discard local edits).
        if !force {
            if let Some(conflict) = self.focused_editor_mut().and_then(|e| e.external_conflict.clone()) {
                use crate::editor::ExternalConflict as C;
                let hint = match conflict {
                    C::ModifiedOnDisk => "disk changed — :conflict to merge, :w! to overwrite, :e! to reload",
                    C::Deleted        => "disk file gone — :w! to recreate",
                };
                self.status.push_auto(hint.into());
                return;
            }
        }
        let out = self.focused_editor_mut().map(|ed| {
            let r = ed.save();
            (r, ed.file_name().to_string())
        });
        match out {
            Some((Ok(()),  name)) => {
                self.refresh_git();
                let prefix = if force { "forced " } else { "" };
                self.status.push_auto(format!("{prefix}wrote {name}"));
            }
            Some((Err(e),  _)) => self.status.push_auto(format!("write failed: {e}")),
            None               => self.status.push_auto(":w needs editor focus".into()),
        }
    }

    /// `:conflict` — open a 3-way merge view for the focused editor.
    /// Works in two modes:
    ///   * Editor has `ExternalConflict::ModifiedOnDisk` → build a
    ///     ConflictView from buffer (ours) + disk (theirs) + saved
    ///     snapshot (base).
    ///   * File on disk contains `<<<<<<< / ======= / >>>>>>>` markers
    ///     → parse them into a ConflictView regardless of buffer state.
    fn cmd_conflict(&mut self) {
        use crate::conflict::ConflictView;
        use crate::editor::ExternalConflict as EC;

        // Snapshot what we need from the focused editor first, then
        // drop the borrow so we can mutate cells to inject a new one.
        let prep: Option<(PathBuf, Vec<String>, Vec<String>, bool)> = self.focused_editor_mut().and_then(|ed| {
            let path = ed.path.clone()?;
            let ours = ed.textarea.lines().to_vec();
            let base = ed.saved_lines().to_vec();
            let has_ext_conflict = ed.external_conflict == Some(EC::ModifiedOnDisk);
            Some((path, ours, base, has_ext_conflict))
        });

        let Some((path, ours, base, has_ext_conflict)) = prep else {
            self.status.push_auto(":conflict needs a focused editor with a file".into());
            return;
        };

        let view = if has_ext_conflict {
            let disk = std::fs::read_to_string(&path)
                .map(|s| s.lines().map(String::from).collect::<Vec<_>>())
                .unwrap_or_default();
            ConflictView::for_external(path, &ours, &base, &disk)
        } else {
            // No external conflict — maybe the file has git markers.
            match ConflictView::for_git_file(&path) {
                Ok(v) if v.total_hunks() > 0 => v,
                Ok(_)  => {
                    self.status.push_auto("no conflict to resolve".into());
                    return;
                }
                Err(e) => {
                    self.status.push_auto(format!("conflict open failed: {e}"));
                    return;
                }
            }
        };

        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells — close one first"));
            return;
        }
        self.insert_cell_at_top(Cell::with_session(Session::Conflict(view)));
        self.status.push_auto("opened conflict view — o/t/b to resolve, :w to save".into());
        self.on_sessions_changed();
    }

    /// `:42` — jump to line 42. Clamps into range.
    fn cmd_goto_line(&mut self, n: usize) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.goto_line(n);
        } else {
            self.status.push_auto(":{N} needs editor focus".into());
        }
    }

    /// `:%s/old/new/g` — whole-file literal substitution. Flags are
    /// optional; we accept `g` (every match on every line, our only
    /// mode) and silently ignore others. The separator is whatever
    /// follows `%s` — `/` is conventional but anything works, so
    /// `%s,old,new,` is fine too.
    fn cmd_substitute(&mut self, cmd: &str) {
        // Strip leading "%s" then grab the separator.
        let rest = cmd.strip_prefix("%s").unwrap_or(cmd);
        let mut chars = rest.chars();
        let sep = match chars.next() {
            Some(c) if !c.is_alphanumeric() => c,
            _ => {
                self.status.push_auto("usage: :%s/old/new/g".into());
                return;
            }
        };
        // Split by the separator into at most 3 segments + optional flags.
        let rest: String = chars.collect();
        let parts: Vec<&str> = rest.splitn(3, sep).collect();
        let (old, new) = match parts.as_slice() {
            [o, n]        => (*o, *n),
            [o, n, _flags] => (*o, *n),
            _ => {
                self.status.push_auto("usage: :%s/old/new/g".into());
                return;
            }
        };
        let out = self.focused_editor_mut().map(|ed| (ed.substitute_all(old, new), ed.file_name().to_string()));
        match out {
            Some((0, _))       => self.status.push_auto(format!("no match: {old}")),
            Some((n, name))    => self.status.push_auto(format!("{n} substitutions in {name}")),
            None               => self.status.push_auto(":%s needs editor focus".into()),
        }
    }

    /// `/pattern` — forward search in the focused editor. Called by
    /// `run_command` when the command buffer starts with `/` or `?`.
    fn cmd_search(&mut self, pattern: &str, backward: bool) {
        // Hex cell focus — search for the literal ASCII bytes of the
        // pattern. Matches keep the cursor at the byte offset of the
        // first byte of the match.
        if self.focused_session_is_hex() {
            if pattern.is_empty() {
                let ok = self.focused_hex_mut().map(|h| h.search_next(backward));
                if let Some(false) = ok {
                    self.status.push_auto("no previous search".into());
                }
                return;
            }
            let ok = self.focused_hex_mut().map(|h| h.set_search_and_find(pattern));
            match ok {
                Some(true)  => self.status.push_auto(format!("/{pattern}")),
                Some(false) => self.status.push_auto(format!("no match: {pattern}")),
                None        => self.status.push_auto("search needs editor focus".into()),
            }
            return;
        }
        if pattern.is_empty() {
            // Empty pattern: repeat the last search in the chosen direction.
            let ok = self.focused_editor_mut().map(|ed| ed.search_next(backward));
            if let Some(false) = ok {
                self.status.push_auto("no previous search".into());
            }
            return;
        }
        let ok = self.focused_editor_mut().map(|ed| ed.set_search_and_find(pattern));
        match ok {
            Some(true)  => self.status.push_auto(format!("/{pattern}")),
            Some(false) => self.status.push_auto(format!("no match: {pattern}")),
            None        => self.status.push_auto("search needs editor focus".into()),
        }
    }

    /// `:nohl` — clear the current search highlight. Just resets the
    /// pattern to an empty regex, which matches nothing.
    fn cmd_clear_search(&mut self) {
        if let Some(ed) = self.focused_editor_mut() {
            let _ = ed.textarea.set_search_pattern("");
        }
    }

    /// `:set wrap` / `:set nowrap` — toggle soft-wrap on the focused
    /// editor. Matches vim's semantics: pure display concern, buffer
    /// unchanged. `nowrap` falls back to tui-textarea's horizontal
    /// scroll.
    fn cmd_set_wrap(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.wrap = on;
            ed.scroll_top.set(0);
            self.status.push_auto(if on { "wrap on".into() } else { "wrap off".into() });
        } else {
            self.status.push_auto(":set wrap needs editor focus".into());
        }
    }

    /// `:set list` / `:set nolist` — toggle listchars rendering.
    fn cmd_set_list(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.list_mode = on;
            self.status.push_auto(if on { "list on".into() } else { "list off".into() });
        } else {
            self.status.push_auto(":set list needs editor focus".into());
        }
    }

    /// `:set autopair` / `:set noautopair` — toggle bracket/quote
    /// autoclosing on the focused editor.
    fn cmd_set_autopair(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.autopair = on;
            self.status.push_auto(if on { "autopair on".into() } else { "autopair off".into() });
        } else {
            self.status.push_auto(":set autopair needs editor focus".into());
        }
    }

    /// `:set autoindent` / `:set noautoindent` — toggle smart Enter /
    /// `o` / `O` indent inheritance on the focused editor.
    fn cmd_set_autoindent(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.autoindent = on;
            self.status.push_auto(if on { "autoindent on".into() } else { "autoindent off".into() });
        } else {
            self.status.push_auto(":set autoindent needs editor focus".into());
        }
    }

    /// `:set expandtab` / `:set noexpandtab` — toggle whether Tab in
    /// insert mode inserts spaces or a literal tab.
    fn cmd_set_expandtab(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.expandtab = on;
            self.status.push_auto(if on { "expandtab on".into() } else { "expandtab off".into() });
        } else {
            self.status.push_auto(":set expandtab needs editor focus".into());
        }
    }

    /// `:set completion` / `:set nocompletion` — toggle the
    /// buffer-word completion popup on the focused editor. Disabling
    /// also clears any active popup.
    fn cmd_set_completion(&mut self, on: bool) {
        if let Some(ed) = self.focused_editor_mut() {
            ed.completion_enabled = on;
            if !on { ed.completion = None; }
            self.status.push_auto(if on { "completion on".into() } else { "completion off".into() });
        } else {
            self.status.push_auto(":set completion needs editor focus".into());
        }
    }

    /// `:help` — open a read-only editor cell prefilled with the ace
    /// reference card. Re-invoking focuses the existing help cell
    /// instead of stacking more copies.
    fn cmd_help(&mut self) {
        // Already open? Focus it.
        for (i, cell) in self.cells.iter().enumerate() {
            if let Session::Edit(ed) = cell.active_session() {
                if ed.read_only && ed.path.as_ref().and_then(|p| p.to_str()) == Some("[help]") {
                    self.set_focus(FocusId::Cell(i));
                    return;
                }
            }
        }
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells — close one first"));
            return;
        }
        let ed = Editor::read_only_from(HELP_TEXT, "[help]");
        self.insert_cell_at_top(Cell::with_session(Session::Edit(ed)));
        self.status.push_auto("help — :q to close".into());
    }

    /// `:e!` — discard local buffer and reload the file from disk.
    /// Resolves a `ModifiedOnDisk` conflict in favour of disk. When the
    /// focused cell is a Hex cell, this is also the lossy escape hatch
    /// for switching back to text on a buffer with invalid UTF-8.
    fn cmd_edit_force(&mut self, _was_force_arg: bool) {
        // Hex focus: behave as `:edit!` toggle — convert to text via
        // lossy UTF-8 (replacement chars for invalid bytes).
        if self.focused_session_is_hex() {
            self.cmd_edit_toggle(true);
            return;
        }
        // Pull the editor's path up front so we can fall back to hex on
        // an invalid-UTF-8 reload without re-borrowing.
        let editor_path = self.focused_editor_mut().and_then(|e| e.path.clone());
        let out = self.focused_editor_mut().map(|ed| {
            let r = ed.reload_from_disk();
            (r, ed.file_name().to_string())
        });
        match out {
            Some((Ok(()),  name)) => self.status.push_auto(format!("reloaded {name}")),
            Some((Err(e),  name)) => {
                if let (Some(path), FocusId::Cell(idx)) = (editor_path, self.focus) {
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            let hv = HexView::from_bytes(Some(path.clone()), bytes, false, false);
                            let active = self.cells[idx].active;
                            self.cells[idx].sessions[active] = Session::Hex(hv);
                            self.status.push_auto(format!("{name}: binary file — opened in hex mode"));
                            return;
                        }
                        Err(e2) => {
                            self.status.push_auto(format!("reload failed: {e2}"));
                            return;
                        }
                    }
                }
                self.status.push_auto(format!("reload failed: {e}"));
            }
            None => self.status.push_auto(":e! needs editor focus".into()),
        }
    }

    /// `:edit` (no path) — when focused on a Hex cell, swap it back to
    /// an Edit cell using the buffer's bytes as UTF-8 text. If `lossy`
    /// (the `:edit!` form) is true, invalid UTF-8 is rendered with
    /// replacement chars; otherwise refuse with a hint.
    fn cmd_edit_toggle(&mut self, lossy: bool) {
        let FocusId::Cell(idx) = self.focus else {
            self.status.push_auto(":edit toggle needs a focused hex cell".into());
            return;
        };
        let Some(_hv) = self.cells[idx].active_session_mut().as_hex_mut() else {
            self.status.push_auto(":edit toggle needs a focused hex cell (try :hex first)".into());
            return;
        };
        let hv = self.cells[idx].active_session_mut().as_hex_mut().unwrap();
        let path = hv.path.clone();
        let dirty = hv.dirty;
        let is_new = hv.is_new;
        let text = if lossy {
            hv.to_text_lossy()
        } else {
            match hv.to_text() {
                Ok(s)  => s,
                Err(_) => {
                    self.status.push_auto("non-UTF-8 bytes — :edit! to discard (lossy)".into());
                    return;
                }
            }
        };
        let mut ed = Editor::empty();
        if let Some(p) = path.as_ref() {
            // Best-effort: load from disk to seed mtime/size; then
            // overwrite the textarea contents with the buffer-derived
            // text so unsaved hex edits are preserved.
            let _ = ed.load(p);
        }
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(String::from).collect()
        };
        ed.textarea = tui_textarea::TextArea::new(lines);
        crate::editor::style_textarea(&mut ed.textarea);
        ed.dirty = dirty;
        ed.is_new = is_new;
        ed.syntax = path.as_deref().and_then(crate::syntax::SyntaxHighlighter::new);
        ed.syntax_stale = true;
        let name = ed.file_name().to_string();
        let active = self.cells[idx].active;
        self.cells[idx].sessions[active] = Session::Edit(ed);
        self.mode = self.natural_mode_for_focus();
        self.status.push_auto(format!("{name}: switched to edit mode"));
    }

    /// `:hex` (no path) — toggle the focused Edit cell into Hex mode,
    /// carrying the dirty buffer across as raw UTF-8 bytes. `force` is
    /// reserved (no current use; keeps symmetry with `:edit!`).
    fn cmd_hex_toggle(&mut self, _force: bool) {
        let FocusId::Cell(idx) = self.focus else {
            self.status.push_auto(":hex needs a focused editor cell".into());
            return;
        };
        let Some(ed) = self.cells[idx].active_session_mut().as_editor_mut() else {
            // Already a Hex cell? Toggle back to Edit, like vim's
            // mode-toggle dual: `:hex` on a hex view drops you back.
            if self.focused_session_is_hex() {
                self.cmd_edit_toggle(false);
                return;
            }
            self.status.push_auto(":hex needs an editor cell".into());
            return;
        };
        if ed.read_only {
            self.status.push_auto(":hex on read-only buffer is not supported".into());
            return;
        }
        let path = ed.path.clone();
        let mut text = ed.textarea.lines().join("\n");
        if !text.is_empty() && !text.ends_with('\n') {
            // Match save_to behaviour so the byte view matches what `:w`
            // would produce from the same buffer.
            text.push('\n');
        }
        let dirty = ed.dirty;
        let is_new = ed.is_new;
        let bytes = text.into_bytes();
        let hv = HexView::from_bytes(path, bytes, dirty, is_new);
        let name = hv.file_name().to_string();
        let active = self.cells[idx].active;
        self.cells[idx].sessions[active] = Session::Hex(hv);
        self.mode = self.natural_mode_for_focus();
        self.status.push_auto(format!("{name}: switched to hex mode"));
    }

    /// `:hex <path>` — open a file directly in Hex mode. Replaces the
    /// focused Edit/Hex cell when one for this path is already open;
    /// otherwise creates a new cell.
    fn cmd_hex_path(&mut self, path: &str) {
        if path.is_empty() {
            self.status.push_auto("usage: :hex <path>".into());
            return;
        }
        let expanded = expand_tilde(path);
        let p = std::path::Path::new(&expanded);
        if p.is_dir() {
            self.status.push_auto(format!("{} is a directory", p.display()));
            return;
        }
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let mut hv = HexView::empty();
        if let Err(e) = hv.load(&abs) {
            self.status.push_auto(format!("open failed: {e}"));
            return;
        }
        let name = hv.file_name().to_string();
        // If a cell already has this path open (Edit or Hex), swap it in
        // place rather than spawning a duplicate.
        let existing = self.cells.iter().position(|c| {
            matches!(c.active_session(), Session::Edit(e) if e.path.as_deref() == Some(abs.as_path()))
                || matches!(c.active_session(), Session::Hex(h) if h.path.as_deref() == Some(abs.as_path()))
        });
        if let Some(idx) = existing {
            let active = self.cells[idx].active;
            self.cells[idx].sessions[active] = Session::Hex(hv);
            self.set_focus(FocusId::Cell(idx));
            self.status.push_auto(format!("opened {name} in hex mode"));
            return;
        }
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells"));
            return;
        }
        self.insert_cell_at_top(Cell::with_session(Session::Hex(hv)));
        self.status.push_auto(format!("opened {name} in hex mode"));
    }

    /// `:w <path>` — save the focused editor to `path`, adopting that
    /// path as the buffer's own (vim's dual-use of `:w`: save-in-place
    /// for named buffers, save-as for unnamed ones). `:w!` overwrites
    /// an existing file even if disk changed externally.
    fn cmd_write_as(&mut self, path: &str, _force: bool) {
        if path.is_empty() {
            self.status.push_auto("usage: :w <path>".into());
            return;
        }
        let expanded = expand_tilde(path);
        let p = std::path::Path::new(&expanded).to_path_buf();
        if p.is_dir() {
            self.status.push_auto(format!("{} is a directory", p.display()));
            return;
        }
        if self.focused_session_is_hex() {
            let out = self.focused_hex_mut().map(|h| (h.save_as(&p), h.file_name().to_string()));
            match out {
                Some((Ok(()), _))  => {
                    self.refresh_git();
                    self.status.push_auto(format!("wrote {path}"));
                }
                Some((Err(e), _)) => self.status.push_auto(format!("write failed: {e}")),
                None              => self.status.push_auto(":w <path> needs editor focus".into()),
            }
            return;
        }
        let out = self.focused_editor_mut().map(|ed| (ed.save_as(&p), ed.file_name().to_string()));
        match out {
            Some((Ok(()),  _)) => {
                self.refresh_git();
                self.status.push_auto(format!("wrote {path}"));
            }
            Some((Err(e), _)) => self.status.push_auto(format!("write failed: {e}")),
            None              => self.status.push_auto(":w <path> needs editor focus".into()),
        }
    }

    fn cmd_write_quit(&mut self) {
        let r = self.focused_editor_mut().map(|ed| ed.save());
        match r {
            Some(Ok(()))  => {
                self.refresh_git();
                // Close the focused cell — same split as `:q` (cell)
                // vs. `:Q` (app). The app quits naturally when the
                // last cell closes.
                self.cmd_close(false);
            }
            Some(Err(e))  => self.status.push_auto(format!("write failed: {e}")),
            None          => self.status.push_auto(":wq needs editor focus".into()),
        }
    }

    /// `:wQ` — write the focused editor, then quit the whole app.
    /// Mirrors the `:Q` (quit-app) vs. `:q` (close-cell) split at the
    /// "write then quit" level. `:wQ!` overrides an external-conflict
    /// block so the save always lands before we exit.
    fn cmd_write_quit_app(&mut self, force: bool) {
        if !force {
            if let Some(conflict) = self.focused_editor_mut().and_then(|e| e.external_conflict.clone()) {
                use crate::editor::ExternalConflict as C;
                let hint = match conflict {
                    C::ModifiedOnDisk => "disk changed — :wQ! to overwrite and quit",
                    C::Deleted        => "disk file gone — :wQ! to recreate and quit",
                };
                self.status.push_auto(hint.into());
                return;
            }
        }
        let r = self.focused_editor_mut().map(|ed| ed.save());
        match r {
            Some(Ok(()))  => {
                self.refresh_git();
                self.should_quit = true;
            }
            Some(Err(e))  => self.status.push_auto(format!("write failed: {e}")),
            None          => self.status.push_auto(":wQ needs editor focus".into()),
        }
    }

    /// `:w` variant that optionally targets a specific cell or every
    /// cell (`*`). `None` → forwards to the focused-editor
    /// `cmd_write` path. The targeted form bypasses the Conflict-view
    /// save and the external-conflict guard — it's meant for routine
    /// saves on cells you aren't currently looking at.
    pub fn cmd_write_target(&mut self, force: bool, target: Option<CellTarget>) {
        let idx = match target {
            None                       => { self.cmd_write(force); return; }
            Some(CellTarget::All)      => { self.cmd_write_all(force); return; }
            Some(CellTarget::Idx(i))   => i,
        };
        let Some(cell) = self.cells.get_mut(idx) else {
            self.status.push_auto(format!("no cell {}", idx + 1));
            return;
        };
        let Some(ed) = cell.active_session_mut().as_editor_mut() else {
            self.status.push_auto(format!("cell {} isn't an editor", idx + 1));
            return;
        };
        if !force {
            if let Some(conflict) = ed.external_conflict.clone() {
                use crate::editor::ExternalConflict as C;
                let hint = match conflict {
                    C::ModifiedOnDisk => "disk changed — :conflict to merge, :w! to overwrite, :e! to reload",
                    C::Deleted        => "disk file gone — :w! to recreate",
                };
                self.status.push_auto(hint.into());
                return;
            }
        }
        let name = ed.file_name().to_string();
        match ed.save() {
            Ok(())  => {
                self.refresh_git();
                let prefix = if force { "forced " } else { "" };
                self.status.push_auto(format!("{prefix}wrote {name} (cell {})", idx + 1));
            }
            Err(e) => self.status.push_auto(format!("write failed: {e}")),
        }
    }

    /// `:wq` variant that writes the targeted cell's editor then
    /// closes just that cell (not the whole app). With `*`, writes
    /// every editor cell and closes every cell.
    pub fn cmd_write_quit_target(&mut self, target: Option<CellTarget>) {
        match target {
            None => self.cmd_write_quit(),
            Some(CellTarget::All) => {
                self.cmd_write_all(false);
                if self.status.last_has("failed") {
                    return;
                }
                self.cmd_close_all(false);
            }
            Some(CellTarget::Idx(i)) => {
                self.cmd_write_target(false, Some(CellTarget::Idx(i)));
                if self.status.last_starts_with("write failed") {
                    return;
                }
                self.cmd_close_target(false, Some(CellTarget::Idx(i)));
            }
        }
    }

    /// `:Q` — quit the app. Refuses if any editor (in any cell, any
    /// tab) is dirty unless `force`.
    fn cmd_quit_app(&mut self, force: bool) {
        if !force {
            if let Some(name) = self.cells.iter().flat_map(|c| c.sessions.iter()).find_map(|s| match s {
                Session::Edit(e) if e.dirty => Some(e.file_name().to_string()),
                _ => None,
            }) {
                self.status.push_auto(format!("unsaved: {name} (use :Q!)"));
                return;
            }
            // Busy-PTY guard: same as `:q`, but walks every session in
            // every cell — `:Q` tears the whole app down, so any live
            // shell/claude anywhere is a reason to refuse.
            if let Some(label) = self.cells.iter()
                .flat_map(|c| c.sessions.iter())
                .find_map(pty_busy_label_from_session)
            {
                self.status.push_auto(format!("{label} is busy (use :Q!)"));
                return;
            }
        }
        self.should_quit = true;
    }

    /// Close every cell at once. Refuses on the first dirty editor
    /// unless `force`. Parks focus on the explorer afterwards.
    fn cmd_close_all(&mut self, force: bool) {
        if !force {
            if let Some(name) = self.cells.iter().find_map(|c| match c.active_session() {
                Session::Edit(e) if e.dirty => Some(e.file_name().to_string()),
                _ => None,
            }) {
                self.status.push_auto(format!("unsaved: {name} (use :q! *)"));
                return;
            }
            if let Some(label) = self.cells.iter().find_map(cell_active_pty_busy_label) {
                self.status.push_auto(format!("{label} is busy (use :q! *)"));
                return;
            }
        }
        let n = self.cells.len();
        self.cells.clear();
        self.preview_cell_idx = None;
        self.last_cell_focus = 0;
        self.set_focus(FocusId::Explorer);
        self.on_sessions_changed();
        self.status.push_auto(match n {
            0 => "no cells to close".into(),
            1 => "closed last cell — quitting".into(),
            _ => format!("closed {n} cells"),
        });
    }

    /// Write every editor cell. Skips non-editors and editors with
    /// external conflicts (unless `force`). Reports a combined count.
    fn cmd_write_all(&mut self, force: bool) {
        let mut written = 0usize;
        let mut failed  = 0usize;
        let mut skipped = 0usize;
        for cell in self.cells.iter_mut() {
            let Some(ed) = cell.active_session_mut().as_editor_mut() else { continue; };
            if !force && ed.external_conflict.is_some() {
                skipped += 1;
                continue;
            }
            match ed.save() {
                Ok(())  => written += 1,
                Err(_)  => failed  += 1,
            }
        }
        self.refresh_git();
        let mut parts = vec![format!("wrote {written}")];
        if skipped > 0 { parts.push(format!("{skipped} skipped (conflict)")); }
        if failed  > 0 { parts.push(format!("{failed} failed")); }
        self.status.push_auto(parts.join(", "));
    }

    fn cmd_edit(&mut self, path: &str) {
        if path.is_empty() {
            self.status.push_auto("usage: :e <path>".into());
            return;
        }
        let expanded = expand_tilde(path);
        let p = std::path::Path::new(&expanded);
        if p.is_dir() {
            self.status.push_auto(format!("{} is a directory", p.display()));
            return;
        }
        let name = p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
        // Focused cell has a writable editor → replace its contents
        // (vim-style). Read-only buffers (help, welcome) MUST NOT be
        // clobbered — otherwise `:e foo.rs` while the help cell is
        // focused would overwrite the help content with foo.rs but
        // keep the `[ACodeEditor]` badge forever. Open those in a new
        // cell instead.
        let focused_writable = self.focused_editor_mut()
            .map(|ed| !ed.read_only)
            .unwrap_or(false);
        if focused_writable {
            if let Some(ed) = self.focused_editor_mut() {
                match ed.load(p) {
                    Ok(())  => {
                        let msg = if ed.is_new {
                            format!("new file: {name} — :w creates it")
                        } else {
                            format!("opened {name}")
                        };
                        self.status.push_auto(msg);
                        return;
                    }
                    Err(_)  => {
                        // Binary fallback — invalid UTF-8 (or another
                        // text-decode failure). Replace the focused
                        // editor's session with a Hex cell so the file
                        // is still openable.
                        if let FocusId::Cell(idx) = self.focus {
                            match std::fs::read(p) {
                                Ok(bytes) => {
                                    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
                                    let hv = HexView::from_bytes(Some(abs), bytes, false, false);
                                    let active = self.cells[idx].active;
                                    self.cells[idx].sessions[active] = Session::Hex(hv);
                                    self.status.push_auto(format!("{name}: binary file — opened in hex mode"));
                                    return;
                                }
                                Err(e2) => {
                                    self.status.push_auto(format!("open failed: {e2}"));
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Otherwise create a new cell with an editor for this file.
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells"));
            return;
        }
        let mut ed = Editor::empty();
        match ed.load(p) {
            Ok(()) => {
                let is_new = ed.is_new;
                self.insert_cell_at_top(Cell::with_session(Session::Edit(ed)));
                let msg = if is_new {
                    format!("new file: {name} — :w creates it")
                } else {
                    format!("opened {name} in new cell")
                };
                self.status.push_auto(msg);
            }
            Err(_) => {
                // Binary fallback — open as hex.
                match std::fs::read(p) {
                    Ok(bytes) => {
                        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
                        let hv = HexView::from_bytes(Some(abs), bytes, false, false);
                        self.insert_cell_at_top(Cell::with_session(Session::Hex(hv)));
                        self.status.push_auto(format!("{name}: binary file — opened in hex mode"));
                    }
                    Err(e2) => self.status.push_auto(format!("open failed: {e2}")),
                }
            }
        }
    }

    fn cmd_new(&mut self, spec: &str) {
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells"));
            return;
        }
        let (kind_str, rest) = split_kind_path(spec);
        let Some(kind) = SessionKind::parse(kind_str) else {
            self.status.push_auto("usage: :new shell|claude|edit [path]".into());
            return;
        };
        // New cells land in the master slot (index 0), so compute the
        // prospective rect for slot 0 of the future layout — not the
        // old `len()` slot, which was a smaller stack cell.
        let new_total = self.cells.len() + 1;
        let rect = self.prospective_cell_rect(0, new_total);
        let (rows, cols) = crate::ui::inner_size(rect);
        match self.build_session(kind, rest, rows.max(3), cols.max(20)) {
            Ok(session) => {
                self.insert_cell_at_top(Cell::with_session(session));
                self.status.push_auto(format!("new cell 1 ({})", kind_label(kind)));
                self.on_sessions_changed();
            }
            Err(e) => self.status.push_auto(e),
        }
    }

    /// `:ex <cmd>` — run a one-off command in an ephemeral, minimized
    /// pty cell. When the command exits, `reap_exited_ptys` drops the
    /// cell on the next tick. Not persisted to `.acedata`.
    fn cmd_execute(&mut self, cmdline: &str) {
        if cmdline.is_empty() {
            self.status.push_auto("usage: :ex <cmd>".into());
            return;
        }
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells"));
            return;
        }
        // Size against the slot the cell would occupy as visible — it's
        // immediately minimized, but the spawned PTY still parses
        // against these dimensions if the user later restores it.
        let new_total = self.cells.len() + 1;
        let rect = self.prospective_cell_rect(0, new_total);
        let (rows, cols) = crate::ui::inner_size(rect);
        match PtySession::spawn_exec(
            cmdline,
            rows.max(3),
            cols.max(20),
            self.pty_cwd().as_deref(),
            self.tx.clone(),
        ) {
            Ok(pty) => {
                let mut cell = Cell::with_session(Session::Shell(pty));
                cell.ephemeral = true;
                cell.minimized = true;
                // Minimized cells live at the tail so the
                // visible-first invariant holds.
                self.cells.push(cell);
                self.status.push_auto(format!("ex: {cmdline}"));
                // No on_sessions_changed(): ephemeral cells are
                // excluded from the snapshot, so nothing would change
                // on disk. But we still need to nudge the explorer so
                // the new row shows in OPEN CELLS — normally the
                // `.acedata` write's FS event handles that.
                self.pending_explorer_refresh = true;
            }
            Err(e) => self.status.push_auto(format!("spawn failed: {e}")),
        }
    }

    fn cmd_tab(&mut self, spec: &str) {
        let cell_idx = match self.focus {
            FocusId::Cell(i) => i,
            _ => {
                self.status.push_auto(":tab needs cell focus".into());
                return;
            }
        };
        let (kind_str, rest) = split_kind_path(spec);
        let Some(kind) = SessionKind::parse(kind_str) else {
            self.status.push_auto("usage: :tab shell|claude|edit [path]".into());
            return;
        };
        // Use the current cell's rect for the spawn geometry so the new
        // PTY starts at the right size.
        let rect = self.prospective_cell_rect(cell_idx, self.cells.len());
        let (rows, cols) = crate::ui::inner_size(rect);
        // Welcome is a preview — tabbing into a sole-welcome cell
        // swaps the welcome editor out instead of sitting beside it.
        let replacing_welcome = {
            let cell = &self.cells[cell_idx];
            cell.sessions.len() == 1
                && matches!(&cell.sessions[0], Session::Edit(ed) if ed.is_welcome)
        };
        match self.build_session(kind, rest, rows.max(3), cols.max(20)) {
            Ok(session) => {
                let (active, total) = {
                    let cell = &mut self.cells[cell_idx];
                    if replacing_welcome {
                        cell.sessions[0] = session;
                        cell.active = 0;
                    } else {
                        cell.sessions.push(session);
                        cell.active = cell.sessions.len() - 1;
                    }
                    (cell.active, cell.sessions.len())
                };
                self.mode = self.natural_mode_for_focus();
                self.status.push_auto(format!("tab session {}/{}", active + 1, total));
                self.on_sessions_changed();
            }
            Err(e) => self.status.push_auto(e),
        }
    }

    fn cmd_split(&mut self) {
        let cell_idx = match self.focus {
            FocusId::Cell(i) => i,
            _ => {
                self.status.push_auto(":split needs cell focus".into());
                return;
            }
        };
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {MAX_CELLS} cells"));
            return;
        }
        {
            let cell = &self.cells[cell_idx];
            if cell.sessions.len() <= 1 {
                self.status.push_auto(":split needs more than one session in the cell".into());
                return;
            }
        }
        let session = {
            let cell = &mut self.cells[cell_idx];
            let s = cell.sessions.remove(cell.active);
            if cell.active >= cell.sessions.len() {
                cell.active = cell.sessions.len() - 1;
            }
            s
        };
        self.insert_cell_at_top(Cell::with_session(session));
        self.status.push_auto("split to cell 1".into());
        self.on_sessions_changed();
    }

    pub fn cmd_close(&mut self, force: bool) {
        self.cmd_close_target(force, None);
    }

    /// Close a cell by target. Backing for `:q N`, `:q *`, and the
    /// explorer's `c` on an open-cell row. `None` → close the focused
    /// cell (original behavior); `All` → close every cell at once.
    pub fn cmd_close_target(&mut self, force: bool, target: Option<CellTarget>) {
        let cell_idx = match target {
            Some(CellTarget::All) => { self.cmd_close_all(force); return; }
            Some(CellTarget::Idx(i)) if i < self.cells.len() => i,
            Some(CellTarget::Idx(i)) => {
                self.status.push_auto(format!("no cell {}", i + 1));
                return;
            }
            None => match self.focus {
                FocusId::Cell(i) => i,
                _ => {
                    self.status.push_auto("nothing to close here".into());
                    return;
                }
            },
        };

        // Unsaved editor guard: refuse unless forced.
        if !force && cell_active_editor_is_dirty(&self.cells[cell_idx]) {
            let name = match self.cells[cell_idx].active_session() {
                Session::Edit(e) => e.file_name().to_string(),
                _ => String::new(),
            };
            self.status.push_auto(format!("unsaved: {name} (use :close!)"));
            return;
        }

        // Busy-PTY guard: refuse to close a claude/shell cell while
        // something's actively producing output (command running,
        // Claude mid-response). User must `:q!` to force.
        if !force {
            if let Some(label) = cell_active_pty_busy_label(&self.cells[cell_idx]) {
                self.status.push_auto(format!("{label} is busy (use :q!)"));
                return;
            }
        }

        let was_focused = matches!(self.focus, FocusId::Cell(i) if i == cell_idx);
        let focused_idx = if let FocusId::Cell(i) = self.focus { Some(i) } else { None };

        let cell_empty = {
            let cell = &mut self.cells[cell_idx];
            cell.sessions.remove(cell.active);
            cell.sessions.is_empty()
        };
        if cell_empty {
            self.cells.remove(cell_idx);
            // Preview-cell bookkeeping: if the closed cell was the
            // preview, drop the pointer; otherwise shift it down if it
            // was above the removed cell.
            self.preview_cell_idx = match self.preview_cell_idx {
                Some(i) if i == cell_idx => None,
                Some(i) if i > cell_idx  => Some(i - 1),
                other                    => other,
            };
            if self.cells.is_empty() {
                // Last cell gone. The main loop's post-event check
                // either switches to another open project (preferred)
                // or flips `should_quit` on the next tick. Park focus
                // somewhere safe either way. Defer the status message
                // to that fallback path so it reflects what actually
                // happened (switched vs. quitting).
                self.set_focus(FocusId::Explorer);
                if self.projects.projects.len() <= 1 {
                    self.status.push_auto("closed last cell — quitting".into());
                }
            } else if was_focused {
                // Move focus to the nearest survivor.
                let new_focus = cell_idx.min(self.cells.len() - 1);
                self.set_focus(FocusId::Cell(new_focus));
                self.status.push_auto(format!("closed cell ({} remain)", self.cells.len()));
            } else {
                // Closed a non-focused cell — keep the current focus
                // but shift its index down if the removal was above it.
                if let Some(i) = focused_idx {
                    if i > cell_idx {
                        self.set_focus(FocusId::Cell(i - 1));
                    }
                }
                self.status.push_auto(format!("closed cell ({} remain)", self.cells.len()));
            }
        } else {
            let (cur, total) = {
                let cell = &mut self.cells[cell_idx];
                if cell.active >= cell.sessions.len() {
                    cell.active = cell.sessions.len() - 1;
                }
                (cell.active, cell.sessions.len())
            };
            self.mode = self.natural_mode_for_focus();
            self.status.push_auto(format!("closed session ({}/{} in cell)", cur + 1, total));
        }
        self.on_sessions_changed();
    }

    fn cmd_layout(&mut self, which: &str) {
        match LayoutMode::parse(which) {
            Some(mode) => {
                self.layout_mode = mode;
                self.status.push_auto(format!("layout: {}", mode.label()));
            }
            None => self.status.push_auto(format!("unknown layout: {which}")),
        }
    }

    /// Build a fresh session of the requested kind at the given geometry.
    /// String error is user-facing.
    fn build_session(
        &self,
        kind: SessionKind,
        rest: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Session, String> {
        match kind {
            SessionKind::Shell => {
                // `:new shell` uses the configured default; `:new shell
                // <argv>` spawns the given executable instead (e.g. `:new
                // shell zsh` or `:new shell "bash -i"`). argv is parsed
                // with the same CMD-style splitter the `.acerc` shell
                // key uses, so quoted paths with spaces round-trip.
                let result = if rest.is_empty() {
                    PtySession::spawn_shell(rows, cols, self.pty_cwd().as_deref(), self.tx.clone())
                } else {
                    let argv = crate::config::parse_argv(rest)
                        .ok_or_else(|| format!("bad shell spec: {rest}"))?;
                    PtySession::spawn_shell_custom(argv, rows, cols, self.pty_cwd().as_deref(), self.tx.clone())
                };
                result.map(Session::Shell).map_err(|e| format!("spawn failed: {e}"))
            }
            SessionKind::Claude => PtySession::spawn_claude(rows, cols, self.pty_cwd().as_deref(), self.tx.clone())
                .map(Session::Claude)
                .map_err(|e| format!("spawn failed: {e}")),
            SessionKind::Edit => {
                let mut ed = Editor::empty();
                if !rest.is_empty() {
                    let expanded = expand_tilde(rest);
                    let p = std::path::Path::new(&expanded);
                    if p.is_dir() {
                        return Err(format!("{} is a directory", p.display()));
                    }
                    match ed.load(p) {
                        Ok(()) => {}
                        Err(_) => {
                            // Binary fallback: invalid UTF-8 (or other
                            // non-text decode failure) opens as Hex so
                            // every file is reachable through `:new edit`.
                            let bytes = std::fs::read(p)
                                .map_err(|e2| format!("open failed: {e2}"))?;
                            let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
                            let hv = HexView::from_bytes(Some(abs), bytes, false, false);
                            return Ok(Session::Hex(hv));
                        }
                    }
                }
                Ok(Session::Edit(ed))
            }
            SessionKind::Hex => {
                let mut hv = HexView::empty();
                if !rest.is_empty() {
                    let expanded = expand_tilde(rest);
                    let p = std::path::Path::new(&expanded);
                    if p.is_dir() {
                        return Err(format!("{} is a directory", p.display()));
                    }
                    hv.load(p).map_err(|e| format!("open failed: {e}"))?;
                }
                Ok(Session::Hex(hv))
            }
            // Diff sessions aren't user-spawnable from `:new diff` —
            // they're always the result of `v` on a change/log entry,
            // which goes through `open_diff_in_cell` directly.
            SessionKind::Diff => Err("diff cells open via `v` on a git change or log entry".into()),
            // Same for Conflict — opened via `:conflict` or auto-launched
            // when entering an external conflict state.
            SessionKind::Conflict => Err("conflict cells open via :conflict on a dirty editor".into()),
        }
    }

    /// Inner rect for cell `cell_idx` in a prospective layout of `n_total`
    /// cells. Used when spawning a new PTY so its parser starts sized right.
    fn prospective_cell_rect(&self, cell_idx: usize, n_total: usize) -> Rect {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let root = Rect { x: 0, y: 0, width: cols, height: rows };
        let main = crate::ui::compute_main_area(root, self);
        // `rects` now tiles only visible cells. A newly-spawned cell
        // always becomes visible, so we size against `visible + 1`,
        // ignoring any currently-minimized cells.
        let visible = self.cells.iter().filter(|c| !c.minimized).count();
        let n_visible = if cell_idx < n_total { visible + 1 } else { visible };
        self.layout_mode
            .rects(main, n_visible)
            .get(cell_idx.min(n_visible.saturating_sub(1)))
            .copied()
            .unwrap_or(Rect { x: 0, y: 0, width: 80, height: 24 })
    }

    // ── FS reconciliation ────────────────────────────────────────────────

    /// Called on every `AppEvent::FsChange`. Reconciles any editor
    /// whose path matches the changed file and surfaces the outcome.
    /// Recursive watchers fire events for paths inside directories, so
    /// we also drop project-state caches to trigger a rail refresh on
    /// the next tick.
    pub fn handle_fs_change(&mut self, path: &Path) {
        use crate::editor::ReconcileOutcome;
        let mut status: Option<String> = None;
        let mut any_deleted = false;
        for cell in self.cells.iter_mut() {
            for sess in cell.sessions.iter_mut() {
                match sess {
                    Session::Edit(ed) => {
                        if ed.path.as_deref() != Some(path) { continue; }
                        let name = ed.file_name().to_string();
                        match ed.reconcile() {
                            ReconcileOutcome::AutoReloaded =>
                                status = Some(format!("{name}: reloaded from disk")),
                            ReconcileOutcome::ConflictMarked =>
                                status = Some(format!("{name}: changed on disk — :e! reload, :w! overwrite")),
                            ReconcileOutcome::Deleted => {
                                status = Some(format!("{name}: deleted on disk — :w to recreate"));
                                any_deleted = true;
                            }
                            ReconcileOutcome::NoOp => {}
                        }
                    }
                    Session::Hex(hv) => {
                        if hv.path.as_deref() != Some(path) { continue; }
                        use crate::hex::ReconcileOutcome as R;
                        let name = hv.file_name().to_string();
                        match hv.reconcile() {
                            R::AutoReloaded =>
                                status = Some(format!("{name}: reloaded from disk")),
                            R::ConflictMarked =>
                                status = Some(format!("{name}: changed on disk — :e! reload, :w! overwrite")),
                            R::Deleted => {
                                status = Some(format!("{name}: deleted on disk — :w to recreate"));
                                any_deleted = true;
                            }
                            R::NoOp => {}
                        }
                    }
                    _ => {}
                }
            }
        }

        // Rename detection: any editor that just entered Deleted might
        // actually have been renamed. Scan its parent dir for a sibling
        // with matching saved_size that isn't already owned by another
        // editor. On a match, retarget the editor's path — the buffer
        // stays, dirty flag stays, conflict clears.
        if any_deleted {
            if let Some(rename) = self.detect_rename() {
                status = Some(format!(
                    "renamed: {} → {}",
                    rename.old.display(),
                    rename.new.display()
                ));
            }
        }

        if let Some(msg) = status {
            self.status.push_auto(msg);
        }
    }

    /// Inspect every editor currently in Deleted-state; for each, look
    /// for a same-size sibling in the parent dir that no other open
    /// editor already owns. On match, retarget in-place and return the
    /// first rename found (one message per event).
    fn detect_rename(&mut self) -> Option<RenameDetected> {
        use crate::editor::ExternalConflict as C;
        // Snapshot the set of paths currently owned by any open editor.
        let open_paths: HashSet<PathBuf> = self
            .cells
            .iter()
            .flat_map(|c| c.sessions.iter())
            .filter_map(|s| match s {
                Session::Edit(e) => e.path.clone(),
                _ => None,
            })
            .collect();

        // Two-phase so we don't hold a mutable borrow of self across
        // the read_dir walk.
        let mut plan: Option<(usize, usize, PathBuf, PathBuf)> = None;
        'outer: for (ci, cell) in self.cells.iter().enumerate() {
            for (si, sess) in cell.sessions.iter().enumerate() {
                let Session::Edit(ed) = sess else { continue; };
                if ed.external_conflict != Some(C::Deleted) { continue; }
                let Some(old_path) = ed.path.clone() else { continue; };
                let Some(parent) = old_path.parent().map(|p| p.to_path_buf()) else { continue; };
                let Some(size) = ed.saved_size() else { continue; };

                let Ok(rd) = std::fs::read_dir(&parent) else { continue; };
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p == old_path { continue; }
                    if open_paths.contains(&p) { continue; }
                    let Ok(md) = entry.metadata() else { continue; };
                    if !md.is_file() { continue; }
                    if md.len() != size { continue; }
                    plan = Some((ci, si, old_path.clone(), p));
                    break 'outer;
                }
            }
        }

        let (ci, si, old, new) = plan?;
        if let Some(Session::Edit(ed)) = self
            .cells
            .get_mut(ci)
            .and_then(|c| c.sessions.get_mut(si))
        {
            ed.retarget_path(new.clone());
        }
        Some(RenameDetected { old, new })
    }

    // ── .acedata per-project session persistence ─────────────────────────

    /// Root of the currently active project, if any. Used to key the
    /// `.acedata` load/save — rootless (files-only) sessions don't
    /// persist session state.
    pub fn current_project_root(&self) -> Option<PathBuf> {
        self.projects.projects.get(self.projects.active).map(|p| p.root.clone())
    }

    /// Called from the main loop when `cells` has gone empty. Closing
    /// the last cell in a project closes the project itself — if others
    /// are open, `project_close_idx` swaps in project 0; if this was the
    /// only project, the caller flips `should_quit` on the next tick.
    /// Returns `true` if another project took over.
    pub fn try_switch_project_on_empty(&mut self) -> bool {
        if self.cells.is_empty() && self.projects.projects.len() > 1 {
            self.project_close_idx(self.projects.active);
            return !self.cells.is_empty();
        }
        false
    }

    /// Working directory to hand to a newly-spawned shell or claude PTY.
    /// Prefers the active project root; otherwise inherits ace's current
    /// cwd (which is the launch directory when there's no project).
    pub fn pty_cwd(&self) -> Option<PathBuf> {
        self.current_project_root()
    }

    /// Pipe the last output line of every running ephemeral (`:ex`) pty
    /// to the status bar, one message per distinct tail. We trim and
    /// cap to a single line so a noisy command (cargo, npm) nudges the
    /// status without flooding it. Skips exited PTYs — `reap_exited_ptys`
    /// handles the final message on completion.
    pub fn poll_ephemeral_status(&mut self) {
        let mut updates: Vec<(usize, String)> = Vec::new();
        for (ci, cell) in self.cells.iter().enumerate() {
            if !cell.ephemeral { continue; }
            let Some(pty) = cell.active_session().as_pty() else { continue; };
            if pty.has_exited() { continue; }
            let Some(line) = pty.last_nonempty_line() else { continue; };
            if cell.exec_last_pushed.as_deref() == Some(line.as_str()) { continue; }
            updates.push((ci, line));
        }
        for (ci, line) in updates {
            self.status.push_live("ex:", format!("ex: {}", truncate_ex_line(&line)));
            self.cells[ci].exec_last_pushed = Some(line);
        }
    }

    /// Scan every cell for PTY sessions whose child has exited and
    /// remove those sessions. If a removal empties a cell, the cell is
    /// dropped too. Returns the number of sessions reaped.
    pub fn reap_exited_ptys(&mut self) -> usize {
        let mut reaped = 0usize;
        // Collect exits as (cell_idx, session_idx) before mutating so we
        // can work back-to-front and keep indices stable.
        let mut hits: Vec<(usize, usize)> = Vec::new();
        // Ephemeral final-output push: capture each `:ex` cell's last
        // line before we drop its PTY, so the user sees the tail of the
        // output even if it landed between the last poll and exit.
        let mut ephemeral_finals: Vec<String> = Vec::new();
        for (ci, cell) in self.cells.iter().enumerate() {
            for (si, sess) in cell.sessions.iter().enumerate() {
                if let Some(pty) = sess.as_pty() {
                    if pty.has_exited() {
                        hits.push((ci, si));
                        if cell.ephemeral && cell.active == si {
                            if let Some(line) = pty.last_nonempty_line() {
                                if cell.exec_last_pushed.as_deref() != Some(line.as_str()) {
                                    ephemeral_finals.push(line);
                                }
                            }
                        }
                    }
                }
            }
        }
        if hits.is_empty() { return 0; }
        for line in ephemeral_finals {
            self.status.push_live("ex:", format!("ex: {}", truncate_ex_line(&line)));
        }

        // Drop session-by-session. We process in reverse so shifting
        // indices within the same cell don't invalidate earlier hits.
        hits.sort_unstable_by(|a, b| b.cmp(a));
        // Cells that became empty get removed in a second pass.
        let mut drop_cells: Vec<usize> = Vec::new();
        for (ci, si) in hits {
            if let Some(cell) = self.cells.get_mut(ci) {
                if si < cell.sessions.len() {
                    cell.sessions.remove(si);
                    if cell.active >= cell.sessions.len() && !cell.sessions.is_empty() {
                        cell.active = cell.sessions.len() - 1;
                    }
                    reaped += 1;
                    if cell.sessions.is_empty() {
                        drop_cells.push(ci);
                    }
                }
            }
        }

        if !drop_cells.is_empty() {
            drop_cells.sort_unstable();
            drop_cells.dedup();
            // Track focus/preview against each removal so indices stay
            // pointing at the right survivor.
            let focus_cell = match self.focus { FocusId::Cell(i) => Some(i), _ => None };
            let mut new_focus = focus_cell;
            let mut new_preview = self.preview_cell_idx;
            let mut new_last = Some(self.last_cell_focus);
            for &ci in drop_cells.iter().rev() {
                self.cells.remove(ci);
                new_focus = match new_focus {
                    Some(f) if f == ci => None,
                    Some(f) if f > ci  => Some(f - 1),
                    other              => other,
                };
                new_preview = match new_preview {
                    Some(p) if p == ci => None,
                    Some(p) if p > ci  => Some(p - 1),
                    other              => other,
                };
                new_last = match new_last {
                    Some(l) if l == ci => Some(0),
                    Some(l) if l > ci  => Some(l - 1),
                    other              => other,
                };
            }
            self.preview_cell_idx = new_preview;
            self.last_cell_focus = new_last.unwrap_or(0);
            if !self.cells.is_empty() {
                self.last_cell_focus = self.last_cell_focus.min(self.cells.len() - 1);
            }
            match new_focus {
                Some(i) if i < self.cells.len() => self.set_focus(FocusId::Cell(i)),
                _ if self.cells.is_empty()     => self.set_focus(FocusId::Explorer),
                _ => {
                    let f = focus_cell.unwrap_or(0).min(self.cells.len() - 1);
                    self.set_focus(FocusId::Cell(f));
                }
            }
            self.on_sessions_changed();
            // Ephemeral cell removal doesn't change the `.acedata`
            // snapshot, so its FS event doesn't fire and the explorer
            // wouldn't otherwise notice. Nudge it here so the OPEN
            // CELLS list drops the row immediately.
            self.pending_explorer_refresh = true;
        }
        reaped
    }


    /// Snapshot current cells/sessions in the form `.acedata` expects.
    /// PTY sessions are recorded as kind-only; editor sessions carry
    /// their file path (if any). Diff/scratch sessions are skipped.
    pub fn acedata_snapshot(&self) -> StateSnapshot {
        let mut snap = StateSnapshot::default();
        let focused_self_idx = match self.focus {
            FocusId::Cell(i) => Some(i),
            _                => None,
        };
        for (self_idx, cell) in self.cells.iter().enumerate() {
            if cell.ephemeral {
                continue;
            }
            let mut cs = CellState { active: cell.active, sessions: Vec::new() };
            for sess in &cell.sessions {
                let (kind, path) = match sess {
                    Session::Shell(_)  => (SessionKind::Shell,  None),
                    Session::Claude(_) => (SessionKind::Claude, None),
                    Session::Edit(ed)  => match ed.path.clone() {
                        Some(p) => (SessionKind::Edit, Some(p)),
                        None    => continue, // scratch — nothing to persist
                    },
                    Session::Hex(h)    => match h.path.clone() {
                        Some(p) => (SessionKind::Hex, Some(p)),
                        None    => continue,
                    },
                    Session::Diff(_)     => continue, // ephemeral
                    Session::Conflict(_) => continue, // ephemeral; re-derives from disk
                };
                cs.sessions.push(SessionState { kind, path });
            }
            if !cs.sessions.is_empty() {
                if cs.active >= cs.sessions.len() {
                    cs.active = 0;
                }
                // Record the focused cell so restart-restore can re-park
                // the user where they were. Explorer focus is the `None`
                // default. We store the snap-relative index (where `cs`
                // is about to land) so restore maps cleanly onto the
                // cells we'll rebuild, even if scratch editors were
                // skipped upstream and shifted positions.
                if focused_self_idx == Some(self_idx) {
                    snap.focus = Some(snap.cells.len());
                }
                snap.cells.push(cs);
            }
        }
        snap
    }

    /// Write `.acedata` into the active project's root. No-op when no
    /// project is active (files-only session, or empty list). Also
    /// refreshes `last_saved_hash` so the next `persist_cells_if_dirty`
    /// tick doesn't rewrite the file we just wrote.
    pub fn persist_cells(&mut self) {
        let Some(root) = self.current_project_root() else { return; };
        let snap = self.acedata_snapshot();
        let _ = crate::session_state::save(&root, &snap);
        self.last_saved_hash = hash_snapshot(&snap);
        // Gitignore injection happens alongside persistence so users
        // don't commit their session state by accident. Guarded by
        // `.acerc auto_gitignore = false` if they want to opt out.
        ensure_gitignore_entries(&root);
    }

    /// End-of-loop persistence: hash the current snapshot and save if
    /// it diverged from `last_saved_hash`. Called after every event so
    /// structural changes land in `.acedata` within milliseconds, not
    /// the old 1 s window. Cheap — hash is over structural metadata
    /// only (cell count + session kinds + paths), never content.
    pub fn persist_cells_if_dirty(&mut self) {
        if self.current_project_root().is_none() {
            return;
        }
        // Skip the snapshot build + hash unless the dispatch layer saw
        // something that could plausibly have mutated structural state.
        // Event-free ticks (status bar decay, frame-budget wakeups) are
        // a no-op here.
        if !self.persist_dirty_hint {
            return;
        }
        self.persist_dirty_hint = false;
        let h = hash_snapshot(&self.acedata_snapshot());
        if h != self.last_saved_hash {
            self.persist_cells();
            self.last_saved_hash = h;
        }
    }

    /// Shorthand: after any mutation that changes the cell/session set,
    /// persist `.acedata` and re-hook the FS watcher (in case a new
    /// editor path lives outside currently-watched roots).
    pub fn on_sessions_changed(&mut self) {
        self.persist_cells();
        self.refresh_watchers();
    }

    /// Read `.acedata` from a project root. Thin wrapper so `main.rs`
    /// doesn't have to reach into `session_state` itself.
    pub fn load_acedata(root: &Path) -> Option<StateSnapshot> {
        crate::session_state::load(root)
    }

    /// Append cells from a `.acedata` snapshot. PTYs are respawned at
    /// an approximate geometry; the main loop's resize pass tightens
    /// them on the first draw. Returns the number of cells actually
    /// restored — `0` means the caller should fall back to a scratch
    /// editor.
    pub fn restore_cells_from_snapshot(&mut self, snap: &StateSnapshot) -> usize {
        // Seed the dirty-check hash with the on-disk snapshot we're
        // restoring from. Otherwise the very first `persist_cells_if_dirty`
        // tick sees hash 0 ≠ current and rewrites `.acedata` identically.
        self.last_saved_hash = hash_snapshot(snap);
        const DEFAULT_ROWS: u16 = 24;
        const DEFAULT_COLS: u16 = 80;
        let mut restored = 0;
        for cs in &snap.cells {
            if self.cells.len() >= MAX_CELLS {
                break;
            }
            let mut sessions: Vec<Session> = Vec::new();
            for ss in &cs.sessions {
                if let Some(sess) = self.restore_session(ss, DEFAULT_ROWS, DEFAULT_COLS) {
                    sessions.push(sess);
                }
            }
            if sessions.is_empty() {
                continue;
            }
            let active = cs.active.min(sessions.len().saturating_sub(1));
            self.cells.push(Cell { sessions, active, minimized: false, ephemeral: false, exec_last_pushed: None });
            restored += 1;
        }
        restored
    }

    /// Build a fresh `Session` for a persisted kind, reusing
    /// `build_session` for PTYs. Editor sessions are built locally so
    /// non-existent paths are tolerated (just primes `editor.path`).
    pub fn restore_session(&self, state: &SessionState, rows: u16, cols: u16) -> Option<Session> {
        match state.kind {
            SessionKind::Edit => {
                let mut ed = Editor::empty();
                if let Some(p) = state.path.as_ref() {
                    // `Editor::load` already treats a nonexistent path
                    // as `[NEW]` (flips `is_new = true`). Calling it
                    // unconditionally keeps the badge consistent after
                    // a restore — the previous `exists()` branch set
                    // the path but forgot to mark the buffer new.
                    let _ = ed.load(p);
                }
                Some(Session::Edit(ed))
            }
            SessionKind::Shell => PtySession::spawn_shell(rows, cols, self.pty_cwd().as_deref(), self.tx.clone())
                .ok().map(Session::Shell),
            SessionKind::Claude => PtySession::spawn_claude(rows, cols, self.pty_cwd().as_deref(), self.tx.clone())
                .ok().map(Session::Claude),
            SessionKind::Hex => {
                let mut hv = HexView::empty();
                if let Some(p) = state.path.as_ref() {
                    let _ = hv.load(p);
                }
                Some(Session::Hex(hv))
            }
            SessionKind::Diff     => None, // never persisted
            SessionKind::Conflict => None, // never persisted
        }
    }

    // ── projects ─────────────────────────────────────────────────────────

    /// Switch to a project by its index in the list.
    /// Switch project but keep current focus. Used when the switch is a
    /// side-effect of something else the user is doing in the explorer
    /// (Enter on a project row, PgUp/PgDn project cycling, `:proj add`
    /// making the new project active). They're still navigating the
    /// panel — yanking them into a cell would be surprising.
    pub fn project_switch_idx_keep_focus(&mut self, idx: usize) {
        let Some(root) = self.projects.projects.get(idx).map(|p| p.root.clone()) else {
            return;
        };
        self.switch_to_project_idx(idx, &root, true);
    }

    /// Switch to a project by name.
    pub fn project_switch_named(&mut self, name: &str) {
        match self.projects.find_by_name(name) {
            Some(i) => {
                let root = self.projects.projects[i].root.clone();
                self.switch_to_project_idx(i, &root, false);
            }
            None => self.status.push_auto(format!("no project: {name}")),
        }
    }

    /// Add a new project (defaults to cwd if path is empty). Leaves the
    /// current active project unchanged — use `:proj switch` to move.
    ///
    /// Windows filesystems are case-insensitive, so `root.exists()` will
    /// return true for a lowercase path even when the true on-disk name
    /// is mixed-case — and git2 then fails to open the repo because
    /// libgit2 matches case-exactly on the working directory. Reject
    /// instead of silently adding a half-broken project.
    pub fn project_add(&mut self, path: &str) {
        let root = if path.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(expand_tilde(path))
        };
        if !root.exists() {
            self.status.push_auto(format!("no such path: {}", root.display()));
            return;
        }
        if let Some(actual) = real_path_case(&root) {
            self.status.push_auto(format!(
                "case mismatch: did you mean {}?",
                actual.display()
            ));
            return;
        }
        let idx = self.projects.add(root);
        self.projects.refresh_states();
        let _ = self.projects.save();
        self.refresh_watchers();
        let name = self.projects.projects[idx].name.clone();
        // Adding a project usually means the user wants to see it —
        // unhide the explorer so the new project isn't invisible, and
        // focus it so they can start navigating the new tree right away.
        self.explorer_hidden = false;
        self.project_switch_idx_keep_focus(idx);
        self.set_focus(FocusId::Explorer);
        self.status.push_auto(format!("added {}", name));
    }

    pub fn project_remove_named(&mut self, name: &str) {
        let Some(idx) = self.projects.find_by_name(name) else {
            self.status.push_auto(format!("no project: {name}"));
            return;
        };
        self.project_close_idx(idx);
    }

    /// Close the project at `idx`. Non-active → plain remove. Active →
    /// persist its cells, drop it, then load project 0 (user's rule:
    /// "first project in index"). No projects left → hide the explorer
    /// and keep whatever cells are open as a files-only session.
    pub fn project_close_idx(&mut self, idx: usize) {
        let Some(name) = self.projects.projects.get(idx).map(|p| p.name.clone()) else {
            return;
        };
        let is_active = idx == self.projects.active;

        if !is_active {
            // Drop any parked cells for this project — their PTYs were
            // still running in the background but the user is explicitly
            // closing the project, so the sessions go with it.
            if let Some(root) = self.projects.projects.get(idx).map(|p| p.root.clone()) {
                self.parked_cells.remove(&root);
            }
            if self.projects.remove(idx) {
                let _ = self.projects.save();
                self.explorer.on_project_switch(&self.projects, self.cells.len());
                self.status.push_auto(format!("closed {name}"));
            }
            return;
        }

        // Active project: flush its cells to its own `.acedata` so the
        // work survives, then tear down outgoing state before removing
        // the project from the list.
        //
        // Split cells by affiliation with the outgoing project:
        //   * editor cells whose file lives outside the project root —
        //     the user's working on them independently of the project
        //     being closed, so carry them forward.
        //   * everything else (editors inside the root, PTYs, diffs,
        //     conflicts) — these are project-scoped, drop them.
        // Scratch editors (no path) are dropped with the project since
        // they belong to it conceptually.
        self.persist_cells();
        let outgoing_root = self.projects.projects[idx].root.clone();
        // Project is being closed, not just switched away from — drop any
        // parked cells so we don't leak running PTYs for a project that
        // no longer exists.
        self.parked_cells.remove(&outgoing_root);
        let external_cells: Vec<Cell> = std::mem::take(&mut self.cells)
            .into_iter()
            .filter(|c| cell_is_external_to(c, &outgoing_root))
            .collect();
        self.preview_cell_idx = None;
        self.last_cell_focus = 0;
        if !self.projects.remove(idx) {
            // remove failed — put the external cells back so the user
            // doesn't lose their buffers. Internal cells are already
            // persisted in the project's `.acedata`.
            self.cells = external_cells;
            return;
        }

        if self.projects.projects.is_empty() {
            // Empty list → no project context at all. Carry the
            // externals forward as a rootless session; if none
            // survived, fall back to a scratch editor so the app has
            // something to show. Hide the explorer either way.
            self.cells = external_cells;
            if self.cells.is_empty() {
                self.cells.push(Cell::with_session(Session::Edit(Editor::empty())));
            }
            self.explorer_hidden     = true;
            self.explorer_mode       = ExplorerMode::Normal;
            self.explorer.on_project_switch(&self.projects, self.cells.len());
            self.refresh_git();
            self.projects.refresh_states();
            let _ = self.projects.save();
            self.refresh_watchers();
            if !self.cells.is_empty() {
                self.set_focus(FocusId::Cell(0));
            }
            self.status.push_auto(format!("closed {name}"));
            return;
        }

        // Switch to project 0. Inline what switch_to_project_idx does,
        // minus the persist step (we already persisted above and the
        // outgoing project no longer exists to persist *to*).
        self.projects.active = 0;
        let root = self.projects.projects[0].root.clone();
        if std::env::set_current_dir(&root).is_err() {
            self.status.push_auto(format!("cd failed: {}", root.display()));
            // Best-effort recovery: at least restore the externals so
            // the user doesn't lose their editor buffers.
            self.cells = external_cells;
            return;
        }
        self.explorer.on_project_switch(&self.projects, self.cells.len());
        self.refresh_git();
        self.projects.refresh_states();
        let _ = self.projects.save();
        let (restored, saved_focus) = if let Some(parked) = self.parked_cells.remove(&root) {
            let focus = parked.focus;
            let n = parked.cells.len();
            self.cells.extend(parked.cells);
            (n, focus)
        } else {
            match crate::session_state::load(&root) {
                Some(snap) => {
                    let f = snap.focus;
                    (self.restore_cells_from_snapshot(&snap), f)
                }
                None => (0, None),
            }
        };
        // Append externals carried over from the closed project so the
        // user's non-project work survives the switch.
        self.cells.extend(external_cells);
        if restored == 0 && self.cells.is_empty() {
            self.cells.push(Cell::with_session(Session::Edit(Editor::welcome())));
        }
        self.enforce_welcome_solo();
        self.refresh_watchers();
        let target = match saved_focus {
            Some(i) if i < self.cells.len() => FocusId::Cell(i),
            _ if self.explorer_hidden && !self.cells.is_empty() => FocusId::Cell(0),
            _ => FocusId::Explorer,
        };
        self.set_focus(target);

        let new_name = self.projects.projects[0].name.clone();
        self.status.push_auto(format!("closed {name} → {new_name}"));
    }

    pub fn project_rename_active(&mut self, new_name: &str) {
        if new_name.is_empty() {
            self.status.push_auto("usage: :proj rename <name>".into());
            return;
        }
        let idx = self.projects.active;
        if let Some(p) = self.projects.projects.get_mut(idx) {
            p.name = new_name.to_string();
            let _ = self.projects.save();
            self.status.push_auto(format!("renamed → {new_name}"));
        }
    }

    pub fn project_list_summary(&mut self) {
        let names: Vec<&str> = self.projects.projects.iter().map(|p| p.name.as_str()).collect();
        let active = self
            .projects
            .projects
            .get(self.projects.active)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        self.status.push_auto(format!(
            "{} projects ({}): {}",
            names.len(),
            active,
            names.join(", ")
        ));
    }

    fn switch_to_project_idx(&mut self, idx: usize, root: &std::path::Path, preserve_focus: bool) {
        if !root.exists() {
            self.status.push_auto(format!(
                "project root unreachable: {} (deleted or moved?)",
                root.display()
            ));
            return;
        }
        if std::env::set_current_dir(root).is_err() {
            self.status.push_auto(format!("cd failed: {}", root.display()));
            return;
        }

        // When `projects.active == idx`, there's no real outgoing
        // project to persist or park — either the list was empty before
        // the incoming project was pushed (`:proj add` on a welcome
        // session), or the user asked to switch to the already-active
        // project. Persisting would overwrite the incoming project's
        // `.acedata` with whatever rootless/scratch cells are currently
        // on screen, clobbering the state we're about to restore. Any
        // existing cells are treated as rootless `kept` so external
        // work survives.
        let reentering = self.projects.active == idx;

        // 1. Flush outgoing project's cell state to its `.acedata` so
        //    anything opened since startup (or last tick) survives the
        //    switch. We do this before flipping `active`.
        if !reentering {
            self.persist_cells();
        }

        // 2. Park outgoing cells in memory so their PTYs keep running in
        //    the background. Returning to this project later pulls them
        //    back verbatim instead of respawning from `.acedata`. Cells
        //    from a rootless (files-only) session have nowhere to park,
        //    so they carry forward as `kept` like before.
        let outgoing_root = if reentering { None } else { self.current_project_root() };
        let preserve_all = outgoing_root.is_none();
        let outgoing_focus = if let FocusId::Cell(i) = self.focus { Some(i) } else { None };
        let kept: Vec<Cell> = if preserve_all {
            std::mem::take(&mut self.cells)
        } else {
            let cells = std::mem::take(&mut self.cells);
            if let Some(root) = outgoing_root {
                self.parked_cells.insert(root, ParkedProject { cells, focus: outgoing_focus });
            }
            Vec::new()
        };
        self.last_cell_focus = 0;
        // Preview cell didn't survive the swap — its index is no longer
        // meaningful. Next explorer Enter starts a fresh preview.
        self.preview_cell_idx = None;

        // 3. Flip the active project — explorer + git refresh + rail
        //    save all key off `projects.active`.
        self.projects.active = idx;
        self.explorer.on_project_switch(&self.projects, self.cells.len());
        self.refresh_git();
        self.projects.refresh_states();
        let _ = self.projects.save();

        // 4. Restore the incoming project's cells. Parked cells (still
        //    alive from a prior switch) win over the disk snapshot —
        //    that's what keeps shells/claude running in the background.
        //    Falls back to `.acedata` when nothing is parked (first
        //    switch this session).
        let (restored, saved_focus) = if let Some(parked) = self.parked_cells.remove(root) {
            let focus = parked.focus;
            let n = parked.cells.len();
            self.cells.extend(parked.cells);
            (n, focus)
        } else {
            match crate::session_state::load(root) {
                Some(snap) => {
                    let f = snap.focus;
                    (self.restore_cells_from_snapshot(&snap), f)
                }
                None => (0, None),
            }
        };
        // Re-insert preserved open-file cells. Only fall back to a
        // scratch editor if nothing survived and nothing was restored —
        // otherwise we'd get a stray empty cell on top of real work.
        if restored == 0 && kept.is_empty() {
            self.cells.push(Cell::with_session(Session::Edit(Editor::welcome())));
        }
        self.cells.extend(kept);
        self.enforce_welcome_solo();
        if !preserve_focus {
            // Honour the project's saved focus when we have it; fall
            // back to explorer (or cell 0 if explorer is hidden).
            let target = match saved_focus {
                Some(i) if i < self.cells.len() => FocusId::Cell(i),
                _ if self.explorer_hidden && !self.cells.is_empty() => FocusId::Cell(0),
                _ => FocusId::Explorer,
            };
            self.set_focus(target);
        }

        // Hook the FS watcher to any new paths the incoming project
        // introduced (its root was likely already watched, but editor
        // files could live in subdirs we didn't know about).
        self.refresh_watchers();

        let name = self
            .projects
            .projects
            .get(idx)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        let suffix = if restored > 0 {
            format!(" ({restored} cell{} restored)", if restored == 1 { "" } else { "s" })
        } else {
            String::new()
        };
        self.status.push_auto(format!("switched to {name}{suffix}"));
    }

    // ── git pane: sub-mode transitions ───────────────────────────────────

    /// `g` from Explorer/Normal — enter the expanded git overview (no
    /// cursor in sections yet). Silently no-ops outside a repo.
    pub fn enter_git_overview(&mut self) {
        if !self.git.is_repo() {
            self.status.push_auto("not a git repo".into());
            return;
        }
        self.explorer_mode = ExplorerMode::GitOverview;
    }

    /// `b` from any git mode — cursor into branches list.
    pub fn enter_git_branches(&mut self) {
        if !self.explorer_mode.is_git() {
            return;
        }
        if self.git.branches.is_empty() {
            self.status.push_auto("no branches".into());
            return;
        }
        // Start on the current branch (listed first by `list_branches`).
        self.git_branch_sel = 0;
        self.explorer_mode = ExplorerMode::GitBranches;
    }

    /// `c` from any git mode — cursor into changes list.
    pub fn enter_git_changes(&mut self) {
        if !self.explorer_mode.is_git() {
            return;
        }
        if self.git.change_rows().is_empty() {
            self.status.push_auto("no changes".into());
            return;
        }
        self.git_change_sel = 0;
        self.explorer_mode = ExplorerMode::GitChanges;
    }

    /// Esc in a branches/changes/log sub-mode → back to overview. Esc
    /// in overview → back to Normal (explorer tree).
    ///
    /// When the final state is `Normal`, any open diff cells close —
    /// diffs are a byproduct of git exploration, and leaving the git
    /// pane means the user is done with them. Leaves other cells
    /// (editors, shells) untouched.
    pub fn exit_git_submode(&mut self) {
        self.explorer_mode = match self.explorer_mode {
            ExplorerMode::GitBranches
            | ExplorerMode::GitChanges
            | ExplorerMode::GitLog           => ExplorerMode::GitOverview,
            ExplorerMode::GitOverview        => ExplorerMode::Normal,
            ExplorerMode::Normal             => ExplorerMode::Normal,
        };
        if matches!(self.explorer_mode, ExplorerMode::Normal) {
            self.close_all_diff_cells();
        }
    }

    /// Remove every cell whose active session is a Diff view. Shifts
    /// `focus` and `last_cell_focus` back toward still-valid slots so
    /// the user doesn't end up pointing at an empty cell index.
    fn close_all_diff_cells(&mut self) {
        let n_before = self.cells.len();
        let mut removed_before_focus = 0usize;
        let mut removed_before_last  = 0usize;
        let focus_cell = match self.focus { FocusId::Cell(i) => Some(i), _ => None };

        let mut i = 0;
        while i < self.cells.len() {
            if matches!(self.cells[i].active_session(), Session::Diff(_)) {
                self.cells.remove(i);
                if focus_cell.map_or(false, |f| i < f) { removed_before_focus += 1; }
                if i < self.last_cell_focus          { removed_before_last   += 1; }
                self.preview_cell_idx = match self.preview_cell_idx {
                    Some(p) if p == i => None,
                    Some(p) if p > i  => Some(p - 1),
                    other             => other,
                };
            } else {
                i += 1;
            }
        }
        if n_before == self.cells.len() {
            return;
        }
        if let FocusId::Cell(f) = self.focus {
            // Victim itself isn't a diff (focus was on Explorer when we
            // ran; but be defensive for future callers). Clamp to the
            // new bounds.
            let new_f = f.saturating_sub(removed_before_focus);
            if self.cells.is_empty() {
                self.focus = FocusId::Explorer;
            } else {
                self.focus = FocusId::Cell(new_f.min(self.cells.len() - 1));
            }
        }
        self.last_cell_focus = self.last_cell_focus.saturating_sub(removed_before_last);
        if !self.cells.is_empty() {
            self.last_cell_focus = self.last_cell_focus.min(self.cells.len() - 1);
        }
    }

    /// `l` from any git mode — load the commit log and enter `GitLog`.
    pub fn enter_git_log(&mut self) {
        if !self.explorer_mode.is_git() {
            return;
        }
        let cwd = self.git_cwd();
        match crate::git::commit_log(&cwd, 200) {
            Ok(v) if v.is_empty() => {
                self.status.push_auto("no commits".into());
                return;
            }
            Ok(v) => {
                self.git_log = v;
                self.git_log_sel = 0;
                self.explorer_mode = ExplorerMode::GitLog;
            }
            Err(e) => self.status.push_auto(format!("log: {e}")),
        }
    }

    pub fn git_log_move(&mut self, delta: isize) {
        let n = self.git_log.len();
        if n == 0 { return; }
        let cur = self.git_log_sel as isize;
        let next = (cur + delta).clamp(0, (n - 1) as isize) as usize;
        self.git_log_sel = next;
    }

    /// `Enter`/`v` on a log entry — show its diff in a new cell.
    pub fn git_log_open_diff(&mut self) {
        let Some(entry) = self.git_log.get(self.git_log_sel).cloned() else { return; };
        let title = format!("{} {}", entry.sha_short, entry.summary);
        match crate::diff::DiffView::for_commit(&self.git_cwd(), entry.oid, title) {
            Ok(view) => self.open_diff_in_cell(view),
            Err(e)   => self.status.push_auto(format!("diff: {e}")),
        }
    }

    /// `c` on a log entry — stash the short SHA in the status bar so
    /// the user can read it off and paste it wherever.
    pub fn git_log_copy_sha(&mut self) {
        let Some(entry) = self.git_log.get(self.git_log_sel) else { return; };
        self.status.push_auto(format!("sha: {}", entry.sha_short));
    }

    /// Cached change list — kept as a method so callers don't repeatedly
    /// re-materialize it. Cheap, but not free.
    pub fn git_change_rows(&self) -> Vec<ChangeRow> {
        self.git.change_rows()
    }

    pub fn git_branch_move(&mut self, delta: isize) {
        let n = self.git.branches.len();
        if n == 0 { return; }
        let cur = self.git_branch_sel as isize;
        let next = (cur + delta).clamp(0, (n - 1) as isize) as usize;
        self.git_branch_sel = next;
    }

    pub fn git_change_move(&mut self, delta: isize) {
        let n = self.git_change_rows().len();
        if n == 0 { return; }
        let cur = self.git_change_sel as isize;
        let next = (cur + delta).clamp(0, (n - 1) as isize) as usize;
        self.git_change_sel = next;
    }

    /// Switch to the branch under the cursor. No-op if the cursor is
    /// already on the checked-out branch.
    pub fn git_switch_selected_branch(&mut self) {
        let Some(name) = self.git.branches.get(self.git_branch_sel).cloned() else { return; };
        if name == self.git.branch {
            self.status.push_auto(format!("already on {name}"));
            return;
        }
        if let Err(msg) = self.require_clean_op_state("switch") {
            self.status.push_auto(msg);
            return;
        }
        let cwd = self.git_cwd();
        match git::switch_branch(&cwd, &name) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("on {name}"));
            }
            Err(e) => self.status.push_auto(hint_switch_failure(&name, &e)),
        }
    }

    /// Return Err(msg) if the repo is in a mid-operation state (merge,
    /// rebase, etc.) — caller should bail rather than proceed with a
    /// destructive op that would entrench the half-finished state.
    fn require_clean_op_state(&self, label: &str) -> Result<(), String> {
        if self.git.op_state.is_clean() {
            Ok(())
        } else {
            Err(format!("{label}: repo is {} — finish or abort first", self.git.op_state.label()))
        }
    }

    /// `d` in branches — safe delete. Refuses if the branch has
    /// unmerged commits reachable only from it.
    pub fn git_delete_selected_branch(&mut self) {
        self.do_delete_branch(false);
    }

    /// `D` in branches — force delete. Queues a y/N confirm because
    /// dropping a branch with commits only reachable from that branch
    /// is the fastest way to lose work.
    pub fn git_force_delete_selected_branch(&mut self) {
        let Some(name) = self.git.branches.get(self.git_branch_sel).cloned() else { return; };
        self.request_confirm(PendingConfirm::ForceDeleteBranch { name });
    }

    fn do_delete_branch(&mut self, force: bool) {
        let Some(name) = self.git.branches.get(self.git_branch_sel).cloned() else { return; };
        let cwd = self.git_cwd();
        match git::delete_branch(&cwd, &name, force) {
            Ok(()) => {
                self.refresh_git();
                let verb = if force { "force-deleted" } else { "deleted" };
                self.status.push_auto(format!("{verb} {name}"));
            }
            Err(e) => self.status.push_auto(format!("delete: {e}")),
        }
    }

    /// `n` in branches — open command mode pre-filled with `git branch `
    /// so the user can name a new one.
    pub fn git_begin_new_branch(&mut self) {
        self.mode = Mode::Command { buffer: "git branch ".to_string() };
        self.status.clear();
        self.pending_jump = false;
    }

    /// Toggle staged/unstaged for the change under the cursor. Untracked
    /// entries get staged. Conflicted entries refuse — the user has to
    /// resolve the conflict first (edit the file and pick ours/theirs)
    /// because a plain `git add` on a file with `<<<<<<<` markers bakes
    /// the broken content into the next commit.
    pub fn git_toggle_selected_change(&mut self) {
        let Some(row) = self.git_change_rows().get(self.git_change_sel).cloned() else { return; };
        match row.group {
            crate::git::ChangeGroup::Staged     => self.git_unstage_path(&row.path),
            crate::git::ChangeGroup::Unstaged   => self.git_stage_path(&row.path),
            crate::git::ChangeGroup::Untracked  => self.git_stage_path(&row.path),
            crate::git::ChangeGroup::Conflicted => {
                self.status.push_auto("conflicted: resolve first".into());
            }
        }
    }

    /// `o` in changes — resolve conflict with our side (asks y/N).
    pub fn git_resolve_selected_ours(&mut self) {
        self.queue_resolve(crate::git::ConflictSide::Ours);
    }

    /// `t` in changes — resolve conflict with their side (asks y/N).
    pub fn git_resolve_selected_theirs(&mut self) {
        self.queue_resolve(crate::git::ConflictSide::Theirs);
    }

    fn queue_resolve(&mut self, side: crate::git::ConflictSide) {
        let Some(row) = self.git_change_rows().get(self.git_change_sel).cloned() else { return; };
        if row.group != crate::git::ChangeGroup::Conflicted {
            self.status.push_auto("not a conflicted row".into());
            return;
        }
        self.request_confirm(PendingConfirm::ResolveConflict {
            path: row.path.clone(),
            side,
        });
    }

    /// Apply a queued `ResolveConflict` — called from `resolve_confirm`.
    fn do_resolve(&mut self, path: &str, side: crate::git::ConflictSide) {
        let cwd = self.git_cwd();
        match git::resolve_conflict_side(&cwd, path, side) {
            Ok(()) => {
                self.refresh_git();
                let label = match side {
                    crate::git::ConflictSide::Ours   => "ours",
                    crate::git::ConflictSide::Theirs => "theirs",
                };
                self.status.push_auto(format!("resolved {path} ({label})"));
            }
            Err(e) => self.status.push_auto(format!("resolve: {e}")),
        }
    }

    /// `e` in changes — open the selected path in an editor cell so the
    /// user can resolve conflict markers by hand, then `s` to stage.
    pub fn git_open_selected_in_editor(&mut self) {
        let Some(row) = self.git_change_rows().get(self.git_change_sel).cloned() else { return; };
        let Some(wd) = self.git.workdir.clone() else { return; };
        let abs = wd.join(&row.path);
        self.open_path_in_cell(abs);
    }

    /// `v` in changes — open a diff view in its own cell. If the
    /// selected row is a conflicted file (merge in progress + unmerged
    /// entry), open a ConflictView instead so the user can actually
    /// resolve it, not just inspect it.
    pub fn git_open_diff_for_selected(&mut self) {
        let Some(row) = self.git_change_rows().get(self.git_change_sel).cloned() else { return; };
        let cwd = self.git_cwd();
        if matches!(row.group, crate::git::ChangeGroup::Conflicted) {
            let abs = cwd.join(&row.path);
            match crate::conflict::ConflictView::for_git_file(&abs) {
                Ok(view) if view.total_hunks() > 0 => {
                    if self.cells.len() >= MAX_CELLS {
                        self.status.push_auto(format!("max {MAX_CELLS} cells — close one first"));
                        return;
                    }
                    self.insert_cell_at_top(Cell::with_session(Session::Conflict(view)));
                    self.status.push_auto("conflict view — o/t/b/:w".into());
                    self.on_sessions_changed();
                    return;
                }
                Ok(_)  => { /* no markers yet — fall through to diff */ }
                Err(e) => {
                    self.status.push_auto(format!("conflict open failed: {e}"));
                    return;
                }
            }
        }
        match crate::diff::DiffView::for_row(&cwd, &row) {
            Ok(view) => self.open_diff_in_cell(view),
            Err(e)   => self.status.push_auto(format!("diff: {e}")),
        }
    }

    /// Remove the cell whose sole session is an untouched welcome
    /// buffer, if one exists. Welcome is a landing-page preview — any
    /// real cell spawn should overtake it rather than sit next to it.
    /// Adjusts `focus`, `last_cell_focus`, and `preview_cell_idx` the
    /// same way `cmd_close_target` does so an evict-then-insert flow
    /// leaves bookkeeping coherent.
    /// Enforce the invariant that a welcome cell may only exist when
    /// it's the *only* cell. Called from restore/switch paths that can
    /// end up with welcome sitting next to real work (e.g. an outgoing
    /// `kept` set including welcome, or a parked project that contained
    /// welcome when it was parked). Cheap: noop when no welcome exists,
    /// noop when it's solo.
    pub fn enforce_welcome_solo(&mut self) {
        if self.cells.len() <= 1 {
            return;
        }
        let has_welcome = self.cells.iter().any(|c| {
            c.sessions.len() == 1
                && matches!(&c.sessions[0], Session::Edit(ed) if ed.is_welcome)
        });
        if has_welcome {
            self.evict_welcome_cell();
        }
    }

    pub fn evict_welcome_cell(&mut self) -> Option<usize> {
        let idx = self.cells.iter().position(|c| {
            c.sessions.len() == 1
                && matches!(&c.sessions[0], Session::Edit(ed) if ed.is_welcome)
        })?;

        let focused_on_welcome = matches!(self.focus, FocusId::Cell(i) if i == idx);
        let focused_idx = if let FocusId::Cell(i) = self.focus { Some(i) } else { None };

        self.cells.remove(idx);

        if focused_on_welcome {
            if self.cells.is_empty() {
                self.set_focus(FocusId::Explorer);
            } else {
                let new_focus = idx.min(self.cells.len() - 1);
                self.set_focus(FocusId::Cell(new_focus));
            }
        } else if let Some(i) = focused_idx {
            if i > idx {
                self.set_focus(FocusId::Cell(i - 1));
            }
        }

        if self.last_cell_focus > idx {
            self.last_cell_focus -= 1;
        } else if self.last_cell_focus == idx {
            self.last_cell_focus = 0;
        }

        self.preview_cell_idx = match self.preview_cell_idx {
            Some(i) if i == idx => None,
            Some(i) if i > idx  => Some(i - 1),
            other               => other,
        };

        Some(idx)
    }

    /// Insert a cell at index 0 (master slot of the MasterStack
    /// layout). Existing cells shift by one — any focus or
    /// `last_cell_focus` pointing at a pre-existing cell is bumped so
    /// it keeps pointing at the same *cell*, not whichever slid into
    /// its slot. Focus is left where the caller had it; callers that
    /// want the new cell in focus should call `set_focus(Cell(0))`
    /// afterwards (or use `insert_cell_at_top`).
    ///
    /// Evicts the welcome cell (if any) before inserting — the
    /// landing-page buffer is a preview, not a real cell, and any
    /// spawn overtakes it.
    pub fn insert_cell_at_top_raw(&mut self, cell: Cell) {
        self.evict_welcome_cell();
        self.cells.insert(0, cell);
        if let FocusId::Cell(i) = self.focus {
            self.focus = FocusId::Cell(i + 1);
        }
        self.last_cell_focus = self.last_cell_focus.saturating_add(1);
        if let Some(i) = self.preview_cell_idx {
            self.preview_cell_idx = Some(i + 1);
        }
    }

    /// Insert + focus the new top cell. The common case for `:new`,
    /// `:split`, and "no editor anywhere" fallbacks.
    pub fn insert_cell_at_top(&mut self, cell: Cell) {
        self.insert_cell_at_top_raw(cell);
        self.set_focus(FocusId::Cell(0));
    }

    /// Open a diff view in a cell without stealing focus. The user
    /// triggered this from the Explorer/git pane — they want to keep
    /// scrolling changes/log, with the diff visible alongside. Reuses
    /// an existing diff cell (focused or otherwise) when one exists;
    /// otherwise inserts a fresh diff cell at the top of the stack.
    /// In every case the current `focus` stays put.
    pub fn open_diff_in_cell(&mut self, view: crate::diff::DiffView) {
        let title_echo = view.title.clone();

        // Any existing diff cell wins as the reuse target — we don't
        // care whether it's the focused one, since we're not going to
        // focus it anyway.
        if let Some(idx) = self.cells.iter()
            .position(|c| matches!(c.active_session(), Session::Diff(_)))
        {
            let cell = &mut self.cells[idx];
            cell.sessions[cell.active] = Session::Diff(view);
            self.status.push_auto(format!("diff {title_echo}"));
            return;
        }

        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {} cells — close one first", MAX_CELLS));
            return;
        }
        self.insert_cell_at_top_raw(Cell::with_session(Session::Diff(view)));
        self.status.push_auto(format!("diff {title_echo}"));
    }

    /// Open a path in an editor cell. If the focused cell is an editor
    /// it gets the file; otherwise the last-focused editor cell is
    /// reused; otherwise a new editor cell is created. Matches the
    /// behaviour of `Enter` on a file row in the sidebar.
    pub fn open_path_in_cell(&mut self, path: std::path::PathBuf) {
        let name = path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        if let Some(ed) = self.focused_editor_mut() {
            match ed.load(&path) {
                Ok(()) => self.status.push_auto(format!("opened {name}")),
                Err(e) => self.status.push_auto(format!("open failed: {e}")),
            }
            return;
        }
        if let Some(cell) = self.cells.get_mut(self.last_cell_focus) {
            if let Some(ed) = cell.active_session_mut().as_editor_mut() {
                match ed.load(&path) {
                    Ok(()) => {
                        let idx = self.last_cell_focus;
                        self.set_focus(FocusId::Cell(idx));
                        self.status.push_auto(format!("opened {name}"));
                    }
                    Err(e) => self.status.push_auto(format!("open failed: {e}")),
                }
                return;
            }
        }
        if self.cells.len() >= MAX_CELLS {
            self.status.push_auto(format!("max {} cells — close one first", MAX_CELLS));
            return;
        }
        let mut ed = Editor::empty();
        match ed.load(&path) {
            Ok(()) => {
                self.insert_cell_at_top(Cell::with_session(Session::Edit(ed)));
                self.status.push_auto(format!("opened {name} in new cell"));
            }
            Err(e) => self.status.push_auto(format!("open failed: {e}")),
        }
    }

    /// `d` in changes — discard the unstaged change (or dismiss the
    /// untracked file). Staged-only entries don't have a simple single-
    /// command "discard" meaning; we refuse and tell the user. Conflicted
    /// entries refuse for the same reason the stage toggle does — a
    /// discard would silently clobber one side's changes.
    ///
    /// Queues a y/N confirm rather than discarding directly — the
    /// action is irreversible for untracked files and loses unstaged
    /// work for modified ones.
    pub fn git_discard_selected_change(&mut self) {
        let Some(row) = self.git_change_rows().get(self.git_change_sel).cloned() else { return; };
        match row.group {
            crate::git::ChangeGroup::Unstaged
            | crate::git::ChangeGroup::Untracked => {
                self.request_confirm(PendingConfirm::DiscardChange {
                    path: row.path.clone(),
                });
            }
            crate::git::ChangeGroup::Staged     => {
                self.status.push_auto("discard: unstage first".into());
            }
            crate::git::ChangeGroup::Conflicted => {
                self.status.push_auto("conflicted: resolve first".into());
            }
        }
    }

    // ── git: path-based actions ──────────────────────────────────────────

    /// Cwd for git ops. Routes to the *active* discovered repo's
    /// workdir so nested repos under a project root get their own
    /// commands (status, stage, commit, …) targeted correctly. Falls
    /// back to the process cwd when no repo has been discovered yet
    /// (e.g. `:git init` in a fresh dir).
    fn git_cwd(&self) -> std::path::PathBuf {
        if let Some(repo) = self.git.active() {
            if let Some(wd) = repo.workdir.as_ref() {
                return wd.clone();
            }
        }
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    }

    pub fn git_stage_path(&mut self, rel: &str) {
        let rel = rel.to_string();
        self.do_git_action("stage", move |cwd| git::stage_path(cwd, &rel));
    }

    pub fn git_unstage_path(&mut self, rel: &str) {
        let rel = rel.to_string();
        self.do_git_action("unstage", move |cwd| git::unstage_path(cwd, &rel));
    }

    pub fn git_discard_path(&mut self, rel: &str) {
        let rel = rel.to_string();
        self.do_git_action("discard", move |cwd| git::discard_path(cwd, &rel));
    }

    pub fn git_stage_all(&mut self) {
        self.spawn_git_local_op("stage all", git::stage_all);
    }

    pub fn git_unstage_all(&mut self) {
        self.spawn_git_local_op("unstage all", git::unstage_all);
    }

    pub fn git_init_here(&mut self) {
        self.do_git_action("init", git::init_repo);
    }

    /// Offload a libgit2 write op to a background thread. Used for
    /// commands that stat-walk the worktree (`stage_all` /
    /// `unstage_all`) — those can block for seconds on large repos or
    /// OneDrive-synced roots and would otherwise freeze the UI.
    fn spawn_git_local_op<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(&std::path::Path) -> Result<(), String> + Send + 'static,
    {
        if !self.git.is_repo() {
            self.status.push_auto(format!("{label}: not a repo"));
            return;
        }
        let cwd = self.git_cwd();
        git::spawn_local_op(cwd, label.to_string(), self.tx.clone(), f);
        self.status.push_auto(format!("{label}…"));
    }

    fn do_git_action<F>(&mut self, label: &str, f: F)
    where
        F: FnOnce(&std::path::Path) -> Result<(), String>,
    {
        let cwd = self.git_cwd();
        match f(&cwd) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("{label} ok"));
            }
            Err(e) => self.status.push_auto(format!("{label}: {e}")),
        }
    }

    /// `m` key in the git pane — enter command mode pre-filled with
    /// `git commit `, ready for the user to type a message.
    pub fn git_begin_commit(&mut self) {
        self.mode = Mode::Command { buffer: "git commit ".to_string() };
        self.status.clear();
        self.pending_jump = false;
    }

    pub fn git_commit_now(&mut self, msg: &str) {
        if msg.is_empty() {
            self.status.push_auto("usage: :git commit <msg>".into());
            return;
        }
        // A merge/cherry-pick commit *is* part of finishing that op —
        // those states are fine. Only block if something else is in
        // flight (rebase/bisect/etc.) that a plain commit would leave
        // half-done.
        if !matches!(
            self.git.op_state,
            crate::git::RepoOpState::Clean
            | crate::git::RepoOpState::Merge
            | crate::git::RepoOpState::CherryPick
            | crate::git::RepoOpState::Revert
        ) {
            self.status.push_auto(format!(
                "commit: repo is {} — finish or abort first",
                self.git.op_state.label(),
            ));
            return;
        }
        // Refuse with unresolved conflicts — committing would bake
        // the `<<<<<<<` markers into the tree.
        if self.git.has_conflicts() {
            self.status.push_auto("commit: unresolved conflicts".into());
            return;
        }
        self.spawn_git_commit_shell(msg);
    }

    /// Shell out `git commit -m <msg>`. Background thread because:
    ///   * pre-commit hooks can run for seconds
    ///   * GPG signing can prompt for pinentry (blocks indefinitely)
    ///   * post-commit hooks (lints, tagging) can also be slow
    /// Using the shell CLI (not libgit2's `Repository::commit`) is
    /// what gets us hook + `commit.gpgsign` support in the first
    /// place — libgit2's API bypasses both.
    fn spawn_git_commit_shell(&mut self, msg: &str) {
        if !self.git.is_repo() {
            self.status.push_auto("commit: not a repo".into());
            return;
        }
        let cwd = self.git_cwd();
        git::spawn_git_shell(
            cwd,
            "commit".to_string(),
            vec!["commit".into(), "-m".into(), msg.to_string()],
            self.tx.clone(),
        );
        self.status.push_auto("commit…".into());
    }

    // ── git: shell-out background ops ────────────────────────────────────

    /// `git push`. First push on a branch without upstream tracking
    /// gets `-u origin <branch>` automatically so subsequent pushes
    /// can be bare.
    pub fn git_push(&mut self) {
        let args = if self.git.has_upstream {
            vec!["push".into()]
        } else if self.git.branch.is_empty() {
            vec!["push".into()]
        } else {
            let branch = self.git.branch.clone();
            vec!["push".into(), "-u".into(), "origin".into(), branch]
        };
        self.spawn_shell_git("push", args);
    }

    /// `git pull`. Honors the user's `pull.ff` / `pull.rebase` config
    /// rather than forcing `--ff-only` — plain `git pull` semantics.
    pub fn git_pull(&mut self) {
        if let Err(msg) = self.require_clean_op_state("pull") {
            self.status.push_auto(msg);
            return;
        }
        self.spawn_shell_git("pull", vec!["pull".into()]);
    }

    pub fn git_fetch(&mut self) {
        self.spawn_shell_git("fetch", vec!["fetch".into()]);
    }

    fn spawn_shell_git(&mut self, label: &str, args: Vec<String>) {
        if !self.git.is_repo() {
            self.status.push_auto(format!("{label}: not a repo"));
            return;
        }
        let cwd = self.git_cwd();
        git::spawn_git_shell(cwd, label.to_string(), args, self.tx.clone());
        self.status.push_auto(format!("{label}…"));
    }

    // ── git: :git <sub> dispatcher ───────────────────────────────────────

    fn cmd_git(&mut self, args: &str) {
        let (sub, rest) = split_kind_path(args);
        match sub {
            ""                  => self.set_focus(FocusId::Explorer),
            "status"            => self.set_focus(FocusId::Explorer),
            "init"              => self.git_init_here(),
            "refresh"           => self.refresh_git(),
            "stage" | "add"     => self.run_git_path_op("stage", rest, git::stage_path),
            "unstage" | "reset" => self.run_git_path_op("unstage", rest, git::unstage_path),
            "discard"           => self.run_git_path_op("discard", rest, git::discard_path),
            "stage-all"         => self.git_stage_all(),
            "unstage-all"       => self.git_unstage_all(),
            "commit"            => self.git_commit_now(rest),
            "push"              => self.git_push(),
            "pull"              => self.git_pull(),
            "fetch"             => self.git_fetch(),
            "log"               => self.cmd_git_log(rest),
            "branch"            => self.cmd_git_branch(rest),
            "branches"          => self.cmd_git_branch_list(),
            "switch" | "checkout" => self.cmd_git_switch(rest),
            "delete"            => self.cmd_git_delete(rest, false),
            "delete!"           => self.cmd_git_delete(rest, true),
            "amend"             => self.cmd_git_amend(rest),
            "merge"             => self.cmd_git_op_action("merge", rest),
            "rebase"            => self.cmd_git_op_action("rebase", rest),
            "cherry-pick"
            | "cp"              => self.cmd_git_op_action("cherry-pick", rest),
            "revert"            => self.cmd_git_op_action("revert", rest),
            "continue"          => self.cmd_git_op_dispatch_continue(),
            "abort"             => self.cmd_git_op_dispatch_abort(),
            "stash"             => self.cmd_git_stash(rest),
            "remote"            => self.cmd_git_remote(rest),
            _ => self.status.push_auto(format!("unknown: :git {sub}")),
        }
    }

    /// `:git amend [msg]` — re-write HEAD. Shells out so hooks +
    /// GPG signing apply. No `--no-verify`. Gated on clean op state
    /// *and* on having a HEAD to amend in the first place.
    fn cmd_git_amend(&mut self, rest: &str) {
        if let Err(msg) = self.require_clean_op_state("amend") {
            self.status.push_auto(msg);
            return;
        }
        let args: Vec<String> = if rest.is_empty() {
            vec!["commit".into(), "--amend".into(), "--no-edit".into()]
        } else {
            vec!["commit".into(), "--amend".into(), "-m".into(), rest.to_string()]
        };
        self.spawn_shell_git("amend", args);
    }

    /// Dispatch `:git <op> <sub>` where `<op>` is a multi-step op
    /// (merge / rebase / cherry-pick / revert) and `<sub>` is
    /// typically `continue`, `abort`, `skip`, or a target ref. The
    /// `continue`/`abort` shortcuts below figure out the right `<op>`
    /// from the current repo state automatically.
    fn cmd_git_op_action(&mut self, op: &str, rest: &str) {
        if rest.is_empty() {
            self.status.push_auto(format!("usage: :git {op} <ref|continue|abort|skip>"));
            return;
        }
        let args: Vec<String> = std::iter::once(op.to_string())
            .chain(rest.split_whitespace().map(str::to_string))
            .collect();
        self.spawn_shell_git(op, args);
    }

    /// `:git continue` — resolve based on current op_state so the user
    /// doesn't have to remember which op is in flight.
    fn cmd_git_op_dispatch_continue(&mut self) {
        use crate::git::RepoOpState::*;
        let args: Vec<String> = match self.git.op_state {
            Merge       => vec!["commit".into(), "--no-edit".into()],
            Rebase      => vec!["rebase".into(), "--continue".into()],
            CherryPick  => vec!["cherry-pick".into(), "--continue".into()],
            Revert      => vec!["revert".into(), "--continue".into()],
            Clean       => { self.status.push_auto("no op in progress".into()); return; }
            other       => {
                self.status.push_auto(format!("no continue for {}", other.label()));
                return;
            }
        };
        self.spawn_shell_git("continue", args);
    }

    /// `:git abort` — cancels whichever op is in flight.
    fn cmd_git_op_dispatch_abort(&mut self) {
        use crate::git::RepoOpState::*;
        let args: Vec<String> = match self.git.op_state {
            Merge        => vec!["merge".into(), "--abort".into()],
            Rebase       => vec!["rebase".into(), "--abort".into()],
            CherryPick   => vec!["cherry-pick".into(), "--abort".into()],
            Revert       => vec!["revert".into(), "--abort".into()],
            Bisect       => vec!["bisect".into(), "reset".into()],
            ApplyMailbox => vec!["am".into(), "--abort".into()],
            Clean        => { self.status.push_auto("no op in progress".into()); return; }
        };
        self.spawn_shell_git("abort", args);
    }

    // ── confirm prompts ──────────────────────────────────────────────────

    /// Queue a destructive action awaiting y/N. Displays the prompt in
    /// the status bar; the next keystroke resolves it (see
    /// `main::handle_key`).
    pub fn request_confirm(&mut self, pc: PendingConfirm) {
        self.status.push_auto(pc.prompt());
        self.pending_confirm = Some(pc);
    }

    /// Consume a pending confirm with the user's answer. `true` runs
    /// the action; `false` cancels with a status-bar note so the user
    /// sees that the key was registered.
    pub fn resolve_confirm(&mut self, yes: bool) {
        let Some(pc) = self.pending_confirm.take() else { return; };
        if !yes {
            self.status.push_auto("cancelled".into());
            return;
        }
        match pc {
            PendingConfirm::DiscardChange { path } => {
                // Reuse the lower-level path op — it already refreshes
                // git state + reports errors.
                self.git_discard_path(&path);
            }
            PendingConfirm::ForceDeleteBranch { name } => {
                let cwd = self.git_cwd();
                match git::delete_branch(&cwd, &name, true) {
                    Ok(()) => {
                        self.refresh_git();
                        self.status.push_auto(format!("force-deleted {name}"));
                    }
                    Err(e) => self.status.push_auto(format!("delete: {e}")),
                }
            }
            PendingConfirm::ResolveConflict { path, side } => {
                self.do_resolve(&path, side);
            }
            PendingConfirm::StashDrop { idx } => {
                let cwd = self.git_cwd();
                match git::stash_drop(&cwd, idx) {
                    Ok(()) => {
                        self.refresh_git();
                        self.status.push_auto(format!("stash drop {idx}"));
                    }
                    Err(e) => self.status.push_auto(format!("stash drop: {e}")),
                }
            }
            PendingConfirm::DeletePath { path, is_dir } => {
                let result = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                let name = path.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                match result {
                    Ok(()) => {
                        self.refresh_git();
                        self.explorer.refresh(&self.projects, self.cells.len());
                        self.status.push_auto(format!("deleted {name}"));
                    }
                    Err(e) => self.status.push_auto(format!("delete: {e}")),
                }
            }
        }
    }

    /// `:git stash [msg]` / `pop [idx]` / `apply [idx]` / `drop [idx]` /
    /// `list` — libgit2 has native stash APIs so we don't need to shell
    /// out. `push` with untracked files uses `-u` semantics by default
    /// because a dirty worktree with ONLY untracked changes otherwise
    /// silently no-ops and is a common "huh, why didn't it stash"
    /// complaint.
    fn cmd_git_stash(&mut self, rest: &str) {
        let (sub, tail) = split_kind_path(rest);
        let cwd = self.git_cwd();
        match sub {
            "" | "push" | "save" => {
                let msg = if tail.is_empty() { None } else { Some(tail) };
                // Include untracked only when the worktree is otherwise
                // clean except for untracked. Saves footguns while
                // matching user intent for the common "stash everything"
                // ask.
                let include_untracked =
                    self.git.untracked > 0 && self.git.staged + self.git.modified == 0;
                match git::stash_save(&cwd, msg, include_untracked) {
                    Ok(sha) => {
                        self.refresh_git();
                        self.status.push_auto(format!("stash {sha}"));
                    }
                    Err(e) => self.status.push_auto(format!("stash: {e}")),
                }
            }
            "pop"   => self.cmd_git_stash_index("pop", tail, git::stash_pop),
            "apply" => self.cmd_git_stash_index("apply", tail, git::stash_apply),
            "drop"  => {
                let idx = if tail.is_empty() { 0 } else {
                    match tail.parse::<usize>() {
                        Ok(n)  => n,
                        Err(_) => {
                            self.status.push_auto("usage: :git stash drop <index>".into());
                            return;
                        }
                    }
                };
                self.request_confirm(PendingConfirm::StashDrop { idx });
            }
            "list"  => match git::stash_list(&cwd) {
                Ok(v) if v.is_empty() => self.status.push_auto("no stashes".into()),
                Ok(v) => {
                    let head = v.first().cloned().unwrap_or_default();
                    self.status.push_auto(format!("{} stashes · {head}", v.len()));
                }
                Err(e) => self.status.push_auto(format!("stash: {e}")),
            },
            _ => self.status.push_auto(format!("unknown: :git stash {sub}")),
        }
    }

    fn cmd_git_stash_index<F>(&mut self, label: &str, tail: &str, f: F)
    where
        F: FnOnce(&std::path::Path, usize) -> Result<(), String>,
    {
        let idx = if tail.is_empty() { 0 } else {
            match tail.parse::<usize>() {
                Ok(n)  => n,
                Err(_) => {
                    self.status.push_auto(format!("usage: :git stash {label} <index>"));
                    return;
                }
            }
        };
        let cwd = self.git_cwd();
        match f(&cwd, idx) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("stash {label} {idx}"));
            }
            Err(e) => self.status.push_auto(format!("stash {label}: {e}")),
        }
    }

    /// `:git remote [list|add <name> <url>|rm <name>]`. No arg → list.
    fn cmd_git_remote(&mut self, rest: &str) {
        let (sub, tail) = split_kind_path(rest);
        let cwd = self.git_cwd();
        match sub {
            "" | "list" | "ls" => match git::list_remotes(&cwd) {
                Ok(v) if v.is_empty() => self.status.push_auto("no remotes".into()),
                Ok(v) => {
                    let summary = v.iter()
                        .map(|(n, u)| match u {
                            Some(url) => format!("{n} {url}"),
                            None      => n.clone(),
                        })
                        .collect::<Vec<_>>().join(", ");
                    self.status.push_auto(format!("remotes: {summary}"));
                }
                Err(e) => self.status.push_auto(format!("remote: {e}")),
            },
            "add" => {
                // Need `<name> <url>`; split the tail ourselves since
                // `split_kind_path` only splits on the first space.
                let mut parts = tail.splitn(2, char::is_whitespace);
                let name = parts.next().unwrap_or("").trim();
                let url  = parts.next().unwrap_or("").trim();
                if name.is_empty() || url.is_empty() {
                    self.status.push_auto("usage: :git remote add <name> <url>".into());
                    return;
                }
                match git::add_remote(&cwd, name, url) {
                    Ok(()) => self.status.push_auto(format!("remote add {name}")),
                    Err(e) => self.status.push_auto(format!("remote: {e}")),
                }
            }
            "rm" | "remove" | "delete" => {
                if tail.is_empty() {
                    self.status.push_auto("usage: :git remote rm <name>".into());
                    return;
                }
                match git::remove_remote(&cwd, tail) {
                    Ok(()) => self.status.push_auto(format!("remote rm {tail}")),
                    Err(e) => self.status.push_auto(format!("remote: {e}")),
                }
            }
            _ => self.status.push_auto(format!("unknown: :git remote {sub}")),
        }
    }

    fn run_git_path_op<F>(&mut self, label: &str, path: &str, f: F)
    where
        F: FnOnce(&std::path::Path, &str) -> Result<(), String>,
    {
        if path.is_empty() {
            self.status.push_auto(format!("usage: :git {label} <path>"));
            return;
        }
        let cwd = self.git_cwd();
        match f(&cwd, path) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("{label} {path}"));
            }
            Err(e) => self.status.push_auto(format!("{label}: {e}")),
        }
    }

    fn cmd_git_log(&mut self, rest: &str) {
        let limit = rest.parse::<usize>().unwrap_or(20);
        let cwd = self.git_cwd();
        match git::log_lines(&cwd, limit) {
            Ok(lines) if lines.is_empty() => self.status.push_auto("no commits".into()),
            Ok(lines) => {
                let first = lines.first().cloned().unwrap_or_default();
                self.status.push_auto(format!("{} commits · {first}", lines.len()));
            }
            Err(e) => self.status.push_auto(format!("log: {e}")),
        }
    }

    fn cmd_git_branch(&mut self, rest: &str) {
        if rest.is_empty() {
            self.cmd_git_branch_list();
            return;
        }
        let cwd = self.git_cwd();
        match git::create_branch(&cwd, rest) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("branch {rest}"));
            }
            Err(e) => self.status.push_auto(format!("branch: {e}")),
        }
    }

    fn cmd_git_branch_list(&mut self) {
        let cwd = self.git_cwd();
        match git::branch_names(&cwd) {
            Ok(names) if names.is_empty() => self.status.push_auto("no branches".into()),
            Ok(names) => {
                self.status.push_auto(format!("branches: {}", names.join(", ")));
            }
            Err(e) => self.status.push_auto(format!("branch: {e}")),
        }
    }

    // ── projects: :proj <sub> dispatcher ─────────────────────────────────

    fn cmd_proj(&mut self, args: &str) {
        let (sub, rest) = split_kind_path(args);
        match sub {
            ""                  => { self.set_focus(FocusId::Explorer); }
            "list" | "ls"       => self.project_list_summary(),
            "add"               => self.project_add(rest),
            "rm" | "remove"     => {
                if rest.is_empty() {
                    self.status.push_auto("usage: :proj rm <name>".into());
                } else {
                    self.project_remove_named(rest);
                }
            }
            "switch" | "sw" | "use" | "cd" => {
                if rest.is_empty() {
                    self.status.push_auto("usage: :proj switch <name>".into());
                } else {
                    self.project_switch_named(rest);
                }
            }
            "rename" => self.project_rename_active(rest),
            "refresh" => {
                self.projects.refresh_states();
                self.status.push_auto("projects refreshed".into());
            }
            _ => self.status.push_auto(format!("unknown: :proj {sub}")),
        }
    }

    fn cmd_git_delete(&mut self, rest: &str, force: bool) {
        if rest.is_empty() {
            let tail = if force { "!" } else { "" };
            self.status.push_auto(format!("usage: :git delete{tail} <branch>"));
            return;
        }
        if force {
            // Queue a confirm for the destructive path; the safe
            // delete below already refuses on unmerged commits, so a
            // prompt there would be noise.
            self.request_confirm(PendingConfirm::ForceDeleteBranch { name: rest.to_string() });
            return;
        }
        let cwd = self.git_cwd();
        match git::delete_branch(&cwd, rest, false) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("deleted {rest}"));
            }
            Err(e) => self.status.push_auto(format!("delete: {e}")),
        }
    }

    fn cmd_git_switch(&mut self, rest: &str) {
        if rest.is_empty() {
            self.status.push_auto("usage: :git switch <branch>".into());
            return;
        }
        if let Err(msg) = self.require_clean_op_state("switch") {
            self.status.push_auto(msg);
            return;
        }
        let cwd = self.git_cwd();
        match git::switch_branch(&cwd, rest) {
            Ok(()) => {
                self.refresh_git();
                self.status.push_auto(format!("on {rest}"));
            }
            Err(e) => self.status.push_auto(hint_switch_failure(rest, &e)),
        }
    }
}

/// Reshape a libgit2 switch error into actionable guidance. libgit2's
/// default message ("1 conflict prevents checkout") doesn't tell the
/// user *what* the next move is, so we rephrase.
fn hint_switch_failure(target: &str, raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("conflict") || lower.contains("overwrite") || lower.contains("untracked") {
        format!(
            "switch {target}: working tree conflicts — stash (`:git stash`) or commit, then retry"
        )
    } else {
        format!("switch: {raw}")
    }
}

/// A cell is "external to a project" iff its active session is an
/// editor whose file lives outside that project's root. PTYs, diffs,
/// conflicts, and scratch editors (no path) are always project-scoped
/// and get dropped when their project is closed.
/// True when the cell's active session is an editor with unsaved
/// changes. Used by `:q N` before deciding whether the non-focused
/// target needs the force-close flag.
fn cell_active_editor_is_dirty(cell: &Cell) -> bool {
    matches!(cell.active_session(), Session::Edit(e) if e.dirty)
        || matches!(cell.active_session(), Session::Hex(h) if h.dirty)
}

/// If the cell's active session is a claude/shell PTY currently
/// producing output (see `PtySession::is_busy`), returns a short label
/// for the status-bar message (e.g. "claude", "shell"). Non-PTY or
/// idle PTY cells return `None` — `:q` closes them normally.
fn cell_active_pty_busy_label(cell: &Cell) -> Option<&'static str> {
    pty_busy_label_from_session(cell.active_session())
}

/// Same busy check as `cell_active_pty_busy_label`, but takes a
/// session directly — used by `:Q` to scan every session in every
/// cell (not just the active one in each).
fn pty_busy_label_from_session(s: &Session) -> Option<&'static str> {
    match s {
        Session::Claude(p) if p.is_busy() => Some("claude"),
        Session::Shell(p)  if p.is_busy() => Some("shell"),
        _ => None,
    }
}

/// Spawn `sudo -S tee <path>` and feed it the password followed by the
/// file content. Returns the first line of stderr on failure so the
/// user sees "incorrect password" / "not in the sudoers file" instead
/// of a bare non-zero exit code.
///
/// Uses `-S` (password on stdin) and `-p ""` (no prompt echo) so the
/// only thing sudo writes to the tty is the password prompt we handle
/// ourselves via the status bar.
fn exec_sudo_write(path: &Path, content: &[u8], password: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let path_str = path.to_string_lossy().into_owned();
    let mut child = Command::new("sudo")
        .arg("-S")
        .arg("-p").arg("")
        .arg("tee")
        .arg(&path_str)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn sudo: {e}"))?;
    {
        let stdin = child.stdin.as_mut().ok_or_else(|| "sudo: no stdin".to_string())?;
        stdin.write_all(password.as_bytes()).map_err(|e| e.to_string())?;
        stdin.write_all(b"\n").map_err(|e| e.to_string())?;
        stdin.write_all(content).map_err(|e| e.to_string())?;
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("sudo failed");
    Err(first.to_string())
}

/// Parse the sudo-prefixed / force-bang command shortcuts. Returns the
/// sudo action to queue up (the password prompt opens after this
/// returns `Some`). Aliases handled here:
///   :sudo w             → Write
///   :sudo wq | :sudo x  → WriteClose
///   :sudo wQ            → WriteQuitApp
///   :w!                 → Write
///   :w!q                → WriteClose
///   :x!                 → WriteClose
pub fn parse_sudo_command(cmd: &str) -> Option<SudoAction> {
    let trimmed = cmd.trim();
    if let Some(rest) = trimmed.strip_prefix("sudo ") {
        return match rest.trim() {
            "w"  | "write"      => Some(SudoAction::Write),
            "wq" | "x"          => Some(SudoAction::WriteClose),
            "wQ"                => Some(SudoAction::WriteQuitApp),
            _                   => None,
        };
    }
    match trimmed {
        "w!"         => Some(SudoAction::Write),
        "w!q" | "x!" => Some(SudoAction::WriteClose),
        _            => None,
    }
}

/// A cell argument parsed from a command suffix.
#[derive(Copy, Clone, Debug)]
pub enum CellTarget {
    /// A specific cell (0-based).
    Idx(usize),
    /// Wildcard — apply the command to every cell.
    All,
}

/// Parse `"<base> <target>"` where target is either a 1-based cell
/// number or `*` (wildcard = all cells). Returns `(base, target)` on
/// success; `None` when there's no trailing argument or the argument
/// isn't recognized. Used to route `:q 2` / `:q *` and friends.
fn parse_cell_target(cmd: &str) -> Option<(&str, CellTarget)> {
    let (base, rest) = cmd.rsplit_once(' ')?;
    match rest.trim() {
        "*" => Some((base.trim(), CellTarget::All)),
        s   => {
            let n: usize = s.parse().ok()?;
            if n == 0 {
                return None;
            }
            Some((base.trim(), CellTarget::Idx(n - 1)))
        }
    }
}

fn cell_is_external_to(cell: &Cell, project_root: &std::path::Path) -> bool {
    match cell.active_session() {
        Session::Edit(ed) => match ed.path.as_ref() {
            Some(p) => !p.starts_with(project_root),
            None    => false,
        },
        _ => false,
    }
}

fn split_kind_path(spec: &str) -> (&str, &str) {
    match spec.split_once(' ') {
        Some((k, r)) => (k, r.trim()),
        None         => (spec, ""),
    }
}

/// Canonicalize `p` to its true on-disk casing (Windows file systems are
/// case-insensitive, so `exists()` alone can't catch a user typing the
/// wrong case). Returns `Some(actual)` only when the user's input and
/// the on-disk path differ by case alone — in every other case (already
/// matches, different components entirely, symlink indirection, UNC
/// variants) we return `None` and let the add proceed.
///
/// Strips Windows' `\\?\` verbatim prefix from `fs::canonicalize` output so
/// the returned path is suitable for display back to the user.
fn real_path_case(user_path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let canonical = std::fs::canonicalize(user_path).ok()?;
    let s = canonical.to_string_lossy();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(s.as_ref()).to_string();
    let actual = PathBuf::from(stripped);

    // Resolve the user's input to an absolute, dot-free path without
    // touching its casing: `PathBuf::join` doesn't collapse `..`, and
    // `fs::canonicalize` would rewrite components to the true on-disk
    // case (defeating the whole check). We walk components manually so
    // the final string preserves whatever the user typed, just made
    // absolute and cleaned of `.`/`..` so we can string-compare it
    // against `actual`.
    let base = if user_path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().ok()?
    };
    let mut resolved = PathBuf::new();
    for comp in base.join(user_path).components() {
        match comp {
            Component::ParentDir => { resolved.pop(); }
            Component::CurDir    => {}
            other                => resolved.push(other.as_os_str()),
        }
    }

    let user_norm = resolved.to_string_lossy().replace('/', "\\");
    let actual_norm = actual.to_string_lossy().to_string();

    if user_norm == actual_norm { return None; }
    if !user_norm.eq_ignore_ascii_case(&actual_norm) { return None; }
    Some(actual)
}

/// Clip a single output line to a status-bar-friendly length. 120 chars
/// is comfortable for most terminals; beyond that we add an ellipsis so
/// the user still sees where it was cut.
fn truncate_ex_line(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}…")
}

fn kind_label(k: SessionKind) -> &'static str {
    match k {
        SessionKind::Claude   => "claude",
        SessionKind::Shell    => "shell",
        SessionKind::Edit     => "edit",
        SessionKind::Hex      => "hex",
        SessionKind::Diff     => "diff",
        SessionKind::Conflict => "conflict",
    }
}

/// Reference card rendered by `:help`. Kept as one big string so it
/// stays trivial to edit — it's just a buffer's worth of text, not a
/// structured widget. Grouped by mode so users can scroll to the
/// section they want.
const HELP_TEXT: &str = "\
ACE — quick reference
─────────────────────

GLOBAL
  :               enter command mode
  Esc             leave Insert / Visual / Command → Normal
                  in a PTY cell already in Normal: send a literal ESC
                  byte to the child (claude / shell escape key)
  Ctrl-c          in a PTY cell: forward SIGINT to the child
                  in an editor + Insert: drop back to Normal
                  elsewhere: no-op (use `:q` / `:Q` to quit)
  Space           enter jump mode (status bar shows [0 explorer  1-9 cell])
  Space again     leave jump mode

MOUSE
  Left click      cell     → focus
                  explorer → focus + select / activate the row
                             (opens files, switches projects, toggles
                             dirs, restores minimized open cells)
  Double click    cell     → enter Insert
                  explorer → toggle git overview (same as `g`)
  Middle click    cell     → close it (same dirty-editor guard as :q;
                             forced drops still require `:q!`)
  Right click     cell     → arm swap/move (border turns orange)
                             then left-click a cell to swap + follow,
                             left-click the explorer to minimize, or
                             right-click again to cancel
  Scroll wheel    explorer → move selection up / down

JUMP MODE (after Space)
  0               toggle / focus explorer, leave jump mode
  1-9             focus cell N, leave jump mode
  Tab             cycle to next session in the focused cell (stay armed)
  Shift-Tab       cycle to previous session in the focused cell (stay armed)
  s               swap mode — next digit swaps content (focus stays at slot)
                    s 0     minimize focused cell
                    s 1-9   swap focused ↔ cell N
  m               move mode — next digit moves focused cell (focus follows)
                    m 0     minimize focused cell
                    m 1-9   move focused to slot N
                    m <any> minimize focused cell (stand-alone)
  q               quit the focused cell (same as `:q`)
  Space           leave jump mode
  anything else   leave jump mode (key is dropped)

EDITOR — NORMAL MODE
  Motion
    h j k l          char left / down / up / right
    w  b  e          word forward / back / word-end
    0  ^  $          line head / first non-blank / line end
    { }              paragraph back / forward
    gg  G  {N}G      top / bottom / line N
    ge               end of previous word
    gj  gk           down / up by *visual* row (soft-wrap aware)
    g0  g$           head / end of visual row
    zt  zz  zb       park cursor at top / middle / bottom of view
    zh  zl           scroll left / right (nowrap)
    Ctrl-d  Ctrl-u   half-page down / up
  Insert-mode entries (drop into INS)
    i  a             insert at / after cursor
    I  A             insert at first non-blank / end of line
    o  O             open line below / above
    s  S             substitute char / whole line
    C                change to end of line
  Edits
    x  X             delete char forward / back
    dd  yy  cc       delete / yank / change whole line
    D                delete to end of line
    J                join next line up
    p  P             paste after / before cursor
    r{c}             replace single char
    u   Ctrl-r       undo / redo
    .                repeat last change
    >  <             indent / dedent current line
  Operators + motion (count-aware)
    d{motion}        delete      (dw, d$, d3j, dgg, dG)
    c{motion}        change      (cw, c$, cc)
    y{motion}        yank        (yw, y$, yy)
  Text objects (after d / c / y)
    iw  aw           inner / around word
    i(  a(           inside / around parens  — also [ ] { } ' \"
  Counts
    3dw  5j  12gg    prefix any motion / operator with a count

EDITOR — INSERT MODE
  Esc              back to Normal (records the last edit for `.`)
  all others       insert at cursor

EDITOR — VISUAL MODE
  v   V            enter charwise / linewise visual
  motions          extend the selection
  d   x            delete selection
  y                yank selection
  c                change selection (drops into Insert)
  >   <            indent / dedent selected lines

EDITOR — SEARCH
  /pattern         forward search (regex)
  ?pattern         backward search
  n   N            repeat search forward / backward
  :nohl            clear highlight

EX COMMANDS
  :w               write focused editor
  :w <path>        save as (adopts the path). Used to name an unnamed
                   scratch buffer (`unknown [NEW]` → real file)
  :q  :q!          close focused cell / force-discard dirty buffer
  :wq  :x          write focused editor then close the focused cell
  :wQ  :wQ!        write focused editor then quit the whole app

SUDO (root-owned files)
  :sudo w          write focused editor via `sudo tee`; prompts for
                   password in the status bar (input is masked)
  :sudo wq         sudo-write then close focused cell
  :sudo x          alias for :sudo wq
  :sudo wQ         sudo-write then quit the whole app
  :w!              shorthand for :sudo w
  :w!q             shorthand for :sudo wq
  :x!              shorthand for :sudo wq
                   Esc in the password prompt cancels without spawning
                   sudo. Requires `sudo` on PATH.
  :{N}             jump to line N
  :%s/old/new/g    whole-file literal substitute
  :e <path>        open file in focused cell (or new cell if not an editor).
                   Nonexistent paths open as a `[NEW]` buffer — `:w` creates
                   the file on save. `~/…` expands to the home directory.
  :e!              reload from disk (discard buffer)
  :new kind [path] new cell: shell | claude | edit [path]
                   `:new shell <exec>` runs <exec> instead of the default
                   shell (e.g. `:new shell zsh`, `:new shell \"bash -i\"`).
  :c  :claude      shorthand for `:new claude`
  :s  :shell       shorthand for `:new shell`; `:shell <exec>` runs <exec>
  :tab kind [path] new session in the focused cell (tabbed)
  :split           split current cell's sessions into their own cells
  :conflict        3-way merge view for a modified-on-disk file
  :proj <name>     switch project by name
  :proj add <dir>  add a project (accepts `~/…` for the home dir)
  :proj rm  <name> close a project
  :git             open git overview in the explorer
  :layout <name>   master-bottom | mb | master-right | mr (default set in .acerc)
  :swap N          swap focused cell with cell N (1..9). Shorthand for
                   the `Space s N` / `Space m N` chords.
  :min :minimize   hide focused cell; it stays live in Open Cells.
  :min N           minimize cell N (1..9) without focusing it.
  :min *           minimize every cell except the focused one.
  :restore N       un-hide cell N and focus it (space+N does the same).
  :set wrap        soft-wrap long lines in the focused editor
  :set nowrap      horizontal scroll instead of wrapping
  :set list        show tabs (→---), trailing spaces (·), EOL (¬)
  :set nolist      hide listchars
  :help            this card
  :Q  :Q!          quit the app / force-quit with dirty buffers

CELL TARGETS
  Any of :q :q! :w :wq :x :close :bd :bd! accept a trailing
  cell number or `*` — `:q 3` closes cell 3, `:w *` saves every editor,
  `:wq *` writes all then closes all. (Sudo variants act on the
  focused cell only.)

EXPLORER — NORMAL
  j  k            move cursor
  Enter           preview selected file
  e               open selected file in a new cell
  a               add project (prefills `:proj add`)
  c               close selected project / cell (prefills `:proj rm` / `:q N`)
  o               open path (prefills `:e`)
  n               new file in the selected dir / project (prefills `:e <dir>/`)
  g               git overview
  PgUp  PgDn      jump between projects

TITLE BADGES
  [CLAUDE]        claude PTY cell                       (amber)
  [SHELL]         shell PTY cell                        (amber)
  [ACodeEditor]   built-in buffer (help, welcome)       (purple)
  [NEW]           file doesn't exist on disk yet        (green)
  [CONFLICT]      on-disk file changed externally       (red)
  [DELETED]       on-disk file was removed externally   (red)
  [EXTERNAL]      editor file is outside the project    (amber)

BORDER COLOURS (focused pane)
  Normal          cyan       default mode
  Insert          green      editor / PTY insert mode
  Visual          magenta    editor visual selection
  Swap / move     orange     source cell while a swap/move is armed
  Git overview    green      explorer in a git sub-mode overview / log
  Git branches    purple     explorer cursor in the branches list
  Git changes     orange     explorer cursor in the changes list

CLI
  ace                         welcome / global list / cwd-only (see `.acerc`)
  ace <dir> [<dir>…]          open those as projects
  ace <file> [<file>…]        one editor cell per file (nonexistent → [NEW])
  ace --update [<tag>]        update ace in place (latest release or a
                              specific tag, e.g. `ace --update v0.2.0`).
                              Runs the cargo-dist installer for this OS.
  ace --version               print version and exit

~/.acerc   (key = value, spaces around `=` optional — `key=value` also works)
  shell     = \"powershell -NoLogo\"   # shell cell argv
  claude    = \"claude\"               # claude cell argv
  claude_skip_permissions = true
  on_launch = \"welcome\"              # what `ace` (no args) opens:
                                     #   \"welcome\" (default) — landing page
                                     #   \"cwd\"               — cwd as sole project
                                     #   \"global\"            — saved project list
  layout    = \"master-bottom\"        # default cell tiling
  auto_gitignore = true              # inject .acedata / .acerc into a
                                     # project's .gitignore on first save;
                                     # set to false to opt out

GIT — EXPLORER SUB-MODES
  g               overview (from Normal); anywhere inside git →
                  exit straight back to Normal (one step, unlike Esc)
  Esc             step one level up (Branches/Changes/Log → Overview
                  → Normal)
  b               branches  (in overview: j/k nav, Enter check out, D delete)
  c               changes   (in overview: s stage, u unstage, d discard, Enter diff)
  l               git log

CONFLICT CELL (3-way merge)
  j  k            next / previous hunk
  o               resolve with ours
  t               resolve with theirs
  b               keep both
  e               hand-edit (drops into Insert on the hunk's textarea)
  :w              save the resolved file
";

