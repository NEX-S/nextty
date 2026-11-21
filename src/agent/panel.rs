use crate::agent::markdown::{self, Markdown};
use crate::config::Config;
use crate::context::{self, ContextItem, ContextSnapshot};
use crate::core::{Flags, Mode, WindowSize};
use crate::input::keysym;
use crate::panel::{Grid, mix_color, rect, rgb};
use crate::ui::{Focus, Ui};
use crate::width::{scalar_width, text_width};
use crate::worker::Worker;
use crate::workspace::{
    AgentChange, AgentDraft, AgentDrafts, AgentEvent, AgentMessage, AgentModelPatch, AgentRecovery,
    AgentRequest, AgentSend, Client, PaneId, Rect,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::ops::Range;
use std::os::fd::RawFd;
use std::sync::Arc;

const INPUT_LIMIT: usize = 64 * 1024;

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
    pub revision: u64,
    pub pending: Vec<AgentRequest>,
}

impl AgentView {
    pub fn restore_patch(&mut self, patch: &AgentModelPatch) -> io::Result<bool> {
        let start = patch.start as usize;
        let busy = patch.busy
            || self
                .pending
                .iter()
                .any(|request| matches!(request, AgentRequest::Send(_)));
        let changes_changed = self.changes_omitted != patch.changes_omitted
            || patch
                .changes
                .as_ref()
                .is_some_and(|changes| changes != &self.changes);
        let redraw = changes_changed
            || self.busy != busy
            || self.messages.len() != start + patch.messages.len()
            || patch.messages.iter().enumerate().any(|(index, new)| {
                self.messages.get(start + index).is_none_or(|old| {
                    old.turn != new.turn
                        || old.role != new.role
                        || old.id != new.id
                        || if index == 0 && patch.prefix_bytes != 0 {
                            new.role != "Agent"
                                || new.id.is_some()
                                || patch.prefix_bytes as usize != old.text.len()
                        } else {
                            old.text != new.text
                                && (new.role != "Agent"
                                    || new.id.is_some()
                                    || !new.text.starts_with(&old.text))
                        }
                })
            });
        patch.apply_messages(&mut self.messages)?;
        if changes_changed && let Some(changes) = &patch.changes {
            self.changes.clone_from(changes);
        }
        self.changes_omitted = patch.changes_omitted;
        self.busy = busy;
        self.revision = patch.revision;
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
        let before = || {
            self.input
                .floor_char_boundary(self.cursor.saturating_sub(1))
        };
        let after = || self.input.ceil_char_boundary(self.cursor + 1);
        let selection = self.selection();
        let offset = match symbol {
            keysym::LEFT => {
                if !shift && let Some(range) = selection {
                    range.start
                } else {
                    before()
                }
            }
            keysym::RIGHT => {
                if !shift && let Some(range) = selection {
                    range.end
                } else {
                    after()
                }
            }
            keysym::HOME => self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1),
            keysym::END => self.input[self.cursor..]
                .find('\n')
                .map_or(self.input.len(), |offset| self.cursor + offset),
            keysym::BACKSPACE | keysym::DELETE => {
                let range = selection.unwrap_or_else(|| {
                    if symbol == keysym::BACKSPACE {
                        before()..self.cursor
                    } else {
                        self.cursor..after()
                    }
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

pub(crate) struct Panel {
    pub grid: Grid,
    pub backdrop: Option<Grid>,
    pub hits: Vec<(Rect, AgentAction)>,
    pub chat: Rect,
    pub chat_rows: Vec<(usize, Rect)>,
    #[cfg(feature = "gui")]
    pub editor: Rect,
    pub preview: Option<Rect>,
    pub cursor: Option<(u16, u16)>,
    bg: u32,
}

impl Panel {
    fn text(&mut self, x: u16, y: u16, columns: u16, text: &str, foreground: u32) {
        self.grid.text(x, y, columns, text, foreground, self.bg);
    }

    fn hit(&mut self, x: u16, y: u16, cols: u16, action: AgentAction) {
        self.hits.push((rect(x, y, cols, 1), action));
    }
}

pub(crate) fn editor(ui: &Ui, side: Rect) -> (Rect, EditorLines) {
    let session = ui.agent.active();
    let max_rows = usize::from((side.rows / 3).clamp(1, 8));
    let columns = session.editor_columns(usize::from(side.cols.saturating_sub(2)));
    let layout = session.editor_layout(columns, max_rows);
    let rows = layout.lines.len().max(3).min(max_rows) as u16;
    (
        rect(
            side.x + 1,
            (side.y + side.rows).saturating_sub(rows),
            side.cols.saturating_sub(2),
            rows,
        ),
        layout,
    )
}

pub(crate) fn draw(
    ui: &mut Ui,
    config: &Config,
    side: Rect,
    size: WindowSize,
    pointer: Option<(u16, u16)>,
    preedit: &str,
) -> io::Result<Panel> {
    let backdrop = if let Some(preview) = ui.agent.active_mut().context.preview.take() {
        let base = draw(ui, config, side, size, None, "");
        ui.agent.active_mut().context.preview = Some(preview);
        Some(base?.grid)
    } else {
        None
    };
    let (editor, editor_layout) = editor(ui, side);
    let rect = if ui.agent.active().context.preview.is_some() {
        crate::file_panel::preview_rect(rect(0, 1, size.num_cols, size.num_lines.saturating_sub(2)))
    } else {
        side
    };
    let (foreground, bg) = (config.foreground, config.background);
    let grid = match ui.agent.spare_grid.take() {
        Some(mut grid) => {
            grid.reset(rect, foreground, bg);
            grid
        }
        None => Grid::new(rect, foreground, bg),
    };
    let mut painter = AgentPainter {
        ui,
        side,
        pointer,
        frame: Panel {
            backdrop,
            grid,
            hits: Vec::new(),
            chat: Rect::default(),
            chat_rows: Vec::new(),
            #[cfg(feature = "gui")]
            editor,
            preview: None,
            cursor: None,
            bg,
        },
        preedit,
        palette: Palette {
            bg,
            foreground,
            muted: mix_color(foreground, bg, 180),
            user_bg: mix_color(foreground, bg, 24),
            selection_bg: mix_color(foreground, bg, 70),
            error: mix_color(0xe87575, foreground, 128),
        },
    };
    painter.draw_agent(editor, editor_layout)?;
    Ok(painter.frame)
}

struct AgentPainter<'a> {
    ui: &'a mut Ui,
    side: Rect,
    pointer: Option<(u16, u16)>,
    frame: Panel,
    preedit: &'a str,
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
    palette: Palette,
) {
    let (added_color, removed_color) = crate::panel::diff_colors(palette.foreground);
    let (plus, minus) = (format!(" +{added}"), format!(" -{removed}"));
    let counts = (plus.len() + minus.len()).min(usize::from(cols)) as u16;
    let plus_width = (plus.len() as u16).min(counts);
    let path_width = cols - counts;
    let bg = palette.bg;
    let path = crate::width::fit_middle(path, path_width);
    grid.text(x, y, path_width, &path, palette.foreground, bg);
    let x = x + path_width;
    grid.text(x, y, plus_width, &plus, added_color, bg);
    grid.text(
        x + plus_width,
        y,
        counts - plus_width,
        &minus,
        removed_color,
        bg,
    );
}

impl AgentPainter<'_> {
    fn recolor_text(&mut self, rect: Rect, color: u32) {
        let grid = &mut self.frame.grid;
        let row = &mut grid.rows[usize::from(rect.y - grid.rect.y)];
        let start = usize::from(rect.x.saturating_sub(grid.rect.x));
        let end = (start + usize::from(rect.cols)).min(row.cells.len());
        for cell in &mut row.cells[start..end] {
            cell.fg = rgb(color);
        }
    }

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
            let attachments = session.context.basket.len();
            let attachment_rows = (attachments as u16)
                .min(3)
                .min(editor.y.saturating_sub(side.y + 6));
            let attachment_first = match self.ui.agent.keyboard {
                AgentAction::Inspect(index) | AgentAction::Remove(index) => {
                    index.saturating_sub(usize::from(attachment_rows.saturating_sub(1)))
                }
                _ => attachments.saturating_sub(usize::from(attachment_rows)),
            };
            let rewind = session.rewind_target.is_some();
            let hint_rows = if rewind { 3 } else { 0 };
            let changes = session.changed_files();
            let change_rows = if session.changes_expanded {
                (changes.len() as u16).min(4).min(
                    editor
                        .y
                        .saturating_sub(side.y + 9 + attachment_rows + hint_rows),
                )
            } else {
                0
            };
            let change_height = change_rows + u16::from(!changes.is_empty());
            let chat_end = editor
                .y
                .saturating_sub(1 + attachment_rows + hint_rows + change_height);
            self.draw_transcript(editor, side.y + 1, chat_end)?;
            if rewind {
                self.draw_rewind(editor);
            }
            self.draw_attachments(editor, chat_end, attachment_rows, attachment_first);
            if !changes.is_empty() {
                self.draw_changes(editor, chat_end + attachment_rows, change_rows, &changes);
            }
            let row = &mut self.frame.grid.rows[usize::from(editor.y - side.y - 1)];
            row.cells.fill(crate::core::Cell {
                c: '─',
                fg: rgb(p.user_bg),
                bg: rgb(p.bg),
                flags: Flags::empty(),
            });
            row.cells[0].c = '├';
            row.used = row.cells.len();
            self.draw_editor(editor, editor_layout);
        }
        let hovered = self
            .pointer
            .and_then(|(x, y)| crate::ui::agent_hit(&self.frame.hits, x, y));
        let focused = (self.ui.focus == Focus::Controls).then_some(self.ui.agent.keyboard);
        let hits = std::mem::take(&mut self.frame.hits);
        for &(rect, action) in &hits {
            if hovered != Some(action) && focused != Some(action) {
                continue;
            }
            match action {
                AgentAction::Session(_)
                | AgentAction::Editor
                | AgentAction::Output
                | AgentAction::Copy(_) => (),
                AgentAction::NewSession | AgentAction::ClosePanel => {
                    self.recolor_text(rect, p.foreground)
                }
                AgentAction::CloseSession(_) => self.recolor_text(rect, 0xef5350),
                _ => {
                    self.frame.grid.recolor(rect, p.user_bg);
                    if action == AgentAction::Stop {
                        self.recolor_text(rect, p.error);
                    }
                }
            }
        }
        self.frame.hits = hits;
        Ok(())
    }

    fn draw_changes(&mut self, editor: Rect, top: u16, rows: u16, files: &[(usize, u32, u32)]) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let (added, removed) = files
            .iter()
            .fold((0, 0), |(a, r), entry| (a + entry.1, r + entry.2));
        let omitted = match session.model.changes_omitted {
            0 => String::new(),
            count => format!(" · {count} earlier edits omitted"),
        };
        draw_edit_row(
            &mut self.frame.grid,
            (editor.x + 3, top, editor.cols.saturating_sub(3)),
            &format!("{} files{omitted}", files.len()),
            (added, removed),
            p,
        );
        let marker = crate::panel::expand_icon(session.changes_expanded);
        self.frame
            .text(editor.x, top, 3, &format!(" {marker} "), p.muted);
        self.frame
            .hit(editor.x, top, editor.cols, AgentAction::ToggleChanges);
        let first = match self.ui.agent.keyboard {
            AgentAction::ChangedFile(index) => files
                .iter()
                .position(|entry| entry.0 == index)
                .unwrap_or(0)
                .saturating_sub(usize::from(rows.saturating_sub(1))),
            _ => files.len().saturating_sub(usize::from(rows)),
        };
        for (row, &(index, added, removed)) in
            files.iter().skip(first).take(usize::from(rows)).enumerate()
        {
            let y = top + 1 + row as u16;
            draw_edit_row(
                &mut self.frame.grid,
                (editor.x, y, editor.cols),
                &session.model.changes[index].path.to_string_lossy(),
                (added, removed),
                p,
            );
            self.frame
                .hit(editor.x, y, editor.cols, AgentAction::ChangedFile(index));
        }
    }

    fn draw_transcript(&mut self, editor: Rect, chat_top: u16, chat_end: u16) -> io::Result<()> {
        let p = self.palette;
        let rows = usize::from(chat_end.saturating_sub(chat_top));
        self.ui.agent.prepare_layout(usize::from(editor.cols))?;
        let session = self.ui.agent.active_mut();
        let total = session.lines.len();
        if let Some(line) = session
            .rewind_index()
            .and_then(|message| session.line_of(message))
        {
            let first = total.saturating_sub(rows + session.model.scroll);
            if line < first {
                session.model.scroll = total.saturating_sub(rows + line);
            } else if line >= first + rows {
                session.model.scroll = total.saturating_sub(line + 1);
            }
        }
        session.model.scroll = session.model.scroll.min(total.saturating_sub(rows));
        let first = total.saturating_sub(rows + session.model.scroll);
        let session = self.ui.agent.active();
        self.frame.chat = rect(editor.x, chat_top, editor.cols, rows as u16);
        for (index, line) in session.lines.iter().enumerate().skip(first).take(rows) {
            let y = chat_top + (index - first) as u16;
            let indent = line.indent(&session.model.messages);
            if let Some(indent) = indent {
                self.frame.chat_rows.push((
                    index,
                    rect(editor.x + indent, y, editor.cols.saturating_sub(indent), 1),
                ));
            }
            match line {
                AgentLine::Blank => (),
                AgentLine::Edit(change) => {
                    let change = *change;
                    let record = &session.model.changes[change];
                    draw_edit_row(
                        &mut self.frame.grid,
                        (editor.x + 2, y, editor.cols.saturating_sub(2)),
                        &record.path.to_string_lossy(),
                        (record.added, record.removed),
                        p,
                    );
                    self.frame
                        .hit(editor.x, y, editor.cols, AgentAction::EditDiff(change));
                }
                AgentLine::Role(message) => {
                    let role = &session.model.messages[*message];
                    let marker = if role
                        .id
                        .as_ref()
                        .is_some_and(|id| session.expanded_tools.contains(id))
                    {
                        "▾"
                    } else {
                        "›"
                    };
                    self.frame.text(
                        editor.x,
                        y,
                        editor.cols,
                        &format!(" {marker} {} ", role.role),
                        p.muted,
                    );
                    self.frame.hit(
                        editor.x,
                        y,
                        editor.cols.min(text_width(&role.role) as u16 + 4),
                        AgentAction::Tool(*message),
                    );
                }
                AgentLine::Text { message, range } => {
                    let message = *message;
                    let record = &session.model.messages[message];
                    let user = record.role == "You";
                    let rewinding = user && session.rewind_target == Some(record.turn);
                    let message_bg = match (rewinding, user) {
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
                    let indent = message_indent(record);
                    let full = session.message_text(message);
                    let text = &full[range.clone()];
                    self.frame
                        .grid
                        .text(editor.x, y, editor.cols, "", p.foreground, message_bg);
                    if let Some(document) = session.markdown(message) {
                        let mut x = editor.x + indent;
                        for (span, style) in document.spans(range.clone()) {
                            let fragment = &full[span];
                            let columns = text_width(fragment) as u16;
                            self.frame
                                .grid
                                .flags
                                .set(Flags::BOLD, style & markdown::BOLD != 0);
                            self.frame
                                .grid
                                .flags
                                .set(Flags::ITALIC, style & markdown::ITALIC != 0);
                            self.frame.grid.text(
                                x,
                                y,
                                columns,
                                fragment,
                                markdown::color(style, color),
                                if style & markdown::CODE != 0 {
                                    p.user_bg
                                } else {
                                    message_bg
                                },
                            );
                            x += columns;
                        }
                        self.frame.grid.flags.remove(Flags::BOLD | Flags::ITALIC);
                    } else {
                        self.frame.grid.text(
                            editor.x + indent,
                            y,
                            editor.cols.saturating_sub(indent),
                            text,
                            color,
                            message_bg,
                        );
                    }
                    self.frame
                        .hit(editor.x, y, editor.cols, AgentAction::Copy(message));
                    let label_width = text_width(REWIND_LABEL);
                    if user
                        && usize::from(editor.cols) >= label_width + 5
                        && (index + 1 == first + rows
                            || !matches!(
                                session.lines.get(index + 1),
                                Some(AgentLine::Text { message: next, .. }) if *next == message
                            ))
                    {
                        let width = label_width as u16;
                        let x = editor.x + editor.cols.saturating_sub(width);
                        let hovered = self.pointer.is_some_and(|(column, row)| {
                            row == y && column >= x && column < x + width
                        });
                        self.frame.grid.text(
                            x,
                            y,
                            width,
                            REWIND_LABEL,
                            if hovered { p.foreground } else { p.muted },
                            message_bg,
                        );
                        self.frame.hit(x, y, width, AgentAction::Rewind(message));
                    }
                }
            }
            if let Some(indent) = indent
                && let Some((column, selected)) = session.selection_span(index)
            {
                self.frame.grid.text(
                    editor.x + indent + column as u16,
                    y,
                    text_width(selected) as u16,
                    selected,
                    p.foreground,
                    p.selection_bg,
                );
            }
        }
        Ok(())
    }

    fn draw_rewind(&mut self, editor: Rect) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let label = session.rewind_index().map_or(String::new(), |index| {
            let messages = &session.model.messages[index..];
            format!(
                "Rewind {} turns: {}",
                messages.iter().filter(|m| m.role == "You").count(),
                messages[0].text.lines().next().unwrap_or("")
            )
        });
        let top = editor.y.saturating_sub(4);
        for (row, (text, color)) in [
            ("j/k select · Enter confirm · Esc exits", p.muted),
            (label.as_str(), p.foreground),
            ("Commands and file changes stay", p.foreground),
        ]
        .into_iter()
        .enumerate()
        {
            self.frame
                .text(editor.x, top + row as u16, editor.cols, text, color);
        }
    }

    fn draw_attachments(&mut self, editor: Rect, top: u16, rows: u16, first: usize) {
        let p = self.palette;
        let basket = &self.ui.agent.active().context.basket;
        let remove = editor.x + editor.cols.saturating_sub(3);
        for (row, (index, item)) in basket
            .iter()
            .enumerate()
            .skip(first)
            .take(usize::from(rows))
            .enumerate()
        {
            let y = top + row as u16;
            self.frame.text(
                editor.x,
                y,
                editor.cols.saturating_sub(3),
                &format!(" +{} ", item.label),
                p.foreground,
            );
            self.frame.text(remove, y, 3, " × ", p.muted);
            self.frame.hit(
                editor.x,
                y,
                editor
                    .cols
                    .saturating_sub(3)
                    .min(text_width(&item.label) as u16 + 3),
                AgentAction::Inspect(index),
            );
            self.frame.hit(remove, y, 3, AgentAction::Remove(index));
        }
    }

    fn draw_editor(&mut self, editor: Rect, layout: EditorLines) {
        let p = self.palette;
        let session = self.ui.agent.active();
        let model = &session.model;
        let composing = self.ui.focus == Focus::Editor && !self.preedit.is_empty();
        let display;
        let (input, layout) = if composing {
            display = [
                &model.input[..model.cursor],
                self.preedit,
                &model.input[model.cursor..],
            ]
            .concat();
            let layout = editor_lines(
                &display,
                model.cursor + self.preedit.len(),
                session.editor_columns(usize::from(editor.cols)),
                usize::from(editor.rows),
                None,
            );
            (display.as_str(), layout)
        } else {
            (model.input.as_str(), layout)
        };
        let EditorLines {
            lines,
            first,
            caret_row,
            caret_column,
        } = layout;
        self.frame.cursor = (self.ui.focus == Focus::Editor
            && (first..first + usize::from(editor.rows)).contains(&caret_row))
        .then(|| {
            (
                editor.x + caret_column as u16,
                editor.y + (caret_row - first) as u16,
            )
        });
        let selection = model.selection().filter(|_| self.preedit.is_empty());
        for row in 0..editor.rows {
            let y = editor.y + row;
            let range = lines.get(first + usize::from(row));
            self.frame.text(
                editor.x,
                y,
                editor.cols,
                range.map_or("", |range| &input[range.clone()]),
                p.foreground,
            );
            if let Some(range) = range
                && let Some(selected) = &selection
            {
                let from = selected.start.clamp(range.start, range.end);
                let to = selected.end.clamp(range.start, range.end);
                // A selected line break shows as one extra selected cell.
                let newline = selected.start <= range.end && selected.end > range.end;
                if from < to || newline {
                    let cols = text_width(&input[from..to]) + usize::from(newline);
                    self.frame.grid.recolor(
                        rect(
                            editor.x + text_width(&input[range.start..from]) as u16,
                            y,
                            cols as u16,
                            1,
                        ),
                        p.selection_bg,
                    );
                }
            }
        }
        self.frame.hits.push((editor, AgentAction::Editor));
        let stop = editor.x + editor.cols.saturating_sub(3);
        if model.busy {
            self.frame.text(stop, editor.y, 3, " ■ ", p.error);
            self.frame.hit(stop, editor.y, 3, AgentAction::Stop);
        }
        if session.context.menu {
            for (row, (label, action)) in [
                ("Selection", AgentAction::Selection),
                ("Visible screen", AgentAction::Screen),
            ]
            .into_iter()
            .enumerate()
            {
                let y = editor.y.saturating_sub(3) + row as u16;
                self.frame.text(
                    editor.x,
                    y,
                    editor.cols,
                    &format!(" {label} "),
                    p.foreground,
                );
                self.frame.hit(
                    editor.x,
                    y,
                    editor.cols.min(text_width(label) as u16 + 2),
                    action,
                );
            }
        }
    }

    fn draw_preview(&mut self) {
        let p = self.palette;
        let rect = self.frame.grid.rect;
        let Some(preview) = &mut self.ui.agent.active_mut().context.preview else {
            return;
        };
        let body = crate::panel::popup_body(rect);
        let rows = usize::from(body.rows);
        preview.prepare(usize::from(body.cols.max(1)));
        let ContextPreview {
            item,
            scroll,
            lines,
            ..
        } = preview;
        *scroll = (*scroll).min(lines.len().saturating_sub(rows));
        self.frame.grid = Grid::popup(rect, p.foreground, p.bg, &item.label);
        let bg = mix_color(p.foreground, p.bg, 9);
        let muted = mix_color(p.foreground, bg, 176);
        let (added_color, removed_color) = crate::panel::diff_colors(p.foreground);
        let text = &item.snapshot.text;
        for (row, range) in lines.iter().skip(*scroll).take(rows).enumerate() {
            let color = if item.kind == "diff" {
                match text.as_bytes()[text[..range.start].rfind('\n').map_or(0, |at| at + 1)..]
                    .first()
                {
                    Some(b'+') => added_color,
                    Some(b'-') => removed_color,
                    Some(b'@') => mix_color(0x83b9d6, p.foreground, 128),
                    _ => p.foreground,
                }
            } else {
                p.foreground
            };
            self.frame.grid.text(
                body.x,
                body.y + row as u16,
                body.cols,
                &text[range.clone()],
                color,
                bg,
            );
        }
        let footer = rect.y + rect.rows.saturating_sub(2);
        self.frame
            .grid
            .text(body.x, footer, body.cols, " ⧉   Esc close", muted, bg);
        let close = crate::panel::popup_close(rect);
        self.frame
            .hit(close.x, close.y, close.cols, AgentAction::ClosePreview);
        self.frame.hit(
            body.x,
            footer,
            text_width(" ⧉ ") as u16,
            AgentAction::CopyPreview,
        );
        self.frame.preview = Some(body);
    }

    fn tab_icon(&mut self, rect: Rect, label: &str, colors: (u32, u32), background: u32) {
        let hovered = self.pointer.is_some_and(|(x, y)| rect.contains(x, y));
        let foreground = if hovered { colors.1 } else { colors.0 };
        let grid = &mut self.frame.grid;
        grid.text(rect.x, rect.y, rect.cols, "", foreground, background);
        grid.text(rect.x + 1, rect.y, 1, label, foreground, background);
    }

    fn draw_agent_tabs(&mut self, editor: Rect) {
        let (side, p) = (self.side, self.palette);
        let (muted, line_bg) = (0x606060, 0x111111);
        self.frame
            .grid
            .text(side.x, side.y, side.cols, "", 0xcccccc, line_bg);
        self.frame.text(side.x, side.y, 1, "│", p.user_bg);
        let mut x = editor.x;
        for (index, column, columns) in self.ui.agent.tabs(usize::from(side.cols.saturating_sub(1)))
        {
            x = editor.x + column as u16;
            let width = columns as u16;
            let active = index == self.ui.agent.current;
            let hovered = self
                .pointer
                .is_some_and(|(column, row)| row == side.y && (x..x + width).contains(&column));
            let session = &self.ui.agent.sessions[index];
            let status = if session.model.busy {
                0xfe9b30
            } else if session.failed() {
                0xef5350
            } else {
                0xb5d2a2
            };
            let tab_bg = if active { 0x363636 } else { 0x1b1b1b };
            let grid = &mut self.frame.grid;
            grid.flags.set(Flags::UNDERLINE, active);
            grid.text(x, side.y, width, "", muted, tab_bg);
            grid.text(x + 1, side.y, 1, "•", status, tab_bg);
            grid.text(
                x + 3,
                side.y,
                width.saturating_sub(6),
                session.title(),
                if active {
                    0xdddddd
                } else if hovered {
                    0xcccccc
                } else {
                    muted
                },
                tab_bg,
            );
            self.frame
                .hit(x, side.y, width, AgentAction::Session(index));
            let close = rect(x + width.saturating_sub(3), side.y, 3, 1);
            self.tab_icon(
                close,
                "×",
                (if active { 0xdddddd } else { muted }, 0xef5350),
                tab_bg,
            );
            self.frame
                .hits
                .push((close, AgentAction::CloseSession(index)));
            if active {
                let grid = &mut self.frame.grid;
                let row = &mut grid.rows[usize::from(side.y - grid.rect.y)];
                for column in x..x + width {
                    row.extra_mut(usize::from(column - grid.rect.x))
                        .underline_color = Some(rgb(0xcccccc));
                }
            }
            self.frame.grid.flags.remove(Flags::UNDERLINE);
            x += width;
        }
        x += 1;
        self.frame.grid.text(x, side.y, 2, "+", 0xcccccc, line_bg);
        self.frame.hit(x, side.y, 2, AgentAction::NewSession);
        let close = rect(side.x + side.cols.saturating_sub(3), side.y, 3, 1);
        self.tab_icon(close, "›", (muted, 0xcccccc), line_bg);
        self.frame.hits.push((close, AgentAction::ClosePanel));
    }
}

pub(crate) struct ContextPreview {
    pub item: ContextItem,
    pub scroll: usize,
    columns: usize,
    pub lines: Vec<Range<usize>>,
}

impl ContextPreview {
    pub fn new(item: ContextItem) -> Self {
        Self {
            item,
            scroll: 0,
            columns: 0,
            lines: Vec::new(),
        }
    }

    fn prepare(&mut self, columns: usize) {
        let columns = columns.max(1);
        if self.columns != columns {
            self.lines = wrap_context(&self.item.snapshot.text, columns);
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

pub(crate) const REWIND_LABEL: &str = " ← ";

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

#[derive(Clone)]
struct RenderedMarkdown {
    turn: u64,
    source: Arc<str>,
    document: Arc<Markdown>,
}

impl RenderedMarkdown {
    fn matches(&self, message: &AgentMessage) -> bool {
        message.role == "Agent"
            && message.id.is_none()
            && message.turn == self.turn
            && message.text.starts_with(self.source.as_ref())
    }
}

struct MarkdownJob {
    session: u64,
    request: u64,
    messages: Vec<(usize, u64, Arc<str>, Option<RenderedMarkdown>)>,
}

struct MarkdownResult {
    session: u64,
    request: u64,
    messages: Vec<(usize, RenderedMarkdown)>,
}

fn markdown_worker() -> io::Result<Worker<MarkdownJob, MarkdownResult>> {
    let notification = Arc::new(crate::sys::new()?);
    Worker::new(
        "nextty-markdown",
        notification,
        |job: MarkdownJob, publish| {
            let messages = job
                .messages
                .into_iter()
                .map(|(index, turn, source, cached)| {
                    let old = cached
                        .as_ref()
                        .map(|rendered| (rendered.source.as_ref(), rendered.document.as_ref()));
                    let document = Arc::new(Markdown::render(&source, old));
                    (
                        index,
                        RenderedMarkdown {
                            turn,
                            source,
                            document,
                        },
                    )
                })
                .collect();
            publish(MarkdownResult {
                session: job.session,
                request: job.request,
                messages,
            });
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
    next_id: u64,
    remote: RemoteState,
    view_dirty: bool,
    markdown_worker: Option<Worker<MarkdownJob, MarkdownResult>>,
    next_render: u64,
    // The conversation and request id of the markdown job in flight.
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
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == patch.session)
        else {
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

    fn poll_remote(&mut self, client: &mut Client, config: &Config) -> io::Result<(bool, bool)> {
        if client.snapshot().is_none() {
            return Ok((false, false));
        }
        if self.remote == RemoteState::Unopened {
            if !self.open
                && self
                    .sessions
                    .iter()
                    .all(|session| session.model.pending.is_empty())
            {
                return Ok((false, false));
            }
            client.send(crate::workspace::Action::AgentOpen)?;
            self.remote = RemoteState::Loading;
        }
        let mut changed = false;
        let mut messages = false;
        let mut models_changed = false;
        while let Some(event) = client.next_agent_event() {
            match event {
                AgentEvent::Draft(drafts) => {
                    self.view_dirty = false;
                    self.restore_view_state(drafts);
                    changed = true;
                }
                AgentEvent::Model(patch) => {
                    let redraw = self.apply_model(patch)?;
                    changed |= redraw;
                    messages |= redraw;
                    models_changed = true;
                }
                AgentEvent::Recovery(recovery) => {
                    if let Some(session) =
                        self.sessions.iter_mut().find(|s| s.id == recovery.session)
                    {
                        let busy = session.model.busy;
                        changed |= session.recover(recovery) || busy != session.model.busy;
                    }
                }
                AgentEvent::Start(payload) => crate::agent::owner::spawn(client, &payload, config)?,
                AgentEvent::Ready => self.remote = RemoteState::Ready,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Unexpected Agent frontend event",
                    ));
                }
            }
        }
        if models_changed {
            self.request_markdown()?;
        }
        if self.remote != RemoteState::Ready {
            return Ok((changed, messages));
        }
        let writable = client.can_agent_input();
        if writable {
            let complete = std::mem::take(&mut self.view_dirty);
            let sessions: Vec<_> = (self.sessions.iter_mut())
                .filter(|session| complete || session.edited)
                .map(|session| {
                    session.edited = false;
                    session.draft()
                })
                .collect();
            if complete || !sessions.is_empty() {
                client.send(crate::workspace::Action::AgentView(AgentDrafts {
                    next_id: self.next_id,
                    complete,
                    sessions,
                }))?;
            }
        }
        for session in &mut self.sessions {
            if session.model.pending.is_empty() {
                continue;
            }
            for request in std::mem::take(&mut session.model.pending) {
                if writable {
                    client.send(crate::workspace::Action::AgentRequest(request))?;
                } else if let AgentRequest::Send(send) = request {
                    session.restore_draft(send.input, send.basket);
                    session.model.busy = false;
                    session.edited = true;
                }
            }
            changed |= !writable;
        }
        Ok((changed, messages))
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

    fn prepare_layout(&mut self, columns: usize) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        // Commit with wrapping during draw so input keeps the displayed frame's geometry.
        self.apply_markdown();
        if !self.rendering_active() {
            self.active_mut().sync_rendered();
        }
        self.request_markdown()?;
        self.active_mut().layout(columns);
        Ok(())
    }

    fn request_markdown(&mut self) -> io::Result<()> {
        if !self.open || self.rendering_active() {
            return Ok(());
        }
        let session = &mut self.sessions[self.current];
        // Model updates and layout preparation keep only matching prefixes.
        let messages: Vec<_> = session
            .model
            .messages
            .iter()
            .enumerate()
            .filter(|&(index, message)| {
                message.role == "Agent"
                    && message.id.is_none()
                    && session.rendered[index]
                        .as_ref()
                        .is_none_or(|rendered| rendered.source.len() != message.text.len())
            })
            .map(|(index, message)| {
                (
                    index,
                    message.turn,
                    Arc::from(message.text.as_str()),
                    session.rendered[index].clone(),
                )
            })
            .collect();
        if !messages.is_empty() {
            if self.markdown_worker.is_none() {
                self.markdown_worker = Some(markdown_worker()?);
            }
            self.next_render += 1;
            let session = self.active().id;
            self.rendering = Some((session, self.next_render));
            let request = self.next_render;
            self.markdown_worker.as_ref().unwrap().submit(MarkdownJob {
                session,
                request,
                messages,
            });
        }
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
        Ok(crate::sys::consume_notification(worker.fd())? && self.open && worker.ready(|_| true))
    }

    fn apply_markdown(&mut self) {
        let Some(result) = self.markdown_worker.as_mut().and_then(Worker::take) else {
            return;
        };
        if self.rendering != Some((result.session, result.request)) {
            return;
        }
        self.rendering = None;
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == result.session) else {
            return;
        };
        // New deltas can arrive between parsing and drawing the next frame.
        if !result.messages.iter().all(|(index, rendered)| {
            session
                .model
                .messages
                .get(*index)
                .is_some_and(|message| rendered.matches(message))
        }) {
            return;
        }
        let stamp = session.selection_stamp();
        // An unchanged rendered prefix only needs its last line wrapped again.
        session.append_from = None;
        if let [(index, rendered)] = result.messages.as_slice()
            && *index + 1 == session.model.messages.len()
            && session.columns != 0
            && let Some(old) = &session.rendered[*index]
            && rendered.document.text.starts_with(&old.document.text)
        {
            session.append_from =
                session
                    .lines
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(line, value)| match value {
                        AgentLine::Text { message, range } if message == index => {
                            Some((line, range.start))
                        }
                        _ => None,
                    });
        }
        for (index, rendered) in result.messages {
            session.rendered[index] = Some(rendered);
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
                actions.extend(
                    (session.changed_files().iter()).map(|entry| AgentAction::ChangedFile(entry.0)),
                );
            }
        }
        for index in 0..session.context.basket.len() {
            actions.extend([AgentAction::Inspect(index), AgentAction::Remove(index)]);
        }
        for (index, message) in session.model.messages.iter().enumerate().rev() {
            if message.role == "You" {
                actions.push(AgentAction::Rewind(index));
            }
            actions.push(if message.id.is_some() {
                AgentAction::Tool(index)
            } else {
                AgentAction::Copy(index)
            });
            if let Some(id) = &message.id {
                actions.extend(
                    (session.model.changes.iter().enumerate())
                        .filter(|(_, change)| &change.tool_id == id)
                        .map(|(index, _)| AgentAction::EditDiff(index)),
                );
            }
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
                let title = self.sessions[index].title();
                let mut width = 6;
                for ch in title.chars() {
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
        (widths.into_iter().enumerate())
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

fn wrap_context(text: &str, columns: usize) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    wrap_context_into(text, columns, |line| result.push(line));
    result
}

fn wrap_context_into(text: &str, columns: usize, mut push: impl FnMut(Range<usize>)) {
    let mut offset = 0;
    let columns = columns.max(1);
    let mut ends = memchr::memchr_iter(b'\n', text.as_bytes());
    while offset < text.len() {
        let end = ends.next().map_or(text.len(), |index| index + 1);
        let part = &text[offset..end];
        let line = part
            .strip_suffix("\r\n")
            .or_else(|| part.strip_suffix('\n'))
            .unwrap_or(part);
        let content_end = offset + line.len();
        let mut start = offset;
        if line.len() <= columns || line.is_ascii() {
            while content_end - start > columns {
                push(start..start + columns);
                start += columns;
            }
        } else {
            let ascii_end = offset + crate::width::printable_ascii_prefix(line.as_bytes());
            // Leave the last full row for any following zero-width scalars.
            while ascii_end - start > columns {
                push(start..start + columns);
                start += columns;
            }
            let begin = start;
            let mut width = 0;
            for (index, c) in line[begin - offset..].char_indices() {
                let index = begin + index;
                let next = cell_width(c);
                if width + next > columns && index > start {
                    push(start..index);
                    start = index;
                    width = 0;
                }
                width += next;
            }
        }
        push(start..content_end);
        offset = end;
    }
    if text.ends_with('\n') {
        push(text.len()..text.len());
    }
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
    let rows = rows.max(1);
    let prefix = &input[..cursor];
    let mut lines = wrap_context(input, columns);
    let (caret_lines, caret_range) = if cursor == input.len() {
        (lines.len(), lines.last().cloned().unwrap_or(0..0))
    } else {
        let row = lines.partition_point(|range| range.end < cursor);
        // A caret inside CRLF falls between adjacent displayed ranges.
        let row = row.saturating_sub(usize::from(lines[row].start > cursor));
        let offset = lines[row].start;
        let mut caret = (0usize, 0..0);
        wrap_context_into(&prefix[offset..], columns, |range| {
            caret.0 += 1;
            caret.1 = range;
        });
        (
            row + caret.0.max(1),
            offset + caret.1.start..offset + caret.1.end,
        )
    };
    let width = prefix[caret_range].chars().map(cell_width).sum::<usize>();
    let caret_row = caret_lines.saturating_sub(1) + usize::from(width >= columns);
    if caret_row >= lines.len() {
        lines.push(input.len()..input.len());
    }
    EditorLines {
        first: scroll
            .unwrap_or_else(|| caret_row.saturating_sub(rows - 1))
            .min(lines.len().saturating_sub(rows)),
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
    fn changed_files(&self) -> Vec<(usize, u32, u32)> {
        let mut positions = HashMap::new();
        let mut files: Vec<(usize, u32, u32)> = Vec::new();
        let mut position = 0;
        for (index, change) in self.model.changes.iter().enumerate() {
            if files.is_empty() || self.model.changes[files[position].0].path != change.path {
                position = *positions.entry(&change.path).or_insert_with(|| {
                    files.push((index, 0, 0));
                    files.len() - 1
                });
            }
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
        self.model.busy = false;
        let AgentRecovery {
            rewind,
            input,
            basket,
            ..
        } = recovery;
        if rewind {
            self.model.input = input;
            self.model.set_cursor(self.model.input.len(), false);
            self.model.scroll = 0;
            self.context = ChatContext {
                basket,
                ..ChatContext::default()
            };
            self.rewind_target = None;
        } else if input.is_empty() && basket.is_empty() {
            return false;
        } else {
            self.restore_draft(input, basket);
        }
        self.editor_scroll = None;
        self.edited = true;
        true
    }

    fn failed(&self) -> bool {
        let messages = &self.model.messages;
        messages.last().is_some_and(|last| {
            (messages.iter().rev())
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
        if !items.is_empty() {
            items.append(&mut self.context.basket);
            self.context.basket = items;
        }
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

    pub fn message_text(&self, index: usize) -> &str {
        let message = &self.model.messages[index];
        if message.role == "Agent" && message.id.is_none() {
            self.markdown(index).map_or("", |document| &document.text)
        } else {
            &message.text
        }
    }

    pub fn markdown(&self, index: usize) -> Option<&Markdown> {
        self.rendered
            .get(index)
            .and_then(Option::as_ref)
            .map(|rendered| rendered.document.as_ref())
    }

    fn sync_rendered(&mut self) {
        self.rendered.resize(self.model.messages.len(), None);
        for (message, rendered) in self.model.messages.iter().zip(&mut self.rendered) {
            if rendered.as_ref().is_some_and(|old| !old.matches(message)) {
                *rendered = None;
                self.columns = 0;
            }
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
        Some((
            TranscriptPoint {
                message,
                part,
                offset,
            },
            range,
            text,
        ))
    }

    pub fn point(&self, line: usize, column: usize) -> Option<TranscriptPoint> {
        let (mut point, range, text) = self.line_text(line)?;
        point.offset = column_offset(text, range, column as f32);
        Some(point)
    }

    pub fn begin_selection(&mut self, point: TranscriptPoint, extend: bool) {
        self.rewind_target = None;
        let anchor = if extend {
            self.selection
                .as_ref()
                .map_or(point, |selection| selection.anchor)
        } else {
            point
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

    fn selected_range(
        selection: &ChatSelection,
        first: TranscriptPoint,
        range: Range<usize>,
    ) -> Option<Range<usize>> {
        let (start, end) = (
            selection.anchor.min(selection.head),
            selection.anchor.max(selection.head),
        );
        let mut last = first;
        last.offset = range.end;
        if start >= last || end <= first {
            return None;
        }
        let from = if (start.message, start.part) == (first.message, first.part) {
            start.offset.max(range.start)
        } else {
            range.start
        };
        let to = if (end.message, end.part) == (first.message, first.part) {
            end.offset.min(range.end)
        } else {
            range.end
        };
        (from < to).then_some(from..to)
    }

    fn selection_span(&self, line: usize) -> Option<(usize, &str)> {
        let selection = self.selection.as_ref()?;
        let (point, range, text) = self.line_text(line)?;
        let selected = Self::selected_range(selection, point, range.clone())?;
        Some((
            text_width(&text[range.start..selected.start]),
            &text[selected],
        ))
    }

    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let mut output = String::new();
        let mut previous: Option<TranscriptPoint> = None;
        for index in 0..self.lines.len() {
            let Some((mut point, range, text)) = self.line_text(index) else {
                continue;
            };
            let Some(range) = Self::selected_range(selection, point, range) else {
                continue;
            };
            if let Some(last) = previous {
                if (last.message, last.part) == (point.message, point.part) {
                    output.push_str(&text[last.offset..range.start]);
                } else {
                    output.push_str(if last.message == point.message {
                        "\n"
                    } else {
                        "\n\n"
                    });
                }
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

    pub fn editor_layout(&self, columns: usize, rows: usize) -> EditorLines {
        editor_lines(
            &self.model.input,
            self.model.cursor,
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
        let cursor = column_offset(
            &self.model.input,
            layout.lines[row].clone(),
            layout.caret_column as f32,
        );
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
        let range = layout
            .lines
            .get(layout.first + row)
            .cloned()
            .unwrap_or(end..end);
        let cursor = column_offset(&self.model.input, range, column);
        self.editor_scroll = Some(layout.first);
        self.model.set_cursor(cursor, extend);
    }
    fn add_context(&mut self, item: ContextItem) -> Result<(), &'static str> {
        if self.context.basket.contains(&item) {
            return Err("Already attached");
        }
        if self.context.basket.len() >= 3
            || self
                .context
                .basket
                .iter()
                .map(|i| i.snapshot.text.len())
                .sum::<usize>()
                + item.snapshot.text.len()
                > 160 * 1024
        {
            return Err("Context is full; remove an attachment first");
        }
        self.context.basket.push(item);
        Ok(())
    }
    fn toggle_tool(&mut self, index: usize) {
        self.clear_selection();
        if let Some(id) = self
            .model
            .messages
            .get(index)
            .and_then(|message| message.id.as_ref())
        {
            if !self.expanded_tools.remove(id) {
                self.expanded_tools.insert(id.clone());
            }
            self.columns = 0;
        }
    }
    pub fn send(&mut self, client: &Client) -> io::Result<()> {
        if self.model.busy || self.model.input.trim().is_empty() {
            return Ok(());
        }
        let pane = client
            .snapshot()
            .ok_or_else(|| io::Error::other("No workspace"))?
            .focused;
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
        self.context.preview = None;
        self.context.menu = false;
        self.columns = 0;
        self.editor_scroll = None;
        self.edited = true;
        Ok(())
    }

    fn recall_prompt(&mut self, up: bool) {
        let prompts: Vec<_> = (self.model.messages.iter())
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
        self.model.input = (prompts.get(index))
            .map_or_else(|| self.prompt_draft.clone(), |message| message.text.clone());
        self.prompt_index = (index < prompts.len()).then_some(index);
        self.model
            .set_cursor(if up { 0 } else { self.model.input.len() }, false);
        self.edited = true;
    }

    fn attach_screen(
        &mut self,
        client: &Client,
        pane: PaneId,
        selection: bool,
    ) -> Result<(), &'static str> {
        let screen = client.screens().get(&pane).ok_or("No active pane")?;
        let (label, kind, range) = if selection {
            let range = screen
                .selection_range()
                .ok_or("Select text in the terminal first")?;
            ("Visible selection", "Selection", Some(range))
        } else {
            ("Visible screen", "Screen", None)
        };
        self.add_context(ContextItem {
            label: label.into(),
            kind: kind.into(),
            source: context_source(client, pane),
            snapshot: context::snapshot(screen, range),
        })
    }
    pub fn layout(&mut self, columns: usize) {
        let append = self.append_from.take().filter(|_| self.columns == columns);
        if self.columns == columns && append.is_none() {
            return;
        }
        let previous_lines = self.lines.len();
        let anchor = (self.model.scroll > 0)
            .then(|| {
                let last = self.lines.len().saturating_sub(self.model.scroll + 1);
                (0..=last).rev().find_map(|index| {
                    self.line_text(index)
                        .map(|(point, _, _)| (point, last - index))
                })
            })
            .flatten();
        let mut lines = std::mem::take(&mut self.lines);
        if let Some((line, _)) = append {
            lines.truncate(line);
        } else {
            lines.clear();
            self.expanded_tools.retain(|id| {
                self.model
                    .messages
                    .iter()
                    .any(|message| message.id.as_ref() == Some(id))
            });
        }
        let first = append.map_or(0, |_| self.model.messages.len() - 1);
        let offset = append.map_or(0, |(_, offset)| offset);
        for (index, message) in self.model.messages.iter().enumerate().skip(first) {
            let tool = message.id.as_ref();
            if tool.is_some() {
                lines.push(AgentLine::Role(index));
            }
            if tool.is_none_or(|id| self.expanded_tools.contains(id)) {
                let label = text_width(REWIND_LABEL);
                let reserved = if message.role == "You" && columns >= label + 5 {
                    1 + label
                } else if tool.is_some() || message.role == "You" {
                    2
                } else {
                    0
                };
                wrap_context_into(
                    &self.message_text(index)[offset..],
                    columns.saturating_sub(reserved),
                    |range| {
                        lines.push(AgentLine::Text {
                            message: index,
                            range: offset + range.start..offset + range.end,
                        });
                    },
                );
            }
            if let Some(id) = tool {
                lines.extend(
                    (self.model.changes.iter().enumerate())
                        .filter(|(_, change)| &change.tool_id == id)
                        .map(|(change, _)| AgentLine::Edit(change)),
                );
            }
            lines.push(AgentLine::Blank);
        }
        self.lines = lines;
        if let Some((anchor, gap)) = anchor {
            if let Some(index) = self.lines.iter().position(|line| match line {
                AgentLine::Role(message) => *message == anchor.message && anchor.part == 0,
                AgentLine::Text { message, range } => {
                    *message == anchor.message
                        && anchor.part == 1
                        && (range.contains(&anchor.offset)
                            || range.is_empty() && range.start == anchor.offset)
                }
                AgentLine::Blank | AgentLine::Edit(_) => false,
            }) {
                self.model.scroll = self.lines.len().saturating_sub(index + gap + 1);
            }
        } else if self.model.scroll > 0 {
            self.model.scroll = self
                .model
                .scroll
                .saturating_add(self.lines.len().saturating_sub(previous_lines));
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
    format!(
        "{name} · pane {pane} · {}{}",
        if client.is_remote() { "SSH" } else { "local" },
        if alternate {
            " · TUI snapshot"
        } else {
            " · visible snapshot"
        }
    )
}

impl Ui {
    fn agent_tab(&mut self, reverse: bool) {
        let actions = self.agent.controls();
        let current = match self.focus {
            Focus::Editor => AgentAction::Editor,
            Focus::Output => AgentAction::Output,
            _ => self.agent.keyboard,
        };
        let index = actions
            .iter()
            .position(|action| *action == current)
            .unwrap_or(0);
        self.agent.keyboard =
            actions[(index + if reverse { actions.len() - 1 } else { 1 }) % actions.len()];
        self.focus = match self.agent.keyboard {
            AgentAction::Editor => Focus::Editor,
            AgentAction::Output => Focus::Output,
            _ => Focus::Controls,
        };
        let keyboard = self.agent.keyboard;
        let session = self.agent.active_mut();
        let line = match keyboard {
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
        let hint = match self.agent.keyboard {
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
        };
        self.message(hint);
    }
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
        match symbol {
            keysym::ESCAPE => {
                if self.agent.active().context.menu {
                    self.agent.active_mut().context.menu = false;
                    self.agent.keyboard = AgentAction::ContextMenu;
                } else {
                    self.focus = Focus::Editor;
                }
                None
            }
            keysym::UP | keysym::DOWN => {
                self.agent_tab(symbol == keysym::UP);
                None
            }
            keysym::RETURN | keysym::KP_ENTER | 32 => Some(self.agent.keyboard),
            keysym::DELETE => match self.agent.keyboard {
                AgentAction::Inspect(index) | AgentAction::Remove(index) => {
                    Some(AgentAction::Remove(index))
                }
                _ => None,
            },
            _ => None,
        }
    }
    pub fn activate_agent(
        &mut self,
        action: AgentAction,
        client: &Client,
    ) -> io::Result<Option<String>> {
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
            return Ok(None);
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
                session.model.pending.push(AgentRequest::Abort {
                    session: session.id,
                });
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
                if let Some(pane) = client.snapshot().map(|s| s.focused) {
                    let session = self.agent.active_mut();
                    let count = session.context.basket.len();
                    session
                        .attach_screen(client, pane, action == AgentAction::Selection)
                        .map_err(io::Error::other)?;
                    session.edited |= count != session.context.basket.len();
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
                let single = matches!(action, AgentAction::EditDiff(_));
                let changes = &self.agent.active().model.changes;
                if let Some(path) = changes.get(index).map(|change| change.path.clone()) {
                    let mut text = String::new();
                    let mut incomplete = false;
                    for (_, change) in changes.iter().enumerate().filter(|(at, change)| {
                        if single {
                            *at == index
                        } else {
                            change.path == path
                        }
                    }) {
                        writeln!(
                            text,
                            "Turn {} · +{} -{}\n{}",
                            change.turn, change.added, change.removed, change.diff
                        )
                        .unwrap();
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
                return Ok(messages.get(index).map(|message| message.text.clone()));
            }
            AgentAction::CopyPreview => {
                let preview = self.agent.active().context.preview.as_ref();
                return Ok(preview.map(|preview| preview.item.snapshot.text.clone()));
            }
            AgentAction::ClosePreview => {
                self.agent.active_mut().context.preview = None;
                self.focus = Focus::Editor;
            }
        }
        Ok(None)
    }

    fn open_preview(&mut self, item: ContextItem) {
        self.agent.active_mut().context.preview = Some(ContextPreview::new(item));
        self.agent.keyboard = AgentAction::CopyPreview;
        self.focus = Focus::Controls;
    }

    fn select_agent_rewind(&mut self, index: usize) {
        let session = self.agent.active_mut();
        let Some(turn) = (session.model.messages.get(index))
            .filter(|message| message.role == "You")
            .map(|message| message.turn)
        else {
            return;
        };
        session.rewind_target = Some(turn);
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
        let index = if up {
            messages[..current.unwrap_or(messages.len())]
                .iter()
                .rposition(|message| message.role == "You")
        } else {
            current.and_then(|current| {
                (messages[current + 1..].iter())
                    .position(|message| message.role == "You")
                    .map(|offset| current + 1 + offset)
            })
        };
        if let Some(index) = index {
            self.select_agent_rewind(index);
        }
    }

    pub fn poll_agent(&mut self, client: &mut Client, config: &Config) -> io::Result<(bool, bool)> {
        let rejected = !client.can_agent_input()
            && self
                .agent
                .sessions
                .iter()
                .any(|session| !session.model.pending.is_empty());
        let result = self.agent.poll_remote(client, config)?;
        if rejected {
            self.message("Workspace is read-only");
        }
        Ok(result)
    }

    pub fn cancel_rewind(&mut self) {
        self.agent.active_mut().escape_pending = false;
        if self.agent.active_mut().rewind_target.take().is_some() {
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
            session.model.pending.push(AgentRequest::Rewind {
                session: session.id,
                turn,
            });
        }
    }

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
        viewport: (usize, usize),
    ) -> io::Result<Option<crate::input::KeyResult>> {
        use crate::input::KeyResult;
        let (control, shift, letter) = (key.control(), key.shift(), key.letter());
        let step = if matches!(key.symbol, keysym::PAGE_UP | keysym::PAGE_DOWN) {
            10
        } else {
            1
        };
        let up = matches!(key.symbol, keysym::UP | keysym::PAGE_UP);
        let down = matches!(key.symbol, keysym::DOWN | keysym::PAGE_DOWN);
        if control && letter == Some('c') && self.agent.open && self.focus.is_agent() {
            self.agent.active_mut().escape_pending = false;
            if self.agent.active().model.input.is_empty() {
                if self.agent.active().model.busy {
                    self.activate_agent(AgentAction::Stop, client)?;
                }
            } else {
                self.agent.clear_input();
            }
            return Ok(Some(KeyResult::Handled));
        }
        let activate = |ui: &mut Self, client: &mut Client| {
            let Some(action) = ui.agent_key(key.symbol, shift, control) else {
                return Ok(Some(KeyResult::Handled));
            };
            let copy = ui.activate_agent(action, client)?;
            Ok(Some(copy.map_or(KeyResult::Handled, KeyResult::Copy)))
        };
        if self.agent.active().context.preview.is_some() {
            if matches!(key.symbol, keysym::TAB | keysym::LEFT_TAB)
                || self.focus == Focus::Controls
                    && matches!(key.symbol, keysym::RETURN | keysym::KP_ENTER | 32)
            {
                return activate(self, client);
            } else if let Some(preview) = &mut self.agent.active_mut().context.preview {
                if key.symbol == keysym::ESCAPE {
                    self.agent.active_mut().context.preview = None;
                } else if up {
                    preview.scroll = preview.scroll.saturating_sub(step);
                } else if down {
                    preview.scroll = preview.scroll.saturating_add(step);
                }
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
        if matches!(key.symbol, keysym::TAB | keysym::LEFT_TAB)
            || control && key.symbol == 32
            || self.focus == Focus::Controls
        {
            return activate(self, client);
        }
        if self.focus == Focus::Output {
            let session = self.agent.active_mut();
            if key.symbol == keysym::ESCAPE {
                session.clear_selection();
                self.focus = Focus::Terminal;
            } else if key.symbol == keysym::END {
                session.model.scroll = 0;
            } else if up {
                session.model.scroll = session.model.scroll.saturating_add(step);
            } else if down {
                session.model.scroll = session.model.scroll.saturating_sub(step);
            }
            return Ok(Some(KeyResult::Handled));
        }
        if !(control && matches!(letter, Some('h' | 'j' | 'k' | 'l'))) {
            self.agent.active_mut().prompt_index = None;
        }
        if key.paste() {
            return Ok(Some(KeyResult::Paste));
        }
        if control {
            match letter {
                Some('a') => self.agent.active_mut().model.select_all(),
                Some('x') => {
                    if let Some(range) = self.agent.active().model.selection() {
                        let text = self.agent.active().model.input[range].to_owned();
                        self.insert_active("");
                        return Ok(Some(KeyResult::Copy(text)));
                    }
                }
                Some('u') => self.agent.clear_input(),
                Some('h') => self.agent.active_mut().model.key(keysym::LEFT, false),
                Some('l') => self.agent.active_mut().model.key(keysym::RIGHT, false),
                Some('j' | 'k') => {
                    let up = letter == Some('k');
                    let session = self.agent.active_mut();
                    let layout = session.editor_layout(viewport.0, viewport.1);
                    if if up {
                        layout.caret_row == 0
                    } else {
                        layout.caret_row + 1 == layout.lines.len()
                    } {
                        session.recall_prompt(up);
                    } else {
                        session.move_vertical(layout, up, false);
                    }
                }
                _ => (),
            }
        } else {
            match key.symbol {
                keysym::ESCAPE => {
                    if self.agent.active().context.menu {
                        self.agent.active_mut().context.menu = false;
                    } else {
                        self.focus = Focus::Terminal;
                    }
                }
                keysym::RETURN | keysym::KP_ENTER if shift => self.insert_active("\n"),
                keysym::RETURN | keysym::KP_ENTER => {
                    if let Err(error) = self.agent.active_mut().send(client) {
                        self.message(format!("Agent: {error}"));
                    }
                }
                keysym::UP | keysym::DOWN => {
                    let session = self.agent.active_mut();
                    let layout = session.editor_layout(viewport.0, viewport.1);
                    session.move_vertical(layout, key.symbol == keysym::UP, shift);
                }
                _ => {
                    let session = self.agent.active_mut();
                    let length = session.model.input.len();
                    session.model.key(key.symbol, shift);
                    session.edited |= session.model.input.len() != length;
                    if !key.text.is_empty() {
                        self.insert_active(key.text);
                    }
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
        self.agent.sessions[self.agent.current].context.menu = false;
        self.agent.sessions[self.agent.current].context.preview = None;
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
        fn wait_layout(&mut self, columns: usize) {
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
                    revision: session.model.revision,
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
    fn shift_navigation_extends_from_anchor_and_arrows_collapse_selection() {
        let mut agent = AgentView::default();
        agent.insert("ab\n中文").unwrap();
        agent.key(keysym::LEFT, true);
        agent.key(keysym::LEFT, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(keysym::RIGHT, false);
        assert_eq!(agent.cursor, 9);
        assert_eq!(agent.selection(), None);
        agent.key(keysym::HOME, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.key(keysym::LEFT, false);
        assert_eq!(agent.cursor, 3);
        agent.key(keysym::END, true);
        assert_eq!(agent.selection(), Some(3..9));
        agent.clear_input();
        assert!(agent.input.is_empty());
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
        let mut press = |ui: &mut Ui, symbol, modifiers| {
            ui.agent_key_input(
                &mut client,
                &crate::input::Key {
                    symbol,
                    modifiers,
                    text: "",
                },
                (40, 8),
            )
            .unwrap();
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
        session.begin_selection(session.point(0, 0).unwrap(), false);
        session.extend_selection(session.point(0, 7).unwrap());
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
            let full =
                crate::agent::markdown::Markdown::render(&session.model.messages[1].text, None);
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
            let from = session.point(from.0, from.1).unwrap();
            let to = session.point(to.0, to.1).unwrap();
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
        let start = session.point(0, 1).unwrap();
        let last = session
            .lines
            .iter()
            .rposition(|line| matches!(line, AgentLine::Text { message: 2, .. }))
            .unwrap();
        let end = session.point(last, 99).unwrap();
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
        session.begin_selection(session.point(0, 0).unwrap(), false);
        session.extend_selection(session.point(0, 4).unwrap());
        assert_eq!(session.selected_text().as_deref(), Some("keep"));

        owner.active_mut().model.messages[0].text.push_str("**");
        owner.active_mut().model.messages[1].text = "done".into();
        owner.active_mut().model.revision += 1;
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
        owner.active_mut().model.revision += 1;
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
        session.begin_selection(session.point(0, 2).unwrap(), false);
        session.extend_selection(session.point(0, 4).unwrap());
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
        ui.activate_agent(AgentAction::EditDiff(0), &client)
            .unwrap();
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
        ui.activate_agent(AgentAction::ChangedFile(0), &client)
            .unwrap();
        assert!(diff(&ui).contains("-old") && diff(&ui).contains("+second"));
    }

    #[test]
    fn drawing_the_transcript_exposes_message_tool_and_edit_hits() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        let model = &mut ui.agent.active_mut().model;
        model.messages = vec![
            message(1, "You", "text", None),
            message(1, "edit_file · done", "text", Some("one")),
            message(1, "Error", "text", None),
        ];
        model.changes = vec![change(1, "one", 2, 1, "+x\n")].into();
        let size = WindowSize {
            num_cols: 100,
            num_lines: 30,
            cell_width: 0,
            cell_height: 0,
        };
        let side = rect(50, 1, 50, 28);
        let panel = draw(&mut ui, &Config::default(), side, size, None, "").unwrap();
        let hits: Vec<_> = panel.hits.iter().map(|(_, action)| *action).collect();
        for expected in [
            AgentAction::Copy(0),
            AgentAction::Rewind(0),
            AgentAction::Tool(1),
            AgentAction::EditDiff(0),
            AgentAction::Copy(2),
            AgentAction::ToggleChanges,
            AgentAction::Editor,
            AgentAction::NewSession,
            AgentAction::ClosePanel,
        ] {
            assert!(hits.contains(&expected));
        }
        let text: String = panel
            .grid
            .rows
            .iter()
            .flat_map(|row| row.cells.iter().map(|cell| cell.c))
            .collect();
        assert!(text.contains("› edit_file · done") && text.contains("+2 -1"));
    }

    #[test]
    fn composition_shows_in_the_editor_without_changing_the_draft() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        ui.focus = Focus::Editor;
        ui.agent.active_mut().model.insert("e\u{301}中").unwrap();
        let size = WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 0,
            cell_height: 0,
        };
        let side = rect(40, 1, 40, 22);
        let composing = draw(&mut ui, &Config::default(), side, size, None, "文").unwrap();
        assert_eq!(ui.agent.active().model.input, "e\u{301}中");
        let editor = editor(&ui, side).0;
        assert_eq!(composing.cursor, Some((editor.x + 5, editor.y)));
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
            let lines: Vec<String> = wrap_context(text, 3)
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
        assert!(wrap_context("", 3).is_empty());
    }

    #[test]
    fn vertical_selection_uses_the_displayed_caret_before_a_wide_wrap() {
        let mut panel = AgentSession::default();
        panel.model.insert("ab中").unwrap();
        panel.model.set_cursor(2, false);
        let layout = panel.editor_layout(3, 3);
        assert_eq!((layout.caret_row, layout.caret_column), (0, 2));
        panel.move_vertical(layout, true, false);
        assert_eq!(panel.model.cursor, 2);
        panel.move_vertical(panel.editor_layout(3, 3), false, true);
        assert_eq!(panel.model.selection(), Some(2..5));
    }

    #[test]
    fn editor_caret_preserves_positions_inside_crlf() {
        let mut session = AgentSession::default();
        session.model.insert("ab\r\n中").unwrap();
        for (cursor, expected) in [(2, (1, 0)), (3, (1, 1)), (4, (1, 0))] {
            session.model.set_cursor(cursor, false);
            let layout = session.editor_layout(2, 3);
            assert_eq!((layout.caret_row, layout.caret_column), expected);
        }
    }

    #[test]
    fn editor_viewport_retains_scrolled_draft_and_full_width_caret() {
        let mut panel = AgentSession::default();
        panel.model.insert("ab中").unwrap();
        let editor = panel.editor_layout(4, 3);
        assert_eq!((editor.caret_row, editor.caret_column), (1, 0));
        assert_eq!(editor.lines, vec![0..5, 5..5]);
        assert_eq!(column_offset(&panel.model.input, 0..5, 3.), 5);
        panel.model.clear_input();
        panel
            .model
            .insert(
                &(0..12)
                    .map(|n| format!("draft {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .unwrap();
        assert_eq!(panel.editor_layout(20, 3).first, 9);
        panel.editor_scroll = Some(3);
        let editor = panel.editor_layout(20, 3);
        assert_eq!(editor.first, 3);
        assert_eq!(editor.caret_row, 11);
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
