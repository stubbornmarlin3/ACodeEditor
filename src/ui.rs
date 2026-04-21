use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::layout::Alignment;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::path::Path;

use crate::app::{App, ExplorerMode, FocusId, Mode};
use crate::cell::{Cell, Session};
use crate::explorer::{Entry, EntryKind};
use crate::diff::DiffTag;
use crate::git::{ChangeGroup, ChangeRow, FileStatus};
use crate::projects::ProjectState;
use crate::theme::Theme;

pub struct Areas {
    pub sidebar:   Rect,    // explorer (projects + files) or git
    pub statusbar: Rect,
    pub cells:     Vec<Rect>,
}

/// Split the full terminal area into named regions. This is the single
/// source of truth for layout so `main.rs` can use it to size PTYs
/// without duplicating constraints.
pub fn layout(area: Rect, app: &App) -> Areas {
    let (sidebar_r, statusbar_r, main) = split_frame(area, app);
    // Only tile visible cells. Minimized cells get a zero-size rect so
    // `cells` stays aligned 1:1 with `app.cells` (callers index by cell
    // idx) without taking any screen space.
    let visible = app.cells.iter().filter(|c| !c.minimized).count();
    let visible_rects = app.layout_mode.rects(main, visible);
    let mut cells: Vec<Rect> = Vec::with_capacity(app.cells.len());
    let mut v = 0;
    for c in &app.cells {
        if c.minimized {
            cells.push(Rect { x: 0, y: 0, width: 0, height: 0 });
        } else {
            cells.push(visible_rects[v]);
            v += 1;
        }
    }
    Areas {
        sidebar:   sidebar_r,
        statusbar: statusbar_r,
        cells,
    }
}

/// The rect that the cell area occupies, without computing cell rects.
/// Used when we need a prospective geometry before pushing the new cell.
pub fn compute_main_area(area: Rect, app: &App) -> Rect {
    split_frame(area, app).2
}

fn split_frame(area: Rect, app: &App) -> (Rect, Rect, Rect) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body      = root[0];
    let statusbar = root[1];

    let sidebar_width = compute_explorer_width(area, app);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(sidebar_width),
            Constraint::Min(40),
        ])
        .split(body);

    (cols[0], statusbar, cols[1])
}

/// Content area of a bordered panel (Block inset by 1 on each side).
pub fn inner_size(r: Rect) -> (u16, u16) {
    let rows = r.height.saturating_sub(2).max(1);
    let cols = r.width.saturating_sub(2).max(1);
    (rows, cols)
}

fn compute_explorer_width(area: Rect, app: &App) -> u16 {
    // Hidden explorer collapses the column entirely so cells get the
    // full terminal width. `space+0` or `e` restores it.
    if app.explorer_hidden {
        return 0;
    }
    // Hard floor (very narrow terminals) vs. comfortable default.
    // Both the tree and the git panel share this column, so the default
    // has to be wide enough to read `UNSTAGED (n)` / git footer rows
    // without clipping.
    const HARD_MIN: u16 = 18;
    const DEFAULT:  u16 = 32;

    let focused = matches!(app.focus, FocusId::Explorer);

    // Unfocused: park at DEFAULT so the cell area stays maximal —
    // long filenames clip, which is fine when the user's attention is
    // on the editor. Focused: grow to fit the longest visible row,
    // reserving 40 cols for the cell area so we never starve it.
    let max = if focused {
        area.width.saturating_sub(40).max(HARD_MIN)
    } else {
        (area.width / 3).clamp(HARD_MIN, 60)
    };
    let base = DEFAULT.min(max);

    if !focused {
        return base;
    }

    // File-tree rows — always relevant in Normal mode; in git modes the
    // project headers still sit above the git panel, so their widths
    // count too.
    let tree_max = app.explorer.entries.iter().map(|e| {
        let depth_off = match e.kind {
            EntryKind::Project { .. } | EntryKind::SectionHeader(_) => 0,
            _                                                       => (e.depth as usize) * 2,
        };
        let name_len = match e.kind {
            EntryKind::SectionHeader(label) => label.chars().count(),
            EntryKind::OpenCell { idx } => {
                use crate::cell::Session;
                let (title_len, badge_len, external) = match app.cells.get(idx).map(|c| c.active_session()) {
                    Some(Session::Edit(ed)) => {
                        if ed.read_only {
                            // Read-only buffers (help, …) render with
                            // an `[ACodeEditor]` badge and never flag
                            // external — size both in.
                            let name_len = ed.path.as_ref()
                                .and_then(|p| p.file_name())
                                .and_then(|n| n.to_str())
                                .map(|s| s.trim_start_matches('[').trim_end_matches(']').chars().count())
                                .unwrap_or(4); // "help"
                            (name_len, 1 + "[ACodeEditor]".len(), false)
                        } else {
                            let root = app.projects.projects
                                .get(app.projects.active)
                                .map(|p| p.root.clone());
                            let ext = match (root, &ed.path) {
                                (Some(r), Some(p)) => !p.starts_with(&r),
                                _                  => false,
                            };
                            // `[NEW]` renders for on-disk-missing
                            // buffers — same shape as PTY badges, so
                            // size it in.
                            use crate::editor::ExternalConflict as C;
                            let mut badge_w = if ed.is_new {
                                1 + "[NEW]".len()
                            } else if ed.external_conflict == Some(C::ModifiedOnDisk) {
                                1 + "[CONFLICT]".len()
                            } else {
                                0
                            };
                            if ed.external_conflict == Some(C::Deleted) {
                                badge_w += 1 + "[DELETED]".len();
                            }
                            (ed.file_name().chars().count(), badge_w, ext)
                        }
                    }
                    Some(Session::Shell(p))  => (pty_display_name(p, "shell").chars().count(),  1 + "[SHELL]".len(),  false),
                    Some(Session::Claude(p)) => (pty_display_name(p, "claude").chars().count(), 1 + "[CLAUDE]".len(), false),
                    Some(Session::Diff(v))     => (v.title.chars().count(), 0, false),
                    Some(Session::Conflict(v)) => (v.title.chars().count(), 0, false),
                    None => (0, 0, false),
                };
                title_len + badge_len + if external { "  [EXTERNAL]".chars().count() } else { 0 }
            }
            _ => e.path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.chars().count())
                .unwrap_or(0),
        };
        depth_off + 2 + name_len + 2
    }).max().unwrap_or(0);

    // Git-panel rows — the widest source when a git sub-mode is active.
    // Each case mirrors what the draw_* fn formats.
    let git_max = match app.explorer_mode {
        ExplorerMode::GitBranches => app.git.branches.iter()
            .map(|b| 3 + b.chars().count() + 1) // " ● " + name + trailing slack
            .max().unwrap_or(0),
        ExplorerMode::GitChanges => app.git.change_rows().iter()
            .map(|r| 5 + r.path.chars().count() + 1) // "   {marker} " + path
            .max().unwrap_or(0),
        ExplorerMode::GitLog => app.git_log.iter()
            .map(|e| {
                let top = 1 + e.sha_short.chars().count() + 2 + e.summary.chars().count();
                let bot = 10 + e.author.chars().count() + 3 + e.when.chars().count();
                top.max(bot)
            })
            .max().unwrap_or(0),
        _ => 0,
    };

    let needed = tree_max.max(git_max) as u16;
    needed.clamp(base, max)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let a = layout(frame.area(), app);

    if !app.explorer_hidden {
        draw_explorer_panel(frame, a.sidebar, app);
    }

    for (i, rect) in a.cells.iter().enumerate() {
        // Minimized cells get a zero-size rect — skip them entirely so
        // no border/garbage is painted into the corner.
        if rect.width == 0 || rect.height == 0 { continue; }
        draw_cell(frame, *rect, i, app);
    }

    let any_visible = app.cells.iter().any(|c| !c.minimized);
    if !any_visible {
        draw_empty_main(frame, compute_main_area(frame.area(), app), &app.theme);
    }

    draw_statusbar(frame, a.statusbar, app);

    // Native terminal cursor only for the focused Shell session —
    // Claude and the editor render their own. Also skip when the PTY
    // is scrolled back through history (the live cursor no longer
    // corresponds to anything visible), or when we're in Normal /
    // Visual — those modes drive a separate virtual cursor rendered
    // inside the PTY block.
    if let FocusId::Cell(i) = app.focus {
        if let (Some(cell), Some(rect)) = (app.cells.get(i), a.cells.get(i)) {
            if let Session::Shell(session) = cell.active_session() {
                if matches!(app.mode, Mode::Insert) {
                    if let Ok(parser) = session.parser.lock() {
                        if parser.screen().scrollback() == 0 {
                            let (cy, cx) = parser.screen().cursor_position();
                            let x = rect.x.saturating_add(1).saturating_add(cx);
                            let y = rect.y.saturating_add(1).saturating_add(cy);
                            let max_x = rect.x + rect.width.saturating_sub(1);
                            let max_y = rect.y + rect.height.saturating_sub(1);
                            if x < max_x && y < max_y {
                                frame.set_cursor_position((x, y));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn draw_empty_main(frame: &mut Frame, area: Rect, t: &Theme) {
    let p = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  no cells — :new shell | claude | edit",
            Style::default().fg(t.dim),
        )),
    ]);
    frame.render_widget(p, area);
}

fn draw_cell(frame: &mut Frame, area: Rect, cell_idx: usize, app: &App) {
    let t = &app.theme;
    let focused = app.focus == FocusId::Cell(cell_idx);
    let border_set   = if focused { border::THICK } else { border::PLAIN };
    let border_style = if focused {
        cell_mode_border_style(app)
    } else {
        t.border_unfocused()
    };

    let cell = &app.cells[cell_idx];
    let project_root = app.projects.projects
        .get(app.projects.active)
        .map(|p| p.root.as_path());
    let left_title  = cell_title_line(cell, project_root, area.width, focused, t);
    // Right-hand digit hint: shown only while Space has armed a jump,
    // so users can see which digit targets which cell. Cells are 1..9
    // (cell index 0 → `1`, …, index 8 → `9`); Explorer panel is 0.
    // Digit hint shown while a digit-triggered action is armed.
    //   * pending_jump → cyan (t.attn) — focus jump
    //   * pending_swap → orange        — swap cells (distinct from jump
    //                                    so the user can tell at a glance
    //                                    which action the digit will take)
    let swap_armed = app.pending_swap || app.pending_swap_follow;
    let right_title = if swap_armed {
        Some(
            Line::from(Span::styled(
                format!(" [{}] ", cell_idx + 1),
                Style::default()
                    .fg(Color::Rgb(0xff, 0xa8, 0x60))
                    .add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
        )
    } else if app.pending_jump {
        Some(
            Line::from(Span::styled(
                format!(" [{}] ", cell_idx + 1),
                Style::default().fg(t.attn).add_modifier(Modifier::BOLD),
            ))
            .alignment(Alignment::Right),
        )
    } else {
        None
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(border_style)
        .title_top(left_title);
    if let Some(r) = right_title {
        block = block.title_top(r);
    }

    match cell.active_session() {
        Session::Claude(_) | Session::Shell(_) => {
            let lines = render_pty(cell, &app.theme, focused, &app.mode);
            let paragraph = Paragraph::new(lines).block(block);
            frame.render_widget(paragraph, area);
        }
        Session::Edit(editor) => {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            if editor.is_welcome {
                // Welcome buffer: ignore the textarea's positioning
                // and render a centered banner + hint card. First
                // keystroke demotes the buffer (in editor.rs) and the
                // next frame falls back to the regular branch below.
                draw_welcome(frame, inner, &app.theme);
            } else if editor.wrap {
                draw_editor_wrapped(frame, inner, editor, t, focused);
            } else {
                frame.render_widget(&editor.textarea, inner);
            }
        }
        Session::Diff(view) => {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            draw_diff_lines(frame, inner, view, t);
        }
        Session::Conflict(view) => {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            draw_conflict(frame, inner, view, t);
        }
    }
}

/// Render an editor cell with soft-wrapped long lines. Logical rows
/// (the file's lines) can spill onto multiple visual rows; the gutter
/// shows a line number on the first visual row of each logical line
/// and is blank on continuation rows — matching vim's default
/// `set wrap` behaviour. Cursor and selection are overlaid in visual
/// coords but map back to logical `(row, col)` the rest of the app
/// uses. Scroll (`editor.scroll_top`) is a visual-row index, adjusted
/// here each frame so the cursor stays on screen.
///
/// Wrap break points prefer the last whitespace (linebreak) and
/// continuation rows inherit the leading indent of the logical line
/// (breakindent) so wrapped code stays visually nested. Wide chars
/// (CJK, emoji) are measured by display width via `unicode-width`.
fn draw_editor_wrapped(
    frame:   &mut Frame,
    area:    Rect,
    editor:  &crate::editor::Editor,
    theme:   &Theme,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 { return; }
    let lines = editor.textarea.lines();
    let (cur_row, cur_col) = editor.textarea.cursor();
    let sel = editor.textarea.selection_range();

    // Gutter: right-aligned line number + 1 space. Size to the widest
    // line number in the file, min 3 so single-digit files don't flicker.
    let total = lines.len().max(1);
    let digits = total.to_string().chars().count().max(3);
    let gutter_w = digits + 1;
    let content_w = (area.width as usize).saturating_sub(gutter_w).max(1);
    editor.last_content_w.set(content_w.min(u16::MAX as usize) as u16);
    editor.last_viewport_h.set(area.height);

    let rows = build_wrap_rows(lines, content_w);
    if rows.is_empty() { return; }

    // Cursor visual position. If the cursor sits past the last char of a
    // line whose content fills the last visual row, it lives on a phantom
    // next row — vim's default wrap behaviour. We model that as a
    // half-row past `rows[cur_vrow]` instead of adding a row to the
    // layout itself (so motions that ask "how many visual rows" stay
    // honest for the file's actual content).
    let (cur_vrow, cur_vcol) = cursor_visual_pos(&rows, lines, cur_row, cur_col, content_w);

    // Keep cursor in view. `scroll_top` is a *visual*-row index. The
    // clamp uses `rows.len() + 1` so the cursor's phantom row (one past
    // the last real row at a wrap boundary) stays visible.
    let height = area.height as usize;
    let mut scroll_top = editor.scroll_top.get();
    if cur_vrow < scroll_top {
        scroll_top = cur_vrow;
    } else if cur_vrow >= scroll_top + height {
        scroll_top = cur_vrow + 1 - height;
    }
    let max_top = (rows.len() + 1).saturating_sub(height);
    scroll_top = scroll_top.min(max_top);
    editor.scroll_top.set(scroll_top);

    let gutter_style   = Style::default().fg(theme.dim);
    let content_style  = Style::default().fg(theme.fg);
    let listchar_style = Style::default().fg(theme.muted);
    let sel_style      = Style::default().bg(theme.bg_sel);
    let cursor_style   = Style::default().fg(theme.bg).bg(theme.fg);
    let list_mode      = editor.list_mode;

    let buf = frame.buffer_mut();
    let visible_end = (scroll_top + height).min(rows.len());
    for vrow_idx in scroll_top..visible_end {
        let vrow = &rows[vrow_idx];
        let screen_y = area.y + (vrow_idx - scroll_top) as u16;

        // Gutter.
        let gutter_text = if vrow.start_char == 0 {
            format!("{:>w$} ", vrow.logical_row + 1, w = digits)
        } else {
            " ".repeat(gutter_w)
        };
        buf.set_string(area.x, screen_y, gutter_text, gutter_style);

        // Continuation prefix: `↳ ` (showbreak) then breakindent spaces.
        let content_x = area.x + gutter_w as u16;
        if vrow.start_char > 0 && vrow.prefix_cols >= crate::wrap::SHOWBREAK_COLS {
            buf.set_string(content_x, screen_y, crate::wrap::SHOWBREAK, listchar_style);
            let pad_w = vrow.prefix_cols as usize - crate::wrap::SHOWBREAK_COLS as usize;
            if pad_w > 0 {
                let pad: String = " ".repeat(pad_w);
                buf.set_string(content_x + crate::wrap::SHOWBREAK_COLS, screen_y, pad, content_style);
            }
        }

        // Content: render char-by-char so tabs expand, trailing spaces
        // and tabs pick up list-mode markers, and an EOL marker lands on
        // the last visual row of the logical line.
        let line = lines.get(vrow.logical_row).map(String::as_str).unwrap_or("");
        let slice_x = content_x + vrow.prefix_cols;
        let is_last_visual_of_line = vrow.end_char == line.chars().count();
        let syntax_line = editor.syntax.as_ref().map(|sh| (sh, vrow.logical_row));
        let end_col = render_row_content(
            buf, slice_x, screen_y, line, vrow, list_mode,
            content_style, listchar_style, syntax_line,
        );
        if list_mode && is_last_visual_of_line {
            let eol_x = slice_x + end_col;
            if eol_x < content_x + content_w as u16 {
                buf.set_string(eol_x, screen_y, "¬", listchar_style);
            }
        }

        // Selection overlay: translate the logical selection into screen
        // columns for this row, accounting for wide chars and prefix.
        if let Some(((sr, sc), (er, ec))) = sel {
            if let Some((lo, hi)) = clip_selection_to_row(sr, sc, er, ec, vrow) {
                let (lo_scr, hi_scr) = selection_screen_span(line, vrow, lo, hi);
                let start_x = content_x + lo_scr;
                let width = hi_scr.saturating_sub(lo_scr);
                paint_bg(buf, start_x, screen_y, width, sel_style);
            }
        }
    }

    // Cursor overlay. Drawn last so it sits above selection shading.
    // Only the focused cell gets a cursor block — unfocused cells
    // render their text "cold".
    if focused && cur_vrow >= scroll_top && cur_vrow < scroll_top + height {
        let screen_y = area.y + (cur_vrow - scroll_top) as u16;
        let content_x = area.x + gutter_w as u16;
        let cx = content_x + cur_vcol;
        // Grab the underlying char (space if past EOL) so the cursor
        // block stays readable.
        let ch = rows.get(cur_vrow)
            .and_then(|r| lines.get(r.logical_row).map(|l| (r, l)))
            .and_then(|(r, line)| char_at_visual_col(line, r, cur_vcol))
            .unwrap_or(' ');
        buf.set_string(cx, screen_y, ch.to_string(), cursor_style);
    }
}

use crate::wrap::{VisualRow, build_wrap_rows, cell_width, slice_display_width};

/// Cursor's visual row/col. Handles end-of-line phantom rows (cursor
/// past the last char of a wrapping line lands on a phantom next row
/// at col 0).
fn cursor_visual_pos(
    rows: &[VisualRow],
    lines: &[String],
    cur_row: usize,
    cur_col: usize,
    content_w: usize,
) -> (usize, u16) {
    // Find the visual row that contains cur_col (or the last row of
    // this logical line if cur_col is at/past the end).
    let mut last_in_row = 0usize;
    for (i, r) in rows.iter().enumerate() {
        if r.logical_row != cur_row { continue; }
        last_in_row = i;
        if cur_col >= r.start_char && cur_col < r.end_char {
            let line = lines.get(cur_row).map(String::as_str).unwrap_or("");
            let sub = slice_display_width(line, r.start_char, cur_col);
            let col = r.prefix_cols as usize + sub;
            return (i, col as u16);
        }
    }
    let vrow = last_in_row;
    // Cursor at end-of-line (or at the exact break boundary).
    let r = &rows[vrow];
    let line = lines.get(cur_row).map(String::as_str).unwrap_or("");
    let sub = slice_display_width(line, r.start_char, cur_col.min(line.chars().count()));
    let col = r.prefix_cols as usize + sub;
    if col >= content_w {
        (vrow + 1, 0)
    } else {
        (vrow, col as u16)
    }
}

/// The char whose cell contains `target_screen_col` within this visual
/// row, for the cursor overlay. Returns space if the target is past the
/// end of the slice (EOL cursor).
fn char_at_visual_col(line: &str, row: &VisualRow, target_screen_col: u16) -> Option<char> {
    let target = target_screen_col as usize;
    if target < row.prefix_cols as usize { return Some(' '); }
    let mut col = row.prefix_cols as usize;
    for c in line.chars().skip(row.start_char).take(row.end_char - row.start_char) {
        let w = cell_width(c);
        if target < col + w {
            // Tabs (and other control chars) render as a blank in the
            // cursor cell — show a space rather than the raw glyph.
            return Some(if c == '\t' || c.is_control() { ' ' } else { c });
        }
        col += w;
    }
    Some(' ')
}

fn clip_selection_to_row(
    sr: usize, sc: usize, er: usize, ec: usize, vrow: &VisualRow,
) -> Option<(usize, usize)> {
    // Selection is inclusive at start, exclusive at end — matches
    // tui_textarea's convention for selection_range().
    let row = vrow.logical_row;
    if row < sr || row > er { return None; }
    let lo_logical = if row == sr { sc } else { 0 };
    let hi_logical = if row == er { ec } else { usize::MAX };
    let lo = lo_logical.max(vrow.start_char);
    let hi = hi_logical.min(vrow.end_char);
    if lo >= hi { return None; }
    Some((lo, hi))
}

/// Translate a (lo, hi) char range within `vrow` into screen-column
/// offsets from the start of the content area (past the gutter).
fn selection_screen_span(line: &str, vrow: &VisualRow, lo: usize, hi: usize) -> (u16, u16) {
    let prefix = vrow.prefix_cols as usize;
    let lo_off = prefix + slice_display_width(line, vrow.start_char, lo);
    let hi_off = prefix + slice_display_width(line, vrow.start_char, hi);
    (lo_off as u16, hi_off as u16)
}

/// Render one visual row of content. Expands tabs to `TABSTOP` spaces
/// (or `→---` in list mode), swaps trailing spaces for `·`, and
/// returns the total screen columns written (used by the caller to
/// place an optional EOL marker). Indent spaces for breakindent are
/// NOT drawn here — caller already painted the row prefix.
fn render_row_content(
    buf:   &mut ratatui::buffer::Buffer,
    start_x: u16,
    y:       u16,
    line:    &str,
    row:     &VisualRow,
    list_mode: bool,
    content_style:  Style,
    listchar_style: Style,
    syntax: Option<(&crate::syntax::SyntaxHighlighter, usize)>,
) -> u16 {
    // Identify trailing-space region so list-mode can dim them. Counts
    // only spaces (not tabs) as trailing — matching vim's `trail:` list
    // char — and only when this row reaches the end of the logical line.
    let last_non_ws_char = {
        let mut last = None;
        for (i, c) in line.chars().enumerate() {
            if !matches!(c, ' ' | '\t') { last = Some(i); }
        }
        last
    };
    let is_last_visual = row.end_char == line.chars().count();

    // Pre-fetch sorted highlight spans for this line; advance a pointer
    // left-to-right through them as we render to avoid repeated lookups.
    let spans: &[(usize, usize, ratatui::style::Color)] =
        syntax.map(|(sh, lr)| sh.get_line(lr)).unwrap_or(&[]);
    let mut span_ptr = 0usize;

    let mut screen_col = 0u16;
    let tab = crate::wrap::TABSTOP as u16;
    for (i, c) in line.chars().enumerate()
        .skip(row.start_char).take(row.end_char - row.start_char)
    {
        // Advance past spans that end before this char.
        while span_ptr < spans.len() && spans[span_ptr].1 <= i {
            span_ptr += 1;
        }
        // Pick syntax color if this char falls inside the current span.
        let char_style = if span_ptr < spans.len() && i >= spans[span_ptr].0 {
            content_style.fg(spans[span_ptr].2)
        } else {
            content_style
        };

        let trailing_space = list_mode && is_last_visual && c == ' '
            && last_non_ws_char.map(|lws| i > lws).unwrap_or(true);
        match c {
            '\t' => {
                if list_mode {
                    buf.set_string(start_x + screen_col, y, "→", listchar_style);
                    let fill_w = tab.saturating_sub(1);
                    if fill_w > 0 {
                        let fill: String = "-".repeat(fill_w as usize);
                        buf.set_string(start_x + screen_col + 1, y, fill, listchar_style);
                    }
                } else {
                    let fill: String = " ".repeat(tab as usize);
                    buf.set_string(start_x + screen_col, y, fill, char_style);
                }
                screen_col += tab;
            }
            _ if trailing_space => {
                buf.set_string(start_x + screen_col, y, "·", listchar_style);
                screen_col += 1;
            }
            _ => {
                buf.set_string(start_x + screen_col, y, c.to_string(), char_style);
                screen_col += cell_width(c) as u16;
            }
        }
    }
    screen_col
}

fn paint_bg(buf: &mut ratatui::buffer::Buffer, x: u16, y: u16, width: u16, style: Style) {
    let end = x.saturating_add(width);
    let mut xi = x;
    while xi < end {
        if let Some(cell) = buf.cell_mut((xi, y)) {
            cell.set_style(style);
        }
        xi += 1;
    }
}

/// Render a `DiffView`'s visible slice of lines inside `area`, tinted
/// Render the welcome buffer: banner + version/author + hint lines,
/// centered horizontally and vertically inside `area`. The banner
/// is tinted `accent`; each hint is `cmd` in accent + `desc` in fg
/// so the eye picks out the commands quickly.
fn draw_welcome(frame: &mut Frame, area: Rect, t: &Theme) {
    // Project name stacked vertically: "A" / "Code" / "Editor". Each
    // block is 6 glyph rows; banners are rendered in order with a blank
    // row between them. Leading capitals ("A", "C", "E") are rendered in
    // near-white; trailing lowercase ("ode", "ditor") in dim.
    let a_banner: &[&str] = &[
        " █████╗ ",
        "██╔══██╗",
        "███████║",
        "██╔══██║",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ];
    let code_banner: &[&str] = &[
        " ██████╗ ██████╗ ██████╗ ███████╗",
        "██╔════╝██╔═══██╗██╔══██╗██╔════╝",
        "██║     ██║   ██║██║  ██║█████╗  ",
        "██║     ██║   ██║██║  ██║██╔══╝  ",
        "╚██████╗╚██████╔╝██████╔╝███████╗",
        " ╚═════╝ ╚═════╝ ╚═════╝ ╚══════╝",
    ];
    let editor_banner: &[&str] = &[
        "███████╗██████╗ ██╗████████╗ ██████╗ ██████╗ ",
        "██╔════╝██╔══██╗██║╚══██╔══╝██╔═══██╗██╔══██╗",
        "█████╗  ██║  ██║██║   ██║   ██║   ██║██████╔╝",
        "██╔══╝  ██║  ██║██║   ██║   ██║   ██║██╔══██╗",
        "███████╗██████╔╝██║   ██║   ╚██████╔╝██║  ██║",
        "╚══════╝╚═════╝ ╚═╝   ╚═╝    ╚═════╝ ╚═╝  ╚═╝",
    ];

    let version = env!("CARGO_PKG_VERSION");
    // `CARGO_PKG_AUTHORS` is `:`-joined when set; empty when absent.
    let authors_raw = env!("CARGO_PKG_AUTHORS");
    let author = if authors_raw.is_empty() { "stubbornmarlin3" } else {
        // Show only the first listed author to keep the line short.
        authors_raw.split(':').next().unwrap_or(authors_raw)
    };

    let accent = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
    let dim    = Style::default().fg(t.dim);
    let fg     = Style::default().fg(t.fg);
    let letter_style = Style::default().fg(t.fg).add_modifier(Modifier::BOLD);
    let rest_style   = dim;

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(
        a_banner.len() + code_banner.len() + editor_banner.len() + 10,
    );

    // Banner layout trick: pad each banner row on the right to the
    // widest banner ("Editor", 45 cols), so all three words have
    // identical line widths. The Paragraph's Alignment::Center then
    // centers the block as a whole, and because the padding is on the
    // right, the glyphs inside read as left-aligned against the same
    // column — "A" and "Code" hang off the left edge of "Editor" rather
    // than each being individually centered.
    let banner_w = a_banner.iter()
        .chain(code_banner.iter())
        .chain(editor_banner.iter())
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(0);

    // "A" — whole glyph highlighted (it's the whole word).
    for row in a_banner {
        let pad = " ".repeat(banner_w - row.chars().count());
        lines.push(Line::from(vec![
            Span::styled((*row).to_string(), letter_style),
            Span::raw(pad),
        ]));
    }
    lines.push(Line::from(""));

    // "Code" — leading "C" (first 8 cols) in fg, "ode" dim.
    // "Editor" — leading "E" (first 8 cols) in fg, "ditor" dim.
    let push_word = |lines: &mut Vec<Line<'static>>, rows: &[&str], banner_w: usize| {
        for row in rows {
            let chars: Vec<char> = row.chars().collect();
            let split = chars.len().min(8);
            let head: String = chars[..split].iter().collect();
            let tail: String = chars[split..].iter().collect();
            let pad  = " ".repeat(banner_w - chars.len());
            lines.push(Line::from(vec![
                Span::styled(head, letter_style),
                Span::styled(tail, rest_style),
                Span::raw(pad),
            ]));
        }
    };
    push_word(&mut lines, code_banner, banner_w);
    lines.push(Line::from(""));
    push_word(&mut lines, editor_banner, banner_w);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("v{version}  —  by {author}"),
        dim,
    )));
    lines.push(Line::from(""));

    // Each hint gets its own line. Pad the command column to the widest
    // command so the descriptions line up even with per-line centering
    // (Alignment::Center centers each line by its own width — padding
    // each line to a uniform length gives us block-centered alignment).
    const HINTS: &[(&str, &str)] = &[
        (":Q",        "quit the app"),
        (":help",     "full reference card"),
        (":e <path>", "open a file (nonexistent → [NEW])"),
        (":new <cell>", "new cell (shell | claude | edit [path])"),
    ];
    let cmd_w = HINTS.iter().map(|(c, _)| c.chars().count()).max().unwrap_or(0);
    for (cmd, desc) in HINTS {
        let pad = " ".repeat(cmd_w - cmd.chars().count());
        lines.push(Line::from(vec![
            Span::styled(format!("{cmd}{pad}"), accent),
            Span::raw("   "),
            Span::styled((*desc).to_string(), fg),
        ]));
    }

    // Vertical centering — shrink the area from the top by half the
    // leftover height so the block sits in the middle. If the content
    // is taller than `area`, just render from the top and let the
    // bottom clip.
    let content_h = lines.len() as u16;
    let top_pad = area.height.saturating_sub(content_h) / 2;
    let centered = Rect {
        x:      area.x,
        y:      area.y + top_pad,
        width:  area.width,
        height: area.height.saturating_sub(top_pad),
    };
    let para = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(para, centered);
}

/// by `DiffTag`. Shared between diff cells and any future per-line
/// diff renderer.
/// Render a `ConflictView` in two columns: ours on the left, theirs
/// on the right. Selected hunk gets a solid background; other hunks
/// get a subtle one. Resolved hunks show their chosen side with a
/// check-style marker; unresolved show both sides flagged.
fn draw_conflict(frame: &mut Frame, area: Rect, view: &crate::conflict::ConflictView, t: &Theme) {
    use crate::conflict::{Resolution as R, Segment};
    if area.width < 20 || area.height == 0 { return; }
    let col_w = (area.width.saturating_sub(3)) / 2;
    let left_rect  = Rect { x: area.x,              y: area.y, width: col_w, height: area.height };
    let sep_rect   = Rect { x: area.x + col_w,      y: area.y, width: 3,     height: area.height };
    let right_rect = Rect { x: area.x + col_w + 3,  y: area.y, width: area.width - col_w - 3, height: area.height };

    let selected_seg_idx = view.hunk_indices.get(view.selected).copied();
    let editing_seg_idx  = view.editing.as_ref().map(|e| e.seg_idx);

    let mut left_lines:  Vec<Line> = Vec::new();
    let mut right_lines: Vec<Line> = Vec::new();

    for (i, seg) in view.segments.iter().enumerate() {
        match seg {
            Segment::Context(lines) => {
                let s = Style::default().fg(t.dim);
                for l in lines {
                    left_lines.push(Line::from(Span::styled(pad_to_width(l, col_w as usize), s)));
                    right_lines.push(Line::from(Span::styled(pad_to_width(l, col_w as usize), s)));
                }
            }
            Segment::Conflict(h) => {
                let is_sel = selected_seg_idx == Some(i);
                let (ours_style, theirs_style) = conflict_colors(&h.resolution, is_sel, t);
                // While editing this hunk, render the textarea lines on
                // the left instead of h.ours so the user sees what
                // they're typing. Right column still shows theirs for
                // reference.
                let ours_view: Vec<String> = if editing_seg_idx == Some(i) {
                    view.editing.as_ref()
                        .map(|e| e.textarea.lines().iter().cloned().collect())
                        .unwrap_or_else(|| h.ours.clone())
                } else {
                    h.ours.clone()
                };
                let edit_style = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
                let active_ours_style = if editing_seg_idx == Some(i) { edit_style } else { ours_style };
                let max_len = ours_view.len().max(h.theirs.len());
                for row in 0..max_len {
                    let ol = ours_view.get(row).cloned().unwrap_or_default();
                    let tl = h.theirs.get(row).cloned().unwrap_or_default();
                    left_lines.push(Line::from(Span::styled(pad_to_width(&ol, col_w as usize), active_ours_style)));
                    right_lines.push(Line::from(Span::styled(pad_to_width(&tl, col_w as usize), theirs_style)));
                }
            }
        }
    }

    // Scroll both columns together.
    let visible = area.height as usize;
    let total   = left_lines.len();
    let scroll  = view.scroll.min(total.saturating_sub(visible));
    let left:  Vec<Line> = left_lines.into_iter().skip(scroll).take(visible).collect();
    let right: Vec<Line> = right_lines.into_iter().skip(scroll).take(visible).collect();

    // Separator column: a single vertical bar per row.
    let sep: Vec<Line> = (0..visible).map(|_| {
        Line::from(Span::styled(" │ ", Style::default().fg(t.dim)))
    }).collect();

    frame.render_widget(Paragraph::new(left),  left_rect);
    frame.render_widget(Paragraph::new(sep),   sep_rect);
    frame.render_widget(Paragraph::new(right), right_rect);

    // Bottom hint line (overlays last row if tight).
    if area.height >= 2 {
        let hint_rect = Rect { x: area.x, y: area.y + area.height - 1, width: area.width, height: 1 };
        let mut spans: Vec<Span> = Vec::new();
        let editing = view.is_editing();
        if editing {
            spans.push(Span::styled(" editing hunk — Esc to commit",
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)));
        } else {
            spans.push(Span::styled(" j/k next  ", Style::default().fg(t.dim)));
            spans.push(Span::styled("o ours  ",    Style::default().fg(t.info)));
            spans.push(Span::styled("t theirs  ",  Style::default().fg(t.info)));
            spans.push(Span::styled("b both  ",    Style::default().fg(t.info)));
            spans.push(Span::styled("e edit  ",    Style::default().fg(t.info)));
            spans.push(Span::styled(":w save",     Style::default().fg(t.ok)));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), hint_rect);
    }
    let _ = R::Unresolved;
}

fn conflict_colors(r: &crate::conflict::Resolution, selected: bool, t: &Theme) -> (Style, Style) {
    use crate::conflict::Resolution as R;
    let base_ours   = Style::default().fg(t.fg);
    let base_theirs = Style::default().fg(t.fg);
    let (l, rgt) = match r {
        R::Unresolved  => (
            base_ours.bg(t.bg_sel).add_modifier(Modifier::BOLD),
            base_theirs.bg(t.bg_sel).add_modifier(Modifier::BOLD),
        ),
        R::KeepOurs    => (base_ours.fg(t.ok),          Style::default().fg(t.dim).add_modifier(Modifier::CROSSED_OUT)),
        R::KeepTheirs  => (Style::default().fg(t.dim).add_modifier(Modifier::CROSSED_OUT), base_theirs.fg(t.ok)),
        R::KeepBoth    => (base_ours.fg(t.ok),          base_theirs.fg(t.ok)),
        R::Custom(_)   => (base_ours.fg(t.accent).add_modifier(Modifier::BOLD),
                           Style::default().fg(t.dim).add_modifier(Modifier::CROSSED_OUT)),
    };
    if selected {
        (l.add_modifier(Modifier::REVERSED), rgt.add_modifier(Modifier::REVERSED))
    } else {
        (l, rgt)
    }
}

fn draw_diff_lines(frame: &mut Frame, area: Rect, view: &crate::diff::DiffView, t: &Theme) {
    if area.width == 0 || area.height == 0 { return; }
    let visible = area.height as usize;
    let lines: Vec<Line> = view.lines.iter()
        .skip(view.scroll)
        .take(visible)
        .map(|dl| {
            let style = match dl.tag {
                DiffTag::FileHeader => Style::default().fg(t.accent).bold(),
                DiffTag::HunkHeader => Style::default().fg(t.info),
                DiffTag::Addition   => Style::default().fg(t.ok),
                DiffTag::Deletion   => Style::default().fg(t.err),
                DiffTag::Context    => Style::default().fg(t.fg),
                DiffTag::Binary     => Style::default().fg(t.dim).add_modifier(Modifier::ITALIC),
            };
            Line::from(Span::styled(
                pad_to_width(&dl.text, area.width as usize),
                style,
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Title line for a cell. Single session → just the label. Multiple
/// sessions → `lbl1 · lbl2 · lbl3` with the active one styled like a
/// focused tab and others dimmed. If the full tab strip won't fit in
/// the available width, collapse to `{active} N/M`. A `[↑N]` suffix
/// appears when the active PTY is scrolled back through its history.
fn cell_title_line(cell: &Cell, project_root: Option<&Path>, width: u16, focused: bool, t: &Theme) -> Line<'static> {
    let active_style   = if focused { t.title_focused() } else { t.title_unfocused() };
    let inactive_style = Style::default().fg(t.dim);
    let sep_style      = Style::default().fg(t.dim);
    let sb_style       = Style::default().fg(t.warn);
    // Match the [CLAUDE]/[SHELL] badge colour (t.warn) so all
    // title-row badges share the same visual language.
    let ext_style      = Style::default().fg(t.warn).add_modifier(Modifier::BOLD);
    let scroll_suffix  = scrollback_suffix(cell.active_session());

    // Whether the active editor's file is outside the active project —
    // drives a `[EXTERNAL]` suffix after the tab label so the user can
    // tell at a glance the open buffer isn't part of the project.
    let is_external = is_editor_external(cell.active_session(), project_root);

    let parts: Vec<(String, Option<&'static str>)> =
        cell.sessions.iter().map(cell_tab_parts).collect();

    let deleted_badge = session_deleted_badge(cell.active_session());

    if cell.sessions.len() == 1 {
        let (label, badge) = &parts[0];
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(label.clone(), active_style),
        ];
        if let Some(b) = badge {
            spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
        }
        if let Some(b) = deleted_badge {
            spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
        }
        if is_external {
            spans.push(Span::styled(" [EXTERNAL]", ext_style));
        }
        if let Some(sfx) = scroll_suffix.clone() {
            spans.push(Span::styled(sfx, sb_style));
        }
        spans.push(Span::raw(" "));
        return Line::from(spans);
    }

    // Reserve a couple of chars on each side of the title for padding
    // and the right-side digit when CTL is held.
    let avail = width.saturating_sub(8) as usize;
    let joined_len: usize = parts.iter()
        .map(|(l, b)| l.chars().count() + b.map(|s| 1 + s.chars().count()).unwrap_or(0))
        .sum::<usize>()
        + parts.len().saturating_sub(1) * 3 // " · " between tabs
        + scroll_suffix.as_ref().map(|s| s.chars().count()).unwrap_or(0);

    if joined_len <= avail {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        for (i, (label, badge)) in parts.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" · ", sep_style));
            }
            let style = if i == cell.active { active_style } else { inactive_style };
            spans.push(Span::styled(label.clone(), style));
            if let Some(b) = badge {
                let bs = if i == cell.active { badge_style_for(t, b) } else { inactive_style };
                spans.push(Span::styled(format!(" {b}"), bs));
            }
            if i == cell.active {
                if let Some(b) = deleted_badge {
                    spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
                }
                if is_external {
                    spans.push(Span::styled(" [EXTERNAL]", ext_style));
                }
            }
        }
        if let Some(sfx) = scroll_suffix {
            spans.push(Span::styled(sfx, sb_style));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    } else {
        let (label, badge) = &parts[cell.active];
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(label.clone(), active_style),
        ];
        if let Some(b) = badge {
            spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
        }
        if let Some(b) = deleted_badge {
            spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
        }
        if is_external {
            spans.push(Span::styled(" [EXTERNAL]", ext_style));
        }
        spans.push(Span::styled(
            format!("  {}/{}", cell.active + 1, cell.sessions.len()),
            inactive_style,
        ));
        if let Some(sfx) = scroll_suffix {
            spans.push(Span::styled(sfx, sb_style));
        }
        spans.push(Span::raw(" "));
        Line::from(spans)
    }
}

/// Editor file outside the active project root → true. Returns false
/// for non-editors and when there's no active project. Read-only
/// synthetic buffers (help, welcome, previews) and unnamed / [NEW]
/// buffers are never "external" — they're not files on disk, so the
/// label would be misleading.
fn is_editor_external(sess: &Session, project_root: Option<&Path>) -> bool {
    let Session::Edit(ed) = sess else { return false; };
    if ed.read_only || ed.is_welcome || ed.is_new { return false; }
    let (Some(root), Some(path)) = (project_root, ed.path.as_ref()) else { return false; };
    !path.starts_with(root)
}

/// Border tint for the focused cell, keyed off the current outer mode.
/// Insert lights up green, Visual goes magenta, Command stays on the
/// cyan accent (no separate tint — the CMD badge in the statusbar is
/// already unmistakable). Normal / default → accent.
fn cell_mode_border_style(app: &App) -> Style {
    let t = &app.theme;
    let color = match &app.mode {
        Mode::Insert       => t.ok,
        Mode::Visual { .. }=> t.magenta,
        _                  => t.accent,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Border tint for the focused explorer. GitOverview/GitLog → green,
/// GitBranches → purple, GitChanges → orange. Normal → accent (default
/// cyan). Matches the user's colour mapping so the border alone tells
/// you which sub-mode is live.
fn explorer_mode_border_style(app: &App) -> Style {
    let t = &app.theme;
    let color = match app.explorer_mode {
        ExplorerMode::Normal       => t.accent,
        ExplorerMode::GitOverview  => t.ok,
        ExplorerMode::GitLog       => t.ok,
        ExplorerMode::GitBranches  => t.purple,
        ExplorerMode::GitChanges   => t.warn,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Colour for a per-session kind badge (`[CLAUDE]`, `[SHELL]`,
/// `[ACodeEditor]`, `[NEW]`). Each badge gets a distinct hue so a
/// glance at the tab strip tells you what you're looking at.
fn badge_style_for(t: &Theme, badge: &str) -> Style {
    let c = match badge {
        "[CLAUDE]"      => t.warn,
        "[SHELL]"       => t.warn,
        "[ACodeEditor]" => Color::Rgb(0xc7, 0x8e, 0xff),
        "[NEW]"         => t.ok,
        // External-disk state — red + bold so the title row screams
        // at the user when disk content diverged ("[CONFLICT]") or
        // the file vanished behind them ("[DELETED]").
        "[DELETED]" | "[CONFLICT]" => t.err,
        _               => t.accent,
    };
    Style::default().fg(c).add_modifier(Modifier::BOLD)
}

/// Returns `Some("[DELETED]")` when this session is an editor whose
/// on-disk file has been removed externally. `None` otherwise.
fn session_deleted_badge(s: &Session) -> Option<&'static str> {
    use crate::editor::ExternalConflict as C;
    match s {
        Session::Edit(ed) if ed.external_conflict == Some(C::Deleted) => Some("[DELETED]"),
        _ => None,
    }
}

/// `" [↑12]"` when the active PTY is scrolled back 12 lines. `None` for
/// editors or a live view.
fn scrollback_suffix(s: &Session) -> Option<String> {
    let pty = s.as_pty()?;
    let n = pty.scrollback();
    if n == 0 { None } else { Some(format!(" [↑{n}]")) }
}

/// Label + optional badge for a session's tab.
///
/// PTYs show the child's own name: Claude uses its OSC title (the cell
/// name it picks per conversation), shells show the program name (e.g.
/// `bash`, `powershell`). Both carry a badge (`[CLAUDE]` / `[SHELL]`)
/// so the kind is obvious even when the names coincide.
///
/// Editor markers:
///   `*`  dirty buffer
///   `⚠`  file changed externally (conflict — blocks `:w`)
///   `✗`  file deleted externally
fn cell_tab_parts(s: &Session) -> (String, Option<&'static str>) {
    match s {
        Session::Claude(p) => (pty_display_name(p, "claude"), Some("[CLAUDE]")),
        Session::Shell(p)  => (pty_display_name(p, "shell"),  Some("[SHELL]")),
        Session::Edit(ed)  => {
            use crate::editor::ExternalConflict as C;
            // Read-only synthetic buffers (built by `:help` and friends)
            // get a kind-badge like PTYs do — it's not a file, so the
            // label drops the brackets and skips the dirty/conflict
            // decorations that only make sense for real files.
            if ed.read_only {
                let name = ed.path.as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.trim_start_matches('[').trim_end_matches(']').to_string())
                    .unwrap_or_else(|| "help".to_string());
                return (name, Some("[ACodeEditor]"));
            }
            // Welcome landing-pad buffer — a kind, not a file. Becomes a
            // regular `unknown [NEW]` scratch on the first keystroke.
            if ed.is_welcome {
                return ("welcome".to_string(), Some("[ACodeEditor]"));
            }
            // Unnamed scratch — no path yet. Shown as `unknown [NEW]`.
            // `:w <path>` names it and clears is_new.
            if ed.path.is_none() {
                let dirty = if ed.dirty { "*" } else { "" };
                return (format!("unknown{dirty}"), Some("[NEW]"));
            }
            let name = ed.file_name();
            let dirty = if ed.dirty { "*" } else { "" };
            // External-disk states carry a dedicated red badge so the
            // condition reads as loud as it warrants:
            //   ModifiedOnDisk → [CONFLICT]
            //   Deleted        → [DELETED]   (attached elsewhere via
            //                    session_deleted_badge)
            // The [NEW] badge takes priority for a freshly-named buffer
            // that hasn't been written yet.
            let badge = if ed.is_new {
                Some("[NEW]")
            } else if ed.external_conflict == Some(C::ModifiedOnDisk) {
                Some("[CONFLICT]")
            } else {
                None
            };
            (format!("{name}{dirty}"), badge)
        }
        Session::Diff(view) => (format!("diff · {}", view.title), None),
        Session::Conflict(view) => {
            let unresolved = view.unresolved_count();
            let total      = view.total_hunks();
            (
                format!("{} ({}/{})", view.title, total.saturating_sub(unresolved), total),
                None,
            )
        }
    }
}

/// PTY cell name: prefer the OSC title the child set (Claude picks a
/// per-conversation title; some shells set one from `PROMPT_COMMAND`),
/// fall back to the program's basename, and finally `fallback`.
fn pty_display_name(p: &crate::session::PtySession, fallback: &str) -> String {
    let title = p.title();
    let t = title.trim();
    if !t.is_empty() {
        // Windows shells often publish their full exe path as the OSC
        // title (e.g. "C:\Windows\System32\...\powershell.exe"). Strip
        // it to the executable's bare stem so the cell header stays
        // readable. Any non-path title (Claude's per-convo name, a
        // shell prompt string) falls through unchanged.
        if looks_like_exe_path(t) {
            return pad_emoji_widths(&short_program_name(t));
        }
        return pad_emoji_widths(t);
    }
    let prog = short_program_name(&p.program);
    if prog.is_empty() { fallback.to_string() } else { prog }
}

fn looks_like_exe_path(s: &str) -> bool {
    (s.contains('\\') || s.contains('/')) && s.to_ascii_lowercase().ends_with(".exe")
}

/// Insert a compensating space after characters that terminals (notably
/// Windows Terminal) render as 2 columns but `unicode-width` — and thus
/// ratatui's layout — classifies as 1. Without this, the glyph spills
/// into the next cell and eats its content (e.g. Claude's "✳ Claude
/// Code" title renders as "✳Claude Code", swallowing the space).
fn pad_emoji_widths(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        out.push(c);
        if is_wide_but_narrow(c) {
            out.push(' ');
        }
    }
    out
}

/// Narrow-per-unicode-width characters commonly drawn as emoji (2 cols)
/// by terminals. Minimal allow-list targeted at glyphs we actually see
/// in PTY titles — spinners, status dots, Claude Code's prefix.
fn is_wide_but_narrow(c: char) -> bool {
    matches!(c,
        '\u{2733}'  // ✳ EIGHT SPOKED ASTERISK — Claude Code title prefix
      | '\u{23FA}'  // ⏺ BLACK CIRCLE FOR RECORD
      | '\u{25C9}'  // ◉ FISHEYE
      | '\u{25CE}'  // ◎ BULLSEYE
      | '\u{2B24}'  // ⬤ BLACK LARGE CIRCLE
      | '\u{2B55}'  // ⭕ HEAVY LARGE CIRCLE
      | '\u{2B1B}'  // ⬛ BLACK LARGE SQUARE
      | '\u{2B1C}'  // ⬜ WHITE LARGE SQUARE
    )
}

/// Cell index → digit shown in status messages / chord hints. Cells
/// 0..8 are shown as `1..9` (Explorer occupies `0`).
fn cell_digit(idx: usize) -> char {
    if idx < 9 { (b'1' + idx as u8) as char } else { '?' }
}

fn short_program_name(program: &str) -> String {
    std::path::Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_string()
}

fn render_pty(cell: &Cell, t: &Theme, focused: bool, mode: &Mode) -> Vec<Line<'static>> {
    let session = match cell.active_session().as_pty() {
        Some(p) => p,
        None => return Vec::new(),
    };

    // Refresh rows_emitted before we interpret any absolute positions.
    // Without this, a burst of PTY output since the last frame would
    // place the virtual cursor / selection on the wrong row.
    session.tick_rows_emitted();

    let show_vcursor = focused
        && matches!(mode, Mode::Normal | Mode::Visual { .. });
    let vcursor = session.vcursor;
    let anchor  = if focused && matches!(mode, Mode::Visual { .. }) {
        session.visual_anchor
    } else {
        None
    };
    // Viewport positions for overlays — `None` if the position is
    // currently off-screen (user scrolled past it).
    let v_vp = if show_vcursor { session.vpos_viewport_row(vcursor).map(|r| (r, vcursor.col)) } else { None };

    // Normalize selection so `start` is older (smaller abs), `end`
    // newer. Mirrors `visual_selection_text` so what's rendered matches
    // what gets copied.
    let sel_abs_range = anchor.map(|a| {
        let (start, end) = if a.abs < vcursor.abs
            || (a.abs == vcursor.abs && a.col <= vcursor.col)
        {
            (a, vcursor)
        } else {
            (vcursor, a)
        };
        (start, end)
    });

    // Pre-compute the selection's viewport-row bounds. Endpoints that
    // are scrolled off-screen clamp to the visible edge so a partial
    // span still renders coherently.
    let sel_vrange = sel_abs_range.map(|(s, e)| {
        let top_row    = session.vpos_viewport_row(s);
        let bot_row    = session.vpos_viewport_row(e);
        (s.col, e.col, top_row, bot_row)
    });

    match session.parser.lock() {
        Ok(parser) => render_screen(parser.screen(), t, v_vp, sel_vrange),
        Err(_) => vec![Line::from(Span::styled(
            " (pty lock poisoned)",
            Style::default().fg(t.err),
        ))],
    }
}

fn render_screen(
    screen: &vt100::Screen,
    t: &Theme,
    vcursor_vp: Option<(u16, u16)>,
    sel_vrange: Option<(u16, u16, Option<u16>, Option<u16>)>,
) -> Vec<Line<'static>> {
    let (rows, cols) = screen.size();
    let mut lines = Vec::with_capacity(rows as usize);

    // Selection endpoints in viewport-row space. `None` means the
    // endpoint is off-screen — we clamp to 0 / rows-1 so a
    // partially-visible span still paints.
    let (sel_start_col, sel_end_col, sel_start_row, sel_end_row) = match sel_vrange {
        Some((sc, ec, sr, er)) => (Some(sc), Some(ec), sr, er),
        None => (None, None, None, None),
    };
    let sel_active = sel_start_col.is_some();
    let sel_bg = t.bg_sel;
    let cursor_bg = t.fg;
    let cursor_fg = t.bg;

    for r in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut current_style: Option<Style> = None;

        for c in 0..cols {
            let cell = match screen.cell(r, c) {
                Some(c) => c,
                None => continue,
            };
            let mut style = cell_style(cell);
            let content = cell.contents();
            let text = if content.is_empty() { " ".to_string() } else { content };

            // Selection overlay — paint inside the normalized range.
            // Endpoints clamp to 0 / rows-1 when scrolled off-screen,
            // so a partially-visible span still paints coherently.
            if sel_active {
                let top = sel_start_row.unwrap_or(0);
                let bot = sel_end_row.unwrap_or(rows - 1);
                if r >= top && r <= bot {
                    let col_lo = if r == top { sel_start_col.unwrap_or(0) } else { 0 };
                    let col_hi = if r == bot { sel_end_col.unwrap_or(cols - 1) } else { cols - 1 };
                    if c >= col_lo && c <= col_hi {
                        style = style.bg(sel_bg);
                    }
                }
            }

            // Virtual cursor — drawn last so it wins over the selection
            // bg on its own cell.
            if let Some((vr, vc)) = vcursor_vp {
                if vr == r && vc == c {
                    style = style.bg(cursor_bg).fg(cursor_fg);
                }
            }

            match current_style {
                Some(s) if s == style => buf.push_str(&text),
                _ => {
                    if let Some(s) = current_style.take() {
                        spans.push(Span::styled(std::mem::take(&mut buf), s));
                    }
                    buf.push_str(&text);
                    current_style = Some(style);
                }
            }
        }
        if let Some(s) = current_style {
            spans.push(Span::styled(buf, s));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();

    match cell.fgcolor() {
        vt100::Color::Default      => {}
        vt100::Color::Idx(i)       => style = style.fg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style = style.fg(Color::Rgb(r, g, b)),
    }
    match cell.bgcolor() {
        vt100::Color::Default      => {}
        vt100::Color::Idx(i)       => style = style.bg(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => style = style.bg(Color::Rgb(r, g, b)),
    }

    let mut mods = Modifier::empty();
    if cell.bold()      { mods |= Modifier::BOLD; }
    if cell.italic()    { mods |= Modifier::ITALIC; }
    if cell.underline() { mods |= Modifier::UNDERLINED; }
    if cell.inverse()   { mods |= Modifier::REVERSED; }
    style.add_modifier(mods)
}

/// Left-aligned title for the explorer sidebar: just `explorer`. The digit
/// hint is rendered as a separate right-aligned title (see
/// [`sidebar_digit_title`]) so it mirrors the cell title layout.
fn sidebar_title(app: &App) -> Line<'static> {
    let t = &app.theme;
    let focused = app.focus == FocusId::Explorer;
    let style = if focused { t.title_focused() } else { t.title_unfocused() };
    Line::from(vec![
        Span::raw(" "),
        Span::styled("explorer", style),
        Span::raw(" "),
    ])
}

/// Right-aligned `[0]` hint shown on the explorer sidebar while a
/// Space-armed jump is pending — matches the cell title `[1]..[9]`
/// hints so users see all jump targets in one glance.
fn sidebar_digit_title(t: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        " [0] ",
        Style::default().fg(t.attn).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Right)
}

fn draw_explorer_panel(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let focused = app.focus == FocusId::Explorer;
    let border_set = if focused { border::THICK } else { border::PLAIN };
    let border_style = if focused {
        explorer_mode_border_style(app)
    } else {
        t.border_unfocused()
    };

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set)
        .border_style(border_style)
        .title_top(sidebar_title(app));
    if app.pending_jump {
        block = block.title_top(sidebar_digit_title(t));
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    match app.explorer_mode {
        ExplorerMode::Normal => draw_explorer_normal(frame, inner, app),
        _                 => draw_explorer_git_mode(frame, inner, app),
    }
}

/// Normal (file tree) rendering. If we're in a repo, reserve 3 rows at
/// the bottom for the compact git footer — a separator + branch line +
/// counts line.
fn draw_explorer_normal(frame: &mut Frame, inner: Rect, app: &App) {
    let t = &app.theme;
    let show_footer = app.git.is_repo();
    let footer_h: u16 = if show_footer && inner.height >= 5 { 3 } else { 0 };
    let list_h = inner.height.saturating_sub(footer_h);

    let list_rect = Rect { x: inner.x, y: inner.y, width: inner.width, height: list_h };
    let items: Vec<ListItem> = app.explorer.entries.iter().map(|e| {
        ListItem::new(truncate_line_with_arrow(entry_line(e, app, inner.width as usize), inner.width as usize))
    }).collect();
    let list = List::new(items)
        .highlight_style(Style::default().bg(t.bg_sel).add_modifier(Modifier::BOLD));

    // Persisted offset: scroll only when selection leaves the viewport
    // so moving the cursor mid-view doesn't drag the whole list.
    let total = app.explorer.entries.len();
    let h = list_h as usize;
    let sel = app.explorer.selected;
    let max_offset = total.saturating_sub(h.max(1));
    let mut offset = app.explorer.view_offset.get().min(max_offset);
    if h > 0 {
        // Section headers immediately above the selection aren't
        // selectable but belong visually with it — walk upward past
        // them so scrolling to the top selectable row still reveals
        // the "OPEN CELLS" / "PROJECTS" header sitting above.
        let mut anchor = sel;
        while anchor > 0 && !app.explorer.entries[anchor - 1].kind.is_selectable() {
            anchor -= 1;
        }
        if anchor < offset {
            offset = anchor;
        } else if sel >= offset + h {
            offset = sel + 1 - h;
        }
    }
    app.explorer.view_offset.set(offset);

    let mut state = ListState::default().with_offset(offset);
    if !app.explorer.entries.is_empty() {
        state.select(Some(sel));
    }
    frame.render_stateful_widget(list, list_rect, &mut state);

    // Scroll indicators in the top/bottom-right of the list area so the
    // user can tell when content is clipped above or below the viewport.
    if list_rect.width >= 1 && h > 0 && total > h {
        let ind_x = list_rect.x + list_rect.width - 1;
        if offset > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled("▲", Style::default().fg(t.dim))),
                Rect { x: ind_x, y: list_rect.y, width: 1, height: 1 },
            );
        }
        if offset + h < total {
            frame.render_widget(
                Paragraph::new(Span::styled("▼", Style::default().fg(t.dim))),
                Rect { x: ind_x, y: list_rect.y + list_h - 1, width: 1, height: 1 },
            );
        }
    }

    if footer_h > 0 {
        let footer_rect = Rect {
            x: inner.x,
            y: inner.y + list_h,
            width: inner.width,
            height: footer_h,
        };
        draw_git_footer_compact(frame, footer_rect, app);
    }
}

/// 2-line status summary at the bottom of the explorer panel (plus 1-row
/// separator above it). Shown in Normal mode only. Keeps the user
/// oriented without stealing list space.
fn draw_git_footer_compact(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let g = &app.git;

    // Same `─ git ─────` divider the expanded git mode uses, so the
    // separator reads as a section label instead of a nameless rule.
    // Surface mid-op state right after the label so a merge/rebase
    // state shows up without having to expand.
    let (label_style, rule_style) = git_divider_styles(app);
    let mut sep_spans: Vec<Span<'static>> = vec![
        Span::styled("─ ", rule_style),
        Span::styled("git", label_style),
        Span::raw(" "),
    ];
    if !g.op_state.is_clean() {
        sep_spans.push(Span::styled(
            format!("[{}] ", g.op_state.label()),
            Style::default().fg(t.err).add_modifier(Modifier::BOLD),
        ));
    }
    let used: usize = sep_spans.iter().map(|s| s.width()).sum();
    let trailing = (area.width as usize).saturating_sub(used);
    sep_spans.push(Span::styled("─".repeat(trailing), rule_style));
    let sep_line = Line::from(sep_spans);

    let branch_style = if !g.op_state.is_clean() || g.has_conflicts() {
        Style::default().fg(t.err).add_modifier(Modifier::BOLD)
    } else if !g.is_clean() {
        Style::default().fg(t.warn)
    } else {
        Style::default().fg(t.ok)
    };
    let mut line1: Vec<Span> = vec![
        Span::raw(" "),
        Span::styled(format!("⎇ {}", g.branch), branch_style),
    ];
    // Mid-op tag lives on the `─ git ─ [merging] ─` separator above;
    // don't also duplicate it on the branch line.
    if g.ahead > 0 {
        line1.push(Span::styled(format!("  ↑{}", g.ahead), Style::default().fg(t.ok)));
    }
    if g.behind > 0 {
        line1.push(Span::styled(format!(" ↓{}", g.behind), Style::default().fg(t.warn)));
    }

    let mut line2: Vec<Span> = vec![Span::raw(" ")];
    let mut pushed_any = false;
    if g.staged > 0 {
        line2.push(Span::styled(format!("S{}", g.staged), Style::default().fg(t.ok)));
        pushed_any = true;
    }
    if g.modified > 0 {
        if pushed_any { line2.push(Span::raw(" ")); }
        line2.push(Span::styled(format!("M{}", g.modified), Style::default().fg(t.warn)));
        pushed_any = true;
    }
    if g.untracked > 0 {
        if pushed_any { line2.push(Span::raw(" ")); }
        line2.push(Span::styled(format!("?{}", g.untracked), Style::default().fg(t.info)));
        pushed_any = true;
    }
    if g.conflicts > 0 {
        if pushed_any { line2.push(Span::raw(" ")); }
        line2.push(Span::styled(format!("U{}", g.conflicts), Style::default().fg(t.err).add_modifier(Modifier::BOLD)));
        pushed_any = true;
    }
    if !pushed_any {
        line2.push(Span::styled("clean", Style::default().fg(t.dim)));
    }
    if g.stash_count > 0 {
        // `⚑` is a lightweight marker; put the count next to it so the
        // user can tell whether `:git stash pop` has something to pop.
        line2.push(Span::styled(
            format!("  ⚑{}", g.stash_count),
            Style::default().fg(t.info),
        ));
    }
    let w = area.width as usize;
    let p = Paragraph::new(vec![
        sep_line,
        truncate_line_with_arrow(Line::from(line1), w),
        truncate_line_with_arrow(Line::from(line2), w),
    ]);
    frame.render_widget(p, area);
}

/// Explorer panel while any git sub-mode is active: project headers
/// collapsed up top, git section (branches + changes) takes the rest.
fn draw_explorer_git_mode(frame: &mut Frame, inner: Rect, app: &App) {
    if !app.git.is_repo() {
        // Defensive: sub-mode should only be reachable in a repo, but
        // if we somehow got here, fall back to the normal tree.
        draw_explorer_normal(frame, inner, app);
        return;
    }

    let t = &app.theme;
    let n_projects = app.projects.projects.len() as u16;
    if inner.height <= n_projects + 3 {
        // Not enough space for a meaningful git section — just render
        // project headers and bail.
        draw_project_headers(frame, inner, app, inner.height.min(n_projects));
        return;
    }

    // Projects area (collapsed headers only).
    let proj_rect = Rect { x: inner.x, y: inner.y, width: inner.width, height: n_projects };
    draw_project_headers(frame, proj_rect, app, n_projects);

    // Remaining = git section. Layout within it:
    //   row 0           : `─ git ─────────────` divider
    //   row 1           : `branches (N)   ▲▼`  sub-label
    //   rows 2..=b_end  : branches list
    //   row b_end+1     : `changes (N)   ▲▼`   sub-label
    //   rows b_end+2..  : changes list
    //
    // In `GitLog` mode the layout is simpler: divider + log list takes
    // the entire rest — branches/changes are one keypress away.
    let rest = Rect {
        x: inner.x,
        y: inner.y + n_projects,
        width: inner.width,
        height: inner.height - n_projects,
    };

    // Top-level `─ git ─` divider.
    let git_div_rect = Rect { x: rest.x, y: rest.y, width: rest.width, height: 1 };
    draw_git_divider(frame, git_div_rect, app);

    // Area below the divider.
    let body = Rect {
        x: rest.x,
        y: rest.y + 1,
        width: rest.width,
        height: rest.height - 1,
    };

    if app.explorer_mode == ExplorerMode::GitLog {
        draw_git_log_body(frame, body, app);
        return;
    }

    let change_rows = app.git_change_rows();

    // Sub-label (1) + up-to-5 rows of branches.
    let branches_content_h = app.git.branches.len().min(5).max(1) as u16;
    let branches_block_h = 1 + branches_content_h;
    // Changes sub-label (1) + at least 2 list rows.
    let min_changes_h = 3u16;
    let mut changes_block_h = body.height.saturating_sub(branches_block_h);
    if changes_block_h < min_changes_h {
        changes_block_h = min_changes_h.min(body.height);
    }
    let branches_block_h = body.height.saturating_sub(changes_block_h);

    // Branches sub-label + list.
    let b_hdr_rect = Rect { x: body.x, y: body.y, width: body.width, height: 1 };
    let b_list_h = branches_block_h.saturating_sub(1);
    let b_list_rect = Rect { x: body.x, y: body.y + 1, width: body.width, height: b_list_h };
    let b_scroll = scroll_for_selection(
        app.git_branch_sel, b_list_h as usize, app.git.branches.len(),
    );
    draw_sub_label(
        frame, b_hdr_rect,
        "branches",
        app.explorer_mode == ExplorerMode::GitBranches,
        app.git.branches.len(),
        b_scroll,
        b_list_h as usize,
        app,
    );
    draw_branches_list(frame, b_list_rect, app, b_scroll);

    // Changes sub-label + list.
    let c_hdr_y = body.y + branches_block_h;
    let c_hdr_rect = Rect { x: body.x, y: c_hdr_y, width: body.width, height: 1 };
    let c_list_h = changes_block_h.saturating_sub(1);
    let c_list_rect = Rect { x: body.x, y: c_hdr_y + 1, width: body.width, height: c_list_h };
    let c_display = build_changes_display(&change_rows);
    let sel_display = display_index_for_entry(&c_display, app.git_change_sel);
    let c_scroll = scroll_for_selection(sel_display, c_list_h as usize, c_display.len());
    draw_sub_label(
        frame, c_hdr_rect,
        "changes",
        app.explorer_mode == ExplorerMode::GitChanges,
        change_rows.len(),
        c_scroll,
        c_list_h as usize,
        app,
    );
    let _ = t;
    draw_changes_list(frame, c_list_rect, app, &c_display, c_scroll);
}

/// Style pair for the `─ git ─` divider: `(label, rule)`. The label
/// greys out whenever git isn't expanded — so the footer separator in
/// ExplorerMode::Normal reads like a nameplate, not an active heading.
/// When the user opens any git sub-mode it switches to cyan + bold
/// (accent), the same "active pane" cue used for focused borders and
/// titles elsewhere in the UI.
fn git_divider_styles(app: &App) -> (Style, Style) {
    let t = &app.theme;
    if app.explorer_mode.is_git() {
        (
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            Style::default().fg(t.accent),
        )
    } else {
        (
            Style::default().fg(t.dim),
            Style::default().fg(t.muted),
        )
    }
}

/// `─ git ─────────────────────` — single horizontal rule at the top
/// of the git section. Its label color reflects the current sub-mode
/// so you can tell GitBranches vs GitChanges at a glance without
/// reading the status bar.
fn draw_git_divider(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let (label_style, rule_style) = git_divider_styles(app);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("─ ", rule_style),
        Span::styled("git", label_style),
        Span::raw(" "),
    ];
    // Surface mid-op state right next to the `git` label so it's the
    // first thing seen when entering any git sub-mode. Same payload
    // the Normal footer shows, just relocated for the expanded view.
    if !app.git.op_state.is_clean() {
        spans.push(Span::styled(
            format!("[{}] ", app.git.op_state.label()),
            Style::default().fg(t.err).add_modifier(Modifier::BOLD),
        ));
    }
    let used: usize = spans.iter().map(|s| s.width()).sum();
    let trailing = (area.width as usize).saturating_sub(used);
    spans.push(Span::styled("─".repeat(trailing), rule_style));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render just the project-header rows (always arrow-collapsed, even
/// the active one) into `area`. Used at the top of the git modes.
fn draw_project_headers(frame: &mut Frame, area: Rect, app: &App, rows: u16) {
    let w = area.width as usize;
    let lines: Vec<Line> = app.projects.projects.iter().enumerate()
        .take(rows as usize)
        .map(|(idx, _)| truncate_line_with_arrow(project_header_line_collapsed(idx, app), w))
        .collect();
    let p = Paragraph::new(lines);
    frame.render_widget(p, area);
}

fn project_header_line_collapsed(idx: usize, app: &App) -> Line<'static> {
    let t = &app.theme;
    let is_active = app.projects.active == idx;
    let state = if is_active {
        ProjectState::from_snapshot(&app.git)
    } else {
        app.projects.projects.get(idx).map(|p| p.state).unwrap_or(ProjectState::None)
    };

    let arrow = "▸ ";
    let dot   = format!("{} ", state.glyph());
    let name  = app.projects.projects.get(idx).map(|p| p.name.clone()).unwrap_or_default();

    let name_style = if is_active {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg).add_modifier(Modifier::BOLD)
    };

    Line::from(vec![
        Span::styled(arrow.to_string(), Style::default().fg(t.dim)),
        Span::styled(dot, Style::default().fg(state_color(state, t))),
        Span::styled(name, name_style),
    ])
}

/// Sub-section label inside the git section — no horizontal rule,
/// just a soft, indented heading. Shows the label, the current total,
/// a `▸ ` cursor when its section has the cursor, and right-edge
/// `▲`/`▼`/`▲▼` scroll indicators when there's content off-screen.
fn draw_sub_label(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    is_active: bool,
    total: usize,
    scroll: usize,
    visible: usize,
    app: &App,
) {
    let t = &app.theme;
    let indicator = scroll_indicator(scroll, visible, total);

    // Matches the `OPEN CELLS` / `PROJECTS` section headers in
    // Normal-mode explorer: uppercase, dim + bold. Active sub-mode
    // promotes the label to accent so the user can tell which section
    // has the cursor.
    let label_style = if is_active {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.dim).add_modifier(Modifier::BOLD)
    };
    let count_style = Style::default().fg(t.dim);
    let ind_style   = Style::default().fg(t.warn);

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(label.to_uppercase(), label_style),
        Span::styled(format!(" ({total})"), count_style),
    ];

    let used: usize = spans.iter().map(|s| s.width()).sum();
    let ind_w = indicator.chars().count();
    let gap = (area.width as usize).saturating_sub(used + ind_w);
    if gap > 0 {
        spans.push(Span::raw(" ".repeat(gap)));
    }
    if !indicator.is_empty() {
        spans.push(Span::styled(indicator, ind_style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn scroll_indicator(scroll: usize, visible: usize, total: usize) -> String {
    let can_up   = scroll > 0;
    let can_down = scroll + visible < total;
    match (can_up, can_down) {
        (true,  true)  => "▲▼".into(),
        (true,  false) => "▲ ".into(),
        (false, true)  => " ▼".into(),
        _              => "".into(),
    }
}

/// Scroll offset such that `sel` is visible within a window of `visible`
/// rows over a total of `total` entries. Keeps the current scroll
/// stable when it's already in range; otherwise jumps to the edge.
fn scroll_for_selection(sel: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total <= visible { return 0; }
    let half = visible / 2;
    let target = sel.saturating_sub(half);
    target.min(total - visible)
}

fn draw_branches_list(frame: &mut Frame, area: Rect, app: &App, scroll: usize) {
    let t = &app.theme;
    let active = app.explorer_mode == ExplorerMode::GitBranches;
    let current = app.git.branch.clone();
    let visible = area.height as usize;
    let sel = app.git_branch_sel;

    let lines: Vec<Line> = app.git.branches.iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .map(|(idx, name)| {
            let is_current = name == &current;
            let gutter = if is_current { "●" } else { " " };
            let raw = format!(" {gutter} {name}");
            let mut style = if is_current {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.fg)
            };
            if active && idx == sel {
                style = style.bg(t.bg_sel).add_modifier(Modifier::BOLD);
            }
            Line::from(Span::styled(pad_or_arrow(&raw, area.width as usize), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Display row in the changes list: either an in-group file entry (that
/// the cursor can land on) or a group divider (non-selectable).
#[derive(Clone, Debug)]
enum ChangesDisplay<'a> {
    GroupHeader { group: ChangeGroup, count: usize },
    Entry { entry_idx: usize, row: &'a ChangeRow },
}

fn build_changes_display(rows: &[ChangeRow]) -> Vec<ChangesDisplay<'_>> {
    if rows.is_empty() { return Vec::new(); }
    let mut out = Vec::new();
    let mut prev: Option<ChangeGroup> = None;
    let mut counts = [0usize; 4];
    let slot = |g: ChangeGroup| -> usize {
        match g {
            ChangeGroup::Conflicted => 0,
            ChangeGroup::Staged     => 1,
            ChangeGroup::Unstaged   => 2,
            ChangeGroup::Untracked  => 3,
        }
    };
    for r in rows {
        counts[slot(r.group)] += 1;
    }
    for (i, r) in rows.iter().enumerate() {
        if prev != Some(r.group) {
            out.push(ChangesDisplay::GroupHeader { group: r.group, count: counts[slot(r.group)] });
            prev = Some(r.group);
        }
        out.push(ChangesDisplay::Entry { entry_idx: i, row: r });
    }
    out
}

fn display_index_for_entry(display: &[ChangesDisplay<'_>], entry_idx: usize) -> usize {
    display.iter().position(|d| matches!(d, ChangesDisplay::Entry { entry_idx: i, .. } if *i == entry_idx))
        .unwrap_or(0)
}

fn draw_changes_list(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    display: &[ChangesDisplay<'_>],
    scroll: usize,
) {
    let t = &app.theme;
    let active = app.explorer_mode == ExplorerMode::GitChanges;
    let sel = app.git_change_sel;
    let visible = area.height as usize;

    let lines: Vec<Line> = display.iter()
        .skip(scroll)
        .take(visible)
        .map(|d| match d {
            ChangesDisplay::GroupHeader { group, count } => {
                let text = format!(" {} ({count})", group.label());
                let fg = if *group == ChangeGroup::Conflicted { t.err } else { t.dim };
                Line::from(Span::styled(
                    pad_or_arrow(&text, area.width as usize),
                    Style::default().fg(fg).add_modifier(Modifier::BOLD),
                ))
            }
            ChangesDisplay::Entry { entry_idx, row } => {
                let is_sel = active && *entry_idx == sel;
                let marker = entry_marker(row.status);
                let raw = format!("   {marker} {}", row.path);
                let (fg, mods) = git_tint(Some(row.status), t);
                let mut style = Style::default().fg(fg).add_modifier(mods);
                if is_sel {
                    style = style.bg(t.bg_sel).add_modifier(Modifier::BOLD);
                }
                Line::from(Span::styled(pad_or_arrow(&raw, area.width as usize), style))
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One-character status marker shown before the path in the changes
/// list. Matches `git status --short` porcelain (`R` for renamed,
/// `U` for unmerged/conflict).
fn entry_marker(s: FileStatus) -> char {
    match s {
        FileStatus::Modified  => 'M',
        FileStatus::Added     => 'A',
        FileStatus::Deleted   => 'D',
        FileStatus::Renamed   => 'R',
        FileStatus::Untracked => '?',
        FileStatus::Conflict  => 'U',
        FileStatus::Ignored   => '·',
    }
}

/// If `line` is wider than `max`, cut it at `max - 1` columns and
/// replace the trailing character with `›` so the user sees at a
/// glance that content is hidden. No-op when it already fits. The
/// arrow inherits the final retained span's style, so a truncated
/// filename's git tint carries through to the indicator.
fn truncate_line_with_arrow(line: Line<'static>, max: usize) -> Line<'static> {
    if max == 0 {
        return Line::from(Vec::<Span<'static>>::new());
    }
    let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= max {
        return line;
    }

    let keep = max - 1;
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    let mut last_style = Style::default();

    for span in line.spans.into_iter() {
        let len = span.content.chars().count();
        last_style = span.style;
        if used + len <= keep {
            used += len;
            out.push(span);
            if used == keep { break; }
        } else {
            let take = keep - used;
            if take > 0 {
                let cut: String = span.content.chars().take(take).collect();
                out.push(Span::styled(cut, span.style));
            }
            break;
        }
    }

    out.push(Span::styled("›", last_style.add_modifier(Modifier::BOLD)));
    Line::from(out)
}

/// Like [`pad_to_width`] but on overflow the last visible column is
/// replaced with `›` instead of being silently clipped. Used for
/// sidebar rows (branch/change/log lists, git footer) so the user
/// sees when content is hidden.
fn pad_or_arrow(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len <= width {
        let mut out = s.to_string();
        out.extend(std::iter::repeat(' ').take(width - len));
        out
    } else if width == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(width - 1).collect();
        out.push('›');
        out
    }
}

/// Pad or truncate a string so it occupies exactly `width` display
/// columns. Needed so the selection highlight bg fills the whole row.
fn pad_to_width(s: &str, width: usize) -> String {
    let current: usize = s.chars().count();
    if current >= width {
        s.chars().take(width).collect()
    } else {
        let mut out = s.to_string();
        out.extend(std::iter::repeat(' ').take(width - current));
        out
    }
}

/// Render one row of the unified files/projects tree.
fn entry_line(e: &Entry, app: &App, width: usize) -> Line<'static> {
    let t = &app.theme;
    match e.kind {
        EntryKind::SectionHeader(label) => section_header_line(label, t),
        EntryKind::OpenCell { idx }     => open_cell_line(e, idx, app, width),
        EntryKind::Project { idx }      => project_header_line(e, idx, app),
        EntryKind::Dir                  => dir_line(e, t, app),
        EntryKind::File                 => file_line(e, t, app),
    }
}

fn section_header_line(label: &'static str, t: &Theme) -> Line<'static> {
    // Flush-left, dim + bold, uppercase. Not selectable so no indent
    // marker is needed.
    Line::from(Span::styled(
        label.to_uppercase(),
        Style::default().fg(t.dim).add_modifier(Modifier::BOLD),
    ))
}

fn open_cell_line(e: &Entry, idx: usize, app: &App, width: usize) -> Line<'static> {
    let t = &app.theme;
    let indent = "  ".repeat(e.depth as usize);
    let Some(cell) = app.cells.get(idx) else {
        return Line::from(Span::raw(indent));
    };
    let minimized = cell.minimized;
    use crate::cell::Session;
    let (title, badge, is_editor, dirty, external, path) = match cell.active_session() {
        Session::Edit(ed) => {
            if ed.read_only {
                // Synthetic buffer (e.g. `:help`) — show the kind badge
                // like we do for PTYs and drop the project-external
                // suffix (it's not a project file).
                let name = ed.path.as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.trim_start_matches('[').trim_end_matches(']').to_string())
                    .unwrap_or_else(|| "help".to_string());
                (name, Some("[ACodeEditor]"), false, false, false, None)
            } else {
                let root = app.projects.projects
                    .get(app.projects.active)
                    .map(|p| p.root.clone());
                let path = ed.path.clone();
                let external = match (&root, &path) {
                    (Some(r), Some(p)) => !p.starts_with(r),
                    _                  => false,
                };
                // Matches the cell-title badge so the explorer row is
                // consistent with the focused cell.
                use crate::editor::ExternalConflict as C;
                let badge = if ed.is_new {
                    Some("[NEW]")
                } else if ed.external_conflict == Some(C::ModifiedOnDisk) {
                    Some("[CONFLICT]")
                } else {
                    None
                };
                (ed.file_name().to_string(), badge, true, ed.dirty, external, path)
            }
        }
        Session::Shell(p)    => (pty_display_name(p, "shell"),  Some("[SHELL]"),  false, false, false, None),
        Session::Claude(p)   => (pty_display_name(p, "claude"), Some("[CLAUDE]"), false, false, false, None),
        Session::Diff(v)     => (v.title.clone(), None, false, false, false, None),
        Session::Conflict(v) => (v.title.clone(), None, false, false, false, None),
    };

    let marker = if dirty { "● " } else { "  " };
    // Editor rows pick up their git tint (modified/untracked/etc);
    // non-editor rows (shell, claude, diff, conflict) stay on plain
    // foreground — the accent colour was reading as "interactive link"
    // which clashed with the section being a passive list.
    let (fg, mut mods) = if is_editor {
        git_tint(path.as_ref().and_then(|p| app.git.status_for(p)), t)
    } else {
        (t.fg, Modifier::empty())
    };
    // Minimized cells read as a dimmer, italic row so they're visibly
    // distinct from the live cells above them in the list.
    if minimized {
        mods |= Modifier::ITALIC | Modifier::DIM;
    }

    // Prefix + marker together span a fixed 4-col gutter (indent "  "
    // + marker "● "/"  ") so the title always sits at the same column.
    // While an arm is active we overwrite that gutter with a 4-col
    // `[N] ` digit hint — titles stay put, the hint just replaces the
    // padding. Cap at 9 since we only support digits 1..9.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    let swap_armed = app.pending_swap || app.pending_swap_follow;
    if (app.pending_jump || swap_armed) && idx < 9 {
        let color = if swap_armed {
            Color::Rgb(0xff, 0xa8, 0x60) // orange = swap
        } else {
            t.attn                        // cyan   = jump
        };
        spans.push(Span::styled(
            format!("[{}] ", idx + 1),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw(indent));
        spans.push(Span::styled(
            marker.to_string(),
            Style::default().fg(fg).add_modifier(mods),
        ));
    }
    // Right-align the badges. Natural row width is
    //   prefix (4) + title + gap (1) + " [BADGE]" + "  [EXTERNAL]"
    // The explorer-width calculator (`compute_explorer_width`) includes
    // the badges in its per-row length, so when this panel is focused
    // it auto-grows to fit the natural width. When the panel is
    // narrower than natural (unfocused, or other rows are longer), we
    // truncate the title with a `›` arrow — same marker the rest of
    // the explorer uses — while keeping the badges pinned to the right
    // edge so they're always visible.
    let deleted_badge = session_deleted_badge(cell.active_session());
    let prefix_w = 4usize;
    let badge_w  = badge.map(|b| 1 + b.chars().count()).unwrap_or(0);     // " [BADGE]"
    let del_w    = deleted_badge.map(|b| 1 + b.chars().count()).unwrap_or(0);
    let ext_w    = if external { "  [EXTERNAL]".chars().count() } else { 0 };
    let suffix_w = badge_w + del_w + ext_w;
    let budget   = width.saturating_sub(prefix_w).saturating_sub(suffix_w);

    let title_chars: usize = title.chars().count();
    let title_style = Style::default().fg(fg).add_modifier(mods);
    let pad_w: usize;
    if title_chars <= budget {
        spans.push(Span::styled(title, title_style));
        pad_w = budget - title_chars;
    } else if budget == 0 {
        pad_w = 0;
    } else {
        // Truncate so the last visible column of the title becomes the
        // `›` arrow — matches `truncate_line_with_arrow`'s convention.
        let keep = budget - 1;
        let cut: String = title.chars().take(keep).collect();
        spans.push(Span::styled(cut, title_style));
        spans.push(Span::styled(
            "›",
            title_style.add_modifier(Modifier::BOLD),
        ));
        pad_w = 0;
    }
    if pad_w > 0 {
        spans.push(Span::raw(" ".repeat(pad_w)));
    }
    if let Some(b) = badge {
        spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
    }
    if let Some(b) = deleted_badge {
        spans.push(Span::styled(format!(" {b}"), badge_style_for(t, b)));
    }
    if external {
        spans.push(Span::styled(
            "  [EXTERNAL]",
            Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn project_header_line(e: &Entry, idx: usize, app: &App) -> Line<'static> {
    let t = &app.theme;
    let is_active = app.projects.active == idx;

    // Live state for the active project comes from app.git so it tracks
    // edits immediately; others rely on the cached per-project snapshot.
    let state = if is_active {
        ProjectState::from_snapshot(&app.git)
    } else {
        app.projects.projects.get(idx).map(|p| p.state).unwrap_or(ProjectState::None)
    };

    let arrow = if is_active { "▾ " } else { "▸ " };
    let dot   = format!("{} ", state.glyph());
    let name  = app.projects.projects.get(idx).map(|p| p.name.clone()).unwrap_or_default();

    let arrow_style = Style::default().fg(t.dim);
    let dot_style   = Style::default().fg(state_color(state, t));
    let name_style  = if is_active {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg).add_modifier(Modifier::BOLD)
    };

    let _ = e; // depth unused for headers — they're always flush-left
    Line::from(vec![
        Span::styled(arrow.to_string(), arrow_style),
        Span::styled(dot,               dot_style),
        Span::styled(name,              name_style),
    ])
}

fn dir_line(e: &Entry, t: &Theme, app: &App) -> Line<'static> {
    let indent = "  ".repeat(e.depth as usize);
    let marker = if e.expanded { "▾ " } else { "▸ " };
    let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let (fg, mods) = git_tint(app.git.dir_status(&e.path), t);
    Line::from(Span::styled(
        format!("{indent}{marker}{name}/"),
        Style::default().fg(fg).add_modifier(mods),
    ))
}

fn file_line(e: &Entry, t: &Theme, app: &App) -> Line<'static> {
    let indent = "  ".repeat(e.depth as usize);
    let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let (fg, mods) = git_tint(app.git.status_for(&e.path), t);
    Line::from(Span::styled(
        format!("{indent}  {name}"),
        Style::default().fg(fg).add_modifier(mods),
    ))
}

fn git_tint(s: Option<FileStatus>, t: &Theme) -> (Color, Modifier) {
    match s {
        Some(FileStatus::Conflict)  => (t.err,   Modifier::BOLD),
        Some(FileStatus::Deleted)   => (t.err,   Modifier::empty()),
        Some(FileStatus::Modified)  => (t.warn,  Modifier::empty()),
        Some(FileStatus::Added)     => (t.ok,    Modifier::empty()),
        Some(FileStatus::Renamed)   => (t.accent, Modifier::empty()),
        // Untracked files/dirs read as grey — they're visible but not
        // part of the repo yet, so tint them dimmer than tracked ones.
        Some(FileStatus::Untracked) => (t.dim,   Modifier::empty()),
        Some(FileStatus::Ignored)   => (t.muted, Modifier::empty()),
        None                        => (t.fg,    Modifier::empty()),
    }
}

fn state_color(s: crate::projects::ProjectState, t: &Theme) -> Color {
    use crate::projects::ProjectState as S;
    match s {
        S::None    => t.muted,
        S::Ok      => t.ok,
        S::Working => t.warn,
        S::Error   => t.err,
    }
}

fn draw_statusbar(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let g = &app.git;

    let mut left: Vec<Span> = Vec::new();

    // ── mode badge (always) ───────────────────────────────────────────────
    // Outer Mode wins for Insert/Command; in Normal, the Explorer sub-mode
    // decides NOR/GIT/BCH/CHG so the badge tracks git pane state.
    let (badge_text, badge_style) = match &app.mode {
        Mode::Insert        => (app.mode.badge(), Style::default().fg(t.bg).bg(t.ok).bold()),
        Mode::Command {..}  => (app.mode.badge(), Style::default().fg(t.bg).bg(t.warn).bold()),
        Mode::Visual {..}   => (app.mode.badge(), Style::default().fg(t.bg).bg(t.accent).bold()),
        Mode::Password {..} => (app.mode.badge(), Style::default().fg(t.bg).bg(t.err).bold()),
        Mode::Normal => {
            let (text, color) = if app.focus == FocusId::Explorer {
                match app.explorer_mode {
                    ExplorerMode::Normal       => (app.explorer_mode.badge(), t.dim),
                    ExplorerMode::GitOverview  => (app.explorer_mode.badge(), t.accent),
                    ExplorerMode::GitBranches  => (app.explorer_mode.badge(), t.info),
                    ExplorerMode::GitChanges   => (app.explorer_mode.badge(), t.warn),
                    ExplorerMode::GitLog       => (app.explorer_mode.badge(), t.accent),
                }
            } else {
                (app.mode.badge(), t.dim)
            };
            (text, Style::default().fg(t.bg).bg(color).bold())
        }
    };
    left.push(Span::styled(format!(" {badge_text} "), badge_style));
    left.push(Span::raw("  "));

    // ── left context: password prompt in PWD mode, command buffer in
    // CMD mode, focus-aware otherwise ──
    if let Mode::Password { buffer, .. } = &app.mode {
        // Mask every character so shoulder-surfing and any screen
        // recording of the terminal don't leak the password. Width
        // tracks buffer length so Backspace visibly shortens the dots.
        let mask: String = "•".repeat(buffer.chars().count());
        left.push(Span::styled("sudo password: ", Style::default().fg(t.err).bold()));
        left.push(Span::styled(mask, Style::default().fg(t.fg)));
        left.push(Span::styled("▌", Style::default().fg(t.err)));
    } else if let Mode::Command { buffer } = &app.mode {
        // `/` or `?` kicks off a search prompt — show the literal
        // prefix instead of `:` so the user reads "searching" not
        // "running a command".
        let prefix = match buffer.chars().next() {
            Some('/') | Some('?') => "",
            _ => ":",
        };
        left.push(Span::styled(format!("{prefix}{buffer}"), Style::default().fg(t.fg)));
        left.push(Span::styled("▌", Style::default().fg(t.warn)));
        // Wildmenu: when a completion cycle is active with multiple
        // options, show the siblings dimmed after the cursor so the
        // user can see what else `Tab` will rotate to. The currently-
        // selected option is already materialised in the buffer, so
        // we only render the *other* choices here (up to a small cap).
        if let Some(cs) = app.completion.as_ref() {
            // Preview mode: show the hint list even for a single option
            // so the user sees what Tab will commit to. While cycling
            // (preview=false), only bother when there are siblings.
            if cs.options.len() > 1 || cs.preview {
                left.push(Span::styled("  ", Style::default().fg(t.dim)));
                let shown = cs.options.len().min(6);
                for (i, opt) in cs.options.iter().take(shown).enumerate() {
                    let style = if i == cs.sel {
                        Style::default().fg(t.warn).bold()
                    } else {
                        Style::default().fg(t.dim)
                    };
                    if i > 0 { left.push(Span::styled(" ", Style::default().fg(t.dim))); }
                    left.push(Span::styled(opt.clone(), style));
                }
                if cs.options.len() > shown {
                    left.push(Span::styled(
                        format!(" +{}", cs.options.len() - shown),
                        Style::default().fg(t.dim),
                    ));
                }
            }
        }
    } else {
        fill_focus_summary(&mut left, app);
    }

    if let Some(msg) = app.status.current() {
        left.push(Span::raw("   "));
        // While a confirm is armed the message is the prompt — tint
        // it warn+bold so it reads as a question, not an error.
        let style = if app.pending_confirm.is_some() {
            Style::default().fg(t.warn).add_modifier(Modifier::BOLD)
        } else {
            use crate::status::StatusLevel;
            let color = match msg.level {
                StatusLevel::Err  => t.err,
                StatusLevel::Warn => t.warn,
                StatusLevel::Ok   => t.ok,
                StatusLevel::Info => t.info,
            };
            Style::default().fg(color)
        };
        left.push(Span::styled(msg.text.clone(), style));
    }

    // ── right: git · tray · hint (built as independent groups so they
    //    can be dropped individually when a long status message needs
    //    the screen real estate) ───────────────────────────────────────
    let mut git_spans: Vec<Span> = Vec::new();
    if g.is_repo() {
        let git_style = if g.has_conflicts() {
            Style::default().fg(t.err).bold()
        } else if !g.is_clean() {
            Style::default().fg(t.warn)
        } else {
            Style::default().fg(t.ok).add_modifier(Modifier::DIM)
        };
        git_spans.push(Span::styled(format!("⎇ {}", g.branch), git_style));
        if g.ahead > 0 {
            git_spans.push(Span::styled(format!(" ↑{}", g.ahead), git_style));
        }
        if g.behind > 0 {
            git_spans.push(Span::styled(format!(" ↓{}", g.behind), git_style));
        }
        if g.modified > 0 {
            git_spans.push(Span::raw("  "));
            git_spans.push(Span::styled(format!("M{}", g.modified), Style::default().fg(t.warn)));
        }
        if g.untracked > 0 {
            git_spans.push(Span::raw(" "));
            git_spans.push(Span::styled(format!("?{}", g.untracked), Style::default().fg(t.info)));
        }
        if g.conflicts > 0 {
            git_spans.push(Span::raw(" "));
            git_spans.push(Span::styled(format!("U{}", g.conflicts), Style::default().fg(t.err).bold()));
        }
    }

    let mut tray_spans: Vec<Span> = Vec::new();
    if app.tray_count > 0 {
        tray_spans.push(Span::raw("   "));
        tray_spans.push(Span::styled(format!("tray {}", app.tray_count), Style::default().fg(t.attn)));
    }

    let hint: &str = if app.pending_swap || app.pending_swap_follow {
        "   [1-9 swap with cell  0 minimize] "
    } else if app.pending_jump {
        "   [0 explorer  1-9 cell] "
    } else {
        match &app.mode {
            Mode::Insert        => "",
            Mode::Command {..}  => "",
            Mode::Password {..} => "   [Enter submit  Esc cancel] ",
            Mode::Visual {..}   => "   [d delete  y yank  c change  > indent  < dedent  Esc cancel] ",
            Mode::Normal => match app.focus {
                FocusId::Cell(_) if app.focused_session_is_editor()
                                  => "   [i insert] ",
                FocusId::Cell(_) if app.focused_session_is_diff()
                                  => "",
                FocusId::Cell(_)  => "   [i insert] ",
                FocusId::Explorer    => match app.explorer_mode {
                    ExplorerMode::Normal       => "   [↵ preview  e edit  a add  c close  o open  n new  g git] ",
                    ExplorerMode::GitOverview  => "   [b branches  c changes  l log  m commit  a stage  A unstage  p push  P pull  f fetch] ",
                    ExplorerMode::GitBranches  => "   [n new  d del  D force-del  c changes  l log] ",
                    ExplorerMode::GitChanges   => "   [s stage  d discard  o ours  t theirs  e edit  v diff  b branches  l log] ",
                    ExplorerMode::GitLog       => "   [v diff  c sha  b branches] ",
                },
            },
        }
    };
    let hint_spans: Vec<Span> = if hint.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(hint, Style::default().fg(t.dim))]
    };

    // ── assemble ──────────────────────────────────────────────────────────
    let span_width = |spans: &[Span]| -> usize {
        spans.iter().map(|s| s.width()).sum()
    };
    let left_w = span_width(&left);
    let avail  = area.width as usize;

    // Add right-side groups in priority order (git → tray → hint) and
    // stop as soon as one won't fit. This guarantees strict priority —
    // if tray can't fit we don't slip a shorter hint in after it. The
    // message itself is never dropped or truncated.
    let mut right: Vec<Span> = Vec::new();
    for group in [&git_spans, &tray_spans, &hint_spans] {
        if group.is_empty() { continue; }
        let gw = span_width(group);
        if left_w + span_width(&right) + gw + 1 > avail { break; }
        right.extend(group.iter().cloned());
    }

    let right_w = span_width(&right);
    let filler  = avail.saturating_sub(left_w + right_w);

    let mut spans = left;
    spans.push(Span::raw(" ".repeat(filler)));
    spans.extend(right);

    let p = Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg_sel));
    frame.render_widget(p, area);
}

fn fill_focus_summary<'a>(left: &mut Vec<Span<'a>>, app: &'a App) {
    let t = &app.theme;
    match app.focus {
        FocusId::Cell(i) => {
            if let Some(cell) = app.cells.get(i) {
                let (label, accent_color) = match cell.active_session() {
                    Session::Claude(_)   => ("claude",   t.accent),
                    Session::Shell(_)    => ("shell",    t.accent),
                    Session::Edit(_)     => ("edit",     t.accent),
                    Session::Diff(_)     => ("diff",     t.accent),
                    Session::Conflict(_) => ("conflict", t.err),
                };
                left.push(Span::styled(format!("cell {} · {label}", cell_digit(i)), Style::default().fg(accent_color).bold()));
                match cell.active_session() {
                    Session::Claude(pty) | Session::Shell(pty) => {
                        left.push(Span::styled(
                            format!(" · {}", short_program_name(&pty.program)),
                            Style::default().fg(t.fg),
                        ));
                    }
                    Session::Edit(ed) => {
                        left.push(Span::styled(format!(" · {}", ed.file_name()), Style::default().fg(t.fg)));
                        if ed.is_new {
                            left.push(Span::styled(" [NEW]", Style::default().fg(t.ok).bold()));
                        }
                        if ed.dirty {
                            left.push(Span::styled(" +", Style::default().fg(t.warn)));
                        }
                        use crate::editor::ExternalConflict as C;
                        match ed.external_conflict {
                            Some(C::ModifiedOnDisk) =>
                                left.push(Span::styled(" ⚠ disk changed", Style::default().fg(t.err).bold())),
                            Some(C::Deleted) =>
                                left.push(Span::styled(" ✗ disk gone", Style::default().fg(t.err).bold())),
                            None => {}
                        }
                    }
                    Session::Diff(view) => {
                        left.push(Span::styled(
                            format!(" · {}", view.title),
                            Style::default().fg(t.fg),
                        ));
                    }
                    Session::Conflict(view) => {
                        left.push(Span::styled(
                            format!(" · {}", view.title),
                            Style::default().fg(t.fg),
                        ));
                        let un = view.unresolved_count();
                        if un > 0 {
                            left.push(Span::styled(
                                format!(" {un} unresolved"),
                                Style::default().fg(t.err).bold(),
                            ));
                        } else {
                            left.push(Span::styled(
                                " resolved — :w",
                                Style::default().fg(t.ok).bold(),
                            ));
                        }
                    }
                }
                if cell.sessions.len() > 1 {
                    left.push(Span::styled(
                        format!(" ({}/{})", cell.active + 1, cell.sessions.len()),
                        Style::default().fg(t.dim),
                    ));
                }
            }
        }
        FocusId::Explorer => {
            left.push(Span::styled("explorer", Style::default().fg(t.accent).bold()));
            if let Some(e) = app.explorer.entries.get(app.explorer.selected) {
                if let Some(name) = e.path.file_name().and_then(|n| n.to_str()) {
                    left.push(Span::styled(format!(" · {name}"), Style::default().fg(t.fg)));
                }
            }
        }
    }
}

/// Render the log pane inside the explorer panel. Each commit takes two
/// rows — SHA+summary on top, author/relative-date on the second.
fn draw_git_log_body(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    if app.git_log.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            " no commits",
            Style::default().fg(t.dim),
        )));
        frame.render_widget(p, area);
        return;
    }

    // Header sub-label.
    let hdr_rect = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
    let rows_rect = Rect { x: area.x, y: area.y + 1, width: area.width, height: area.height.saturating_sub(1) };

    let per_entry: usize = 2;
    let visible_entries = (rows_rect.height as usize) / per_entry;
    let scroll = scroll_for_selection(app.git_log_sel, visible_entries.max(1), app.git_log.len());
    draw_sub_label(
        frame, hdr_rect,
        "log",
        true,
        app.git_log.len(),
        scroll,
        visible_entries.max(1),
        app,
    );

    let mut lines: Vec<Line> = Vec::with_capacity(visible_entries * per_entry);
    for (i, entry) in app.git_log.iter().enumerate().skip(scroll).take(visible_entries) {
        let is_sel = i == app.git_log_sel;
        let base_fg = if is_sel { t.fg } else { t.fg };
        let mut s1 = Style::default().fg(base_fg);
        let mut s2 = Style::default().fg(t.muted);
        if is_sel {
            s1 = s1.bg(t.bg_sel).add_modifier(Modifier::BOLD);
            s2 = s2.bg(t.bg_sel);
        }
        let top = format!(" {}  {}", entry.sha_short, entry.summary);
        let bot = format!("         {} · {}", entry.author, entry.when);
        lines.push(Line::from(Span::styled(pad_or_arrow(&top, rows_rect.width as usize), s1)));
        lines.push(Line::from(Span::styled(pad_or_arrow(&bot, rows_rect.width as usize), s2)));
    }
    frame.render_widget(Paragraph::new(lines), rows_rect);
}

