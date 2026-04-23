//! Project rail model.
//!
//! A *project* is a folder on disk plus a display name. The rail shows
//! every project the user has opened; switching to one `set_current_dir`s
//! into its root so files + git + new sessions pick it up.
//!
//! Persistence lives in `~/.ace/projects.toml`, hand-rolled so we
//! don't take a TOML-parser dependency for a dozen paths.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::git::GitSnapshot;

/// Aggregate state of a project, derived from a git snapshot of its
/// root. v1 only uses repo health — session-level state (claude idle,
/// claude working, attention) is future work.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProjectState {
    None,       // ○  no repo / unreachable path
    Ok,         // ●  clean working tree
    Working,    // ◐  dirty / staged / untracked
    Error,      // ✕  merge conflicts
}

impl ProjectState {
    pub fn glyph(self) -> char {
        match self {
            ProjectState::None    => '○',
            ProjectState::Ok      => '●',
            ProjectState::Working => '◐',
            ProjectState::Error   => '✕',
        }
    }

    pub fn from_snapshot(snap: &GitSnapshot) -> Self {
        if !snap.is_repo() {
            return ProjectState::None;
        }
        if snap.has_conflicts() {
            return ProjectState::Error;
        }
        if snap.is_clean() {
            ProjectState::Ok
        } else {
            ProjectState::Working
        }
    }

    /// Aggregate across every repo discovered under a project root.
    /// Any repo with conflicts wins (Error); otherwise any dirty repo
    /// demotes to Working; all clean → Ok; empty set → None. Mirrors
    /// the dot glyph the user sees next to the project header when
    /// the project has nested repos.
    pub fn from_multi(multi: &crate::git::MultiRepo) -> Self {
        if multi.repos.is_empty() {
            return ProjectState::None;
        }
        let mut worst = ProjectState::Ok;
        for r in &multi.repos {
            let s = ProjectState::from_snapshot(r);
            worst = match (worst, s) {
                (ProjectState::Error, _) | (_, ProjectState::Error)     => ProjectState::Error,
                (ProjectState::Working, _) | (_, ProjectState::Working) => ProjectState::Working,
                (ProjectState::Ok, _) | (_, ProjectState::Ok)           => ProjectState::Ok,
                _                                                       => worst,
            };
        }
        worst
    }
}

/// One discovered repo under a project, cached for the cross-project
/// REPOSITORIES list in git mode. Per-repo state is kept so each row
/// can render its own dot without reopening the repo on every frame.
#[derive(Clone, Debug)]
pub struct RepoInfo {
    pub root:  PathBuf,
    pub state: ProjectState,
}

/// One project's recomputed rail entry. Built off the UI thread and
/// applied back to `ProjectList` by the event loop. Carries the root
/// path (not just an index) so the apply step can skip stale entries
/// if the rail shuffled while discovery was in flight.
#[derive(Clone, Debug)]
pub struct RailRefresh {
    pub root:  PathBuf,
    pub state: ProjectState,
    pub repos: Vec<RepoInfo>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub name:  String,
    pub root:  PathBuf,
    pub state: ProjectState,
    /// Every git repo discovered under `root` (root repo first if
    /// present, then nested). Rebuilt by `refresh_states`.
    pub repos: Vec<RepoInfo>,
}

impl Project {
    fn new(root: PathBuf) -> Self {
        // Absolutize (without canonicalizing) so the stored root always
        // matches git2's absolute workdir — strip_prefix-based lookups
        // (git tinting, status_for) break if the root is relative.
        // `std::path::absolute` preserves symlinks and OneDrive reparse
        // points, unlike `fs::canonicalize`.
        let root = std::path::absolute(&root).unwrap_or(root);
        let name = default_name(&root);
        Self { name, root, state: ProjectState::None, repos: Vec::new() }
    }
}

fn default_name(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

pub struct ProjectList {
    pub projects: Vec<Project>,
    pub active:   usize,    // index into `projects`; meaningless if empty
    /// Whether this list owns the persistent global store at
    /// `~/.ace/projects.toml`. `save()` is a no-op when false so session-
    /// only invocations (`ace <dirs>`, `cwd_only` .acerc, `ace <files>`)
    /// never scribble on the global list.
    persistent: bool,
}

impl ProjectList {
    /// `ace` with no args (and no `.acerc` cwd_only) — load the global
    /// saved list. `cwd` is *not* auto-added anymore; if cwd matches an
    /// entry it becomes active, otherwise active lands on index 0 (or
    /// stays 0 for an empty list — callers must guard).
    pub fn global(cwd: &Path) -> Self {
        let projects = read_persisted().unwrap_or_default();
        let active = projects
            .iter()
            .position(|p| paths_equivalent(&p.root, cwd))
            .unwrap_or(0);
        // Defer refresh_states — it calls MultiRepo::discover for every
        // project, which walks trees + loads git status. Done synchronously
        // here it blocks the first frame. The bootstrap thread kicked off
        // from App::new refreshes state in the background and posts a
        // GitBootstrap event. First frame renders with state=None dots.
        Self { projects, active, persistent: true }
    }

    /// Session-only: a single project rooted at `cwd`. Used both for
    /// `.acerc` cwd_only=true and as the fallback when the global list
    /// is empty.
    pub fn cwd_only(cwd: &Path) -> Self {
        let projects = vec![Project::new(cwd.to_path_buf())];
        Self { projects, active: 0, persistent: false }
    }

    /// Session-only: projects from explicit `ace <dir1> <dir2> …` args.
    /// Active = index matching `cwd` if present, else 0.
    pub fn explicit(dirs: Vec<PathBuf>, cwd: &Path) -> Self {
        let projects: Vec<Project> = dirs.into_iter().map(Project::new).collect();
        let active = projects
            .iter()
            .position(|p| paths_equivalent(&p.root, cwd))
            .unwrap_or(0);
        Self { projects, active, persistent: false }
    }

    /// Session-only: no projects at all. Used for files-only invocations
    /// (`ace foo.txt bar.rs`) — the session has editor cells but no
    /// project rail. `active = 0` is a dead index and must not be
    /// dereferenced without a length check.
    pub fn empty() -> Self {
        Self { projects: Vec::new(), active: 0, persistent: false }
    }

    pub fn save(&self) -> io::Result<()> {
        if !self.persistent {
            return Ok(());
        }
        let Some(path) = persist_path() else { return Ok(()); };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serialize(&self.projects))
    }

    /// Recompute every project's `state` by taking a fresh snapshot of
    /// its root. Cheap enough for the sizes we expect; safe to call
    /// after switching or on explicit refresh.
    pub fn refresh_states(&mut self) {
        for p in self.projects.iter_mut() {
            if !p.root.exists() {
                p.state = ProjectState::None;
                p.repos.clear();
                continue;
            }
            let multi = crate::git::MultiRepo::discover(&p.root);
            p.state = ProjectState::from_multi(&multi);
            p.repos = multi.repos.iter()
                .filter_map(|r| {
                    let root = r.workdir.clone()?;
                    Some(RepoInfo { root, state: ProjectState::from_snapshot(r) })
                })
                .collect();
        }
    }

    /// Off-thread twin of `refresh_states`: computes each project's
    /// rail entry without touching `self`. Caller applies via
    /// `apply_rail_refresh` back on the UI thread.
    pub fn compute_rail(roots: &[PathBuf]) -> Vec<RailRefresh> {
        roots.iter().map(|root| {
            if !root.exists() {
                return RailRefresh {
                    root: root.clone(),
                    state: ProjectState::None,
                    repos: Vec::new(),
                };
            }
            let multi = crate::git::MultiRepo::discover(root);
            let state = ProjectState::from_multi(&multi);
            let repos = multi.repos.iter()
                .filter_map(|r| {
                    let root = r.workdir.clone()?;
                    Some(RepoInfo { root, state: ProjectState::from_snapshot(r) })
                })
                .collect();
            RailRefresh { root: root.clone(), state, repos }
        }).collect()
    }

    /// Apply a batch of off-thread rail refreshes. Matches each refresh
    /// to its project by root path (not index) so shuffles while the
    /// work was in flight don't misapply.
    pub fn apply_rail_refresh(&mut self, batch: Vec<RailRefresh>) {
        for r in batch {
            if let Some(p) = self.projects.iter_mut().find(|p| paths_equivalent(&p.root, &r.root)) {
                p.state = r.state;
                p.repos = r.repos;
            }
        }
    }

    /// Add a project, skipping duplicates. Returns its index. The path
    /// is stored as given — matching git2's workdir bit-for-bit is more
    /// important than canonicalizing symlinks (OneDrive reparse points
    /// would otherwise rewrite the path and break prefix-strip lookups).
    pub fn add(&mut self, root: PathBuf) -> usize {
        if let Some(i) = self.projects.iter().position(|p| paths_equivalent(&p.root, &root)) {
            return i;
        }
        self.projects.push(Project::new(root));
        self.projects.len() - 1
    }

    /// Remove by index. Adjusts active to stay in bounds. Allows going
    /// down to zero — the App layer handles the no-projects state by
    /// hiding the explorer.
    pub fn remove(&mut self, idx: usize) -> bool {
        if idx >= self.projects.len() {
            return false;
        }
        self.projects.remove(idx);
        if self.projects.is_empty() {
            self.active = 0;
        } else if self.active >= self.projects.len() {
            self.active = self.projects.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        true
    }

    pub fn find_by_name(&self, name: &str) -> Option<usize> {
        self.projects.iter().position(|p| p.name == name)
    }
}

/// Compare two paths as if canonicalized — for dedup only. We never
/// store the canonical form, because on Windows it resolves OneDrive
/// and symlink reparse points into a different path than what git2
/// uses as its workdir, which breaks prefix-strip lookups in the file
/// tree tinting.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let ca = std::fs::canonicalize(a).map(|p| strip_unc_prefix(&p));
    let cb = std::fs::canonicalize(b).map(|p| strip_unc_prefix(&p));
    matches!((ca, cb), (Ok(x), Ok(y)) if x == y)
}

#[cfg(windows)]
fn strip_unc_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    // Accept both slash forms; our own serializer wrote forward-slash
    // variants for a while, so we see `//?/C:/…` as well as `\\?\C:\…`.
    for prefix in [r"\\?\", "//?/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            // Preserve real UNC server paths (`\\?\UNC\server\share`).
            if !rest.starts_with("UNC\\") && !rest.starts_with("UNC/") {
                return PathBuf::from(rest);
            }
        }
    }
    p.to_path_buf()
}

#[cfg(not(windows))]
fn strip_unc_prefix(p: &Path) -> PathBuf {
    p.to_path_buf()
}

fn persist_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ace").join("projects.toml"))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    { std::env::var_os("USERPROFILE").map(PathBuf::from) }
    #[cfg(not(windows))]
    { std::env::var_os("HOME").map(PathBuf::from) }
}

fn read_persisted() -> Option<Vec<Project>> {
    let path = persist_path()?;
    let content = fs::read_to_string(&path).ok()?;
    Some(deserialize(&content))
}

// ── minimal TOML-ish ser/de ─────────────────────────────────────────────
//
// Format:
//   [[project]]
//   name = "ACodeTerm"
//   root = "C:/Users/arcar/…"
//
// Values are always quoted strings. Backslashes get written as forward
// slashes so Windows paths round-trip without needing escape handling.

fn serialize(projects: &[Project]) -> String {
    let mut s = String::new();
    s.push_str("# ACodeTerm projects — auto-managed; edit carefully.\n\n");
    for p in projects {
        s.push_str("[[project]]\n");
        s.push_str(&format!("name = {}\n", quote(&p.name)));
        s.push_str(&format!("root = {}\n\n", quote(&p.root.to_string_lossy().replace('\\', "/"))));
    }
    s
}

fn deserialize(content: &str) -> Vec<Project> {
    let mut out = Vec::new();
    let mut current: Option<(String, String)> = None;   // (name, root) being built

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[project]]" {
            if let Some((name, root)) = current.take() {
                if !root.is_empty() {
                    out.push(make_project(name, root));
                }
            }
            current = Some((String::new(), String::new()));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue; };
        let key = key.trim();
        let value = unquote(value.trim());
        if let Some((name, root)) = current.as_mut() {
            match key {
                "name" => *name = value,
                "root" => *root = value,
                _      => {}
            }
        }
    }
    if let Some((name, root)) = current {
        if !root.is_empty() {
            out.push(make_project(name, root));
        }
    }
    out
}

fn make_project(name: String, root: String) -> Project {
    // Normalize in case a prior version wrote a `\\?\` path. Going
    // forward we never write one, but this keeps old files readable.
    let root_path = strip_unc_prefix(&PathBuf::from(root));
    let display = if name.is_empty() { default_name(&root_path) } else { name };
    Project { name: display, root: root_path, state: ProjectState::None, repos: Vec::new() }
}

fn quote(s: &str) -> String {
    // Escape `"` and `\` for TOML basic strings.
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
