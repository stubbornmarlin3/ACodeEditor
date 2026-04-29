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
    // Fullscreen: take the whole body. Cells column falls out to 0,
    // and the cell-draw pass skips zero-size rects.
    if app.explorer_fullscreen {
        return area.width;
    }
    // Fixed width: wide enough for `UNSTAGED (n)` / git footer and a
    // reasonable filename, narrow enough not to steal cell real
    // estate. Long filenames clip — consistent behavior whether
    // focused or not beats a sidebar that jumps on focus change.
    const HARD_MIN: u16 = 18;
    const DEFAULT:  u16 = 32;
    let max = area.width.saturating_sub(40).max(HARD_MIN);
    DEFAULT.min(max)
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

    // Shell-cell cursor in Insert mode is drawn as a styled overlay
    // by `render_pty_into` — we no longer position the native terminal
    // cursor here. The native cursor would briefly appear at the
    // previous frame's position whenever a redraw spanned multiple
    // moves, which read as a flickering cursor jumping around the
    // screen during chatty output. The drawn overlay sits on the right
    // cell every frame, no flicker.
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
    let active_title_style = if focused {
        // Match the border tint so the whole focused cell reads in one
        // colour per mode (green in Insert, magenta in Visual, accent
        // otherwise) — no split between title and border.
        cell_mode_border_style(app)
    } else {
        t.title_unfocused()
    };
    let left_title  = cell_title_line(cell, project_root, area.width, focused, t, active_title_style);
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
            let inner = block.inner(area);
            frame.render_widget(block, area);
            render_pty_into(frame.buffer_mut(), inner, cell, &app.theme, focused, &app.mode);
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
        Session::Hex(view) => {
            let inner = block.inner(area);
            frame.render_widget(block, area);
            draw_hex(frame, inner, view, t, focused, &app.mode);
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

    // Completion popup overlay. Drawn last (after cursor) so it sits
    // on top of any underlying content. Only on the focused cell —
    // an unfocused buffer's stale popup would just be visual noise.
    if focused {
        if let Some(comp) = editor.completion.as_ref() {
            draw_completion_popup(
                frame, area, comp,
                cur_vrow, cur_vcol, scroll_top, gutter_w as u16, theme,
            );
        }
    }
}

fn draw_completion_popup(
    frame:      &mut Frame,
    area:       Rect,
    comp:       &crate::editor::CompletionPopup,
    cur_vrow:   usize,
    cur_vcol:   u16,
    scroll_top: usize,
    gutter_w:   u16,
    theme:      &Theme,
) {
    if comp.items.is_empty() { return; }
    if cur_vrow < scroll_top { return; } // cursor scrolled off; skip popup

    // Anchor the popup at the prefix's start, not the cursor — that's
    // where the matching text *begins*, so the popup visually "owns"
    // the word the user is completing. Falls back to cursor position
    // if the visual offset can't be resolved (wrapped/multibyte edge).
    let prefix_chars = comp.prefix.chars().count() as u16;
    let anchor_col = cur_vcol.saturating_sub(prefix_chars);
    let content_x = area.x + gutter_w;
    let popup_x = content_x + anchor_col;

    // Width: longest item, capped so a wide identifier doesn't push
    // the popup off-cell. Min 6 so even short matches show something.
    let max_w = comp.items.iter()
        .map(|s| s.chars().count())
        .max().unwrap_or(0);
    let pad = 2; // 1-col left/right gutter
    let avail = area.x + area.width - popup_x;
    let width  = (max_w as u16 + pad).min(avail).min(40).max(6);

    // Height: prefer below-cursor; flip above when there isn't room.
    let visible: u16 = comp.items.len().min(8) as u16;
    let cursor_y = area.y + (cur_vrow - scroll_top) as u16;
    let below_room = area.y + area.height - cursor_y - 1;
    let above_room = cursor_y - area.y;
    let (popup_y, height) = if below_room >= visible {
        (cursor_y + 1, visible)
    } else if above_room >= visible {
        (cursor_y - visible, visible)
    } else if below_room >= above_room {
        (cursor_y + 1, below_room.max(1))
    } else {
        let h = above_room.max(1);
        (cursor_y - h, h)
    };
    if height == 0 { return; }

    let buf = frame.buffer_mut();
    let bg_normal   = Style::default().fg(theme.fg).bg(theme.bg_sel);
    let bg_selected = Style::default().fg(theme.bg).bg(theme.accent);
    let prefix_style_normal   = Style::default().fg(theme.accent).bg(theme.bg_sel);
    let prefix_style_selected = Style::default().fg(theme.bg).bg(theme.accent);

    // Pick a window of items around `selected` so it's always visible
    // when the list is taller than the popup.
    let total = comp.items.len();
    let window = (height as usize).min(total);
    let mut top = comp.selected.saturating_sub(window.saturating_sub(1));
    if top + window > total { top = total - window; }

    for (i, item) in comp.items.iter().enumerate().skip(top).take(window) {
        let row_y = popup_y + (i - top) as u16;
        let is_sel = i == comp.selected;
        let row_style    = if is_sel { bg_selected }       else { bg_normal };
        let prefix_style = if is_sel { prefix_style_selected } else { prefix_style_normal };

        // Overwrite every cell in the row with a space + the popup
        // style so the editor text underneath doesn't bleed through.
        // `paint_bg` would only restyle (leaving glyphs intact), which
        // is fine for selection shading but garbles a popup overlay.
        let blank: String = " ".repeat(width as usize);
        buf.set_string(popup_x, row_y, blank, row_style);
        // Item: " prefix" in accent, "rest" in fg, both on popup bg.
        let pre_len = comp.prefix.chars().count() as u16;
        let item_x = popup_x + 1; // 1-col left padding
        let max_item_w = width.saturating_sub(2);
        if pre_len <= max_item_w {
            buf.set_string(item_x, row_y, &comp.prefix, prefix_style);
            let rest: String = item.chars().skip(comp.prefix.chars().count())
                .take((max_item_w - pre_len) as usize)
                .collect();
            if !rest.is_empty() {
                buf.set_string(item_x + pre_len, row_y, rest, row_style);
            }
        } else {
            // Pathological: prefix wider than the popup. Just write
            // a truncation of the whole item in the row style.
            let truncated: String = item.chars().take(max_item_w as usize).collect();
            buf.set_string(item_x, row_y, truncated, row_style);
        }
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

/// Render a Hex cell. Layout per row: `OFFSET  HH HH HH HH  HH HH HH HH … │ASCII│`.
/// The ASCII pane is a read-only mirror — non-printable bytes show as
/// `.`. Cursor is rendered as a solid block on the active nibble in the
/// hex pane, with a dim mirror highlight on the same byte in the ASCII
/// pane. Visual selection (when `view.anchor.is_some()`) paints `bg_sel`
/// across the byte range on both panes.
fn draw_hex(frame: &mut Frame, area: Rect, view: &crate::hex::HexView, t: &Theme, focused: bool, mode: &Mode) {
    if area.width == 0 || area.height == 0 { return; }

    // Reserve the bottom row for a status line.
    let body_h = area.height.saturating_sub(1);
    if body_h == 0 { return; }

    // Pick bytes-per-row from cell width. Layout cost per row:
    //   gutter(10) + groups*group_w(group_w = 4*3-1=11) + (groups-1)*2 + 3 + bpr.
    // 16 bpr → 4 groups → 10 + 4*11 + 6 + 3 + 16 = 79.
    //  8 bpr → 2 groups → 10 + 2*11 + 2 + 3 + 8 = 45.
    //  4 bpr → 1 group  → 10 + 1*11 + 0 + 3 + 4 = 28.
    let bpr: u16 = if area.width >= 79 { 16 } else if area.width >= 45 { 8 } else { 4 };
    view.bytes_per_row.set(bpr);
    view.viewport_rows.set(body_h);

    let bpr_u = bpr as usize;
    let total_bytes = view.bytes.len();
    let total_rows  = (total_bytes + bpr_u - 1) / bpr_u;

    // Adjust scroll so the cursor row is visible. Same shape as the
    // editor's autoscroll: bring the cursor to the nearest edge.
    let cursor_row = view.cursor / bpr_u;
    let mut scroll = view.scroll.get();
    if cursor_row < scroll {
        scroll = cursor_row;
    } else if cursor_row >= scroll + body_h as usize {
        scroll = cursor_row + 1 - body_h as usize;
    }
    if scroll > total_rows.saturating_sub(1) {
        scroll = total_rows.saturating_sub(1);
    }
    view.scroll.set(scroll);

    let buf = frame.buffer_mut();
    let dim_style    = Style::default().fg(t.dim).bg(t.bg);
    let fg_style     = Style::default().fg(t.fg).bg(t.bg);
    let muted_style  = Style::default().fg(t.muted).bg(t.bg);
    let sep_style    = Style::default().fg(t.dim).bg(t.bg);
    let sel_bg       = t.bg_sel;
    let cursor_style = if focused {
        // In Normal/Visual: solid accent block. In Insert: green like
        // the rest of insert-mode UI.
        let bg = if matches!(mode, Mode::Insert) { t.ok } else { t.accent };
        Style::default().fg(t.bg).bg(bg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg).bg(t.bg_sel)
    };
    let mirror_style = Style::default().fg(t.fg).bg(t.bg_sel);

    let sel = view.selection_range();

    for vy in 0..body_h {
        let row_idx = scroll + vy as usize;
        let row_start = row_idx * bpr_u;
        if row_start >= total_bytes && row_idx > 0 { break; }
        let y = area.y + vy;

        // Gutter — 8 hex digits + 2 spaces.
        let gutter = format!("{:08x}  ", row_start);
        buf.set_string(area.x, y, &gutter, dim_style);
        let mut x = area.x + gutter.chars().count() as u16;

        // Hex pane. Iterate bytes_per_row slots even past EOF so empty
        // cells render as blanks (keeps column alignment).
        for col in 0..bpr_u {
            let off = row_start + col;
            // Group separator: extra space between groups of 4.
            if col > 0 && col % 4 == 0 {
                buf.set_string(x, y, " ", fg_style);
                x += 1;
            }
            // Per-byte rendering.
            let in_sel = sel.map_or(false, |(lo, hi)| off >= lo && off < hi);
            let is_cursor = focused && off == view.cursor;
            let pair = if off < total_bytes {
                format!("{:02x}", view.bytes[off])
            } else {
                "  ".to_string()
            };
            // Two-character byte; cursor highlight may target only one
            // nibble in Insert mode.
            let mut hi_char = pair.chars().nth(0).unwrap_or(' ');
            let mut lo_char = pair.chars().nth(1).unwrap_or(' ');
            let base_style = if in_sel { fg_style.bg(sel_bg) } else { fg_style };
            // Hi nibble cell.
            let hi_style = if is_cursor && off < total_bytes
                && (matches!(mode, Mode::Insert) || matches!(mode, Mode::Normal | Mode::Visual { .. }))
                && view.nibble_high
            {
                cursor_style
            } else if is_cursor && off < total_bytes && !matches!(mode, Mode::Insert) {
                cursor_style
            } else {
                base_style
            };
            // Lo nibble cell — only the active nibble carries the cursor
            // tint in Insert mode; in Normal/Visual the whole byte does.
            let lo_style = if is_cursor && off < total_bytes && matches!(mode, Mode::Insert) && !view.nibble_high {
                cursor_style
            } else if is_cursor && off < total_bytes && !matches!(mode, Mode::Insert) {
                cursor_style
            } else {
                base_style
            };
            // Render into the buffer.
            buf.set_string(x,     y, hi_char.to_string(), hi_style);
            buf.set_string(x + 1, y, lo_char.to_string(), lo_style);
            // Trailing space between bytes within a group (skip after
            // the last byte of a group; the group separator above adds
            // its own padding).
            let _ = (&mut hi_char, &mut lo_char);
            let space_x = x + 2;
            if col + 1 < bpr_u && (col + 1) % 4 != 0 {
                buf.set_string(space_x, y, " ", base_style);
            } else if col + 1 < bpr_u {
                // First space of the group separator — second one
                // comes from the col%4==0 branch on the next loop.
            }
            x += 3;
        }

        // Separator between hex pane and ASCII pane.
        // x currently sits one past the last hex byte's space. Draw " │".
        buf.set_string(x, y, " │", sep_style);
        x += 2;

        // ASCII pane.
        for col in 0..bpr_u {
            let off = row_start + col;
            if off >= total_bytes {
                buf.set_string(x, y, " ", fg_style);
                x += 1;
                continue;
            }
            let b = view.bytes[off];
            let in_sel = sel.map_or(false, |(lo, hi)| off >= lo && off < hi);
            let is_cursor = focused && off == view.cursor;
            let (ch, style) = if (0x20..=0x7e).contains(&b) {
                (b as char, fg_style)
            } else {
                ('.', muted_style)
            };
            let mut s = if in_sel { style.bg(sel_bg) } else { style };
            if is_cursor { s = mirror_style; }
            let mut tmp = [0u8; 4];
            buf.set_string(x, y, ch.encode_utf8(&mut tmp), s);
            x += 1;
        }
    }

    // Status row at the bottom.
    let sy = area.y + area.height - 1;
    let status = format!(
        "  0x{:08x}/0x{:08x}  {} bpr  {}",
        view.cursor,
        total_bytes.saturating_sub(1),
        bpr,
        if view.dirty { "modified" } else { "" },
    );
    let blank: String = " ".repeat(area.width as usize);
    buf.set_string(area.x, sy, blank, dim_style);
    buf.set_string(area.x, sy, status, dim_style);
}

/// Title line for a cell. Single session → just the label. Multiple
/// sessions → `lbl1 · lbl2 · lbl3` with the active one styled like a
/// focused tab and others dimmed. If the full tab strip won't fit in
/// the available width, collapse to `{active} N/M`. A `[↑N]` suffix
/// appears when the active PTY is scrolled back through its history.
fn cell_title_line(cell: &Cell, project_root: Option<&Path>, width: u16, focused: bool, t: &Theme, active_style: Style) -> Line<'static> {
    let _ = focused;
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
    // Swap/move arm takes precedence over the regular mode colouring —
    // the orange source-cell border is the visual anchor telling the
    // user which cell their next click acts on.
    if app.pending_swap || app.pending_swap_follow {
        return Style::default()
            .fg(Color::Rgb(0xff, 0xa8, 0x60))
            .add_modifier(Modifier::BOLD);
    }
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
        "[HEX]"         => t.info,
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
        Session::Hex(h) => {
            let dirty = if h.dirty { "*" } else { "" };
            let name = h.file_name();
            (format!("{name}{dirty}"), Some("[HEX]"))
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

/// Paint a PTY cell's current screen into `buf` at `area`. Blits the
/// reader-thread-produced snapshot (cell copies — no allocation, no
/// parser-lock contention), then applies the virtual-cursor and
/// Visual-mode selection overlays by mutating a small number of
/// destination cells. This replaces the old `render_pty` → `Paragraph`
/// path: the snapshot is already a ratatui `Buffer`, so there's no
/// `Line`/`Span` materialization at all.
fn render_pty_into(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    cell: &Cell,
    t: &Theme,
    focused: bool,
    mode: &Mode,
) {
    let session = match cell.active_session().as_pty() {
        Some(p) => p,
        None => return,
    };

    // Refresh rows_emitted before we interpret any absolute positions.
    session.tick_rows_emitted();

    let show_vcursor = focused
        && matches!(mode, Mode::Normal | Mode::Visual { .. });
    let vcursor = session.vcursor;
    let anchor  = if focused && matches!(mode, Mode::Visual { .. }) {
        session.visual_anchor
    } else {
        None
    };
    let v_vp = if show_vcursor {
        session.vpos_viewport_row(vcursor).map(|r| (r, vcursor.col))
    } else if focused
        && matches!(mode, Mode::Insert)
        && matches!(cell.active_session(), Session::Shell(_))
    {
        // Shell in Insert mode: draw the cursor as an overlay at the
        // child's real position. The native terminal cursor used to be
        // moved here via `frame.set_cursor_position`, but it would lag
        // a frame behind the buffer blit during chatty output and
        // appear to flicker through whatever was being drawn. The
        // overlay sits on the right cell every frame.
        if let Ok(parser) = session.parser.lock() {
            if parser.screen().scrollback() == 0 {
                let (cy, cx) = parser.screen().cursor_position();
                Some((cy, cx))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let sel_abs_range = anchor.map(|a| {
        if a.abs < vcursor.abs
            || (a.abs == vcursor.abs && a.col <= vcursor.col)
        {
            (a, vcursor)
        } else {
            (vcursor, a)
        }
    });
    let sel_vrange = sel_abs_range.map(|(s, e)| {
        (s.col, e.col, session.vpos_viewport_row(s), session.vpos_viewport_row(e))
    });

    let Some(snap) = session.snapshot() else { return; };

    // Blit: copy snapshot cells into `buf` at `area`. Clamp to the
    // intersection of the snapshot grid and the target area so a
    // mid-resize draw doesn't read past either buffer.
    let w = area.width.min(snap.cols);
    let h = area.height.min(snap.rows);
    for r in 0..h {
        for c in 0..w {
            let src = snap.buf[(c, r)].clone();
            buf[(area.x + c, area.y + r)] = src;
        }
    }

    // Selection overlay. Cheap: iterate the viewport rows in the
    // selection range and OR a bg colour onto each cell's existing
    // style so fg/bold/etc. are preserved.
    if let Some((sc, ec, sr, er)) = sel_vrange {
        let top = sr.unwrap_or(0);
        let bot = er.unwrap_or(h.saturating_sub(1));
        let sel_bg = t.bg_sel;
        for r in top..=bot.min(h.saturating_sub(1)) {
            let col_lo = if r == top { sc } else { 0 };
            let col_hi = if r == bot { ec } else { w.saturating_sub(1) };
            for c in col_lo..=col_hi.min(w.saturating_sub(1)) {
                let dst = &mut buf[(area.x + c, area.y + r)];
                let s = dst.style().bg(sel_bg);
                dst.set_style(s);
            }
        }
    }

    // Cursor overlay: one cell inverted. Drawn last so it wins over
    // selection bg on its own cell.
    if let Some((vr, vc)) = v_vp {
        if vr < h && vc < w {
            let dst = &mut buf[(area.x + vc, area.y + vr)];
            let s = dst.style().bg(t.fg).fg(t.bg);
            dst.set_style(s);
        }
    }
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
    // One 3-row block per discovered repo. Cap so we never starve the
    // file list — at least 5 rows stay for the tree itself. When the
    // cap bites, the last rendered block is clipped and a `+N more`
    // hint lands on the final row.
    let n_repos = app.git.repos.len();
    let show_footer = n_repos > 0;
    let desired = (n_repos as u16) * 3;
    let room    = inner.height.saturating_sub(5);
    let footer_h: u16 = if show_footer && inner.height >= 5 {
        desired.min(room)
    } else {
        0
    };
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

/// Stacked footer: one 3-row block per discovered repo (separator with
/// optional repo label + branch + counts). The label rule:
///   * 1 repo, workdir == project root → unlabeled `─ git ─`
///   * 1 repo, nested                  → `─ git · <rel> ─`
///   * N repos                         → every block labelled (root
///                                       uses the project folder name,
///                                       nested repos use their rel
///                                       path to the project root)
fn draw_git_footer_compact(frame: &mut Frame, area: Rect, app: &App) {
    let repos_count = app.git.repos.len();
    let w = area.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(repos_count * 3);
    for repo in app.git.repos.iter() {
        let label = repo_footer_label(repo, app, repos_count);
        lines.push(build_git_separator_line(app, repo, label.as_deref(), w));
        lines.push(truncate_line_with_arrow(Line::from(build_git_branch_spans(app, repo)), w));
        lines.push(truncate_line_with_arrow(Line::from(build_git_counts_spans(app, repo)), w));
    }
    // Caller caps `area.height` to what fits; rewrite the final visible
    // row as a terse "+N more" hint when at least one full repo block
    // got clipped so the user knows the list is partial.
    let h = area.height as usize;
    if lines.len() > h {
        lines.truncate(h);
        let rendered_blocks = h / 3;
        let hidden_blocks   = repos_count.saturating_sub(rendered_blocks);
        if hidden_blocks > 0 && !lines.is_empty() {
            let last = lines.len() - 1;
            lines[last] = Line::from(vec![
                Span::styled(
                    format!(" +{hidden_blocks} more repo{}", if hidden_blocks == 1 { "" } else { "s" }),
                    Style::default().fg(app.theme.dim),
                )
            ]);
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Label suffix for the `─ git [· name] ─` separator. Returns `None`
/// to mean "unlabelled" (the single-repo-at-project-root case).
fn repo_footer_label(repo: &crate::git::GitSnapshot, app: &App, repos_count: usize) -> Option<String> {
    let wd = repo.workdir.as_ref()?;
    let project_root = app.git.project_root.as_ref();
    let is_root = project_root.map(|pr| path_eq(pr, wd)).unwrap_or(false);

    if repos_count == 1 {
        if is_root { return None; }
        if let Some(pr) = project_root {
            if let Ok(rel) = wd.strip_prefix(pr) {
                return Some(rel.to_string_lossy().replace('\\', "/"));
            }
        }
        return Some(name_of(wd));
    }

    // Multi-repo: every block is labelled. Root takes the project's
    // folder name; nested repos show their path from the project root
    // so sibling repos are distinguishable.
    if is_root {
        return project_root.map(|p| name_of(p));
    }
    if let Some(pr) = project_root {
        if let Ok(rel) = wd.strip_prefix(pr) {
            return Some(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Some(name_of(wd))
}

fn name_of(p: &std::path::Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string()
}

fn path_eq(a: &std::path::Path, b: &std::path::Path) -> bool {
    // Delegates to the authoritative comparator in git.rs so Windows
    // separator / trailing-slash normalisation stays in one place.
    crate::git::paths_equal(a, b)
}

fn build_git_separator_line(
    app: &App,
    repo: &crate::git::GitSnapshot,
    label: Option<&str>,
    width: usize,
) -> Line<'static> {
    let t = &app.theme;
    let _ = repo;
    let (label_style, rule_style) = git_divider_styles(app);
    let mut sep_spans: Vec<Span<'static>> = vec![
        Span::styled("─ ", rule_style),
        Span::styled("git", label_style),
        Span::raw(" "),
    ];
    if let Some(lbl) = label {
        sep_spans.push(Span::styled("· ", rule_style));
        sep_spans.push(Span::styled(lbl.to_string(), label_style));
        sep_spans.push(Span::raw(" "));
    }
    if !repo.op_state.is_clean() {
        sep_spans.push(Span::styled(
            format!("[{}] ", repo.op_state.label()),
            Style::default().fg(t.err).add_modifier(Modifier::BOLD),
        ));
    }
    let used: usize = sep_spans.iter().map(|s| s.width()).sum();
    let trailing = width.saturating_sub(used);
    sep_spans.push(Span::styled("─".repeat(trailing), rule_style));
    Line::from(sep_spans)
}

fn build_git_branch_spans(app: &App, repo: &crate::git::GitSnapshot) -> Vec<Span<'static>> {
    let t = &app.theme;
    let branch_style = if !repo.op_state.is_clean() || repo.has_conflicts() {
        Style::default().fg(t.err).add_modifier(Modifier::BOLD)
    } else if !repo.is_clean() {
        Style::default().fg(t.warn)
    } else {
        Style::default().fg(t.ok)
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(format!("⎇ {}", repo.branch), branch_style),
    ];
    if repo.ahead > 0 {
        spans.push(Span::styled(format!("  ↑{}", repo.ahead), Style::default().fg(t.ok)));
    }
    if repo.behind > 0 {
        spans.push(Span::styled(format!(" ↓{}", repo.behind), Style::default().fg(t.warn)));
    }
    spans
}

fn build_git_counts_spans(app: &App, repo: &crate::git::GitSnapshot) -> Vec<Span<'static>> {
    let t = &app.theme;
    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
    let mut pushed_any = false;
    if repo.staged > 0 {
        spans.push(Span::styled(format!("S{}", repo.staged), Style::default().fg(t.ok)));
        pushed_any = true;
    }
    if repo.modified > 0 {
        if pushed_any { spans.push(Span::raw(" ")); }
        spans.push(Span::styled(format!("M{}", repo.modified), Style::default().fg(t.warn)));
        pushed_any = true;
    }
    if repo.untracked > 0 {
        if pushed_any { spans.push(Span::raw(" ")); }
        spans.push(Span::styled(format!("?{}", repo.untracked), Style::default().fg(t.info)));
        pushed_any = true;
    }
    if repo.conflicts > 0 {
        if pushed_any { spans.push(Span::raw(" ")); }
        spans.push(Span::styled(format!("U{}", repo.conflicts), Style::default().fg(t.err).add_modifier(Modifier::BOLD)));
        pushed_any = true;
    }
    if !pushed_any {
        spans.push(Span::styled("clean", Style::default().fg(t.dim)));
    }
    if repo.stash_count > 0 {
        spans.push(Span::styled(
            format!("  ⚑{}", repo.stash_count),
            Style::default().fg(t.info),
        ));
    }
    spans
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
    // In git mode the top section flips from PROJECTS to REPOSITORIES
    // — one row per discovered repo across every project, with the
    // active repo bolded. PgUp/PgDn cycles through the same flat list
    // (see `App::repo_jump_global`).
    let total_repos: usize = app.projects.projects.iter()
        .enumerate()
        .map(|(pi, p)| {
            // Active project's live repo count beats the stale cache
            // (cache updates on `refresh_states`, not every tick).
            if pi == app.projects.active { app.git.repos.len() } else { p.repos.len() }
        })
        .sum();
    let n_header = (total_repos as u16).max(1);
    if inner.height <= n_header + 3 {
        // Not enough space for a meaningful git section — just render
        // repo headers and bail.
        draw_repo_headers(frame, inner, app, inner.height.min(n_header));
        return;
    }

    // Repositories area (collapsed headers only).
    let proj_rect = Rect { x: inner.x, y: inner.y, width: inner.width, height: n_header };
    draw_repo_headers(frame, proj_rect, app, n_header);
    let n_projects = n_header;

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

/// Git-mode counterpart of `draw_project_headers`: one row per repo
/// across every project. Label format: `<project>/<rel-path>` (or
/// just `<project>` for a root repo). The active repo (matching both
/// `projects.active` and `app.git.active`) is bolded so the user can
/// see which one PgUp/PgDn is steering.
fn draw_repo_headers(frame: &mut Frame, area: Rect, app: &App, rows: u16) {
    let w = area.width as usize;
    let t = &app.theme;
    let mut lines: Vec<Line> = Vec::new();
    for (pi, project) in app.projects.projects.iter().enumerate() {
        let is_active_proj = pi == app.projects.active;
        // For the active project, take live state from `app.git.repos`
        // so tinting reflects edits made this session without waiting
        // on `refresh_states`. For other projects, rely on the cached
        // `project.repos` populated by the last refresh.
        if is_active_proj {
            for (ri, repo) in app.git.repos.iter().enumerate() {
                if lines.len() as u16 >= rows { break; }
                let label = repo_full_label(project, repo.workdir.as_deref(), &app.git.project_root);
                let state = crate::projects::ProjectState::from_snapshot(repo);
                let is_active = is_active_proj && ri == app.git.active;
                lines.push(truncate_line_with_arrow(repo_header_line(t, state, &label, is_active), w));
            }
        } else {
            for ri in 0..project.repos.len() {
                if lines.len() as u16 >= rows { break; }
                let ri_info = &project.repos[ri];
                let label = repo_full_label(project, Some(&ri_info.root), &Some(project.root.clone()));
                lines.push(truncate_line_with_arrow(repo_header_line(t, ri_info.state, &label, false), w));
            }
        }
        if lines.len() as u16 >= rows { break; }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Cross-project repo label: `<project>` for a root repo, or
/// `<project>/<rel>` for a nested repo. Falls back to the project
/// name alone if the workdir can't be made relative.
fn repo_full_label(
    project: &crate::projects::Project,
    workdir: Option<&std::path::Path>,
    project_root: &Option<std::path::PathBuf>,
) -> String {
    let Some(wd) = workdir else { return project.name.clone(); };
    if let Some(pr) = project_root.as_ref() {
        if path_eq(pr, wd) {
            return project.name.clone();
        }
        if let Ok(rel) = wd.strip_prefix(pr) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.is_empty() {
                return project.name.clone();
            }
            return format!("{}/{}", project.name, rel);
        }
    }
    project.name.clone()
}

fn repo_header_line(
    t: &Theme,
    state: crate::projects::ProjectState,
    label: &str,
    is_active: bool,
) -> Line<'static> {
    let arrow = if is_active { "▾ " } else { "▸ " };
    let arrow_style = Style::default().fg(t.dim);
    let name_style  = if is_active {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg).add_modifier(Modifier::BOLD)
    };
    // No-repo rows never reach here — the caller only renders actual
    // repos — but mirror the project-header behaviour: if state is
    // None somehow, skip the dot entirely rather than show a blank.
    if matches!(state, crate::projects::ProjectState::None) {
        return Line::from(vec![
            Span::styled(arrow.to_string(), arrow_style),
            Span::styled(label.to_string(), name_style),
        ]);
    }
    let dot_style = Style::default().fg(state_color(state, t));
    Line::from(vec![
        Span::styled(arrow.to_string(), arrow_style),
        Span::styled(format!("{} ", state.glyph()), dot_style),
        Span::styled(label.to_string(), name_style),
    ])
}

/// Render just the project-header rows (always arrow-collapsed, even
/// the active one) into `area`. Used at the top of the git modes.
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
        Session::Hex(h)      => {
            let root = app.projects.projects
                .get(app.projects.active)
                .map(|p| p.root.clone());
            let path = h.path.clone();
            let external = match (&root, &path) {
                (Some(r), Some(p)) => !p.starts_with(r),
                _                  => false,
            };
            (h.file_name().to_string(), Some("[HEX]"), true, h.dirty, external, path)
        }
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

    // Project header dot reflects the *root* repo only — a project
    // that contains only nested repos gets no dot on its header.
    // Nested repos show their own dots on the folder rows below.
    let state = if is_active {
        match app.git.root_repo() {
            Some(r) => ProjectState::from_snapshot(r),
            None    => ProjectState::None,
        }
    } else {
        // Cached per-repo states: find the one whose root == project root.
        app.projects.projects.get(idx).and_then(|p| {
            p.repos.iter()
                .find(|r| crate::git::paths_equal(&r.root, &p.root))
                .map(|r| r.state)
        }).unwrap_or(ProjectState::None)
    };

    let arrow = if is_active { "▾ " } else { "▸ " };
    let name  = app.projects.projects.get(idx).map(|p| p.name.clone()).unwrap_or_default();

    let arrow_style = Style::default().fg(t.dim);
    let name_style  = if is_active {
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.fg).add_modifier(Modifier::BOLD)
    };

    let _ = e; // depth unused for headers — they're always flush-left
    // No-repo projects: render the name flush against the arrow with
    // no placeholder where the dot would go. Column alignment across
    // mixed (some-repo / no-repo) project rows is a deliberate non-goal.
    if matches!(state, ProjectState::None) {
        return Line::from(vec![
            Span::styled(arrow.to_string(), arrow_style),
            Span::styled(name,              name_style),
        ]);
    }
    let dot_style = Style::default().fg(state_color(state, t));
    Line::from(vec![
        Span::styled(arrow.to_string(), arrow_style),
        Span::styled(format!("{} ", state.glyph()), dot_style),
        Span::styled(name,              name_style),
    ])
}

fn dir_line(e: &Entry, t: &Theme, app: &App) -> Line<'static> {
    let indent = "  ".repeat(e.depth as usize);
    let marker = if e.expanded { "▾ " } else { "▸ " };
    let name = e.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
    let (fg, mods) = git_tint(app.git.dir_status(&e.path), t);
    let name_style = Style::default().fg(fg).add_modifier(mods);
    // Nested repo root? Prefix the row with a status dot (same glyph
    // scheme as the project header). Root-level repos are already
    // covered by the project header dot, so skip them here.
    let nested_dot = nested_repo_dot(&e.path, app);
    if let Some((glyph, color)) = nested_dot {
        // Dot sits after the expand arrow: `  ▸ ● frontend/`. Keeps
        // the arrow column aligned with non-repo sibling folders.
        Line::from(vec![
            Span::styled(format!("{indent}{marker}"), Style::default().fg(t.dim)),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(format!("{name}/"), name_style),
        ])
    } else {
        Line::from(Span::styled(
            format!("{indent}{marker}{name}/"),
            name_style,
        ))
    }
}

/// `Some((glyph, color))` if the folder is a nested git repo (not the
/// project root). `None` otherwise — the caller keeps the plain row.
fn nested_repo_dot(path: &std::path::Path, app: &App) -> Option<(char, Color)> {
    let repo = app.git.repo_at(path)?;
    // Skip the root repo — its dot lives on the project header.
    if let Some(pr) = app.git.project_root.as_ref() {
        if path_eq(pr, path) {
            return None;
        }
    }
    let state = crate::projects::ProjectState::from_snapshot(repo);
    Some((state.glyph(), state_color(state, &app.theme)))
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
        Some(FileStatus::Untracked) => (t.info,  Modifier::empty()),
        Some(FileStatus::Ignored)   => (t.dim,   Modifier::empty()),
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
        "   [0 explorer  1-9 cell  s swap  m move  q close] "
    } else {
        match &app.mode {
            // Insert mode hint is the same across editor / PTY cells —
            // Esc is the universal way out. Put it here rather than
            // leaving it blank so newcomers can discover normal mode
            // without reading :help.
            Mode::Insert        => "   [Esc normal] ",
            Mode::Command {..}  => "",
            Mode::Password {..} => "   [Enter submit  Esc cancel] ",
            Mode::Visual {..}   => "   [d delete  y yank  c change  > indent  < dedent  Esc cancel] ",
            Mode::Normal => match app.focus {
                FocusId::Cell(_) if app.focused_session_is_editor()
                                  => "   [i insert] ",
                FocusId::Cell(_) if app.focused_session_is_diff()
                                  => "",
                // PTY cells (claude / shell): Esc in Normal mode
                // forwards a literal ESC byte to the child (see
                // main.rs::handle_normal). Surface that so users know
                // the key isn't a no-op here.
                FocusId::Cell(_) if app.focused_cell()
                    .map(|c| matches!(c.active_session(),
                        Session::Claude(_) | Session::Shell(_)))
                    .unwrap_or(false)
                                  => "   [i insert  Esc send esc] ",
                FocusId::Cell(_)  => "   [i insert] ",
                FocusId::Explorer    => match app.explorer_mode {
                    ExplorerMode::Normal       => "   [↵ preview  e edit  a add  c close  d del  o open  n new  g git] ",
                    ExplorerMode::GitOverview  => "   [b branches  c changes  l log  m commit  a stage  A unstage  p push  P pull  f fetch  g/Esc back] ",
                    ExplorerMode::GitBranches  => "   [n new  d del  D force-del  c changes  l log  g exit  Esc back] ",
                    ExplorerMode::GitChanges   => "   [s stage  d discard  o ours  t theirs  e edit  v diff  b branches  l log  g exit  Esc back] ",
                    ExplorerMode::GitLog       => "   [v diff  c sha  b branches  g exit  Esc back] ",
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
                    Session::Hex(_)      => ("hex",      t.info),
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
                                left.push(Span::styled(" ⚠  disk changed", Style::default().fg(t.err).bold())),
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
                    Session::Hex(h) => {
                        left.push(Span::styled(format!(" · {}", h.file_name()), Style::default().fg(t.fg)));
                        if h.dirty {
                            left.push(Span::styled(" +", Style::default().fg(t.warn)));
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

