//! Soft-wrap layout shared by the editor render path (`ui::draw_editor_wrapped`)
//! and the editor's visual-row motions (`gj` / `gk`). Pure data + helpers:
//! no ratatui / app types live here.

/// Columns a tab character occupies. Fixed (not position-aware) to keep
/// wrap math simple; good-enough alignment for typical code.
pub const TABSTOP: usize = 4;

/// Continuation prefix drawn on wrapped rows (vim's `showbreak`). Width
/// must match the grapheme count of the string.
pub const SHOWBREAK: &str = "↳ ";
pub const SHOWBREAK_COLS: u16 = 2;

/// One visible row produced by wrapping a logical line within a given
/// content width. `start_char == 0` marks the first visual row of a
/// logical line (the one that shows the line number in the gutter);
/// subsequent rows of the same logical line get `start_char > 0` and
/// `prefix_cols > 0` — the space allotted to `showbreak + breakindent`
/// at the front of the row.
#[derive(Clone, Debug)]
pub struct VisualRow {
    pub logical_row: usize,
    pub start_char:  usize,
    pub end_char:    usize,
    /// Screen columns of leading prefix (showbreak + breakindent) drawn
    /// on this visual row. 0 on the first row of a logical line.
    pub prefix_cols: u16,
}

pub fn char_width(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    c.width().unwrap_or(0)
}

/// "Cell width" for layout purposes: zero-width combining marks count
/// as 0; tabs as `TABSTOP`; every other non-control char contributes at
/// least 1 so a stray non-printable doesn't stall wrap progress.
pub fn cell_width(c: char) -> usize {
    if c == '\t' { return TABSTOP; }
    char_width(c).max(if c.is_control() { 0 } else { 1 })
}

/// Display-width of the leading whitespace on `line`, capped so an
/// overflowing indent can't eat the whole row.
pub fn leading_indent_cols(line: &str, cap: usize) -> u16 {
    let mut col = 0usize;
    for c in line.chars() {
        if !c.is_whitespace() || c == '\n' { break; }
        col += cell_width(c).max(1);
        if col >= cap { return cap as u16; }
    }
    col as u16
}

/// Build the visual-row layout. Breaks prefer the last whitespace
/// (linebreak) so wrapping snaps to word boundaries; hard-breaks fall
/// back only when no whitespace is available in the current window.
pub fn build_wrap_rows(lines: &[String], width: usize) -> Vec<VisualRow> {
    let mut rows = Vec::with_capacity(lines.len());
    if width == 0 { return rows; }
    for (i, line) in lines.iter().enumerate() {
        let total_chars = line.chars().count();
        if total_chars == 0 {
            rows.push(VisualRow {
                logical_row: i, start_char: 0, end_char: 0, prefix_cols: 0,
            });
            continue;
        }
        // Cap the continuation prefix (showbreak + breakindent) at half
        // the content width so we always have room for real content.
        let prefix_cap = (width / 2).max(1);
        let indent = leading_indent_cols(line, prefix_cap.saturating_sub(SHOWBREAK_COLS as usize));
        let continuation_prefix = SHOWBREAK_COLS + indent;

        let mut start = 0usize;
        let mut first = true;
        while start < total_chars {
            let prefix_cols = if first { 0 } else { continuation_prefix };
            let avail = width.saturating_sub(prefix_cols as usize).max(1);
            let end = find_break(line, start, avail);
            rows.push(VisualRow {
                logical_row: i, start_char: start, end_char: end, prefix_cols,
            });
            if end == total_chars { break; }
            start = end;
            first = false;
        }
    }
    rows
}

/// Walk chars from `start`, returning the char index at which to wrap.
/// Prefers breaking AFTER the last whitespace seen so wrapped text
/// starts on a word boundary. Always advances by at least one char so
/// extra-narrow windows can't loop.
pub fn find_break(line: &str, start: usize, avail: usize) -> usize {
    let mut col = 0usize;
    let mut last_ws_end: Option<usize> = None;
    let mut idx = start;
    for (i, c) in line.chars().enumerate().skip(start) {
        let w = cell_width(c);
        if col + w > avail {
            if let Some(b) = last_ws_end {
                if b > start { return b; }
            }
            return if i > start { i } else { start + 1 };
        }
        col += w;
        if c.is_whitespace() {
            last_ws_end = Some(i + 1);
        }
        idx = i + 1;
    }
    idx
}

/// Sum display widths of chars [start..end) in `line`.
pub fn slice_display_width(line: &str, start: usize, end: usize) -> usize {
    line.chars().skip(start).take(end - start)
        .map(cell_width)
        .sum()
}

/// Return the char index within `line` that, when rendered within
/// `row`, lands closest to `target_screen_col` (a column *within* the
/// content area, including any showbreak / breakindent prefix). Used
/// for `gj` / `gk` so the cursor tracks the same visual column across
/// visual rows.
pub fn char_idx_for_screen_col(line: &str, row: &VisualRow, target_screen_col: u16) -> usize {
    let target = target_screen_col as usize;
    if target <= row.prefix_cols as usize { return row.start_char; }
    let mut col = row.prefix_cols as usize;
    let mut idx = row.start_char;
    for c in line.chars().skip(row.start_char).take(row.end_char - row.start_char) {
        let w = cell_width(c);
        if target < col + w { return idx; }
        col += w;
        idx += 1;
    }
    idx
}
