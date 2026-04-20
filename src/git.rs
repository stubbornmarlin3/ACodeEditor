use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use git2::{BranchType, IndexAddOption, ObjectType, Repository, RepositoryState, Status, StatusOptions};

use crate::events::AppEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Conflict,
    Added,
    Deleted,
    Modified,
    Renamed,
    Untracked,
    Ignored,
}

impl FileStatus {
    /// Priority for "worst-status-wins" directory aggregation — lower
    /// number wins. Conflicts still dominate. The rest mirrors ACode's
    /// file-tree tint order (`modified → deleted → added → renamed →
    /// untracked`), which biases folder tint toward the loudest colour
    /// (edits) rather than "whatever happens to come first".
    pub fn priority(self) -> u8 {
        match self {
            FileStatus::Conflict  => 0,
            FileStatus::Modified  => 1,
            FileStatus::Deleted   => 2,
            FileStatus::Added     => 3,
            FileStatus::Renamed   => 4,
            FileStatus::Untracked => 5,
            FileStatus::Ignored   => 6,
        }
    }
}

/// Which section of the changes list a row belongs to. A single file
/// can appear in more than one (e.g. staged *and* unstaged edits) —
/// we emit a row per group it qualifies for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeGroup {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

impl ChangeGroup {
    pub fn label(self) -> &'static str {
        match self {
            ChangeGroup::Conflicted => "CONFLICTED",
            ChangeGroup::Staged     => "STAGED",
            ChangeGroup::Unstaged   => "UNSTAGED",
            ChangeGroup::Untracked  => "UNTRACKED",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChangeRow {
    pub group:  ChangeGroup,
    pub path:   String,
    pub status: FileStatus,
}

/// Mid-operation repo state. "Clean" means no multi-step op is in
/// flight; anything else is a half-finished merge / rebase / etc.
/// that must be completed or aborted before destructive operations
/// (branch switch, pull, commit) are safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoOpState {
    Clean,
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    ApplyMailbox,
}

impl RepoOpState {
    pub fn is_clean(self) -> bool { matches!(self, RepoOpState::Clean) }

    pub fn label(self) -> &'static str {
        match self {
            RepoOpState::Clean        => "clean",
            RepoOpState::Merge        => "merging",
            RepoOpState::Rebase       => "rebasing",
            RepoOpState::CherryPick   => "cherry-picking",
            RepoOpState::Revert       => "reverting",
            RepoOpState::Bisect       => "bisecting",
            RepoOpState::ApplyMailbox => "applying",
        }
    }
}

/// Point-in-time read of the repo's state. Created by [`GitSnapshot::load`].
/// When not in a repo, [`is_repo`] is false and everything is empty/zero.
pub struct GitSnapshot {
    pub branch:        String,
    pub ahead:         u32,
    pub behind:        u32,
    pub staged:        u32,
    pub modified:      u32,
    pub untracked:     u32,
    pub conflicts:     u32,
    pub workdir:       Option<PathBuf>,
    pub branches:      Vec<String>,
    /// Does the current branch have an upstream tracking ref set? If
    /// false, a push needs `-u origin <branch>` to establish tracking.
    pub has_upstream:  bool,
    /// Mid-operation state (merge/rebase/cherry-pick/…). When anything
    /// other than `Clean`, we gate destructive ops on the caller side.
    pub op_state:      RepoOpState,
    /// Number of stash entries on refs/stash. Shown in the git footer
    /// so the user can see at a glance whether `stash pop` would have
    /// something to do.
    pub stash_count:   u32,
    statuses:          HashMap<String, Status>,
}

impl GitSnapshot {
    pub fn empty() -> Self {
        Self {
            branch:       String::new(),
            ahead:        0,
            behind:       0,
            staged:       0,
            modified:     0,
            untracked:    0,
            conflicts:    0,
            workdir:      None,
            branches:     Vec::new(),
            has_upstream: false,
            op_state:     RepoOpState::Clean,
            stash_count:  0,
            statuses:     HashMap::new(),
        }
    }

    pub fn load(start: &Path) -> Self {
        // `mut` because `stash_foreach` needs `&mut Repository` —
        // libgit2 treats stash iteration as a write-adjacent op even
        // though it only reads refs.
        let mut repo = match Repository::discover(start) {
            Ok(r) => r,
            Err(_) => return Self::empty(),
        };
        let workdir = repo.workdir().map(Path::to_path_buf);

        let (branch, ahead, behind, has_upstream) = read_head(&repo);

        let mut opts = StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false)
            .exclude_submodules(true)
            // Rename detection is off by default in libgit2 — without
            // these flags INDEX_RENAMED / WT_RENAMED bits are never set
            // and the `Renamed` variant is unreachable. These do extra
            // similarity work but it's cheap at status-walk time and
            // makes renames show up correctly in both tinting and the
            // changes list.
            .renames_head_to_index(true)
            .renames_index_to_workdir(true);

        let mut statuses: HashMap<String, Status> = HashMap::new();
        let mut staged    = 0u32;
        let mut modified  = 0u32;
        let mut untracked = 0u32;
        let mut conflicts = 0u32;

        if let Ok(entries) = repo.statuses(Some(&mut opts)) {
            for entry in entries.iter() {
                let flags = entry.status();

                if flags.contains(Status::CONFLICTED) {
                    conflicts += 1;
                }
                if flags.intersects(
                    Status::INDEX_NEW | Status::INDEX_MODIFIED | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED | Status::INDEX_TYPECHANGE,
                ) {
                    staged += 1;
                }
                if flags.intersects(
                    Status::WT_MODIFIED | Status::WT_DELETED
                    | Status::WT_TYPECHANGE | Status::WT_RENAMED,
                ) {
                    modified += 1;
                }
                if flags.contains(Status::WT_NEW) {
                    untracked += 1;
                }

                if let Some(path) = entry.path() {
                    statuses.insert(path.to_string(), flags);
                }
            }
        }

        let branches = list_branches(&repo, &branch);
        let op_state = classify_op_state(repo.state());
        let stash_count = count_stashes(&mut repo);

        Self {
            branch,
            ahead, behind, staged, modified, untracked, conflicts,
            workdir, branches, has_upstream, op_state, stash_count, statuses,
        }
    }

    pub fn is_clean(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicts == 0
    }

    pub fn has_conflicts(&self) -> bool {
        self.conflicts > 0
    }

    pub fn is_repo(&self) -> bool {
        self.workdir.is_some()
    }

    /// File-tree tinting: worst-status-wins per file.
    pub fn status_for(&self, abs_path: &Path) -> Option<FileStatus> {
        let wd  = self.workdir.as_ref()?;
        let rel = abs_path.strip_prefix(wd).ok()?;
        let key = rel.to_string_lossy().replace('\\', "/");
        self.statuses.get(&key).copied().map(classify_tree)
    }

    /// Flat, grouped list of changed files for the git pane's changes
    /// section. Matches ACode's emission rules:
    ///
    /// * one row per path per `(staged, unstaged, untracked)` it
    ///   qualifies for — a file with both staged and unstaged edits
    ///   appears **twice**, once in each group
    /// * staged rows are classified by the index bit (added/modified/
    ///   deleted/renamed), unstaged rows by the worktree bit
    /// * `WT_NEW` always emits an Untracked row (the working tree and
    ///   index bits are disjoint here — libgit2 never sets both)
    pub fn change_rows(&self) -> Vec<ChangeRow> {
        let mut conflicted = Vec::new();
        let mut staged     = Vec::new();
        let mut unstaged   = Vec::new();
        let mut untracked  = Vec::new();

        for (path, flags) in &self.statuses {
            // Conflicted paths appear *only* in the conflicted group —
            // they typically also have WT_MODIFIED or INDEX_* bits set,
            // but showing one path in three places ("conflicted / staged
            // / unstaged" for the same file) is just noise. The conflict
            // has to be resolved before any staging matters.
            if flags.contains(Status::CONFLICTED) {
                conflicted.push(ChangeRow {
                    group:  ChangeGroup::Conflicted,
                    path:   path.clone(),
                    status: FileStatus::Conflict,
                });
                continue;
            }
            if flags.intersects(
                Status::INDEX_NEW | Status::INDEX_MODIFIED | Status::INDEX_DELETED
                | Status::INDEX_RENAMED | Status::INDEX_TYPECHANGE,
            ) {
                staged.push(ChangeRow {
                    group:  ChangeGroup::Staged,
                    path:   path.clone(),
                    status: classify_index(*flags),
                });
            }
            if flags.intersects(
                Status::WT_MODIFIED | Status::WT_DELETED
                | Status::WT_TYPECHANGE | Status::WT_RENAMED,
            ) {
                unstaged.push(ChangeRow {
                    group:  ChangeGroup::Unstaged,
                    path:   path.clone(),
                    status: classify_wt(*flags),
                });
            }
            if flags.contains(Status::WT_NEW) {
                untracked.push(ChangeRow {
                    group:  ChangeGroup::Untracked,
                    path:   path.clone(),
                    status: FileStatus::Untracked,
                });
            }
        }

        for v in [&mut conflicted, &mut staged, &mut unstaged, &mut untracked] {
            v.sort_by(|a, b| a.path.cmp(&b.path));
        }
        let mut out = Vec::with_capacity(
            conflicted.len() + staged.len() + unstaged.len() + untracked.len(),
        );
        out.extend(conflicted);
        out.extend(staged);
        out.extend(unstaged);
        out.extend(untracked);
        out
    }

    /// Aggregated status for a directory: the "worst" classification
    /// among any tracked child (recursive). Used to tint folders in the
    /// file tree the same way files are tinted.
    pub fn dir_status(&self, abs_dir: &Path) -> Option<FileStatus> {
        let wd  = self.workdir.as_ref()?;
        let rel = abs_dir.strip_prefix(wd).ok()?;
        let rel_slash = rel.to_string_lossy().replace('\\', "/");
        let prefix = if rel_slash.is_empty() {
            String::new()
        } else {
            format!("{rel_slash}/")
        };

        let mut worst: Option<FileStatus> = None;
        for (path, flags) in &self.statuses {
            if !prefix.is_empty() && !path.starts_with(&prefix) {
                continue;
            }
            let s = classify_tree(*flags);
            worst = Some(match worst {
                None                              => s,
                Some(w) if s.priority() < w.priority() => s,
                Some(w)                           => w,
            });
        }
        worst
    }

}

/// Per-file classification for sidebar tinting. Order of checks
/// matches ACode's intent — modified beats deleted, which beats
/// added, which beats renamed, which beats untracked. Conflicts win
/// outright.
fn classify_tree(s: Status) -> FileStatus {
    if s.contains(Status::CONFLICTED) { return FileStatus::Conflict; }
    if s.intersects(
        Status::INDEX_MODIFIED | Status::WT_MODIFIED
        | Status::INDEX_TYPECHANGE | Status::WT_TYPECHANGE,
    ) { return FileStatus::Modified; }
    if s.intersects(Status::INDEX_DELETED | Status::WT_DELETED) { return FileStatus::Deleted; }
    if s.contains(Status::INDEX_NEW) { return FileStatus::Added; }
    if s.intersects(Status::INDEX_RENAMED | Status::WT_RENAMED) { return FileStatus::Renamed; }
    if s.contains(Status::WT_NEW) { return FileStatus::Untracked; }
    if s.contains(Status::IGNORED) { return FileStatus::Ignored; }
    FileStatus::Modified
}

/// Index-side classification: which kind of staged change this path
/// has. `INDEX_NEW` is checked first so a freshly-added file reports
/// as "added" even when other bits are also set.
fn classify_index(s: Status) -> FileStatus {
    if s.contains(Status::INDEX_NEW)      { return FileStatus::Added; }
    if s.contains(Status::INDEX_MODIFIED) { return FileStatus::Modified; }
    if s.contains(Status::INDEX_DELETED)  { return FileStatus::Deleted; }
    if s.contains(Status::INDEX_RENAMED)  { return FileStatus::Renamed; }
    FileStatus::Modified
}

/// Worktree-side classification: kind of unstaged change this path
/// has. (`WT_NEW` / untracked is handled on its own path in
/// `change_rows`, not via this helper.)
fn classify_wt(s: Status) -> FileStatus {
    if s.contains(Status::WT_MODIFIED) { return FileStatus::Modified; }
    if s.contains(Status::WT_DELETED)  { return FileStatus::Deleted; }
    if s.contains(Status::WT_RENAMED)  { return FileStatus::Renamed; }
    FileStatus::Modified
}

/// Reads HEAD for (branch-or-detached name, ahead, behind, has_upstream).
/// `has_upstream` is true iff the current branch has a configured
/// upstream tracking ref (regardless of ahead/behind being 0).
fn read_head(repo: &Repository) -> (String, u32, u32, bool) {
    let head = match repo.head() {
        Ok(h) => h,
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => {
            // Fresh repo or a pre-first-commit state: HEAD points at a
            // branch that doesn't exist yet. Read the symbolic target
            // so we show `main` instead of `HEAD`.
            let name = repo
                .find_reference("HEAD")
                .ok()
                .and_then(|r| r.symbolic_target().map(str::to_string))
                .and_then(|t| t.strip_prefix("refs/heads/").map(String::from))
                .unwrap_or_else(|| "main".into());
            return (name, 0, 0, false);
        }
        Err(_) => return ("HEAD".into(), 0, 0, false),
    };

    let branch_name = if head.is_branch() {
        head.shorthand().unwrap_or("HEAD").to_string()
    } else if let Ok(obj) = head.peel(ObjectType::Commit) {
        obj.short_id()
            .ok()
            .and_then(|s| s.as_str().map(str::to_string))
            .unwrap_or_else(|| "detached".into())
    } else {
        "HEAD".into()
    };

    let (ahead, behind, has_upstream) = if head.is_branch() {
        let shorthand = head.shorthand().unwrap_or("");
        match repo.find_branch(shorthand, BranchType::Local) {
            Ok(local) => match local.upstream() {
                Ok(upstream) => {
                    let ab = match (local.get().target(), upstream.get().target()) {
                        (Some(lo), Some(uo)) => repo
                            .graph_ahead_behind(lo, uo)
                            .map(|(a, b)| (a as u32, b as u32))
                            .unwrap_or((0, 0)),
                        _ => (0, 0),
                    };
                    (ab.0, ab.1, true)
                }
                Err(_) => (0, 0, false),
            },
            Err(_) => (0, 0, false),
        }
    } else {
        // Detached HEAD has no upstream concept.
        (0, 0, false)
    };

    (branch_name, ahead, behind, has_upstream)
}

/// Collapse libgit2's eight rebase-flavor variants into the smaller
/// set we care about. "Rebase", "RebaseInteractive", and "RebaseMerge"
/// all look the same from the user's perspective — a rebase is
/// in flight.
fn classify_op_state(s: RepositoryState) -> RepoOpState {
    match s {
        RepositoryState::Clean                => RepoOpState::Clean,
        RepositoryState::Merge                => RepoOpState::Merge,
        RepositoryState::Revert
        | RepositoryState::RevertSequence     => RepoOpState::Revert,
        RepositoryState::CherryPick
        | RepositoryState::CherryPickSequence => RepoOpState::CherryPick,
        RepositoryState::Bisect               => RepoOpState::Bisect,
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge        => RepoOpState::Rebase,
        RepositoryState::ApplyMailbox
        | RepositoryState::ApplyMailboxOrRebase => RepoOpState::ApplyMailbox,
    }
}

/// Count stash entries (refs/stash). Returns 0 on error — stashing is
/// optional infrastructure, we shouldn't blow up the snapshot if the
/// stash ref is missing or malformed.
fn count_stashes(repo: &mut Repository) -> u32 {
    let mut n = 0u32;
    let _ = repo.stash_foreach(|_idx, _msg, _oid| { n += 1; true });
    n
}

/// Branch names for the snapshot's switch list. Local branches come
/// first, then remote branches prefixed with their remote name
/// (`origin/feature-x`), minus the implicit `origin/HEAD` alias. A
/// remote branch with the same shortname as a local branch is
/// suppressed (the local shadow is authoritative). Current branch
/// (if any) is first; rest sorted alphabetically within each group.
///
/// Unborn-branch quirk: a freshly `init`-ed repo has HEAD pointing at
/// `refs/heads/main` but no actual ref there yet, so libgit2 reports
/// zero local branches. We synthesize `current` into the list so the
/// user sees their default branch immediately — `git branch` would
/// print nothing, but every other git TUI (lazygit, tig, gitui) shows
/// the unborn branch and we match that convention.
fn list_branches(repo: &Repository, current: &str) -> Vec<String> {
    let local_iter = match repo.branches(Some(BranchType::Local)) {
        Ok(i) => i,
        Err(_) => {
            return if current.is_empty() { Vec::new() } else { vec![current.to_string()] };
        }
    };
    let mut locals: Vec<String> = local_iter
        .filter_map(|b| b.ok())
        .filter_map(|(b, _)| b.name().ok().flatten().map(str::to_string))
        .collect();
    locals.sort();

    // Pre-first-commit: HEAD names an unborn branch. Seed it so the
    // Branches view has something to show and highlight.
    if !current.is_empty() && !locals.iter().any(|n| n == current) {
        locals.insert(0, current.to_string());
    }

    let mut remotes: Vec<String> = Vec::new();
    if let Ok(iter) = repo.branches(Some(BranchType::Remote)) {
        for res in iter {
            let Ok((b, _)) = res else { continue; };
            let Some(name) = b.name().ok().flatten() else { continue; };
            // Skip `origin/HEAD` and any remote that shadows a local
            // branch of the same shortname.
            if name.ends_with("/HEAD") { continue; }
            let short = name.splitn(2, '/').nth(1).unwrap_or(name);
            if locals.iter().any(|l| l == short) { continue; }
            remotes.push(name.to_string());
        }
    }
    remotes.sort();

    let mut names = Vec::with_capacity(locals.len() + remotes.len());
    names.extend(locals);
    names.extend(remotes);

    if !current.is_empty() {
        if let Some(pos) = names.iter().position(|n| n == current) {
            names.swap(0, pos);
        }
    }
    names
}

// ── write ops ────────────────────────────────────────────────────────────

pub fn init_repo(cwd: &Path) -> Result<(), String> {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main");
    Repository::init_opts(cwd, &opts).map(|_| ()).map_err(err_msg)
}

pub fn stage_path(start: &Path, rel_path: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let wd   = repo.workdir().ok_or("bare repo")?.to_path_buf();
    let mut index = repo.index().map_err(err_msg)?;
    let abs = wd.join(rel_path);
    if abs.exists() {
        index.add_path(Path::new(rel_path)).map_err(err_msg)?;
    } else {
        // File was deleted — stage the deletion.
        index.remove_path(Path::new(rel_path)).map_err(err_msg)?;
    }
    index.write().map_err(err_msg)?;
    Ok(())
}

pub fn unstage_path(start: &Path, rel_path: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(commit) => {
            repo.reset_default(Some(commit.as_object()), [rel_path])
                .map_err(err_msg)?;
        }
        None => {
            // Before the first commit: just drop from index.
            let mut index = repo.index().map_err(err_msg)?;
            let _ = index.remove_path(Path::new(rel_path));
            index.write().map_err(err_msg)?;
        }
    }
    Ok(())
}

/// "Discard" for a path means different things depending on state:
///
/// * **Untracked** (`WT_NEW`, not in index): delete from disk. Plain
///   `checkout_head` would be a no-op — the file isn't in HEAD or
///   the index, so there's nothing to restore *from*.
/// * **Staged-new** (`INDEX_NEW`, not in HEAD yet): drop from the
///   index *and* delete from disk. Again, `checkout_head` can't
///   restore something that was never committed.
/// * **Everything else** (modified, staged-modified, staged-deleted,
///   etc.): force-checkout from HEAD, which both restores the
///   working tree content and clears the staged entry for the path.
pub fn discard_path(start: &Path, rel_path: &str) -> Result<(), String> {
    let repo   = Repository::discover(start).map_err(err_msg)?;
    let wd     = repo.workdir().ok_or("bare repo")?.to_path_buf();
    let abs    = wd.join(rel_path);
    let status = repo.status_file(Path::new(rel_path)).map_err(err_msg)?;

    // Clean file (all status bits zero except maybe IGNORED): nothing
    // to discard. A plain `checkout_head` with force would be fine but
    // misleading — the user's "discard" key on an already-clean file
    // is a silent no-op in every other git UI.
    if status.is_empty() || status == Status::IGNORED || status == Status::CURRENT {
        return Ok(());
    }

    // Conflicted paths: discard has ambiguous meaning (ours/theirs?) —
    // leave it to the conflict-resolution UI rather than silently
    // clobbering either side.
    if status.contains(Status::CONFLICTED) {
        return Err("conflicted: resolve first (use :git checkout --ours/--theirs)".into());
    }

    // Untracked: just remove from disk.
    if status.contains(Status::WT_NEW) && !status.contains(Status::INDEX_NEW) {
        remove_from_disk(&abs)?;
        return Ok(());
    }

    // Staged-new (added but never committed): un-stage + remove from disk.
    if status.contains(Status::INDEX_NEW) {
        let mut index = repo.index().map_err(err_msg)?;
        let _ = index.remove_path(Path::new(rel_path));
        index.write().map_err(err_msg)?;
        remove_from_disk(&abs)?;
        return Ok(());
    }

    // Everything else: restore from HEAD, updating both index + worktree.
    let mut opts = git2::build::CheckoutBuilder::new();
    opts.force().update_index(true).path(rel_path);
    repo.checkout_head(Some(&mut opts)).map_err(err_msg)?;
    Ok(())
}

fn remove_from_disk(abs: &Path) -> Result<(), String> {
    if !abs.exists() {
        return Ok(());
    }
    if abs.is_dir() {
        std::fs::remove_dir_all(abs).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(abs).map_err(|e| e.to_string())
    }
}

/// Stage every changed path — except conflicted ones. `git add .`
/// *would* stage conflicted files and mark the conflict resolved,
/// which is almost never what the user wants from a blanket "stage
/// all" command. We use the `add_all` matcher callback to skip any
/// path that still has conflict bits set, so the user has to
/// explicitly resolve + stage those.
pub fn stage_all(start: &Path) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let mut index = repo.index().map_err(err_msg)?;

    // `add_all` calls this for every match before touching the index.
    // Return 0 to include, 1 to skip, negative to abort.
    let mut skipped = 0u32;
    let cb = &mut |path: &Path, _matched: &[u8]| -> i32 {
        match repo.status_file(path) {
            Ok(st) if st.contains(Status::CONFLICTED) => { skipped += 1; 1 }
            _ => 0,
        }
    };
    index
        .add_all(["*"].iter(), IndexAddOption::DEFAULT, Some(cb))
        .map_err(err_msg)?;
    index.write().map_err(err_msg)?;
    if skipped > 0 {
        return Err(format!("staged (skipped {skipped} conflicted)"));
    }
    Ok(())
}

pub fn unstage_all(start: &Path) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(commit) => {
            let head_tree = commit.tree().map_err(err_msg)?;
            let mut index = repo.index().map_err(err_msg)?;
            index.read_tree(&head_tree).map_err(err_msg)?;
            index.write().map_err(err_msg)?;
        }
        None => {
            let mut index = repo.index().map_err(err_msg)?;
            index.clear().map_err(err_msg)?;
            index.write().map_err(err_msg)?;
        }
    }
    Ok(())
}

fn err_msg(e: git2::Error) -> String {
    e.message().to_string()
}

/// A single commit for the log pane. All strings are already
/// short-formatted so the UI layer doesn't need to re-walk commits.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub sha_short: String,
    pub summary:   String,
    pub author:    String,
    pub when:      String,
    /// Full oid so `v` can diff `<commit>^..<commit>`.
    pub oid:       git2::Oid,
}

/// Commit log from HEAD back, capped at `limit`. Returns empty vec
/// when the repo has no commits yet (unborn branch).
pub fn commit_log(start: &Path, limit: usize) -> Result<Vec<LogEntry>, String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    if repo.head().is_err() {
        return Ok(Vec::new());
    }
    let mut walk = repo.revwalk().map_err(err_msg)?;
    walk.push_head().map_err(err_msg)?;
    let mut out = Vec::new();
    for (i, oid) in walk.enumerate() {
        if i >= limit { break; }
        let oid = oid.map_err(err_msg)?;
        let commit = repo.find_commit(oid).map_err(err_msg)?;
        out.push(LogEntry {
            sha_short: oid.to_string().chars().take(7).collect(),
            summary:   commit.summary().unwrap_or("").to_string(),
            author:    commit.author().name().unwrap_or("").to_string(),
            when:      format_relative(commit.time().seconds()),
            oid,
        });
    }
    Ok(out)
}

/// Very rough relative time ("2 hours ago"). Good enough for a log
/// pane; we don't want to pull in chrono just for this.
fn format_relative(ts: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = (now - ts).max(0);
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;
    match diff {
        d if d < MIN     => "just now".into(),
        d if d < HOUR    => format!("{}m ago", d / MIN),
        d if d < DAY     => format!("{}h ago", d / HOUR),
        d if d < WEEK    => format!("{}d ago", d / DAY),
        d if d < MONTH   => format!("{}w ago", d / WEEK),
        d if d < YEAR    => format!("{}mo ago", d / MONTH),
        d                => format!("{}y ago", d / YEAR),
    }
}

/// Recent commit log (up to `limit` entries) formatted one per line.
pub fn log_lines(start: &Path, limit: usize) -> Result<Vec<String>, String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let mut walk = repo.revwalk().map_err(err_msg)?;
    walk.push_head().map_err(err_msg)?;
    let mut out = Vec::new();
    for (i, oid) in walk.enumerate() {
        if i >= limit { break; }
        let oid = oid.map_err(err_msg)?;
        let commit = repo.find_commit(oid).map_err(err_msg)?;
        let sha = oid.to_string().chars().take(7).collect::<String>();
        let summary = commit.summary().unwrap_or("");
        let author = commit.author().name().unwrap_or("").to_string();
        out.push(format!("{sha} {summary} — {author}"));
    }
    Ok(out)
}

/// Local branch names, current branch first.
pub fn branch_names(start: &Path) -> Result<Vec<String>, String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let current = repo.head().ok().and_then(|h| h.shorthand().map(str::to_string));
    let iter = repo.branches(Some(BranchType::Local)).map_err(err_msg)?;
    let mut names: Vec<String> = iter
        .filter_map(|b| b.ok())
        .filter_map(|(b, _)| b.name().ok().flatten().map(str::to_string))
        .collect();
    names.sort();
    if let Some(cur) = current {
        if let Some(pos) = names.iter().position(|n| n == &cur) {
            names.swap(0, pos);
        }
    }
    Ok(names)
}

/// Check out an existing ref. Accepts branch names, tags, or commit
/// SHAs; non-branch refs produce a detached HEAD. Uses the **safe**
/// checkout strategy: non-conflicting local edits are carried over,
/// a conflict with any dirty path aborts the switch. HEAD is only
/// moved if the checkout succeeded (the `?` handles that).
///
/// Passing `None` for options would silently be a dry-run —
/// libgit2's `GIT_CHECKOUT_NONE` default does *not* update the
/// working tree. `opts.safe()` is what gives us `git switch`-like
/// behaviour.
pub fn switch_branch(start: &Path, name: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;

    // Remote branch shorthand like `origin/feature`: create a local
    // tracking branch and switch to that, rather than dropping into a
    // detached HEAD on the remote ref. Matches `git switch feature`
    // when an origin/feature exists.
    if let Some(short) = remote_branch_short(&repo, name) {
        if repo.find_branch(&short, BranchType::Local).is_err() {
            // Create local branch at the remote tip + set upstream.
            let remote_ref = repo.find_reference(&format!("refs/remotes/{name}")).map_err(err_msg)?;
            let commit = remote_ref.peel_to_commit().map_err(err_msg)?;
            let mut new_branch = repo.branch(&short, &commit, false).map_err(err_msg)?;
            let _ = new_branch.set_upstream(Some(name));
        }
        return switch_branch(start, &short);
    }

    let (object, reference) = repo.revparse_ext(name).map_err(err_msg)?;
    let mut opts = git2::build::CheckoutBuilder::new();
    opts.safe();
    repo.checkout_tree(&object, Some(&mut opts)).map_err(err_msg)?;
    match reference.and_then(|r| r.name().map(str::to_string)) {
        Some(ref_name) => repo.set_head(&ref_name).map_err(err_msg)?,
        None           => repo.set_head_detached(object.id()).map_err(err_msg)?,
    }
    Ok(())
}

/// If `name` looks like `<remote>/<branch>` and matches an actual
/// remote branch, return the `<branch>` part. Otherwise `None`.
fn remote_branch_short(repo: &Repository, name: &str) -> Option<String> {
    // Must contain a '/' to be remote-shorthand.
    let (_, short) = name.split_once('/')?;
    if short.is_empty() { return None; }
    // Confirm it's actually a remote branch we know about.
    repo.find_branch(name, BranchType::Remote).ok()?;
    Some(short.to_string())
}

pub fn create_branch(start: &Path, name: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let commit = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(err_msg)?;
    repo.branch(name, &commit, false).map_err(err_msg)?;
    Ok(())
}

/// Delete a local branch.
///
/// * Always refuses to drop the currently-checked-out branch
///   (libgit2 allows it but leaves the repo in a weird state).
/// * When `force == false`, also refuses if the branch has unmerged
///   commits — i.e., its tip isn't reachable from HEAD. This mirrors
///   `git branch -d`. Pass `force == true` to mirror `git branch -D`
///   and delete regardless.
/// * Doesn't check upstream-merged-ness the way `git branch -d` does
///   when an upstream is set; "merged into HEAD" is a conservative,
///   cheap check that catches the common data-loss case.
pub fn delete_branch(start: &Path, name: &str, force: bool) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let head_short = repo.head().ok().and_then(|h| h.shorthand().map(str::to_string));
    if head_short.as_deref() == Some(name) {
        return Err("refuse: can't delete checked-out branch".into());
    }
    let mut branch = repo
        .find_branch(name, BranchType::Local)
        .map_err(err_msg)?;

    if !force {
        let branch_oid = branch.get().target().ok_or("branch has no tip")?;
        let head_oid   = repo.head().ok().and_then(|h| h.target());
        let merged = match head_oid {
            // No HEAD (unborn): nothing can be "merged into HEAD". Let
            // the user delete stale branches anyway — there's no risk
            // of losing work we could otherwise reach.
            None => true,
            Some(h) if h == branch_oid => true,
            Some(h) => repo.graph_descendant_of(h, branch_oid).unwrap_or(false),
        };
        if !merged {
            return Err(format!("refuse: '{name}' has unmerged commits (use force)"));
        }
    }

    branch.delete().map_err(err_msg)?;
    Ok(())
}

// ── conflict resolution ─────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictSide {
    Ours,
    Theirs,
}

/// Resolve a conflict by picking one side wholesale. Writes that
/// stage's blob to the working tree, clears all three conflict
/// entries for the path from the index, and stages the chosen side.
/// Equivalent to `git checkout --ours/--theirs <path> && git add <path>`.
///
/// Stages in the index after a conflict: 0 = common ancestor,
/// 1 = ancestor/base, 2 = ours, 3 = theirs.
pub fn resolve_conflict_side(start: &Path, rel_path: &str, side: ConflictSide) -> Result<(), String> {
    let repo    = Repository::discover(start).map_err(err_msg)?;
    let wd      = repo.workdir().ok_or("bare repo")?.to_path_buf();
    let abs     = wd.join(rel_path);
    let mut idx = repo.index().map_err(err_msg)?;

    // Walk the conflicts iterator to find the entry for this path.
    // git2's Index doesn't expose a per-path `conflict_get` in 0.19 —
    // iteration is the supported path.
    let mut target: Option<git2::IndexEntry> = None;
    {
        let conflicts = idx.conflicts().map_err(err_msg)?;
        for c in conflicts {
            let c = c.map_err(err_msg)?;
            // Any of the three stages carries the path; prefer the
            // side we want, fall back to whichever exists.
            let path_of = |e: &Option<git2::IndexEntry>| -> Option<String> {
                e.as_ref().and_then(|e| std::str::from_utf8(&e.path).ok().map(str::to_string))
            };
            let p = path_of(&c.our).or(path_of(&c.their)).or(path_of(&c.ancestor));
            if p.as_deref() != Some(rel_path) {
                continue;
            }
            target = match side {
                ConflictSide::Ours   => c.our,
                ConflictSide::Theirs => c.their,
            };
            break;
        }
    }
    let target = target.ok_or_else(|| "not a conflicted path (or side deleted)".to_string())?;

    // Write the chosen blob to disk, then replace the conflict with a
    // single clean index entry.
    let blob = repo.find_blob(target.id).map_err(err_msg)?;
    if let Some(parent) = abs.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&abs, blob.content()).map_err(|e| e.to_string())?;
    idx.remove_path(Path::new(rel_path)).map_err(err_msg)?;
    idx.add_path(Path::new(rel_path)).map_err(err_msg)?;
    idx.write().map_err(err_msg)?;
    Ok(())
}

// ── stash ───────────────────────────────────────────────────────────────

/// `git stash push` — save the dirty worktree + index and reset to
/// HEAD. Untracked files are *not* stashed by default (matches
/// `git stash` without `-u`); callers that want that should pass
/// `include_untracked = true`.
///
/// Message is optional; libgit2 auto-generates one (`WIP on <branch>:
/// <sha> <summary>`) when empty, same as the git CLI.
pub fn stash_save(start: &Path, message: Option<&str>, include_untracked: bool) -> Result<String, String> {
    let mut repo = Repository::discover(start).map_err(err_msg)?;
    let sig = repo.signature().map_err(|e| format!("signature: {}", e.message()))?;
    let mut flags = git2::StashFlags::DEFAULT;
    if include_untracked {
        flags |= git2::StashFlags::INCLUDE_UNTRACKED;
    }
    let oid = repo.stash_save2(&sig, message, Some(flags)).map_err(err_msg)?;
    Ok(oid.to_string().chars().take(7).collect())
}

/// `git stash pop [idx]` — apply entry `idx` (default 0) and drop it
/// on success. If apply conflicts, the entry is kept.
pub fn stash_pop(start: &Path, idx: usize) -> Result<(), String> {
    let mut repo = Repository::discover(start).map_err(err_msg)?;
    let mut opts = git2::StashApplyOptions::new();
    // Apply both index + worktree like `git stash pop` does.
    opts.reinstantiate_index();
    repo.stash_pop(idx, Some(&mut opts)).map_err(err_msg)
}

/// `git stash apply [idx]` — like pop but keeps the entry on the stack.
pub fn stash_apply(start: &Path, idx: usize) -> Result<(), String> {
    let mut repo = Repository::discover(start).map_err(err_msg)?;
    let mut opts = git2::StashApplyOptions::new();
    opts.reinstantiate_index();
    repo.stash_apply(idx, Some(&mut opts)).map_err(err_msg)
}

/// `git stash drop [idx]` — remove entry `idx` (default 0).
pub fn stash_drop(start: &Path, idx: usize) -> Result<(), String> {
    let mut repo = Repository::discover(start).map_err(err_msg)?;
    repo.stash_drop(idx).map_err(err_msg)
}

/// `git stash list` — formatted one-line entries.
pub fn stash_list(start: &Path) -> Result<Vec<String>, String> {
    let mut repo = Repository::discover(start).map_err(err_msg)?;
    let mut out = Vec::new();
    repo.stash_foreach(|idx, msg, _oid| {
        out.push(format!("stash@{{{idx}}}: {msg}"));
        true
    }).map_err(err_msg)?;
    Ok(out)
}

// ── remotes ─────────────────────────────────────────────────────────────

pub fn list_remotes(start: &Path) -> Result<Vec<(String, Option<String>)>, String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    let names = repo.remotes().map_err(err_msg)?;
    let mut out = Vec::new();
    for name in names.iter().flatten() {
        let url = repo.find_remote(name).ok().and_then(|r| r.url().map(str::to_string));
        out.push((name.to_string(), url));
    }
    Ok(out)
}

pub fn add_remote(start: &Path, name: &str, url: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    repo.remote(name, url).map_err(err_msg)?;
    Ok(())
}

pub fn remove_remote(start: &Path, name: &str) -> Result<(), String> {
    let repo = Repository::discover(start).map_err(err_msg)?;
    repo.remote_delete(name).map_err(err_msg)
}

// ── background: libgit2 ops that can stat-walk the worktree ─────────────

/// Run a libgit2 write op on a background thread and report the result
/// via `GitCmdResult`. Used for `stage_all` / `unstage_all` because
/// `add_all` stat-walks the entire worktree — fast on small repos,
/// multi-second on OneDrive-synced or otherwise large roots. Running
/// it on the UI thread would freeze rendering and input until it
/// finishes.
pub fn spawn_local_op<F>(cwd: PathBuf, label: String, tx: Sender<AppEvent>, op: F)
where
    F: FnOnce(&Path) -> Result<(), String> + Send + 'static,
{
    std::thread::spawn(move || {
        let result = match op(&cwd) {
            Ok(())  => Ok(label.clone()),
            Err(e)  => Err(format!("{label}: {e}")),
        };
        let _ = tx.send(AppEvent::GitCmdResult(result));
    });
}

// ── background shell-out (push/pull/fetch) ───────────────────────────────

/// Spawn `git <args>` in `cwd` on a background thread; send the result
/// back via `AppEvent::GitCmdResult` when it finishes.
pub fn spawn_git_shell(cwd: PathBuf, label: String, args: Vec<String>, tx: Sender<AppEvent>) {
    std::thread::spawn(move || {
        let out = std::process::Command::new("git")
            .args(args.iter().map(|s| s.as_str()))
            .current_dir(&cwd)
            .output();
        let result = match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                Ok(format_shell_summary(&label, &s))
            }
            Ok(o) => {
                let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                Err(format_shell_summary(&label, &e))
            }
            Err(e) => Err(format!("{label}: {e}")),
        };
        let _ = tx.send(AppEvent::GitCmdResult(result));
    });
}

fn format_shell_summary(label: &str, body: &str) -> String {
    let first_line = body.lines().next().unwrap_or("");
    if first_line.is_empty() {
        label.to_string()
    } else {
        format!("{label}: {first_line}")
    }
}
