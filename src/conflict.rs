//! 3-way conflict resolution view.
//!
//! Two entry points:
//!   * [`ConflictView::for_external`] — feed buffer vs disk lines when
//!     an editor's `ExternalConflict::ModifiedOnDisk` needs resolving.
//!     "ours" = buffer, "theirs" = disk. If a `base` snapshot is
//!     available, a proper diff3 auto-resolves hunks where only one
//!     side diverged from base.
//!   * [`ConflictView::for_git_file`] — read a working-tree file that
//!     git left with `<<<<<<<`/`=======`/`>>>>>>>` markers and parse
//!     those blocks into hunks.
//!
//! Line-level diffs go through the `similar` crate (Myers). Per-hunk
//! resolution picks one side (or both, concatenated, or a hand-edited
//! replacement) and is flushed to disk by
//! [`ConflictView::resolved_output`] + `:w`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use similar::{capture_diff_slices, Algorithm, DiffOp};
use tui_textarea::TextArea;

/// One disagreement between ours and theirs. Either side may be empty
/// (pure insertion / pure deletion).
#[derive(Clone, Debug)]
pub struct Hunk {
    pub ours:       Vec<String>,
    pub theirs:     Vec<String>,
    pub resolution: Resolution,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Unresolved,
    KeepOurs,
    KeepTheirs,
    KeepBoth,               // ours lines followed by theirs lines
    Custom(Vec<String>),    // user hand-edited replacement via `e`
}

/// Flat sequence of agreed context vs disagreement hunks, in file
/// order. Rendering walks this; `resolved_output` folds it into the
/// single file we write on `:w`.
#[derive(Clone, Debug)]
pub enum Segment {
    Context(Vec<String>),
    Conflict(Hunk),
}

/// Inline hand-edit state. While `editing` is `Some`, the textarea
/// replaces the selected hunk's ours column in the UI and takes all
/// Insert-mode keystrokes. Commit swaps the edited lines in as a
/// `Resolution::Custom` and clears this field.
pub struct HunkEdit {
    pub seg_idx:  usize,
    pub textarea: TextArea<'static>,
}

pub struct ConflictView {
    /// Where `:w` writes the resolved output.
    pub path:         PathBuf,
    /// Short label for the cell title.
    pub title:        String,
    pub segments:     Vec<Segment>,
    /// Absolute indices into `segments` pointing at `Conflict`
    /// segments — lets `j`/`k` jump between hunks without scanning
    /// the whole list each time.
    pub hunk_indices: Vec<usize>,
    /// Cursor within `hunk_indices` (not `segments`).
    pub selected:     usize,
    pub scroll:       usize,
    /// Active hand-edit overlay, if any.
    pub editing:      Option<HunkEdit>,
}

impl ConflictView {
    /// Build a view for an editor whose buffer diverged from disk.
    /// `ours` is the live buffer, `theirs` is the current disk content.
    /// `base` is the editor's last-saved snapshot — if non-empty we
    /// run a proper diff3 and auto-resolve hunks where only one side
    /// moved, so the user only sees true conflicts.
    pub fn for_external(
        path:   PathBuf,
        ours:   &[String],
        base:   &[String],
        theirs: &[String],
    ) -> Self {
        let segments = if base.is_empty() {
            build_segments_two_way(ours, theirs)
        } else {
            build_segments_diff3(ours, base, theirs)
        };
        let hunk_indices = collect_hunk_indices(&segments);
        let title = format!(
            "conflict: {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        );
        Self {
            path, title, segments, hunk_indices,
            selected: 0, scroll: 0, editing: None,
        }
    }

    /// Build a view from a file that contains unresolved git merge
    /// markers (`<<<<<<< …` / `======= …` / `>>>>>>> …`).
    pub fn for_git_file(path: &Path) -> io::Result<Self> {
        let content = fs::read_to_string(path)?;
        let lines: Vec<String> = content.lines().map(String::from).collect();
        let segments = parse_git_markers(&lines);
        let hunk_indices = collect_hunk_indices(&segments);
        let title = format!(
            "git conflict: {}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("?")
        );
        Ok(Self {
            path: path.to_path_buf(),
            title,
            segments,
            hunk_indices,
            selected: 0,
            scroll: 0,
            editing: None,
        })
    }

    pub fn total_hunks(&self) -> usize { self.hunk_indices.len() }

    pub fn unresolved_count(&self) -> usize {
        self.segments.iter().filter(|s| matches!(s, Segment::Conflict(h) if h.resolution == Resolution::Unresolved)).count()
    }

    #[allow(dead_code)]
    pub fn is_fully_resolved(&self) -> bool {
        self.unresolved_count() == 0
    }

    pub fn next_hunk(&mut self) {
        if self.editing.is_some() { return; }
        if self.hunk_indices.is_empty() { return; }
        self.selected = (self.selected + 1).min(self.hunk_indices.len() - 1);
    }

    pub fn prev_hunk(&mut self) {
        if self.editing.is_some() { return; }
        if self.selected > 0 { self.selected -= 1; }
    }

    pub fn resolve_selected(&mut self, r: Resolution) {
        if self.editing.is_some() { return; }
        let Some(&seg_idx) = self.hunk_indices.get(self.selected) else { return; };
        if let Some(Segment::Conflict(h)) = self.segments.get_mut(seg_idx) {
            h.resolution = r;
        }
    }

    /// Begin hand-editing the currently selected hunk. Seeds the
    /// textarea from the most recently resolved content (falling back
    /// to ours) so the user starts with something sensible to edit.
    /// Returns true if an edit session was opened.
    pub fn start_edit(&mut self) -> bool {
        if self.editing.is_some() { return true; }
        let Some(&seg_idx) = self.hunk_indices.get(self.selected) else { return false; };
        let Some(Segment::Conflict(h)) = self.segments.get(seg_idx) else { return false; };
        let seed: Vec<String> = match &h.resolution {
            Resolution::Custom(lines) => lines.clone(),
            Resolution::KeepTheirs    => h.theirs.clone(),
            Resolution::KeepBoth      => {
                let mut v = h.ours.clone();
                v.extend(h.theirs.iter().cloned());
                v
            }
            _                         => h.ours.clone(),
        };
        let textarea = TextArea::new(if seed.is_empty() { vec![String::new()] } else { seed });
        self.editing = Some(HunkEdit { seg_idx, textarea });
        true
    }

    /// Commit the active hand-edit into the selected hunk as
    /// `Resolution::Custom`. No-op if not editing. Called from
    /// `App::enter_normal` so Esc ends an edit session cleanly.
    pub fn commit_edit(&mut self) {
        let Some(edit) = self.editing.take() else { return; };
        let HunkEdit { seg_idx, textarea } = edit;
        let lines: Vec<String> = textarea.lines().iter().cloned().collect();
        if let Some(Segment::Conflict(h)) = self.segments.get_mut(seg_idx) {
            h.resolution = Resolution::Custom(lines);
        }
    }

    #[allow(dead_code)]
    pub fn cancel_edit(&mut self) { self.editing = None; }

    pub fn is_editing(&self) -> bool { self.editing.is_some() }

    /// Fold segments + resolutions into the final file content.
    /// Unresolved hunks fall through as "keep ours" so a partial save
    /// doesn't silently corrupt the file — callers should check
    /// `is_fully_resolved` before writing.
    pub fn resolved_output(&self) -> Vec<String> {
        let mut out = Vec::new();
        for seg in &self.segments {
            match seg {
                Segment::Context(lines) => out.extend(lines.iter().cloned()),
                Segment::Conflict(h) => match &h.resolution {
                    Resolution::Unresolved | Resolution::KeepOurs =>
                        out.extend(h.ours.iter().cloned()),
                    Resolution::KeepTheirs =>
                        out.extend(h.theirs.iter().cloned()),
                    Resolution::KeepBoth => {
                        out.extend(h.ours.iter().cloned());
                        out.extend(h.theirs.iter().cloned());
                    }
                    Resolution::Custom(lines) =>
                        out.extend(lines.iter().cloned()),
                },
            }
        }
        out
    }

    /// Write the resolved content to disk. Adds a trailing newline
    /// (convention — see Editor::save).
    pub fn save(&self) -> io::Result<()> {
        let mut text = self.resolved_output().join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        fs::write(&self.path, text)
    }
}

// ── Two-way diff (fallback when no base is available) ──────────────────

fn build_segments_two_way(ours: &[String], theirs: &[String]) -> Vec<Segment> {
    let ops = capture_diff_slices(Algorithm::Myers, ours, theirs);
    let mut segments: Vec<Segment> = Vec::new();
    let mut pending_o: Vec<String> = Vec::new();
    let mut pending_t: Vec<String> = Vec::new();
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                flush_conflict(&mut segments, &mut pending_o, &mut pending_t);
                for k in 0..len {
                    push_context(&mut segments, ours[old_index + k].clone());
                }
            }
            DiffOp::Delete { old_index, old_len, .. } => {
                for k in 0..old_len { pending_o.push(ours[old_index + k].clone()); }
            }
            DiffOp::Insert { new_index, new_len, .. } => {
                for k in 0..new_len { pending_t.push(theirs[new_index + k].clone()); }
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                for k in 0..old_len { pending_o.push(ours[old_index + k].clone()); }
                for k in 0..new_len { pending_t.push(theirs[new_index + k].clone()); }
            }
        }
    }
    flush_conflict(&mut segments, &mut pending_o, &mut pending_t);
    segments
}

fn flush_conflict(
    segments:   &mut Vec<Segment>,
    pending_o:  &mut Vec<String>,
    pending_t:  &mut Vec<String>,
) {
    if pending_o.is_empty() && pending_t.is_empty() { return; }
    segments.push(Segment::Conflict(Hunk {
        ours:       std::mem::take(pending_o),
        theirs:     std::mem::take(pending_t),
        resolution: Resolution::Unresolved,
    }));
}

// ── diff3 (base-aware merge) ────────────────────────────────────────────

/// Proper 3-way merge. Walk base in order; at each base line, consult
/// what each side's diff says about it (kept as-is vs replaced). Runs
/// where both sides keep the line become context; runs where only
/// one side changed get auto-resolved; runs where both changed
/// differently stay Unresolved.
fn build_segments_diff3(
    ours:   &[String],
    base:   &[String],
    theirs: &[String],
) -> Vec<Segment> {
    // equal_in_{ours,theirs}[b] = Some(index in side) iff base[b] is
    // preserved as-is by that side; None means that base line was
    // removed or changed.
    let ours_map   = equal_map(base, ours);
    let theirs_map = equal_map(base, theirs);

    let mut segments: Vec<Segment> = Vec::new();
    let mut cur_o = 0usize;
    let mut cur_t = 0usize;
    let mut i = 0usize;

    while i < base.len() {
        if let (Some(o_at), Some(t_at)) = (ours_map[i], theirs_map[i]) {
            // Stable base line. First flush anything pending before it:
            // those are inserts that landed between the previous stable
            // anchor and this one.
            let ours_before   = ours  [cur_o..o_at].to_vec();
            let theirs_before = theirs[cur_t..t_at].to_vec();
            emit_chunk(&mut segments, ours_before, Vec::new(), theirs_before);

            push_context(&mut segments, base[i].clone());
            cur_o = o_at + 1;
            cur_t = t_at + 1;
            i += 1;
        } else {
            // Unstable run: advance until the next doubly-stable anchor
            // (or the end of base).
            let mut j = i + 1;
            while j < base.len()
                && !(ours_map[j].is_some() && theirs_map[j].is_some())
            {
                j += 1;
            }
            // Determine side ranges that correspond to base[i..j].
            let o_end = if j < base.len() { ours_map[j].unwrap()   } else { ours.len()   };
            let t_end = if j < base.len() { theirs_map[j].unwrap() } else { theirs.len() };

            let base_chunk   = base  [i..j].to_vec();
            let ours_chunk   = ours  [cur_o..o_end].to_vec();
            let theirs_chunk = theirs[cur_t..t_end].to_vec();

            emit_chunk(&mut segments, ours_chunk, base_chunk, theirs_chunk);

            cur_o = o_end;
            cur_t = t_end;
            i = j;
        }
    }

    // Trailing material past the end of base (pure appends).
    let ours_tail   = ours  [cur_o..].to_vec();
    let theirs_tail = theirs[cur_t..].to_vec();
    emit_chunk(&mut segments, ours_tail, Vec::new(), theirs_tail);

    segments
}

/// Emit one chunk worth of output. Auto-resolves the easy cases:
/// * both sides empty         → nothing
/// * ours == base             → theirs made all the changes here
/// * theirs == base           → ours made all the changes here
/// * ours == theirs           → both sides agree, ship it
/// * both empty but base not  → both sides deleted, ship nothing
/// * everything else          → leave Unresolved for the user
fn emit_chunk(
    segments: &mut Vec<Segment>,
    ours:     Vec<String>,
    base:     Vec<String>,
    theirs:   Vec<String>,
) {
    if ours.is_empty() && theirs.is_empty() && base.is_empty() {
        return;
    }

    // Both sides deleted the base region — context of "nothing", no output.
    if ours.is_empty() && theirs.is_empty() && !base.is_empty() {
        return;
    }

    // Both sides agree on the replacement → emit as plain context so
    // the user doesn't see a hunk for something nobody disputes.
    if !base.is_empty() && ours == theirs {
        for l in ours { push_context(segments, l); }
        return;
    }

    let resolution = if !base.is_empty() && ours == base {
        Resolution::KeepTheirs
    } else if !base.is_empty() && theirs == base {
        Resolution::KeepOurs
    } else if ours == theirs {
        // Same insert from both sides.
        Resolution::KeepOurs
    } else {
        Resolution::Unresolved
    };

    segments.push(Segment::Conflict(Hunk { ours, theirs, resolution }));
}

/// For each index `b` in `base`, returns `Some(s)` where
/// `side[s] == base[b]` iff the diff preserves that base line. This
/// is the raw material diff3 walks.
fn equal_map(base: &[String], side: &[String]) -> Vec<Option<usize>> {
    let mut map = vec![None; base.len()];
    if base.is_empty() { return map; }
    let ops = capture_diff_slices(Algorithm::Myers, base, side);
    for op in ops {
        if let DiffOp::Equal { old_index, new_index, len } = op {
            for k in 0..len {
                map[old_index + k] = Some(new_index + k);
            }
        }
    }
    map
}

// ── shared helpers ──────────────────────────────────────────────────────

/// Append `line` as context, coalescing with the prior Context segment
/// if there is one. Keeps the segment list shallow.
fn push_context(segments: &mut Vec<Segment>, line: String) {
    if let Some(Segment::Context(lines)) = segments.last_mut() {
        lines.push(line);
        return;
    }
    segments.push(Segment::Context(vec![line]));
}

fn collect_hunk_indices(segments: &[Segment]) -> Vec<usize> {
    segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| matches!(s, Segment::Conflict(_)).then_some(i))
        .collect()
}

// ── Git conflict marker parser ──────────────────────────────────────────

fn parse_git_markers(lines: &[String]) -> Vec<Segment> {
    enum State { Normal, Ours, Theirs }
    let mut state = State::Normal;
    let mut segments: Vec<Segment> = Vec::new();
    let mut ours_buf:   Vec<String> = Vec::new();
    let mut theirs_buf: Vec<String> = Vec::new();

    for line in lines {
        match state {
            State::Normal => {
                if line.starts_with("<<<<<<<") {
                    ours_buf.clear();
                    theirs_buf.clear();
                    state = State::Ours;
                } else {
                    push_context(&mut segments, line.clone());
                }
            }
            State::Ours => {
                if line.starts_with("=======") {
                    state = State::Theirs;
                } else if line.starts_with(">>>>>>>") {
                    // Malformed — close out as a conflict with just ours.
                    segments.push(Segment::Conflict(Hunk {
                        ours:   std::mem::take(&mut ours_buf),
                        theirs: Vec::new(),
                        resolution: Resolution::Unresolved,
                    }));
                    state = State::Normal;
                } else {
                    ours_buf.push(line.clone());
                }
            }
            State::Theirs => {
                if line.starts_with(">>>>>>>") {
                    segments.push(Segment::Conflict(Hunk {
                        ours:   std::mem::take(&mut ours_buf),
                        theirs: std::mem::take(&mut theirs_buf),
                        resolution: Resolution::Unresolved,
                    }));
                    state = State::Normal;
                } else {
                    theirs_buf.push(line.clone());
                }
            }
        }
    }
    // Unterminated block — flush what we have as a conflict so the
    // user sees their half-parsed content rather than losing it.
    match state {
        State::Ours => segments.push(Segment::Conflict(Hunk {
            ours:   ours_buf,
            theirs: Vec::new(),
            resolution: Resolution::Unresolved,
        })),
        State::Theirs => segments.push(Segment::Conflict(Hunk {
            ours:   ours_buf,
            theirs: theirs_buf,
            resolution: Resolution::Unresolved,
        })),
        State::Normal => {}
    }
    segments
}
