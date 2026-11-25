use crate::agent::markdown::{self, Markdown};
use crate::config::Config;
use crate::context::{self, ContextItem, ContextSnapshot};
use crate::core::{Flags, Mode, Side};
use crate::input::{LEFT, MIDDLE, PasteTarget, PointerKind, RIGHT, SHIFT, keysym};
use crate::panel::{
    ACCENT, DANGER, FAILED, Grid, MIX_MUTED, MIX_POPUP, MIX_SOFT, WARNING, mix_color, rect, rgb,
};
use crate::ui::{Effect, Focus, Pointer, Ui, WHEEL_LINES};
use crate::width::{scalar_width, text_width};
use crate::worker::Worker;
use crate::workspace::{
    Action, AgentChange, AgentDraft, AgentDrafts, AgentEvent, AgentMessage, AgentModelPatch,
    AgentRecovery, AgentRequest, AgentSend, Client, PaneId, Rect,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::ops::Range;
use std::os::fd::RawFd;
use std::sync::Arc;

const INPUT_LIMIT: usize = 64 * 1024;
const REWIND_LABEL: &str = " ← ";

#[derive(Default)]
pub(crate) struct AgentView {
    pub input: String,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub busy: bool,
    pub messages: Vec<AgentMessage>,
    pub changes: Arc<Vec<AgentChange>>,
    pub changes_omitted: u32,
    pub scroll: usize,
    pub pending: Vec<AgentRequest>,
}

impl AgentView {
    /// Applies an owner patch; returns false when it only extends the streamed reply, which
    /// is redrawn once its Markdown is ready.
    pub fn restore_patch(&mut self, patch: &AgentModelPatch) -> io::Result<bool> {
        let reply = |message: &AgentMessage| message.role == "Agent" && message.id.is_none();
        let appended = match (
            self.messages.get(patch.start as usize..),
            &patch.messages[..],
        ) {
            (Some([old]), [new]) => {
                reply(old)
                    && reply(new)
                    && old.turn == new.turn
                    && old.text.len() == patch.prefix_bytes as usize
            }
            _ => false,
        };
        let sending = self
            .pending
            .iter()
            .any(|request| matches!(request, AgentRequest::Send(_)));
        let busy = patch.busy || sending;
        let redraw = !appended
            || busy != self.busy
            || patch.changes.is_some()
            || patch.changes_omitted != self.changes_omitted;
        patch.apply_messages(&mut self.messages)?;
        if let Some(changes) = &patch.changes {
            self.changes.clone_from(changes);
        }
        self.changes_omitted = patch.changes_omitted;
        self.busy = busy;
        Ok(redraw)
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.input.len();
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.set_cursor(0, false);
    }

    fn set_cursor(&mut self, offset: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = offset;
    }

    pub fn insert(&mut self, text: &str) -> Result<(), String> {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        if self.input.len() - range.len() + text.len() > INPUT_LIMIT {
            return Err("Agent input exceeds 64 KiB".into());
        }
        self.set_cursor(range.start + text.len(), false);
        self.input.replace_range(range, text);
        Ok(())
    }

    pub fn key(&mut self, symbol: u32, shift: bool) {
        let before = self
            .input
            .floor_char_boundary(self.cursor.saturating_sub(1));
        let after = self.input.ceil_char_boundary(self.cursor + 1);
        let selection = self.selection();
        let collapse = selection.clone().filter(|_| !shift);
        let offset = match symbol {
            keysym::LEFT => collapse.map_or(before, |range| range.start),
            keysym::RIGHT => collapse.map_or(after, |range| range.end),
            keysym::HOME => self.input[..self.cursor].rfind('\n').map_or(0, |at| at + 1),
            keysym::END => self.input[self.cursor..]
                .find('\n')
                .map_or(self.input.len(), |at| self.cursor + at),
            keysym::BACKSPACE | keysym::DELETE => {
                let range = selection.unwrap_or(if symbol == keysym::BACKSPACE {
                    before..self.cursor
                } else {
                    self.cursor..after
                });
                self.set_cursor(range.start, false);
                self.input.replace_range(range, "");
                return;
            }
            _ => return,
        };
        self.set_cursor(offset, shift);
    }
}

/// A drawn panel; where its controls are stays in `AgentPanel` for the pointer.
pub(crate) struct Panel {
    pub grid: Grid,
    pub backdrop: Option<Grid>,
    pub preview: Option<Rect>,
    pub cursor: Option<(u16, u16)>,
    /// The editor's lines and first visible row.
    #[cfg_attr(not(feature = "gui"), expect(dead_code))]
    pub editor: (usize, usize),
    hits: Vec<(Rect, AgentAction)>,
    chat: Rect,
    chat_rows: Vec<(usize, Rect)>,
    bg: u32,
}

fn hit_at(hits: &[(Rect, AgentAction)], column: u16, row: u16) -> Option<AgentAction> {
    hits.iter()
        .rfind(|(rect, _)| rect.contains(column, row))
        .map(|(_, action)| *action)
}

impl Panel {
    fn text(&mut self, x: u16, y: u16, columns: u16, text: &str, foreground: u32) {
        self.grid.text(x, y, columns, text, foreground, self.bg);
    }

    fn hit(&mut self, x: u16, y: u16, cols: u16, action: AgentAction) {
        self.hits.push((rect(x, y, cols, 1), action));
    }
}

fn editor(ui: &Ui, side: Rect) -> (Rect, EditorLines) {
    let session = ui.agent.active();
    let max_rows = usize::from((side.rows / 3).clamp(1, 8));
    let columns = session.editor_columns(usize::from(side.cols.saturating_sub(2)));
    let layout = session.editor_layout(columns, max_rows);
    let rows = layout.lines.len().max(3).min(max_rows) as u16;
    let y = (side.y + side.rows).saturating_sub(rows);
    (
        rect(side.x + 1, y, side.cols.saturating_sub(2), rows),
        layout,
    )
}

pub(crate) fn draw(
    ui: &mut Ui,
    config: &Config,
    side: Rect,
    // The window without its tab row and footer; a preview floats over it.
    canvas: Rect,
    pointer: Option<(u16, u16)>,
    preedit: &str,
) -> io::Result<Panel> {
    // A preview floats over the panel as it was drawn without it.
    let backdrop = match ui.agent.active_mut().context.preview.take() {
        Some(preview) => {
            let base = draw(ui, config, side, canvas, None, "");
            ui.agent.active_mut().context.preview = Some(preview);
            Some(base?.grid)
        }
        None => None,
    };
    let (editor, editor_layout) = editor(ui, side);
    let area = backdrop
        .as_ref()
        .map_or(side, |_| crate::file_panel::preview_rect(canvas));
    let (foreground, bg) = (config.foreground, config.background);
    let mut grid = ui
        .agent
        .spare_grid
        .take()
        .unwrap_or_else(|| Grid::new(area, foreground, bg));
    grid.reset(area, foreground, bg);
    let (mut hits, mut chat_rows) = (
        std::mem::take(&mut ui.agent.hits),
        std::mem::take(&mut ui.agent.chat_rows),
    );
    hits.clear();
    chat_rows.clear();
    let mut painter = AgentPainter {
        ui,
        side,
        pointer,
        preedit,
        frame: Panel {
            grid,
            backdrop,
            hits,
            chat: Rect::default(),
            chat_rows,
            preview: None,
            cursor: None,
            editor: (editor_layout.lines.len(), editor_layout.first),
            bg,
        },
        palette: Palette {
            bg,
            foreground,
            muted: mix_color(foreground, bg, MIX_MUTED),
            user_bg: mix_color(foreground, bg, MIX_SOFT),
            selection_bg: mix_color(foreground, bg, 70),
            error: mix_color(DANGER, foreground, 128),
        },
    };
    painter.draw_agent(editor, editor_layout)?;
    let AgentPainter { ui, mut frame, .. } = painter;
    ui.agent.hits = std::mem::take(&mut frame.hits);
    ui.agent.chat_rows = std::mem::take(&mut frame.chat_rows);
    ui.agent.chat = frame.chat;
    ui.agent.editor = editor;
    Ok(frame)
}

struct AgentPainter<'a> {
    ui: &'a mut Ui,
    side: Rect,
    pointer: Option<(u16, u16)>,
    preedit: &'a str,
    frame: Panel,
    palette: Palette,
}

#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    foreground: u32,
    muted: u32,
    user_bg: u32,
    selection_bg: u32,
    error: u32,
}

fn draw_edit_row(
    grid: &mut Grid,
    (x, y, cols): (u16, u16, u16),
    path: &str,
    (added, removed): (u32, u32),
    p: Palette,
) {
    let (added_color, removed_color) = crate::panel::diff_colors(p.foreground);
    let (plus, minus) = (format!(" +{added}"), format!(" -{removed}"));
    let counts = (plus.len() + minus.len()).min(usize::from(cols)) as u16;
    let plus_width = (plus.len() as u16).min(counts);
    let path_width = cols - counts;
    let path = crate::width::fit_middle(path, path_width);
    grid.text(x, y, path_width, &path, p.foreground, p.bg);
    grid.text(x + path_width, y, plus_width, &plus, added_color, p.bg);
    let x = x + path_width + plus_width;
    grid.text(x, y, counts - plus_width, &minus, removed_color, p.bg);
}

fn recolor_text(grid: &mut Grid, rect: Rect, color: u32) {
    let row = &mut grid.rows[usize::from(rect.y - grid.rect.y)];
    let start = usize::from(rect.x.saturating_sub(grid.rect.x));
    let end = (start + usize::from(rect.cols)).min(row.cells.len());
    for cell in &mut row.cells[start..end] {
        cell.fg = rgb(color);
    }
}

impl AgentPainter<'_> {
    fn draw_agent(&mut self, editor: Rect, editor_layout: EditorLines) -> io::Result<()> {
        let (side, p) = (self.side, self.palette);
        if side.cols < 12 || side.rows < 6 {
            self.frame
                .text(side.x, side.y, side.cols, "Alt+a: close", p.foreground);
            return Ok(());
        }
        if self.ui.agent.active().context.preview.is_some() {
            self.draw_preview();
        } else {
            for row in &mut self.frame.grid.rows[1..] {
                row.cells[0].c = '│';
                row.cells[0].fg = rgb(p.user_bg);
                row.used = 1;
            }
            self.frame.hits.push((side, AgentAction::Output));
            self.draw_agent_tabs(editor);
            let session = self.ui.agent.active();
            let attachments = (session.context.basket.len() as u16)
                .min(3)
                .min(editor.y.saturating_sub(side.y + 6));
            let hints = 3 * u16::from(session.rewind_target.is_some());
            let files = session.changed_files();
            let listed = if session.changes_expanded {
                let space = editor.y.saturating_sub(side.y + 9 + attachments + hints);
                (files.len() as u16).min(4).min(space)
            } else {
                0
            };
            let summary = listed + u16::from(!files.is_empty());
            let chat_end = editor.y.saturating_sub(1 + attachments + hints + summary);
            self.draw_transcript(editor, side.y + 1, chat_end)?;
            if hints != 0 {
                self.draw_rewind(editor);
            }
            self.draw_attachments(editor, chat_end, attachments);
            if !files.is_empty() {
                self.draw_changes(editor, chat_end + attachments, listed, &files);
            }
            let row = &mut self.frame.grid.rows[usize::from(editor.y - side.y - 1)];
            row.cells.fill(crate::core::Cell {
                c: '─',
                fg: rgb(p.user_bg),
                bg: rgb(p.bg),
                flags: Flags::default(),
            });
            row.cells[0].c = '├';
            row.used = row.cells.len();
            self.draw_editor(editor, editor_layout);
        }
        let hovered = self
            .pointer
            .and_then(|(x, y)| hit_at(&self.frame.hits, x, y));
        let focused = (self.ui.focus == Focus::Controls).then_some(self.ui.agent.keyboard);
        for &(rect, action) in &self.frame.hits {
            if hovered != Some(action) && focused != Some(action) {
                continue;
            }
            let grid = &mut self.frame.grid;
            match action {
                AgentAction::Session(_)
                | AgentAction::Editor
                | AgentAction::Output
                | AgentAction::Copy(_) => (),
                AgentAction::NewSession | AgentAction::ClosePanel | AgentAction::Rewind(_) => {
                    recolor_text(grid, rect, p.foreground)
                }
                AgentAction::CloseSession(_) => recolor_text(grid, rect, FAILED),
                _ => grid.recolor(rect, p.user_bg),
            }
        }
        Ok(())
    }

    fn draw_changes(&mut self, editor: Rect, top: u16, rows: u16, files: &[(usize, u32, u32)]) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let (added, removed) = files
            .iter()
            .fold((0, 0), |(a, r), file| (a + file.1, r + file.2));
        let omitted = match session.model.changes_omitted {
            0 => String::new(),
            count => format!(" · {count} earlier edits omitted"),
        };
        let summary = format!("{} files{omitted}", files.len());
        let place = (editor.x + 3, top, editor.cols - 3);
        draw_edit_row(&mut self.frame.grid, place, &summary, (added, removed), p);
        let marker = crate::panel::expand_icon(session.changes_expanded);
        self.frame
            .text(editor.x, top, 3, &format!(" {marker} "), p.muted);
        self.frame
            .hit(editor.x, top, editor.cols, AgentAction::ToggleChanges);
        let first = match self.ui.agent.keyboard {
            AgentAction::ChangedFile(index) => files
                .iter()
                .position(|file| file.0 == index)
                .unwrap_or(0)
                .saturating_sub(usize::from(rows.saturating_sub(1))),
            _ => files.len().saturating_sub(usize::from(rows)),
        };
        for (y, &(index, added, removed)) in
            (top + 1..).zip(files.iter().skip(first).take(usize::from(rows)))
        {
            let path = session.model.changes[index].path.to_string_lossy();
            let place = (editor.x, y, editor.cols);
            draw_edit_row(&mut self.frame.grid, place, &path, (added, removed), p);
            self.frame
                .hit(editor.x, y, editor.cols, AgentAction::ChangedFile(index));
        }
    }

    fn draw_transcript(&mut self, editor: Rect, top: u16, bottom: u16) -> io::Result<()> {
        let p = self.palette;
        let rows = usize::from(bottom.saturating_sub(top));
        self.ui.agent.prepare_layout(usize::from(editor.cols))?;
        let session = self.ui.agent.active_mut();
        let total = session.lines.len();
        session.model.scroll = session.model.scroll.min(total.saturating_sub(rows));
        let first = total.saturating_sub(rows + session.model.scroll);
        let session = self.ui.agent.active();
        self.frame.chat = rect(editor.x, top, editor.cols, rows as u16);
        for (index, line) in session.lines.iter().enumerate().skip(first).take(rows) {
            let y = top + (index - first) as u16;
            let indent = line.indent(&session.model.messages);
            if let Some(indent) = indent {
                let row = rect(editor.x + indent, y, editor.cols - indent, 1);
                self.frame.chat_rows.push((index, row));
            }
            match *line {
                AgentLine::Blank => (),
                AgentLine::Edit(change) => {
                    let record = &session.model.changes[change];
                    let place = (editor.x + 2, y, editor.cols - 2);
                    let path = record.path.to_string_lossy();
                    let counts = (record.added, record.removed);
                    draw_edit_row(&mut self.frame.grid, place, &path, counts, p);
                    self.frame
                        .hit(editor.x, y, editor.cols, AgentAction::EditDiff(change));
                }
                AgentLine::Role(message) => {
                    let tool = &session.model.messages[message];
                    let expanded = tool
                        .id
                        .as_ref()
                        .is_some_and(|id| session.expanded_tools.contains(id));
                    let marker = if expanded { "▾" } else { "›" };
                    let label = format!(" {marker} {} ", tool.role);
                    self.frame.text(editor.x, y, editor.cols, &label, p.muted);
                    let width = editor.cols.min(text_width(&tool.role) as u16 + 4);
                    self.frame
                        .hit(editor.x, y, width, AgentAction::Tool(message));
                }
                AgentLine::Text { message, ref range } => {
                    let record = &session.model.messages[message];
                    let user = record.role == "You";
                    let rewinding = user && session.rewind_target == Some(record.turn);
                    let background = match (rewinding, user) {
                        (true, _) => p.selection_bg,
                        (_, true) => p.user_bg,
                        _ => p.bg,
                    };
                    let color = if rewinding {
                        crate::panel::ACCENT
                    } else if record.role == "Error" {
                        p.error
                    } else if record.id.is_some() {
                        p.muted
                    } else {
                        p.foreground
                    };
                    let text = session.message_text(message);
                    let mut x = editor.x + message_indent(record);
                    let grid = &mut self.frame.grid;
                    grid.text(editor.x, y, editor.cols, "", p.foreground, background);
                    if let Some(document) = session.markdown(message) {
                        for (span, style) in document.spans(range.clone()) {
                            let columns = text_width(&text[span.clone()]) as u16;
                            let bg = if style & markdown::CODE != 0 {
                                p.user_bg
                            } else {
                                background
                            };
                            grid.flags.set(Flags::BOLD, style & markdown::BOLD != 0);
                            grid.flags.set(Flags::ITALIC, style & markdown::ITALIC != 0);
                            grid.text(
                                x,
                                y,
                                columns,
                                &text[span],
                                markdown::color(style, color),
                                bg,
                            );
                            x += columns;
                        }
                        grid.flags.remove(Flags::BOLD | Flags::ITALIC);
                    } else {
                        let columns = editor.x + editor.cols - x;
                        grid.text(x, y, columns, &text[range.clone()], color, background);
                    }
                    self.frame
                        .hit(editor.x, y, editor.cols, AgentAction::Copy(message));
                    // A user message's last row ends with its rewind button.
                    let width = text_width(REWIND_LABEL) as u16;
                    let last = index + 1 == first + rows
                        || !matches!(session.lines.get(index + 1),
                            Some(AgentLine::Text { message: next, .. }) if *next == message);
                    if user && editor.cols >= width + 5 && last {
                        let x = editor.x + editor.cols - width;
                        let grid = &mut self.frame.grid;
                        grid.text(x, y, width, REWIND_LABEL, p.muted, background);
                        self.frame.hit(x, y, width, AgentAction::Rewind(message));
                    }
                }
            }
            if let Some(indent) = indent
                && let Some((column, selected)) = session.selection_span(index)
            {
                let x = editor.x + indent + column as u16;
                let width = text_width(selected) as u16;
                let grid = &mut self.frame.grid;
                grid.text(x, y, width, selected, p.foreground, p.selection_bg);
            }
        }
        Ok(())
    }

    fn draw_rewind(&mut self, editor: Rect) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let label = session.rewind_index().map_or(String::new(), |index| {
            let messages = &session.model.messages[index..];
            let turns = messages
                .iter()
                .filter(|message| message.role == "You")
                .count();
            let request = messages[0].text.lines().next().unwrap_or("");
            format!("Rewind {turns} turns: {request}")
        });
        let lines = [
            ("j/k select · Enter confirm · Esc exits", p.muted),
            (label.as_str(), p.foreground),
            ("Commands and file changes stay", p.foreground),
        ];
        for (y, (text, color)) in (editor.y.saturating_sub(4)..).zip(lines) {
            self.frame.text(editor.x, y, editor.cols, text, color);
        }
    }

    fn draw_attachments(&mut self, editor: Rect, top: u16, rows: u16) {
        let p = self.palette;
        let basket = &self.ui.agent.active().context.basket;
        let first = match self.ui.agent.keyboard {
            AgentAction::Inspect(index) | AgentAction::Remove(index) => {
                index.saturating_sub(usize::from(rows.saturating_sub(1)))
            }
            _ => basket.len().saturating_sub(usize::from(rows)),
        };
        let remove = editor.x + editor.cols - 3;
        for (y, (index, item)) in (top..)
            .zip(basket.iter().enumerate().skip(first))
            .take(usize::from(rows))
        {
            let label = format!(" +{} ", item.label);
            self.frame
                .text(editor.x, y, editor.cols - 3, &label, p.foreground);
            self.frame.text(remove, y, 3, " × ", p.muted);
            let width = (editor.cols - 3).min(text_width(&item.label) as u16 + 3);
            self.frame
                .hit(editor.x, y, width, AgentAction::Inspect(index));
            self.frame.hit(remove, y, 3, AgentAction::Remove(index));
        }
    }

    fn draw_editor(&mut self, editor: Rect, layout: EditorLines) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let model = &session.model;
        let focused = self.ui.focus == Focus::Editor;
        let composed;
        let (input, layout) = if focused && !self.preedit.is_empty() {
            let (before, after) = model.input.split_at(model.cursor);
            composed = [before, self.preedit, after].concat();
            let caret = model.cursor + self.preedit.len();
            let columns = session.editor_columns(usize::from(editor.cols));
            let layout = editor_lines(&composed, caret, columns, usize::from(editor.rows), None);
            (composed.as_str(), layout)
        } else {
            (model.input.as_str(), layout)
        };
        let EditorLines {
            lines,
            first,
            caret_row,
            caret_column,
        } = layout;
        let rows = usize::from(editor.rows);
        self.frame.cursor = (focused && (first..first + rows).contains(&caret_row)).then(|| {
            let row = (caret_row - first) as u16;
            (editor.x + caret_column as u16, editor.y + row)
        });
        let selection = model.selection().filter(|_| self.preedit.is_empty());
        for (y, row) in (editor.y..).zip(first..first + rows) {
            let range = lines.get(row);
            let text = range.map_or("", |range| &input[range.clone()]);
            self.frame
                .text(editor.x, y, editor.cols, text, p.foreground);
            if let (Some(range), Some(selected)) = (range, &selection) {
                let from = selected.start.clamp(range.start, range.end);
                let to = selected.end.clamp(range.start, range.end);
                // A selected line break shows as one more selected cell.
                let newline = selected.start <= range.end && selected.end > range.end;
                if from < to || newline {
                    let x = editor.x + text_width(&input[range.start..from]) as u16;
                    let cols = text_width(&input[from..to]) + usize::from(newline);
                    let highlight = rect(x, y, cols as u16, 1);
                    self.frame.grid.recolor(highlight, p.selection_bg);
                }
            }
        }
        self.frame.hits.push((editor, AgentAction::Editor));
        let stop = editor.x + editor.cols - 3;
        if model.busy {
            self.frame.text(stop, editor.y, 3, " ■ ", p.error);
            self.frame.hit(stop, editor.y, 3, AgentAction::Stop);
        }
        if session.context.menu {
            let items = [
                ("Selection", AgentAction::Selection),
                ("Visible screen", AgentAction::Screen),
            ];
            for (y, (label, action)) in (editor.y.saturating_sub(3)..).zip(items) {
                let width = editor.cols.min(text_width(label) as u16 + 2);
                self.frame.text(
                    editor.x,
                    y,
                    editor.cols,
                    &format!(" {label} "),
                    p.foreground,
                );
                self.frame.hit(editor.x, y, width, action);
            }
        }
    }

    fn draw_preview(&mut self) {
        let p = self.palette;
        let area = self.frame.grid.rect;
        let body = crate::panel::popup_body(area);
        let preview = self.ui.agent.active_mut().context.preview.as_mut().unwrap();
        preview.prepare(usize::from(body.cols.max(1)));
        let rows = usize::from(body.rows);
        preview.scroll = preview.scroll.min(preview.lines.len().saturating_sub(rows));
        let ContextPreview {
            item,
            scroll,
            lines,
            ..
        } = preview;
        self.frame.grid = Grid::popup(area, p.foreground, p.bg, &item.label);
        let bg = mix_color(p.foreground, p.bg, MIX_POPUP);
        let (added, removed) = crate::panel::diff_colors(p.foreground);
        let text = &item.snapshot.text;
        for (y, range) in (body.y..).zip(lines.iter().skip(*scroll).take(rows)) {
            // Wrapped rows keep the color of the diff line they continue.
            let line = text[..range.start].rfind('\n').map_or(0, |at| at + 1);
            let color = match text.as_bytes()[line..].first() {
                _ if item.kind != "diff" => p.foreground,
                Some(b'+') => added,
                Some(b'-') => removed,
                Some(b'@') => mix_color(0x83b9d6, p.foreground, 128),
                _ => p.foreground,
            };
            self.frame
                .grid
                .text(body.x, y, body.cols, &text[range.clone()], color, bg);
        }
        let footer = area.y + area.rows.saturating_sub(2);
        let muted = mix_color(p.foreground, bg, MIX_MUTED);
        self.frame
            .grid
            .text(body.x, footer, body.cols, " ⧉   Esc close", muted, bg);
        let close = crate::panel::popup_close(area);
        self.frame
            .hit(close.x, close.y, close.cols, AgentAction::ClosePreview);
        self.frame.hit(body.x, footer, 3, AgentAction::CopyPreview);
        self.frame.preview = Some(body);
    }

    fn draw_agent_tabs(&mut self, editor: Rect) {
        let (side, p) = (self.side, self.palette);
        let (muted, line_bg) = (0x606060, 0x111111);
        let grid = &mut self.frame.grid;
        grid.text(side.x, side.y, side.cols, "", 0xcccccc, line_bg);
        grid.text(side.x, side.y, 1, "│", p.user_bg, p.bg);
        let mut x = editor.x;
        let agent = &self.ui.agent;
        for (index, column, width) in agent.tabs(usize::from(side.cols - 1)) {
            x = editor.x + column as u16;
            let width = width as u16;
            let active = index == agent.current;
            let hovered = self
                .pointer
                .is_some_and(|(column, row)| row == side.y && (x..x + width).contains(&column));
            let session = &agent.sessions[index];
            let status = if session.model.busy {
                WARNING
            } else if session.failed() {
                FAILED
            } else {
                ACCENT
            };
            let (tab_bg, title, close) = match (active, hovered) {
                (true, _) => (0x363636, 0xdddddd, 0xdddddd),
                (_, true) => (0x1b1b1b, 0xcccccc, muted),
                _ => (0x1b1b1b, muted, muted),
            };
            let grid = &mut self.frame.grid;
            grid.flags.set(Flags::UNDERLINE, active);
            grid.text(x, side.y, width, "", muted, tab_bg);
            grid.text(x + 1, side.y, 1, "•", status, tab_bg);
            grid.text(
                x + 3,
                side.y,
                width.saturating_sub(6),
                session.title(),
                title,
                tab_bg,
            );
            grid.text(x + width - 2, side.y, 1, "×", close, tab_bg);
            grid.flags.remove(Flags::UNDERLINE);
            if active {
                let row = &mut grid.rows[usize::from(side.y - grid.rect.y)];
                for column in x..x + width {
                    let cell = row.extra_mut(usize::from(column - grid.rect.x));
                    cell.underline_color = Some(rgb(0xcccccc));
                }
            }
            self.frame
                .hit(x, side.y, width, AgentAction::Session(index));
            let close = x + width.saturating_sub(3);
            self.frame
                .hit(close, side.y, 3, AgentAction::CloseSession(index));
            x += width;
        }
        self.frame
            .grid
            .text(x + 1, side.y, 2, "+", 0xcccccc, line_bg);
        self.frame.hit(x + 1, side.y, 2, AgentAction::NewSession);
        let close = side.x + side.cols - 3;
        self.frame
            .grid
            .text(close, side.y, 3, " › ", muted, line_bg);
        self.frame.hit(close, side.y, 3, AgentAction::ClosePanel);
    }
}

pub(crate) struct ContextPreview {
    pub item: ContextItem,
    pub scroll: usize,
    columns: usize,
    pub lines: Vec<Range<usize>>,
}

impl ContextPreview {
    fn prepare(&mut self, columns: usize) {
        if self.columns != columns {
            self.lines = wrap(&self.item.snapshot.text, columns);
            self.columns = columns;
        }
    }
}

#[derive(Default)]
pub(crate) struct ChatContext {
    pub basket: Vec<ContextItem>,
    pub menu: bool,
    pub preview: Option<ContextPreview>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AgentAction {
    #[default]
    Editor,
    Output,
    Stop,
    Tool(usize),
    Copy(usize),
    ContextMenu,
    Selection,
    Screen,
    Inspect(usize),
    Remove(usize),
    ClosePreview,
    CopyPreview,
    NewSession,
    Session(usize),
    CloseSession(usize),
    ClosePanel,
    Rewind(usize),
    ChangedFile(usize),
    EditDiff(usize),
    ToggleChanges,
}

pub(crate) enum AgentLine {
    Role(usize),
    Edit(usize),
    Text { message: usize, range: Range<usize> },
    Blank,
}

fn message_indent(message: &AgentMessage) -> u16 {
    match (message.role == "You", message.id.is_some()) {
        (true, _) => 1,
        (_, true) => 2,
        _ => 0,
    }
}

impl AgentLine {
    pub(crate) fn indent(&self, messages: &[AgentMessage]) -> Option<u16> {
        match self {
            Self::Role(_) => Some(3),
            Self::Text { message, .. } => Some(message_indent(&messages[*message])),
            Self::Blank | Self::Edit(_) => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TranscriptPoint {
    message: usize,
    part: u8,
    offset: usize,
}

struct ChatSelection {
    anchor: TranscriptPoint,
    head: TranscriptPoint,
    dragging: bool,
}

struct RenderedMarkdown {
    turn: u64,
    source: String,
    document: Markdown,
}

impl RenderedMarkdown {
    fn matches(&self, message: &AgentMessage) -> bool {
        message.role == "Agent"
            && message.id.is_none()
            && message.turn == self.turn
            && message.text.starts_with(&self.source)
    }
}

/// Replies of one conversation sent to the Markdown worker and back.
struct Render {
    session: u64,
    request: u64,
    replies: Vec<(usize, RenderedMarkdown)>,
}

fn markdown_worker() -> io::Result<Worker<Render, Render>> {
    let notification = Arc::new(crate::sys::new()?);
    Worker::new(
        "nextty-markdown",
        notification,
        |mut render: Render, publish| {
            for (_, reply) in &mut render.replies {
                reply.document = Markdown::render(&reply.source);
            }
            publish(render);
        },
    )
}

#[derive(PartialEq, Eq)]
enum RemoteState {
    Unopened,
    Loading,
    Ready,
}

pub(crate) struct AgentPanel {
    pub open: bool,
    #[cfg(feature = "gui")]
    pub width: Option<f32>,
    pub sessions: Vec<AgentSession>,
    pub current: usize,
    pub keyboard: AgentAction,
    // Where the last frame drew the controls, transcript rows and editor.
    hits: Vec<(Rect, AgentAction)>,
    pub chat: Rect,
    chat_rows: Vec<(usize, Rect)>,
    pub editor: Rect,
    /// A press in the editor drags its selection until released.
    pub selecting_editor: bool,
    /// A tool header pressed without dragging toggles on release.
    tool_click: Option<usize>,
    next_id: u64,
    remote: RemoteState,
    view_dirty: bool,
    markdown_worker: Option<Worker<Render, Render>>,
    next_render: u64,
    // The conversation and request id of the Markdown job in flight.
    rendering: Option<(u64, u64)>,
    // The previous frame's grid, handed back by the frontend for reuse.
    pub spare_grid: Option<Grid>,
}

impl Default for AgentPanel {
    fn default() -> Self {
        Self {
            open: false,
            #[cfg(feature = "gui")]
            width: None,
            sessions: vec![AgentSession::default()],
            current: 0,
            keyboard: AgentAction::Editor,
            hits: Vec::new(),
            chat: Rect::default(),
            chat_rows: Vec::new(),
            editor: Rect::default(),
            selecting_editor: false,
            tool_click: None,
            next_id: 1,
            remote: RemoteState::Unopened,
            view_dirty: true,
            markdown_worker: None,
            next_render: 0,
            rendering: None,
            spare_grid: None,
        }
    }
}

impl AgentPanel {
    fn apply_model(&mut self, patch: AgentModelPatch) -> io::Result<bool> {
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == patch.session) else {
            return Ok(false);
        };
        let stamp = session.selection_stamp();
        let redraw = session.model.restore_patch(&patch)?;
        if redraw {
            session.columns = 0;
            session.sync_rendered();
        }
        Ok(redraw | session.keep_selection(stamp))
    }

    fn restore_view_state(&mut self, drafts: AgentDrafts) {
        let current = self.active().id;
        let loading = self.remote == RemoteState::Loading;
        let order: Vec<u64> = drafts.sessions.iter().map(|draft| draft.id).collect();
        for draft in drafts.sessions {
            let index = self
                .sessions
                .iter()
                .position(|session| session.id == draft.id)
                .unwrap_or_else(|| {
                    self.sessions.push(AgentSession {
                        id: draft.id,
                        ..AgentSession::default()
                    });
                    self.sessions.len() - 1
                });
            self.view_dirty |= self.sessions[index].restore_snapshot(draft, loading);
        }
        if drafts.complete {
            // Sessions used before the first snapshot arrived are kept and published.
            let mut dirty = false;
            self.sessions.retain(|session| {
                let listed = order.contains(&session.id);
                let keep = listed
                    || loading
                        && (!session.model.input.is_empty()
                            || !session.context.basket.is_empty()
                            || !session.model.pending.is_empty());
                dirty |= keep && !listed;
                keep
            });
            self.view_dirty |= dirty;
            self.sessions.sort_by_key(|session| {
                order
                    .iter()
                    .position(|id| *id == session.id)
                    .unwrap_or(usize::MAX)
            });
        }
        self.current = self
            .sessions
            .iter()
            .position(|session| session.id == current)
            .unwrap_or(0);
        self.next_id = self.next_id.max(drafts.next_id);
    }

    pub fn active(&self) -> &AgentSession {
        &self.sessions[self.current]
    }
    pub fn active_mut(&mut self) -> &mut AgentSession {
        &mut self.sessions[self.current]
    }

    pub fn hit(&self, column: u16, row: u16) -> Option<AgentAction> {
        hit_at(&self.hits, column, row)
    }

    /// The transcript text nearest to column `x` of `row`.
    fn transcript_point(&self, x: f32, row: u16) -> Option<TranscriptPoint> {
        let rows = &self.chat_rows;
        let (line, rect) = rows
            .iter()
            .rfind(|(_, rect)| rect.y <= row)
            .or(rows.first())?;
        let column = (x - f32::from(rect.x)).clamp(0., f32::from(rect.cols));
        self.active().point(*line, column)
    }

    fn prepare_layout(&mut self, columns: usize) -> io::Result<()> {
        // Commit with wrapping during draw so input keeps the displayed frame's geometry.
        self.apply_markdown();
        self.request_markdown()?;
        self.active_mut().layout(columns);
        Ok(())
    }

    fn request_markdown(&mut self) -> io::Result<()> {
        if !self.open || self.rendering_active() {
            return Ok(());
        }
        let session = self.active();
        let replies: Vec<_> = session
            .model
            .messages
            .iter()
            .zip(&session.rendered)
            .enumerate()
            .filter(|(_, (message, rendered))| {
                message.role == "Agent"
                    && message.id.is_none()
                    && rendered
                        .as_ref()
                        .is_none_or(|rendered| rendered.source.len() != message.text.len())
            })
            .map(|(index, (message, _))| {
                let reply = RenderedMarkdown {
                    turn: message.turn,
                    source: message.text.clone(),
                    document: Markdown::default(),
                };
                (index, reply)
            })
            .collect();
        if replies.is_empty() {
            return Ok(());
        }
        if self.markdown_worker.is_none() {
            self.markdown_worker = Some(markdown_worker()?);
        }
        self.next_render += 1;
        let (session, request) = (self.active().id, self.next_render);
        self.rendering = Some((session, request));
        let render = Render {
            session,
            request,
            replies,
        };
        self.markdown_worker.as_ref().unwrap().submit(render);
        Ok(())
    }

    fn rendering_active(&self) -> bool {
        self.rendering
            .is_some_and(|(session, _)| session == self.active().id)
    }

    pub fn layout_fd(&self) -> RawFd {
        self.markdown_worker.as_ref().map_or(-1, Worker::fd)
    }

    pub fn poll_layout(&mut self) -> io::Result<bool> {
        let Some(worker) = &self.markdown_worker else {
            return Ok(false);
        };
        Ok(crate::sys::consume_notification(worker.fd())? && self.open && worker.ready())
    }

    fn apply_markdown(&mut self) {
        let Some(render) = self.markdown_worker.as_mut().and_then(Worker::take) else {
            return;
        };
        if self.rendering != Some((render.session, render.request)) {
            return;
        }
        self.rendering = None;
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == render.session) else {
            return;
        };
        // New deltas can arrive between parsing and drawing the next frame.
        let messages = &session.model.messages;
        if !render
            .replies
            .iter()
            .all(|(index, reply)| messages.get(*index).is_some_and(|m| reply.matches(m)))
        {
            return;
        }
        let stamp = session.selection_stamp();
        // A last reply whose text only grew needs just its last line wrapped again.
        session.append_from = None;
        if let [(index, reply)] = render.replies.as_slice()
            && *index + 1 == messages.len()
            && session.columns != 0
            && session.rendered[*index]
                .as_ref()
                .is_some_and(|old| reply.document.text.starts_with(&old.document.text))
        {
            session.append_from = session
                .lines
                .iter()
                .enumerate()
                .rev()
                .find_map(|(at, line)| match line {
                    AgentLine::Text { message, range } if message == index => {
                        Some((at, range.start))
                    }
                    _ => None,
                });
        }
        for (index, reply) in render.replies {
            session.rendered[index] = Some(reply);
        }
        session.keep_selection(stamp);
        if session.append_from.is_none() {
            session.columns = 0;
        }
    }

    fn leave(&mut self) {
        let session = self.active_mut();
        session.end_selection();
        session.rewind_target = None;
        self.keyboard = AgentAction::Editor;
    }

    fn push_session(&mut self) {
        self.sessions.push(AgentSession {
            id: self.next_id,
            ..AgentSession::default()
        });
        self.next_id += 1;
    }

    fn new_session(&mut self) {
        self.leave();
        self.push_session();
        self.view_dirty = true;
        self.current = self.sessions.len() - 1;
    }

    fn switch(&mut self, index: usize) {
        if index < self.sessions.len() {
            self.leave();
            self.current = index;
        }
    }

    pub fn close(&mut self, index: usize) {
        if index >= self.sessions.len() {
            return;
        }
        self.leave();
        self.sessions.remove(index);
        self.view_dirty = true;
        if self.sessions.is_empty() {
            self.push_session();
        }
        self.current = if index < self.current {
            self.current - 1
        } else {
            self.current.min(self.sessions.len() - 1)
        };
    }

    pub fn insert(&mut self, id: u64, text: &str) -> Result<(), String> {
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or("Conversation was closed; paste discarded")?;
        let model = &mut session.model;
        let replaced = model.selection().unwrap_or(model.cursor..model.cursor);
        session.edited |= model.input[replaced] != *text;
        model.insert(text)?;
        session.editor_scroll = None;
        Ok(())
    }

    pub fn clear_input(&mut self) {
        let session = self.active_mut();
        session.edited |= !session.model.input.is_empty();
        session.model.clear_input();
        session.prompt_index = None;
    }

    pub fn controls(&self) -> Vec<AgentAction> {
        let session = self.active();
        if session.context.preview.is_some() {
            return vec![AgentAction::CopyPreview, AgentAction::ClosePreview];
        }
        if session.context.menu {
            return vec![
                AgentAction::Selection,
                AgentAction::Screen,
                AgentAction::ContextMenu,
            ];
        }
        let mut actions = vec![AgentAction::Editor, AgentAction::Output];
        if session.model.busy {
            actions.push(AgentAction::Stop);
        }
        actions.push(AgentAction::ContextMenu);
        if !session.model.changes.is_empty() {
            actions.push(AgentAction::ToggleChanges);
            if session.changes_expanded {
                let files = session.changed_files();
                actions.extend(files.iter().map(|file| AgentAction::ChangedFile(file.0)));
            }
        }
        for index in 0..session.context.basket.len() {
            actions.extend([AgentAction::Inspect(index), AgentAction::Remove(index)]);
        }
        for (index, message) in session.model.messages.iter().enumerate().rev() {
            if message.role == "You" {
                actions.push(AgentAction::Rewind(index));
            }
            let Some(id) = &message.id else {
                actions.push(AgentAction::Copy(index));
                continue;
            };
            actions.push(AgentAction::Tool(index));
            actions.extend(
                session
                    .model
                    .changes
                    .iter()
                    .enumerate()
                    .filter(|(_, change)| &change.tool_id == id)
                    .map(|(index, _)| AgentAction::EditDiff(index)),
            );
        }
        actions.push(AgentAction::NewSession);
        actions.extend((0..self.sessions.len()).map(AgentAction::Session));
        actions.extend([
            AgentAction::CloseSession(self.current),
            AgentAction::ClosePanel,
        ]);
        actions
    }

    pub fn tabs(&self, columns: usize) -> Vec<(usize, usize, usize)> {
        let available = columns.saturating_sub(7);
        if available == 0 {
            return Vec::new();
        }
        let count = self.sessions.len().min((available / 7).max(1));
        let start = self
            .current
            .saturating_sub(count - 1)
            .min(self.sessions.len() - count);
        let desired: Vec<usize> = (start..start + count)
            .map(|index| {
                let mut width = 6;
                for ch in self.sessions[index].title().chars() {
                    width += usize::from(scalar_width(ch));
                    if width >= available {
                        break;
                    }
                }
                width.max(7)
            })
            .collect();
        // Short titles keep their width; longer ones share what is left.
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by_key(|&tab| desired[tab]);
        let mut widths = vec![0; count];
        let mut remaining = available;
        for (rank, tab) in order.into_iter().enumerate() {
            widths[tab] = desired[tab].min(remaining / (count - rank));
            remaining -= widths[tab];
        }
        widths
            .into_iter()
            .enumerate()
            .scan(0, |column, (offset, width)| {
                *column += width;
                Some((start + offset, *column - width, width))
            })
            .collect()
    }
}

// Control characters display as one blank cell.
fn cell_width(c: char) -> usize {
    usize::from(scalar_width(if c.is_control() { ' ' } else { c }))
}

/// Byte ranges of `text` wrapped to `columns`; hard line breaks are not part of any range.
fn wrap(text: &str, columns: usize) -> Vec<Range<usize>> {
    let columns = columns.max(1);
    let mut lines = Vec::new();
    let mut offset = 0;
    for part in text.split_inclusive('\n') {
        let line = part
            .strip_suffix('\n')
            .map_or(part, |line| line.strip_suffix('\r').unwrap_or(line));
        let end = offset + line.len();
        let mut start = offset;
        if line.is_ascii() {
            while end - start > columns {
                lines.push(start..start + columns);
                start += columns;
            }
        } else {
            let mut width = 0;
            for (index, c) in line.char_indices() {
                let index = offset + index;
                let next = cell_width(c);
                if width + next > columns && index > start {
                    lines.push(start..index);
                    start = index;
                    width = 0;
                }
                width += next;
            }
        }
        lines.push(start..end);
        offset += part.len();
    }
    if text.ends_with('\n') {
        lines.push(text.len()..text.len());
    }
    lines
}

fn column_offset(text: &str, range: Range<usize>, column: f32) -> usize {
    let line = &text[range.clone()];
    let end = column.round().min(line.len() as f32) as usize;
    // A zero-width scalar at the boundary still belongs to this column.
    if line.as_bytes().get(end).is_none_or(u8::is_ascii) && line.as_bytes()[..end].is_ascii() {
        return range.start + end;
    }
    let mut used = 0.;
    for (index, c) in line.char_indices() {
        let width = cell_width(c) as f32;
        if column < used + width * 0.5 {
            return range.start + index;
        }
        used += width;
    }
    range.end
}

fn editor_lines(
    input: &str,
    cursor: usize,
    columns: usize,
    rows: usize,
    scroll: Option<usize>,
) -> EditorLines {
    let columns = columns.max(1);
    let prefix = &input[..cursor];
    let mut lines = wrap(input, columns);
    let (caret_lines, caret) = if cursor == input.len() {
        (lines.len(), lines.last().cloned().unwrap_or(0..0))
    } else {
        let row = lines.partition_point(|range| range.end < cursor);
        // A caret inside CRLF falls between adjacent displayed ranges.
        let row = row.saturating_sub(usize::from(lines[row].start > cursor));
        let start = lines[row].start;
        let wrapped = wrap(&prefix[start..], columns);
        let last = wrapped.last().cloned().unwrap_or(0..0);
        (
            row + wrapped.len().max(1),
            start + last.start..start + last.end,
        )
    };
    let width = prefix[caret].chars().map(cell_width).sum::<usize>();
    let caret_row = caret_lines.saturating_sub(1) + usize::from(width >= columns);
    if caret_row >= lines.len() {
        lines.push(input.len()..input.len());
    }
    let rows = rows.max(1);
    let first = scroll.unwrap_or_else(|| caret_row.saturating_sub(rows - 1));
    EditorLines {
        first: first.min(lines.len().saturating_sub(rows)),
        lines,
        caret_row,
        caret_column: width % columns,
    }
}

pub(crate) struct EditorLines {
    pub lines: Vec<Range<usize>>,
    pub first: usize,
    pub caret_row: usize,
    pub caret_column: usize,
}

#[derive(Default)]
pub(crate) struct AgentSession {
    pub id: u64,
    pub model: AgentView,
    pub context: ChatContext,
    expanded_tools: HashSet<String>,
    changes_expanded: bool,
    pub lines: Vec<AgentLine>,
    pub columns: usize,
    append_from: Option<(usize, usize)>,
    rendered: Vec<Option<RenderedMarkdown>>,
    pub editor_scroll: Option<usize>,
    pub rewind_target: Option<u64>,
    edited: bool,
    escape_pending: bool,
    prompt_index: Option<usize>,
    prompt_draft: String,
    selection: Option<ChatSelection>,
}

impl AgentSession {
    /// Recorded edits grouped by path: the first record's index and the summed line counts.
    fn changed_files(&self) -> Vec<(usize, u32, u32)> {
        let mut positions = HashMap::new();
        let mut files: Vec<(usize, u32, u32)> = Vec::new();
        for (index, change) in self.model.changes.iter().enumerate() {
            let position = *positions.entry(&change.path).or_insert_with(|| {
                files.push((index, 0, 0));
                files.len() - 1
            });
            files[position].1 += change.added;
            files[position].2 += change.removed;
        }
        files
    }

    fn draft(&self) -> AgentDraft {
        AgentDraft {
            id: self.id,
            input: self.model.input.clone(),
            basket: self.context.basket.clone(),
        }
    }

    // Restores the daemon's draft; `merge` keeps what was typed before it arrived.
    fn restore_snapshot(&mut self, draft: AgentDraft, merge: bool) -> bool {
        let merge = merge && (!self.model.input.is_empty() || !self.context.basket.is_empty());
        if merge {
            self.restore_draft(draft.input, draft.basket);
        } else {
            if self.model.input != draft.input {
                self.model.input = draft.input;
                self.model.set_cursor(self.model.input.len(), false);
                self.editor_scroll = None;
            }
            self.context.basket = draft.basket;
        }
        merge
    }

    fn recover(&mut self, recovery: AgentRecovery) -> bool {
        let busy = std::mem::take(&mut self.model.busy);
        if recovery.rewind {
            self.model.input = recovery.input;
            self.model.set_cursor(self.model.input.len(), false);
            self.model.scroll = 0;
            self.context = ChatContext {
                basket: recovery.basket,
                ..ChatContext::default()
            };
            self.rewind_target = None;
        } else if recovery.input.is_empty() && recovery.basket.is_empty() {
            return busy;
        } else {
            self.restore_draft(recovery.input, recovery.basket);
        }
        self.editor_scroll = None;
        self.edited = true;
        true
    }

    fn failed(&self) -> bool {
        let messages = &self.model.messages;
        messages.last().is_some_and(|last| {
            messages
                .iter()
                .rev()
                .take_while(|message| message.turn == last.turn)
                .any(|message| matches!(message.role.as_str(), "Error" | "Blocked"))
        })
    }

    fn restore_draft(&mut self, mut input: String, mut items: Vec<ContextItem>) {
        if self.model.input.is_empty() {
            self.model.cursor = input.len();
            self.model.input = input;
        } else if !input.is_empty() {
            input.push('\n');
            let offset = input.len();
            input.push_str(&self.model.input);
            self.model.input = input;
            self.model.cursor += offset;
            self.model.anchor = self.model.anchor.map(|anchor| anchor + offset);
        }
        items.append(&mut self.context.basket);
        self.context.basket = items;
    }

    fn rewind_index(&self) -> Option<usize> {
        let turn = self.rewind_target?;
        self.model
            .messages
            .iter()
            .position(|message| message.turn == turn && message.role == "You")
    }

    fn line_of(&self, message: usize) -> Option<usize> {
        self.lines
            .iter()
            .position(|line| matches!(line, AgentLine::Text { message: at, .. } if *at == message))
    }

    // Scrolls so `line` is the newest visible row.
    fn reveal(&mut self, line: usize) {
        self.model.scroll = self.lines.len().saturating_sub(line + 1);
    }

    pub fn title(&self) -> &str {
        self.model
            .messages
            .iter()
            .find(|message| message.role == "You")
            .and_then(|message| message.text.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("New chat")
    }

    fn message_text(&self, index: usize) -> &str {
        let message = &self.model.messages[index];
        if message.role == "Agent" && message.id.is_none() {
            self.markdown(index).map_or("", |document| &document.text)
        } else {
            &message.text
        }
    }

    fn markdown(&self, index: usize) -> Option<&Markdown> {
        let rendered = self.rendered.get(index)?.as_ref()?;
        Some(&rendered.document)
    }

    // Drops rendered replies whose text was replaced rather than extended.
    fn sync_rendered(&mut self) {
        self.rendered
            .resize_with(self.model.messages.len(), || None);
        for (message, rendered) in self.model.messages.iter().zip(&mut self.rendered) {
            rendered.take_if(|old| !old.matches(message));
        }
    }

    fn line_text(&self, index: usize) -> Option<(TranscriptPoint, Range<usize>, &str)> {
        let (message, part, text, range) = match self.lines.get(index)? {
            AgentLine::Role(message) => {
                let role = self.model.messages.get(*message)?.role.as_str();
                (*message, 0, role, 0..role.len())
            }
            AgentLine::Text { message, range } => {
                self.model.messages.get(*message)?;
                (*message, 1, self.message_text(*message), range.clone())
            }
            AgentLine::Blank | AgentLine::Edit(_) => return None,
        };
        text.get(range.clone())?;
        let offset = range.start;
        let point = TranscriptPoint {
            message,
            part,
            offset,
        };
        Some((point, range, text))
    }

    fn point(&self, line: usize, column: f32) -> Option<TranscriptPoint> {
        let (mut point, range, text) = self.line_text(line)?;
        point.offset = column_offset(text, range, column);
        Some(point)
    }

    pub fn begin_selection(&mut self, point: TranscriptPoint, extend: bool) {
        self.rewind_target = None;
        let anchor = match &self.selection {
            Some(selection) if extend => selection.anchor,
            _ => point,
        };
        self.selection = Some(ChatSelection {
            anchor,
            head: point,
            dragging: true,
        });
    }

    pub fn extend_selection(&mut self, point: TranscriptPoint) {
        if let Some(selection) = &mut self.selection {
            selection.head = point;
        }
    }

    pub fn end_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.dragging = false;
        }
    }

    pub fn selecting(&self) -> bool {
        self.selection
            .as_ref()
            .is_some_and(|selection| selection.dragging)
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    // Identifies the text before both selection endpoints, so a stream update that
    // rewrites it can drop the selection.
    fn selection_stamp(&self) -> Option<u64> {
        let selection = self.selection.as_ref()?;
        let mut hasher = DefaultHasher::new();
        for point in [selection.anchor, selection.head] {
            let message = self.model.messages.get(point.message)?;
            let text = if point.part == 0 {
                &message.role
            } else {
                self.message_text(point.message)
            };
            (message.turn, &message.id, text.get(..point.offset)?).hash(&mut hasher);
        }
        Some(hasher.finish())
    }

    fn keep_selection(&mut self, before: Option<u64>) -> bool {
        let stale = before.is_some() && self.selection_stamp() != before;
        if stale {
            self.selection = None;
        }
        stale
    }

    /// The selected part of the line that starts at `first` and spans `range`.
    fn selected_range(&self, first: TranscriptPoint, range: Range<usize>) -> Option<Range<usize>> {
        let selection = self.selection.as_ref()?;
        let start = selection.anchor.min(selection.head);
        let end = selection.anchor.max(selection.head);
        let last = TranscriptPoint {
            offset: range.end,
            ..first
        };
        if start >= last || end <= first {
            return None;
        }
        let line = (first.message, first.part);
        let from = if (start.message, start.part) == line {
            start.offset.max(range.start)
        } else {
            range.start
        };
        let to = if (end.message, end.part) == line {
            end.offset.min(range.end)
        } else {
            range.end
        };
        (from < to).then_some(from..to)
    }

    fn selection_span(&self, line: usize) -> Option<(usize, &str)> {
        let (point, range, text) = self.line_text(line)?;
        let selected = self.selected_range(point, range.clone())?;
        let column = text_width(&text[range.start..selected.start]);
        Some((column, &text[selected]))
    }

    pub fn selected_text(&self) -> Option<String> {
        let mut output = String::new();
        let mut previous: Option<TranscriptPoint> = None;
        for index in 0..self.lines.len() {
            let Some((mut point, range, text)) = self.line_text(index) else {
                continue;
            };
            let Some(range) = self.selected_range(point, range) else {
                continue;
            };
            // Wrapped rows rejoin with the text between them; parts and messages with blank lines.
            match previous {
                Some(last) if (last.message, last.part) == (point.message, point.part) => {
                    output.push_str(&text[last.offset..range.start]);
                }
                Some(last) if last.message == point.message => output.push('\n'),
                Some(_) => output.push_str("\n\n"),
                None => (),
            }
            output.push_str(&text[range.clone()]);
            point.offset = range.end;
            previous = Some(point);
        }
        (!output.is_empty()).then_some(output)
    }

    pub fn editor_columns(&self, width: usize) -> usize {
        width
            .saturating_sub(3 * usize::from(self.model.busy))
            .max(1)
    }

    /// The lines of the editor drawn in `editor`, and how many rows it shows.
    fn editor_lines(&self, editor: Rect) -> (EditorLines, usize) {
        let rows = usize::from(editor.rows).max(1);
        let columns = self.editor_columns(usize::from(editor.cols));
        (self.editor_layout(columns, rows), rows)
    }

    pub fn editor_layout(&self, columns: usize, rows: usize) -> EditorLines {
        let model = &self.model;
        editor_lines(
            &model.input,
            model.cursor,
            columns,
            rows,
            self.editor_scroll,
        )
    }

    fn move_vertical(&mut self, layout: EditorLines, up: bool, extend: bool) {
        let row = if up {
            layout.caret_row.saturating_sub(1)
        } else {
            (layout.caret_row + 1).min(layout.lines.len() - 1)
        };
        let range = layout.lines[row].clone();
        let cursor = column_offset(&self.model.input, range, layout.caret_column as f32);
        self.model.set_cursor(cursor, extend);
    }

    pub fn select_editor(
        &mut self,
        mut layout: EditorLines,
        rows: usize,
        (column, row): (f32, f32),
        extend: bool,
    ) {
        if extend {
            if row < 0. {
                layout.first = layout.first.saturating_sub(1);
            } else if row >= rows as f32 {
                layout.first = (layout.first + 1).min(layout.lines.len().saturating_sub(rows));
            }
        }
        let row = (row as usize).min(rows.saturating_sub(1));
        let end = self.model.input.len();
        let range = layout.lines.get(layout.first + row).cloned();
        let cursor = column_offset(&self.model.input, range.unwrap_or(end..end), column);
        self.editor_scroll = Some(layout.first);
        self.model.set_cursor(cursor, extend);
    }

    fn toggle_tool(&mut self, index: usize) {
        self.clear_selection();
        let Some(id) = self.model.messages.get(index).and_then(|m| m.id.as_ref()) else {
            return;
        };
        if !self.expanded_tools.remove(id) {
            self.expanded_tools.insert(id.clone());
        }
        self.columns = 0;
    }

    fn send(&mut self, client: &Client) -> io::Result<()> {
        if self.model.busy || self.model.input.trim().is_empty() {
            return Ok(());
        }
        let pane = client
            .focused()
            .ok_or_else(|| io::Error::other("No workspace"))?;
        if !client.can_agent_input() {
            return Err(io::Error::other("Workspace is read-only"));
        }
        self.model.pending.push(AgentRequest::Send(AgentSend {
            session: self.id,
            pane,
            input: std::mem::take(&mut self.model.input),
            basket: std::mem::take(&mut self.context.basket),
        }));
        self.model.clear_input();
        self.model.busy = true;
        self.model.scroll = 0;
        self.clear_selection();
        self.editor_scroll = None;
        self.edited = true;
        Ok(())
    }

    fn recall_prompt(&mut self, up: bool) {
        let prompts: Vec<_> = self
            .model
            .messages
            .iter()
            .filter(|message| message.role == "You")
            .collect();
        let current = self.prompt_index.unwrap_or(prompts.len());
        if self.prompt_index.is_none() {
            self.prompt_draft = self.model.input.clone();
        }
        let index = if up {
            current.saturating_sub(1)
        } else {
            (current + 1).min(prompts.len())
        };
        let recalled = prompts.get(index).map(|message| message.text.clone());
        self.model.input = recalled.unwrap_or_else(|| self.prompt_draft.clone());
        self.prompt_index = (index < prompts.len()).then_some(index);
        let cursor = if up { 0 } else { self.model.input.len() };
        self.model.set_cursor(cursor, false);
        self.edited = true;
    }

    fn attach_screen(&mut self, client: &Client, pane: PaneId, selection: bool) -> io::Result<()> {
        let screen = client
            .screens()
            .get(&pane)
            .ok_or_else(|| io::Error::other("No active pane"))?;
        let (label, kind, range) = if selection {
            let range = screen
                .selection_range()
                .ok_or_else(|| io::Error::other("Select text in the terminal first"))?;
            ("Visible selection", "Selection", Some(range))
        } else {
            ("Visible screen", "Screen", None)
        };
        let item = ContextItem {
            label: label.into(),
            kind: kind.into(),
            source: context_source(client, pane),
            snapshot: context::snapshot(screen, range),
        };
        let basket = &mut self.context.basket;
        if basket.contains(&item) {
            return Err(io::Error::other("Already attached"));
        }
        let bytes: usize = basket.iter().map(|old| old.snapshot.text.len()).sum();
        if basket.len() >= 3 || bytes + item.snapshot.text.len() > 160 * 1024 {
            return Err(io::Error::other(
                "Context is full; remove an attachment first",
            ));
        }
        basket.push(item);
        self.edited = true;
        Ok(())
    }

    pub fn layout(&mut self, columns: usize) {
        let append = self.append_from.take().filter(|_| self.columns == columns);
        if self.columns == columns && append.is_none() {
            return;
        }
        let previous_lines = self.lines.len();
        // The newest visible row's text position keeps the view in place across reflow.
        let anchor = (self.model.scroll > 0)
            .then(|| {
                let last = self.lines.len().saturating_sub(self.model.scroll + 1);
                (0..=last).rev().find_map(|index| {
                    let (point, _, _) = self.line_text(index)?;
                    Some((point, last - index))
                })
            })
            .flatten();
        let mut lines = std::mem::take(&mut self.lines);
        if let Some((line, _)) = append {
            lines.truncate(line);
        } else {
            lines.clear();
            let messages = &self.model.messages;
            self.expanded_tools.retain(|id| {
                messages
                    .iter()
                    .any(|message| message.id.as_ref() == Some(id))
            });
        }
        let first = append.map_or(0, |_| self.model.messages.len() - 1);
        let offset = append.map_or(0, |(_, offset)| offset);
        let label = text_width(REWIND_LABEL);
        for (index, message) in self.model.messages.iter().enumerate().skip(first) {
            let tool = message.id.as_ref();
            if tool.is_some() {
                lines.push(AgentLine::Role(index));
            }
            if tool.is_none_or(|id| self.expanded_tools.contains(id)) {
                let reserved = match (message.role == "You", tool.is_some()) {
                    (true, _) if columns >= label + 5 => 1 + label,
                    (true, _) | (_, true) => 2,
                    _ => 0,
                };
                let text = &self.message_text(index)[offset..];
                let rows = wrap(text, columns.saturating_sub(reserved));
                lines.extend(rows.into_iter().map(|range| AgentLine::Text {
                    message: index,
                    range: offset + range.start..offset + range.end,
                }));
            }
            if let Some(id) = tool {
                let edits = self.model.changes.iter().enumerate();
                lines.extend(
                    edits
                        .filter(|(_, change)| &change.tool_id == id)
                        .map(|(change, _)| AgentLine::Edit(change)),
                );
            }
            lines.push(AgentLine::Blank);
        }
        self.lines = lines;
        if let Some((anchor, gap)) = anchor {
            let found = self.lines.iter().position(|line| match line {
                AgentLine::Role(message) => *message == anchor.message && anchor.part == 0,
                AgentLine::Text { message, range } => {
                    *message == anchor.message
                        && anchor.part == 1
                        && (range.contains(&anchor.offset)
                            || range.is_empty() && range.start == anchor.offset)
                }
                AgentLine::Blank | AgentLine::Edit(_) => false,
            });
            if let Some(index) = found {
                self.model.scroll = self.lines.len().saturating_sub(index + gap + 1);
            }
        } else if self.model.scroll > 0 {
            let added = self.lines.len().saturating_sub(previous_lines);
            self.model.scroll = self.model.scroll.saturating_add(added);
        }
        self.columns = columns;
    }
}

fn context_source(client: &Client, pane: PaneId) -> String {
    let name = client.snapshot().map_or("", |s| s.name.as_str());
    let alternate = client
        .screens()
        .get(&pane)
        .is_some_and(|screen| screen.mode().contains(Mode::ALT_SCREEN));
    let host = if client.is_remote() { "SSH" } else { "local" };
    let kind = if alternate { "TUI" } else { "visible" };
    format!("{name} · pane {pane} · {host} · {kind} snapshot")
}

impl Ui {
    fn agent_tab(&mut self, reverse: bool) {
        let actions = self.agent.controls();
        let current = match self.focus {
            Focus::Editor => AgentAction::Editor,
            Focus::Output => AgentAction::Output,
            _ => self.agent.keyboard,
        };
        let index = actions.iter().position(|action| *action == current);
        let step = if reverse { actions.len() - 1 } else { 1 };
        let next = actions[(index.unwrap_or(0) + step) % actions.len()];
        self.agent.keyboard = next;
        self.focus = match next {
            AgentAction::Editor => Focus::Editor,
            AgentAction::Output => Focus::Output,
            _ => Focus::Controls,
        };
        let session = self.agent.active_mut();
        let line = match next {
            AgentAction::Tool(index) | AgentAction::Copy(index) | AgentAction::Rewind(index) => {
                session.lines.iter().rposition(|line| match line {
                    AgentLine::Role(at) | AgentLine::Text { message: at, .. } => *at == index,
                    AgentLine::Blank | AgentLine::Edit(_) => false,
                })
            }
            AgentAction::EditDiff(index) => session
                .lines
                .iter()
                .position(|line| matches!(line, AgentLine::Edit(at) if *at == index)),
            _ => None,
        };
        if let Some(line) = line {
            session.reveal(line);
        }
        self.message(match next {
            AgentAction::Editor => "Agent input · Enter sends · Shift+Enter newline",
            AgentAction::Output => "Agent output · Ctrl+C clears input / stops",
            AgentAction::Stop => "Stop generation · Enter",
            AgentAction::ContextMenu => "Attach terminal context · Enter",
            AgentAction::Selection => "Attach visible selection · Enter",
            AgentAction::Screen => "Attach visible screen · Enter",
            AgentAction::Inspect(_) => "Inspect attachment · Enter",
            AgentAction::Remove(_) => "Remove attachment · Enter or Delete",
            AgentAction::Tool(_) => "Expand/collapse tool · Enter",
            AgentAction::Copy(_) => "Copy message · Enter",
            AgentAction::CopyPreview => "Copy snapshot · Enter",
            AgentAction::ClosePreview => "Close snapshot · Enter or Esc",
            AgentAction::NewSession => "New conversation · Enter",
            AgentAction::Session(_) => "Switch conversation · Enter",
            AgentAction::CloseSession(_) => "Close conversation · Enter",
            AgentAction::ClosePanel => "Collapse Agent panel · Enter",
            AgentAction::ChangedFile(_) => "Open recorded file diff · Enter",
            AgentAction::EditDiff(_) => "Open this edit diff · Enter",
            AgentAction::ToggleChanges => "Expand/collapse changed files · Enter",
            AgentAction::Rewind(_) => "History · j/k selects · Esc exits · Enter confirms",
        });
    }

    /// The action a key in the Agent controls triggers; Tab and arrows move between them.
    fn agent_key(&mut self, symbol: u32, shift: bool, control: bool) -> Option<AgentAction> {
        if matches!(symbol, keysym::TAB | keysym::LEFT_TAB) {
            self.agent_tab(shift || symbol == keysym::LEFT_TAB);
            return None;
        }
        if control && symbol == 32 {
            return Some(AgentAction::ContextMenu);
        }
        if self.focus != Focus::Controls {
            return None;
        }
        match (symbol, self.agent.keyboard) {
            // Without the menu, Escape reaches the history rewind first.
            (keysym::ESCAPE, _) => {
                self.agent.active_mut().context.menu = false;
                self.agent.keyboard = AgentAction::ContextMenu;
                None
            }
            (keysym::UP | keysym::DOWN, _) => {
                self.agent_tab(symbol == keysym::UP);
                None
            }
            (keysym::RETURN | keysym::KP_ENTER | 32, action) => Some(action),
            (keysym::DELETE, AgentAction::Inspect(index) | AgentAction::Remove(index)) => {
                Some(AgentAction::Remove(index))
            }
            _ => None,
        }
    }

    /// Runs `action`, reporting failures in the status line; returns text it copies.
    pub fn activate_agent(&mut self, action: AgentAction, client: &Client) -> Option<String> {
        if !matches!(action, AgentAction::Rewind(_)) {
            self.cancel_rewind();
        }
        if action != AgentAction::ContextMenu {
            self.agent.active_mut().context.menu = false;
        }
        if self.agent.remote != RemoteState::Ready
            && matches!(
                action,
                AgentAction::NewSession | AgentAction::CloseSession(_)
            )
        {
            self.message("Loading conversations; try again");
            return None;
        }
        match action {
            AgentAction::ClosePanel => {
                if self.agent.open {
                    self.toggle_agent();
                }
            }
            AgentAction::NewSession => {
                self.agent.new_session();
                self.focus = Focus::Editor;
            }
            AgentAction::Session(index) => {
                self.agent.switch(index);
                self.focus = Focus::Editor;
            }
            AgentAction::CloseSession(index) => {
                self.agent.close(index);
                self.focus = Focus::Editor;
            }
            AgentAction::Rewind(index) => self.select_agent_rewind(index),
            AgentAction::Editor => self.focus = Focus::Editor,
            AgentAction::Output => {
                self.agent.active_mut().clear_selection();
                self.focus = Focus::Output;
            }
            AgentAction::Stop => {
                let session = self.agent.active_mut();
                let id = session.id;
                session
                    .model
                    .pending
                    .push(AgentRequest::Abort { session: id });
            }
            AgentAction::ContextMenu => {
                let menu = !self.agent.active().context.menu;
                self.agent.active_mut().context.menu = menu;
                self.focus = Focus::Controls;
                self.agent.keyboard = if menu {
                    AgentAction::Selection
                } else {
                    AgentAction::ContextMenu
                };
            }
            AgentAction::Selection | AgentAction::Screen => {
                let selection = action == AgentAction::Selection;
                if let Some(pane) = client.focused()
                    && let Err(error) = self
                        .agent
                        .active_mut()
                        .attach_screen(client, pane, selection)
                {
                    self.message(error.to_string());
                    return None;
                }
                if !self.agent.open {
                    self.toggle_agent();
                }
                self.focus = Focus::Editor;
            }
            AgentAction::ToggleChanges => {
                let session = self.agent.active_mut();
                session.changes_expanded = !session.changes_expanded;
                self.agent.keyboard = AgentAction::ToggleChanges;
                self.focus = Focus::Controls;
            }
            AgentAction::EditDiff(index) | AgentAction::ChangedFile(index) => {
                let changes = &self.agent.active().model.changes;
                let path = changes.get(index)?.path.clone();
                let mut text = String::new();
                let mut incomplete = false;
                for (at, change) in changes.iter().enumerate() {
                    let shown = match action {
                        AgentAction::EditDiff(_) => at == index,
                        _ => change.path == path,
                    };
                    if !shown {
                        continue;
                    }
                    let (turn, added, removed) = (change.turn, change.added, change.removed);
                    writeln!(text, "Turn {turn} · +{added} -{removed}\n{}", change.diff).unwrap();
                    if change.truncated {
                        text.push_str("\n[Diff truncated]\n");
                        incomplete = true;
                    }
                    text.push('\n');
                }
                self.open_preview(ContextItem {
                    label: format!("{} · recorded Agent edits", path.display()),
                    source: "Agent edit_file".into(),
                    kind: "diff".into(),
                    snapshot: ContextSnapshot { text, incomplete },
                });
            }
            AgentAction::Inspect(index) => {
                if let Some(item) = self.agent.active().context.basket.get(index).cloned() {
                    self.open_preview(item);
                }
            }
            AgentAction::Remove(index) => {
                let session = self.agent.active_mut();
                if index < session.context.basket.len() {
                    session.context.basket.remove(index);
                    session.edited = true;
                }
                self.agent.keyboard = AgentAction::ContextMenu;
            }
            AgentAction::Tool(index) => {
                self.focus = Focus::Output;
                self.agent.active_mut().toggle_tool(index);
            }
            AgentAction::Copy(index) => {
                self.focus = Focus::Output;
                let messages = &self.agent.active().model.messages;
                return messages.get(index).map(|message| message.text.clone());
            }
            AgentAction::CopyPreview => {
                let preview = self.agent.active().context.preview.as_ref();
                return preview.map(|preview| preview.item.snapshot.text.clone());
            }
            AgentAction::ClosePreview => {
                self.agent.active_mut().context.preview = None;
                self.focus = Focus::Editor;
            }
        }
        None
    }

    /// A pointer event on the Agent panel, or any while the panel holds the pointer.
    pub(crate) fn agent_pointer(&mut self, client: &Client, pointer: Pointer) -> Effect {
        let Pointer {
            kind,
            modifiers,
            column,
            row,
            side,
            ..
        } = pointer;
        // The right half of a cell places the caret after its character.
        let x = f32::from(column) + if side == Side::Right { 0.5 } else { 0. };
        let shift = modifiers & SHIFT != 0;
        let agent = &mut self.agent;
        if agent.active().selecting() {
            let (chat, point) = (agent.chat, agent.transcript_point(x, row));
            let session = agent.active_mut();
            if kind != PointerKind::Release(LEFT) {
                // Dragging past the transcript scrolls it.
                if row < chat.y {
                    session.model.scroll = session.model.scroll.saturating_add(1);
                } else if row >= chat.y + chat.rows {
                    session.model.scroll = session.model.scroll.saturating_sub(1);
                }
                if let Some(point) = point {
                    session.extend_selection(point);
                }
                return Effect::None;
            }
            session.end_selection();
            let text = session.selected_text();
            let tool = agent.tool_click.take();
            return match (text, tool) {
                (Some(text), _) => Effect::Primary(text),
                (None, Some(index)) => {
                    self.activate_agent(AgentAction::Tool(index), client);
                    Effect::None
                }
                (None, None) => Effect::None,
            };
        }
        if agent.selecting_editor {
            if kind == PointerKind::Release(LEFT) {
                agent.selecting_editor = false;
                let model = &agent.active().model;
                let selected = model.selection().map(|range| model.input[range].to_owned());
                return selected.map_or(Effect::None, Effect::Primary);
            }
            self.editor_pointer(x, row, true);
            return Effect::None;
        }
        let hit = agent.hit(column, row);
        let button = match kind {
            PointerKind::Press(button) => button,
            PointerKind::Wheel(lines) => {
                let down = -(lines * WHEEL_LINES) as isize;
                let editor = agent.editor;
                let session = agent.active_mut();
                if let Some(preview) = &mut session.context.preview {
                    preview.scroll = preview.scroll.saturating_add_signed(down);
                } else if hit == Some(AgentAction::Editor) {
                    let (layout, rows) = session.editor_lines(editor);
                    let last = layout.lines.len().saturating_sub(rows);
                    session.editor_scroll =
                        Some(layout.first.saturating_add_signed(down).min(last));
                } else {
                    session.model.scroll = session.model.scroll.saturating_add_signed(-down);
                }
                return Effect::None;
            }
            PointerKind::Motion | PointerKind::Release(_) => return Effect::None,
        };
        if button == RIGHT
            && agent.chat.contains(column, row)
            && let Some(text) = agent.active().selected_text()
        {
            return Effect::Copy(text);
        }
        let action = match (button, hit) {
            (RIGHT, Some(AgentAction::Copy(index) | AgentAction::Tool(index))) => {
                AgentAction::Copy(index)
            }
            (RIGHT, Some(AgentAction::Editor)) => AgentAction::ContextMenu,
            (LEFT, Some(action @ (AgentAction::Copy(_) | AgentAction::Tool(_)))) => {
                self.cancel_rewind();
                self.focus = Focus::Output;
                if let Some(point) = self.agent.transcript_point(x, row) {
                    self.agent.active_mut().begin_selection(point, shift);
                    self.agent.tool_click = match action {
                        AgentAction::Tool(index) => Some(index),
                        _ => None,
                    };
                }
                return Effect::None;
            }
            (LEFT | MIDDLE, Some(AgentAction::Editor)) => {
                self.activate_agent(AgentAction::Editor, client);
                self.editor_pointer(x, row, shift);
                self.agent.selecting_editor = button == LEFT;
                return match button {
                    MIDDLE => Effect::Paste(PasteTarget::Agent(self.agent.active().id), true),
                    _ => Effect::None,
                };
            }
            (LEFT, Some(action)) => action,
            _ => return Effect::None,
        };
        self.activate_agent(action, client)
            .map_or(Effect::None, Effect::Copy)
    }

    /// Moves the editor's caret to column `x` of `row`, extending its selection when `extend`.
    fn editor_pointer(&mut self, x: f32, row: u16, extend: bool) {
        let editor = self.agent.editor;
        let session = self.agent.active_mut();
        let (layout, rows) = session.editor_lines(editor);
        let columns = session.editor_columns(usize::from(editor.cols));
        let column = (x - f32::from(editor.x)).clamp(0., columns as f32);
        let row = f32::from(row) - f32::from(editor.y);
        session.select_editor(layout, rows, (column, row), extend);
    }

    fn open_preview(&mut self, item: ContextItem) {
        self.agent.active_mut().context.preview = Some(ContextPreview {
            item,
            scroll: 0,
            columns: 0,
            lines: Vec::new(),
        });
        self.agent.keyboard = AgentAction::CopyPreview;
        self.focus = Focus::Controls;
    }

    fn select_agent_rewind(&mut self, index: usize) {
        let session = self.agent.active_mut();
        let Some(message) = session
            .model
            .messages
            .get(index)
            .filter(|m| m.role == "You")
        else {
            return;
        };
        session.rewind_target = Some(message.turn);
        session.clear_selection();
        if let Some(line) = session.line_of(index) {
            session.reveal(line);
        }
        self.agent.keyboard = AgentAction::Rewind(index);
        self.focus = Focus::Controls;
    }

    fn move_agent_rewind(&mut self, up: bool) {
        let session = self.agent.active();
        let messages = &session.model.messages;
        let current = session.rewind_index();
        let user = |message: &AgentMessage| message.role == "You";
        let index = if up {
            messages[..current.unwrap_or(messages.len())]
                .iter()
                .rposition(user)
        } else {
            current.and_then(|at| Some(at + 1 + messages[at + 1..].iter().position(user)?))
        };
        if let Some(index) = index {
            self.select_agent_rewind(index);
        }
    }

    /// Exchanges drafts, requests and conversation updates with the workspace daemon;
    /// returns whether to redraw.
    pub fn poll_agent(&mut self, client: &mut Client, config: &Config) -> io::Result<bool> {
        if client.snapshot().is_none() {
            return Ok(false);
        }
        let agent = &mut self.agent;
        if agent.remote == RemoteState::Unopened {
            if !agent.open && agent.sessions.iter().all(|s| s.model.pending.is_empty()) {
                return Ok(false);
            }
            client.send(Action::AgentOpen)?;
            agent.remote = RemoteState::Loading;
        }
        let (mut changed, mut messages, mut models) = (false, false, false);
        while let Some(event) = client.next_agent_event() {
            match event {
                AgentEvent::Draft(drafts) => {
                    agent.view_dirty = false;
                    agent.restore_view_state(drafts);
                    changed = true;
                }
                AgentEvent::Model(patch) => {
                    let redraw = agent.apply_model(patch)?;
                    changed |= redraw;
                    messages |= redraw;
                    models = true;
                }
                AgentEvent::Recovery(recovery) => {
                    let id = recovery.session;
                    if let Some(session) = agent.sessions.iter_mut().find(|s| s.id == id) {
                        changed |= session.recover(recovery);
                    }
                }
                AgentEvent::Start => crate::agent::owner::spawn(client, config)?,
                AgentEvent::Ready => agent.remote = RemoteState::Ready,
                AgentEvent::Request(_) => unreachable!("owner event"),
            }
        }
        if models {
            agent.request_markdown()?;
        }
        // The transcript's places on screen are unknown until it is drawn again.
        if messages {
            let transcript = |action: &AgentAction| {
                matches!(
                    action,
                    AgentAction::Copy(_) | AgentAction::Tool(_) | AgentAction::Rewind(_)
                )
            };
            agent.hits.retain(|(_, action)| !transcript(action));
            agent.chat_rows.clear();
            agent.tool_click = None;
        }
        if agent.remote != RemoteState::Ready {
            return Ok(changed);
        }
        let writable = client.can_agent_input();
        if writable {
            let complete = std::mem::take(&mut agent.view_dirty);
            let sessions: Vec<_> = agent
                .sessions
                .iter_mut()
                .filter(|session| complete || session.edited)
                .map(|session| {
                    session.edited = false;
                    session.draft()
                })
                .collect();
            if complete || !sessions.is_empty() {
                let next_id = agent.next_id;
                client.send(Action::AgentView(AgentDrafts {
                    next_id,
                    complete,
                    sessions,
                }))?;
            }
        }
        let mut rejected = false;
        for session in &mut agent.sessions {
            for request in std::mem::take(&mut session.model.pending) {
                if writable {
                    client.send(Action::AgentRequest(request))?;
                } else {
                    rejected = true;
                    if let AgentRequest::Send(send) = request {
                        session.restore_draft(send.input, send.basket);
                        session.model.busy = false;
                        session.edited = true;
                    }
                }
            }
        }
        if rejected {
            self.message("Workspace is read-only");
        }
        Ok(changed || rejected)
    }

    pub fn cancel_rewind(&mut self) {
        let session = self.agent.active_mut();
        session.escape_pending = false;
        if session.rewind_target.take().is_some() {
            self.agent.keyboard = AgentAction::Editor;
            if self.focus.is_agent() {
                self.focus = Focus::Editor;
            }
        }
    }

    fn confirm_agent_rewind(&mut self) {
        let turn = self.agent.active().rewind_target;
        self.cancel_rewind();
        self.focus = Focus::Editor;
        if let Some(turn) = turn {
            let session = self.agent.active_mut();
            let request = AgentRequest::Rewind {
                session: session.id,
                turn,
            };
            session.model.pending.push(request);
        }
    }

    /// Escape twice selects history to rewind; j/k move the selection and Enter confirms it.
    fn agent_rewind_key(&mut self, symbol: u32) -> bool {
        if !self.focus.is_agent() {
            self.cancel_rewind();
            return false;
        }
        let session = self.agent.active();
        if !self.agent.open || session.context.menu || session.context.preview.is_some() {
            return false;
        }
        if session.rewind_target.is_some() {
            match symbol {
                keysym::ESCAPE => self.cancel_rewind(),
                keysym::RETURN | keysym::KP_ENTER => self.confirm_agent_rewind(),
                0x6b => self.move_agent_rewind(true),
                0x6a => self.move_agent_rewind(false),
                _ => (),
            }
            return true;
        }
        let double = symbol == keysym::ESCAPE && session.escape_pending;
        self.agent.active_mut().escape_pending = symbol == keysym::ESCAPE && !double;
        if double {
            self.move_agent_rewind(true);
        }
        symbol == keysym::ESCAPE
    }

    /// Handles a key aimed at the Agent panel; `None` passes it to the next region.
    pub(crate) fn agent_key_input(
        &mut self,
        client: &mut Client,
        key: &crate::input::Key<'_>,
    ) -> io::Result<Option<crate::input::KeyResult>> {
        use crate::input::KeyResult;
        let (control, shift, letter) = (key.control(), key.shift(), key.letter());
        let step = match key.symbol {
            keysym::PAGE_UP | keysym::PAGE_DOWN => 10,
            _ => 1,
        };
        let up = matches!(key.symbol, keysym::UP | keysym::PAGE_UP);
        let down = matches!(key.symbol, keysym::DOWN | keysym::PAGE_DOWN);
        if control && letter == Some('c') && self.agent.open && self.focus.is_agent() {
            let session = self.agent.active_mut();
            session.escape_pending = false;
            if !session.model.input.is_empty() {
                self.agent.clear_input();
            } else if session.model.busy {
                self.activate_agent(AgentAction::Stop, client);
            }
            return Ok(Some(KeyResult::Handled));
        }
        let activate = |ui: &mut Self, client: &mut Client| {
            let Some(action) = ui.agent_key(key.symbol, shift, control) else {
                return Ok(Some(KeyResult::Handled));
            };
            let copy = ui.activate_agent(action, client);
            Ok(Some(copy.map_or(KeyResult::Handled, KeyResult::Copy)))
        };
        let tab = matches!(key.symbol, keysym::TAB | keysym::LEFT_TAB);
        if let Some(preview) = &mut self.agent.active_mut().context.preview {
            let enter = matches!(key.symbol, keysym::RETURN | keysym::KP_ENTER | 32);
            if tab || self.focus == Focus::Controls && enter {
                return activate(self, client);
            }
            if key.symbol == keysym::ESCAPE {
                self.agent.active_mut().context.preview = None;
            } else if up {
                preview.scroll = preview.scroll.saturating_sub(step);
            } else if down {
                preview.scroll = preview.scroll.saturating_add(step);
            }
            return Ok(Some(KeyResult::Handled));
        }
        if self.search.open || self.resources.open && self.focus == Focus::Monitor {
            return Ok(None);
        }
        if self.agent_rewind_key(key.symbol) {
            return Ok(Some(KeyResult::Handled));
        }
        if !(self.agent.open && self.focus.is_agent()) {
            return Ok(None);
        }
        if tab || control && key.symbol == 32 || self.focus == Focus::Controls {
            return activate(self, client);
        }
        let editor = self.agent.editor;
        let session = self.agent.active_mut();
        if self.focus == Focus::Output {
            if key.symbol == keysym::END {
                session.model.scroll = 0;
            } else if up {
                session.model.scroll = session.model.scroll.saturating_add(step);
            } else if down {
                session.model.scroll = session.model.scroll.saturating_sub(step);
            }
            return Ok(Some(KeyResult::Handled));
        }
        if !(control && matches!(letter, Some('h' | 'j' | 'k' | 'l'))) {
            session.prompt_index = None;
        }
        if key.paste() {
            return Ok(Some(KeyResult::Paste));
        }
        match (control, letter, key.symbol) {
            (true, Some('a'), _) => session.model.select_all(),
            (true, Some('x'), _) => {
                if let Some(range) = session.model.selection() {
                    let text = session.model.input[range].to_owned();
                    self.insert_active("");
                    return Ok(Some(KeyResult::Copy(text)));
                }
            }
            (true, Some('u'), _) => self.agent.clear_input(),
            (true, Some('h'), _) => session.model.key(keysym::LEFT, false),
            (true, Some('l'), _) => session.model.key(keysym::RIGHT, false),
            (true, Some(letter @ ('j' | 'k')), _) => {
                let up = letter == 'k';
                let (layout, _) = session.editor_lines(editor);
                let edge = if up { 0 } else { layout.lines.len() - 1 };
                if layout.caret_row == edge {
                    session.recall_prompt(up);
                } else {
                    session.move_vertical(layout, up, false);
                }
            }
            (true, _, _) => (),
            // Without the menu, Escape reaches the history rewind first.
            (_, _, keysym::ESCAPE) => session.context.menu = false,
            (_, _, keysym::RETURN | keysym::KP_ENTER) if shift => self.insert_active("\n"),
            (_, _, keysym::RETURN | keysym::KP_ENTER) => {
                if let Err(error) = session.send(client) {
                    self.message(format!("Agent: {error}"));
                }
            }
            (_, _, keysym::UP | keysym::DOWN) => {
                let (layout, _) = session.editor_lines(editor);
                session.move_vertical(layout, key.symbol == keysym::UP, shift);
            }
            _ => {
                let length = session.model.input.len();
                session.model.key(key.symbol, shift);
                session.edited |= session.model.input.len() != length;
                if !key.text.is_empty() {
                    self.insert_active(key.text);
                }
            }
        }
        self.agent.active_mut().editor_scroll = None;
        Ok(Some(KeyResult::Handled))
    }

    fn insert_active(&mut self, text: &str) {
        if let Err(error) = self.agent.insert(self.agent.active().id, text) {
            self.message(error);
        }
    }

    pub fn toggle_agent(&mut self) {
        self.cancel_rewind();
        self.agent.open = !self.agent.open;
        let context = &mut self.agent.active_mut().context;
        context.menu = false;
        context.preview = None;
        self.focus = if self.agent.open {
            Focus::Editor
        } else if self.files_open {
            Focus::Files
        } else {
            Focus::Terminal
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl AgentPanel {
        // Tests edit messages in place instead of applying owner patches.
        fn wait_layout(&mut self, columns: usize) {
            self.active_mut().sync_rendered();
            self.prepare_layout(columns).unwrap();
            while self.rendering_active() {
                let mut fd = libc::pollfd {
                    fd: self.layout_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
                self.poll_layout().unwrap();
                self.prepare_layout(columns).unwrap();
            }
        }

        fn model_snapshot(&self) -> Vec<AgentModelPatch> {
            self.sessions
                .iter()
                .map(|session| AgentModelPatch {
                    session: session.id,
                    busy: session.model.busy,
                    messages: session.model.messages.clone(),
                    ..AgentModelPatch::default()
                })
                .collect()
        }

        fn restore_models(&mut self, patches: Vec<AgentModelPatch>) -> io::Result<bool> {
            let mut redraw = false;
            for patch in patches {
                redraw |= self.apply_model(patch)?;
            }
            self.request_markdown()?;
            Ok(redraw)
        }
    }

    #[test]
    fn selection_replacement_and_deletion_preserve_unicode_boundaries() {
        let mut agent = AgentView::default();
        agent.insert("a中文z").unwrap();
        agent.set_cursor(1, false);
        agent.set_cursor(7, true);
        assert_eq!(&agent.input[agent.selection().unwrap()], "中文");
        agent.insert("新").unwrap();
        assert_eq!(agent.input, "a新z");
        assert_eq!(agent.cursor, 4);
        assert_eq!(agent.selection(), None);
        agent.key(keysym::BACKSPACE, false);
        assert_eq!(agent.input, "az");
        agent.select_all();
        agent.key(keysym::DELETE, false);
        assert!(agent.input.is_empty());
        assert_eq!(agent.cursor, 0);
        assert_eq!(agent.anchor, None);
    }

    #[test]
    fn editor_control_navigation_recalls_prompts_and_clear_precedes_stop() {
        let path = std::env::temp_dir().join(format!("nextty-editor-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path).unwrap();
        let (_stream, _) = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        ui.focus = Focus::Editor;
        ui.agent
            .active_mut()
            .model
            .messages
            .push(message(1, "You", "previous", None));
        ui.agent.active_mut().model.insert("draft").unwrap();
        ui.agent.editor = rect(0, 0, 40, 8);
        let mut press = |ui: &mut Ui, symbol, modifiers| {
            let key = crate::input::Key {
                symbol,
                modifiers,
                text: "",
            };
            ui.agent_key_input(&mut client, &key).unwrap();
        };
        press(&mut ui, 'h' as u32, crate::input::CONTROL);
        assert_eq!(ui.agent.active().model.cursor, 4);
        press(&mut ui, 'l' as u32, crate::input::CONTROL);
        assert_eq!(ui.agent.active().model.cursor, 5);
        press(&mut ui, 'k' as u32, crate::input::CONTROL);
        assert_eq!(ui.agent.active().model.input, "previous");
        press(&mut ui, 'j' as u32, crate::input::CONTROL);
        assert_eq!(ui.agent.active().model.input, "draft");
        press(&mut ui, keysym::RETURN, crate::input::SHIFT);
        assert_eq!(ui.agent.active().model.input, "draft\n");
        ui.agent.active_mut().model.busy = true;
        press(&mut ui, 'c' as u32, crate::input::CONTROL);
        assert!(ui.agent.active().model.input.is_empty());
        assert!(ui.agent.active().model.busy);
        assert!(ui.agent.active().model.pending.is_empty());
        press(&mut ui, 'c' as u32, crate::input::CONTROL);
        assert!(matches!(
            ui.agent.active().model.pending.last().unwrap(),
            AgentRequest::Abort { .. }
        ));
    }

    #[test]
    fn double_escape_selects_history_and_jk_navigate_without_changing_draft() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        ui.focus = Focus::Editor;
        ui.agent.active_mut().model.input = "untouched draft".into();
        for turn in 1..=3 {
            ui.agent
                .active_mut()
                .model
                .messages
                .push(crate::workspace::AgentMessage {
                    turn,
                    role: "You".into(),
                    text: format!("message {turn}"),
                    id: None,
                });
        }
        assert!(ui.agent_rewind_key(keysym::ESCAPE));
        assert_eq!(ui.agent.active().rewind_target, None);
        assert!(ui.agent_rewind_key(keysym::ESCAPE));
        assert_eq!(ui.agent.active().rewind_target, Some(3));
        assert!(ui.agent_rewind_key('k' as u32));
        assert_eq!(ui.agent.active().rewind_target, Some(2));
        assert!(ui.agent_rewind_key('k' as u32));
        assert_eq!(ui.agent.active().rewind_target, Some(1));
        assert!(ui.agent_rewind_key('k' as u32));
        assert_eq!(ui.agent.active().rewind_target, Some(1));
        assert_eq!(ui.agent.active().model.input, "untouched draft");
        assert_eq!(ui.agent.active().model.messages.len(), 3);
        assert!(!ui.agent.active().model.busy);
        assert!(ui.agent_rewind_key('j' as u32));
        assert_eq!(ui.agent.active().rewind_target, Some(2));
        assert!(ui.agent_rewind_key(keysym::ESCAPE));
        assert_eq!(ui.agent.active().rewind_target, None);
        assert!(ui.focus == Focus::Editor);
    }

    #[test]
    fn streamed_unicode_geometry_matches_full_layout() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        for text in [
            "Earlier reply\n\nkept for selection",
            "中文🙂 e\u{301} ".repeat(8).as_str(),
        ] {
            panel
                .active_mut()
                .model
                .messages
                .push(message(1, "Agent", text, None));
        }
        panel.wait_layout(12);
        let session = panel.active_mut();
        session.begin_selection(session.point(0, 0.).unwrap(), false);
        session.extend_selection(session.point(0, 7.).unwrap());
        session.end_selection();
        session.model.scroll = 3;
        let rows = |session: &AgentSession| {
            session
                .lines
                .iter()
                .map(|line| match line {
                    AgentLine::Role(message) => ('r', *message, 0..0),
                    AgentLine::Text { message, range } => ('t', *message, range.clone()),
                    AgentLine::Blank => ('b', 0, 0..0),
                    AgentLine::Edit(index) => ('e', *index, 0..0),
                })
                .collect::<Vec<_>>()
        };
        for delta in [
            "追加",
            "e\u{301}🙂",
            "\n\n**bold",
            "**\n\nlast",
            "\r\r*",
            "*",
            "\n\n- a\n*",
            "*",
            "\n\n`a\n****",
            "`",
        ] {
            panel.active_mut().model.messages[1].text.push_str(delta);
            panel.wait_layout(12);
            let session = panel.active_mut();
            let full = crate::agent::markdown::Markdown::render(&session.model.messages[1].text);
            assert_eq!(session.message_text(1), full.text);
            let streamed = rows(session);
            let scroll = session.model.scroll;
            session.columns = 0;
            session.layout(12);
            assert_eq!(streamed, rows(session));
            assert_eq!(scroll, session.model.scroll);
            assert_eq!(session.selected_text().as_deref(), Some("Earlier"));
        }
    }

    #[test]
    fn transcript_copy_preserves_unselected_hard_line_gaps() {
        let mut session = AgentSession::default();
        session
            .model
            .messages
            .push(message(1, "You", "ab\r\ncd\n\nef", None));
        session.layout(80);
        for (from, to, expected) in [
            ((0, 0), (0, 0), None),
            ((0, 2), (1, 0), None),
            ((0, 2), (1, 1), Some("c")),
            ((0, 0), (1, 0), Some("ab")),
            ((0, 1), (1, 1), Some("b\r\nc")),
            ((1, 2), (3, 0), None),
            ((1, 1), (3, 1), Some("d\n\ne")),
        ] {
            let from = session.point(from.0, from.1 as f32).unwrap();
            let to = session.point(to.0, to.1 as f32).unwrap();
            for (anchor, head) in [(from, to), (to, from)] {
                session.begin_selection(anchor, false);
                session.extend_selection(head);
                assert_eq!(session.selected_text().as_deref(), expected);
            }
        }
    }

    #[test]
    fn transcript_selection_preserves_unicode_wraps_and_visible_tool_headers() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.active_mut().model.messages = vec![
            message(1, "You", "a中🙂e\u{301}\nnext", None),
            message(1, "run · done", "hidden tool output", Some("tool")),
            message(1, "Agent", "answer", None),
        ];
        panel.wait_layout(6);
        let session = panel.active_mut();
        let start = session.point(0, 1.).unwrap();
        let last = session
            .lines
            .iter()
            .rposition(|line| matches!(line, AgentLine::Text { message: 2, .. }))
            .unwrap();
        let end = session.point(last, 99.).unwrap();
        session.begin_selection(start, false);
        session.extend_selection(end);
        assert_eq!(session.selection_span(0), Some((1, "中")));
        let expected = "中🙂e\u{301}\nnext\n\nrun · done\n\nanswer";
        assert_eq!(session.selected_text().as_deref(), Some(expected));
        session.end_selection();
        session.layout(12);
        assert_eq!(session.selected_text().as_deref(), Some(expected));
        session.begin_selection(end, false);
        session.extend_selection(start);
        assert_eq!(session.selected_text().as_deref(), Some(expected));
    }

    #[test]
    fn streamed_models_keep_selection_when_selected_text_is_unchanged() {
        let mut owner = AgentPanel::default();
        owner.active_mut().model.messages = vec![
            message(1, "Agent", "keep this\n\n**draft", None),
            message(1, "run · running", "running", Some("cmd")),
        ];
        let mut mirror = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        let session = mirror.active_mut();
        session.begin_selection(session.point(0, 0.).unwrap(), false);
        session.extend_selection(session.point(0, 4.).unwrap());
        assert_eq!(session.selected_text().as_deref(), Some("keep"));

        owner.active_mut().model.messages[0].text.push_str("**");
        owner.active_mut().model.messages[1].text = "done".into();
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        let session = mirror.active_mut();
        assert!(session.selecting());
        assert_eq!(session.selected_text().as_deref(), Some("keep"));
        session.end_selection();

        owner.active_mut().model.messages[0]
            .text
            .push_str(" and more");
        mirror.restore_models(owner.model_snapshot()).unwrap();
        mirror.wait_layout(30);
        assert_eq!(mirror.active().selected_text().as_deref(), Some("keep"));

        owner.active_mut().model.messages[0].text = "replaced".into();
        mirror.restore_models(owner.model_snapshot()).unwrap();
        assert!(mirror.active().selected_text().is_none());
        mirror.wait_layout(30);
        assert!(mirror.active().selected_text().is_none());
    }

    #[test]
    fn streaming_markdown_reformatting_clears_stale_selection_offsets() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel
            .active_mut()
            .model
            .messages
            .push(message(1, "Agent", "**中文", None));
        panel.wait_layout(20);
        let session = panel.active_mut();
        session.begin_selection(session.point(0, 2.).unwrap(), false);
        session.extend_selection(session.point(0, 4.).unwrap());
        panel.active_mut().model.messages[0].text.push_str("**");
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "中文");
        assert_eq!(panel.active().selected_text(), None);
    }

    fn message(turn: u64, role: &str, text: impl Into<String>, id: Option<&str>) -> AgentMessage {
        AgentMessage {
            turn,
            role: role.into(),
            text: text.into(),
            id: id.map(Into::into),
        }
    }

    fn change(
        turn: u64,
        tool_id: &str,
        added: u32,
        removed: u32,
        diff: &str,
    ) -> crate::workspace::AgentChange {
        crate::workspace::AgentChange {
            turn,
            tool_id: tool_id.into(),
            path: "/project/a.rs".into(),
            added,
            removed,
            diff: diff.into(),
            truncated: false,
        }
    }

    #[test]
    fn recorded_edits_group_by_file_and_open_single_or_grouped_diffs() {
        let path = std::env::temp_dir().join(format!("nextty-diff-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let client = Client::connect(&path).unwrap();
        let (_stream, _) = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        let mut ui = Ui::new().unwrap();
        let model = &mut ui.agent.active_mut().model;
        model.changes = vec![
            change(1, "one", 1, 1, "-old\n+new\n"),
            change(2, "two", 2, 0, "+second\n"),
        ]
        .into();
        model.messages = vec![
            message(1, "edit_file · done", "text", Some("one")),
            message(2, "edit_file · done", "text", Some("two")),
        ];
        assert_eq!(ui.agent.active().changed_files(), vec![(0, 3, 1)]);
        let controls = ui.agent.controls();
        assert!(controls.contains(&AgentAction::ToggleChanges));
        assert!(!controls.contains(&AgentAction::ChangedFile(0)));
        assert!(controls.contains(&AgentAction::EditDiff(0)));
        assert!(controls.contains(&AgentAction::EditDiff(1)));
        ui.activate_agent(AgentAction::EditDiff(0), &client);
        let diff = |ui: &Ui| {
            ui.agent
                .active()
                .context
                .preview
                .as_ref()
                .unwrap()
                .item
                .snapshot
                .text
                .clone()
        };
        assert!(diff(&ui).contains("-old") && !diff(&ui).contains("+second"));
        ui.activate_agent(AgentAction::ChangedFile(0), &client);
        assert!(diff(&ui).contains("-old") && diff(&ui).contains("+second"));
    }

    #[test]
    fn restoring_a_draft_keeps_text_typed_while_loading_and_recovery_prepends_failed_sends() {
        let mut panel = AgentPanel {
            remote: RemoteState::Loading,
            ..AgentPanel::default()
        };
        panel.active_mut().model.insert("typed").unwrap();
        panel.restore_view_state(AgentDrafts {
            next_id: 1,
            complete: true,
            sessions: vec![AgentDraft {
                id: 0,
                input: "saved".into(),
                basket: Vec::new(),
            }],
        });
        assert_eq!(panel.active().model.input, "saved\ntyped");
        assert_eq!(panel.active().model.cursor, "saved\ntyped".len());
        assert!(panel.view_dirty);
        panel.active_mut().model.busy = true;
        assert!(panel.active_mut().recover(AgentRecovery {
            input: "failed".into(),
            ..AgentRecovery::default()
        }));
        assert_eq!(panel.active().model.input, "failed\nsaved\ntyped");
        assert!(!panel.active().model.busy && panel.active().edited);
    }

    #[test]
    fn delayed_paste_keeps_its_conversation_and_never_targets_a_replacement_tab() {
        let mut panel = AgentPanel::default();
        let target = panel.active().id;
        panel.new_session();
        panel.insert(target, "captured paste").unwrap();
        assert_eq!(panel.sessions[0].model.input, "captured paste");
        assert!(panel.active().model.input.is_empty());
        panel.close(0);
        panel.new_session();
        assert!(panel.insert(target, "late paste").is_err());
        assert!(
            panel
                .sessions
                .iter()
                .all(|session| session.id != target && session.model.input.is_empty())
        );
    }

    #[test]
    fn wrapped_ranges_preserve_controls_blank_lines_and_unicode_columns() {
        for (text, expected) in [
            ("ab\tcd\r\n\nx\u{7f}y\n", vec!["ab ", "cd", "", "x y", ""]),
            ("a中b\nc\u{301}🙂d", vec!["a中", "b", "c\u{301}🙂", "d"]),
            ("\r", vec![" "]),
        ] {
            let lines: Vec<String> = wrap(text, 3)
                .into_iter()
                .map(|range| {
                    text[range]
                        .chars()
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect()
                })
                .collect();
            assert_eq!(lines, expected);
        }
        assert!(wrap("", 3).is_empty());
    }

    #[test]
    fn history_anchor_survives_reflow_and_collapsed_tool_append() {
        let mut session = AgentSession::default();
        session
            .model
            .messages
            .push(message(1, "You", "0123456789".repeat(50), None));
        session.layout(20);
        session.model.scroll = 10;
        let last = session.lines.len() - session.model.scroll - 1;
        let anchor = (0..=last)
            .rev()
            .find_map(|index| session.line_text(index).map(|(_, range, _)| range.start))
            .unwrap();
        session.layout(40);
        let last = session.lines.len() - session.model.scroll - 1;
        let range = session.line_text(last).unwrap().1;
        assert!(range.contains(&anchor));
        session.model.messages.push(message(
            2,
            "shell · done",
            "hidden output".repeat(500),
            Some("tool"),
        ));
        session.columns = 0;
        session.layout(40);
        let last = session.lines.len() - session.model.scroll - 1;
        assert!(session.line_text(last).unwrap().1.contains(&anchor));
    }
}
