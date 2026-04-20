//! Read-only diff view rendered in its own cell when the user hits
//! `v` on a change row or commit log entry. Renders a unified diff
//! for the appropriate pair of sides (HEAD↔index for staged,
//! index↔worktree for unstaged, HEAD↔worktree for untracked), with
//! keyboard scrolling and `:close` to dismiss the cell.

use std::path::Path;

use git2::{DiffOptions, Repository};

use crate::git::{ChangeGroup, ChangeRow};

/// A line of diff output, tagged so we can style hunks / additions /
/// deletions / context differently in the UI.
#[derive(Clone, Debug)]
pub struct DiffLine {
    pub tag:  DiffTag,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffTag {
    FileHeader,
    HunkHeader,
    Addition,
    Deletion,
    Context,
    Binary,
}

/// Holds the full rendered diff + scroll state. Fresh instance per
/// open — we re-generate on every open so a stale view never hides
/// new changes.
pub struct DiffView {
    pub title: String,
    pub lines: Vec<DiffLine>,
    pub scroll: usize,
}

impl DiffView {
    /// Diff of a single commit vs its first parent. Root commit
    /// (no parents) diffs an empty tree vs the commit's tree — same
    /// thing `git show` does on a root commit.
    pub fn for_commit(cwd: &Path, oid: git2::Oid, title: String) -> Result<Self, String> {
        let repo = Repository::discover(cwd).map_err(|e| e.message().to_string())?;
        let commit = repo.find_commit(oid).map_err(|e| e.message().to_string())?;
        let tree = commit.tree().map_err(|e| e.message().to_string())?;
        let parent_tree = commit.parents().next().and_then(|p| p.tree().ok());
        let diff = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
            .map_err(|e| e.message().to_string())?;
        Ok(Self {
            title,
            lines: render_diff(&diff)?,
            scroll: 0,
        })
    }

    pub fn for_row(cwd: &Path, row: &ChangeRow) -> Result<Self, String> {
        let repo = Repository::discover(cwd).map_err(|e| e.message().to_string())?;
        let (title_kind, lines) = match row.group {
            ChangeGroup::Staged      => ("staged",     diff_head_to_index(&repo, &row.path)?),
            ChangeGroup::Unstaged    => ("unstaged",   diff_index_to_workdir(&repo, &row.path)?),
            ChangeGroup::Untracked   => ("untracked",  diff_untracked(&repo, cwd, &row.path)?),
            ChangeGroup::Conflicted  => ("conflicted", diff_index_to_workdir(&repo, &row.path)?),
        };
        Ok(Self {
            title: format!("{title_kind}: {}", row.path),
            lines,
            scroll: 0,
        })
    }

    pub fn scroll(&mut self, delta: isize) {
        let n = self.lines.len() as isize;
        let next = (self.scroll as isize + delta).clamp(0, (n - 1).max(0));
        self.scroll = next as usize;
    }

    pub fn scroll_page(&mut self, page: usize, forward: bool) {
        let delta = page as isize;
        self.scroll(if forward { delta } else { -delta });
    }
}

fn diff_head_to_index(repo: &Repository, rel: &str) -> Result<Vec<DiffLine>, String> {
    let head_tree = match repo.head().ok().and_then(|h| h.peel_to_tree().ok()) {
        Some(t) => Some(t),
        None    => None,
    };
    let mut opts = DiffOptions::new();
    opts.pathspec(rel).include_untracked(false);
    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
        .map_err(|e| e.message().to_string())?;
    render_diff(&diff)
}

fn diff_index_to_workdir(repo: &Repository, rel: &str) -> Result<Vec<DiffLine>, String> {
    let mut opts = DiffOptions::new();
    opts.pathspec(rel).include_untracked(false);
    let diff = repo
        .diff_index_to_workdir(None, Some(&mut opts))
        .map_err(|e| e.message().to_string())?;
    render_diff(&diff)
}

/// Untracked files don't appear in any tree comparison — the closest
/// thing to "what would this commit add" is reading the file content
/// and rendering every line as a new addition. Cheap and matches what
/// the user expects to see.
fn diff_untracked(_repo: &Repository, cwd: &Path, rel: &str) -> Result<Vec<DiffLine>, String> {
    let abs = cwd.join(rel);
    // Peek at the first bytes to decide if this is binary — same
    // heuristic git uses (NUL byte in the first 8000 bytes).
    let bytes = std::fs::read(&abs).map_err(|e| e.to_string())?;
    let head  = &bytes[..bytes.len().min(8000)];
    let is_binary = head.contains(&0);
    let mut out = Vec::new();
    out.push(DiffLine { tag: DiffTag::FileHeader, text: format!("--- /dev/null") });
    out.push(DiffLine { tag: DiffTag::FileHeader, text: format!("+++ b/{rel}") });
    if is_binary {
        out.push(DiffLine { tag: DiffTag::Binary, text: format!("Binary file ({} bytes)", bytes.len()) });
        return Ok(out);
    }
    let text = String::from_utf8_lossy(&bytes);
    let nlines = text.lines().count();
    out.push(DiffLine { tag: DiffTag::HunkHeader, text: format!("@@ -0,0 +1,{nlines} @@") });
    for l in text.lines() {
        out.push(DiffLine { tag: DiffTag::Addition, text: format!("+{l}") });
    }
    Ok(out)
}

fn render_diff(diff: &git2::Diff<'_>) -> Result<Vec<DiffLine>, String> {
    let mut out = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let text_bytes = line.content();
        let text = String::from_utf8_lossy(text_bytes).trim_end_matches('\n').to_string();
        let tag = match line.origin() {
            'F' => DiffTag::FileHeader,
            'H' => DiffTag::HunkHeader,
            '+' => DiffTag::Addition,
            '-' => DiffTag::Deletion,
            ' ' => DiffTag::Context,
            'B' => DiffTag::Binary,
            _   => DiffTag::Context,
        };
        let prefixed = match line.origin() {
            '+' | '-' | ' ' => format!("{}{}", line.origin(), text),
            _ => text,
        };
        out.push(DiffLine { tag, text: prefixed });
        true
    }).map_err(|e| e.message().to_string())?;
    if out.is_empty() {
        out.push(DiffLine { tag: DiffTag::Context, text: "(no changes)".into() });
    }
    Ok(out)
}
