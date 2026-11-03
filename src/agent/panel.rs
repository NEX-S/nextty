use crate::agent::AgentView;
use crate::config::Config;
use crate::context::{self, Snapshot as ContextSnapshot};
use crate::core::Mode;
use crate::core::{Flags, WindowSize};
use crate::input::keysym;
use crate::panel::{Grid, mix_color, rgb};
use crate::ui::{Focus, Ui};
use crate::width::scalar_width;
use crate::width::text_width;
use crate::worker::Worker;
use crate::workspace::Rect;
use crate::workspace::{AgentEvent, AgentModelPatch, Client, PaneId};
use std::collections::HashSet;
use std::io;
use std::ops::Range;
use std::os::fd::RawFd;
use std::sync::Arc;

pub(crate) struct Panel {
    pub grid: Grid,
    pub hits: Vec<(Rect, AgentAction)>,
    pub chat: Rect,
    pub first: usize,
    pub chat_rows: Vec<(usize, Rect)>,
    #[cfg(feature = "gui")]
    pub editor: Rect,
    pub preview: Option<Rect>,
    pub cursor: Option<(u16, u16)>,
}

impl Panel {
    fn hit(&mut self, x: u16, y: u16, cols: u16, action: AgentAction) {
        self.hits.push((
            Rect {
                x,
                y,
                cols,
                rows: 1,
            },
            action,
        ));
    }
}

pub(crate) fn editor(ui: &Ui, side: Rect) -> (Rect, EditorLines) {
    let session = ui.agent.active();
    let max_rows = usize::from((side.rows / 3).clamp(1, 8));
    let columns = usize::from(
        side.cols
            .saturating_sub(2)
            .saturating_sub(u16::from(session.model.busy) * 3)
            .max(1),
    );
    let layout = session.editor_layout(columns, max_rows);
    let rows = layout.lines.len().max(3).min(max_rows) as u16;
    (
        Rect {
            x: side.x + 1,
            y: (side.y + side.rows).saturating_sub(rows),
            cols: side.cols.saturating_sub(2),
            rows,
        },
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
    let (editor, editor_layout) = editor(ui, side);
    let rect = if ui.agent.active().context.preview.is_some() {
        Rect {
            x: 0,
            y: 1,
            cols: size.num_cols,
            rows: size.num_lines.saturating_sub(2),
        }
    } else {
        side
    };
    let frame = Panel {
        grid: match ui.agent.spare_grid.take() {
            Some(mut grid) => {
                grid.reset(rect, config.foreground, config.background);
                grid
            }
            None => Grid::new(rect, config.foreground, config.background),
        },
        hits: Vec::new(),
        chat: Rect::default(),
        first: 0,
        chat_rows: Vec::new(),
        #[cfg(feature = "gui")]
        editor,
        preview: None,
        cursor: None,
    };
    let mut painter = AgentPainter {
        ui,
        config,
        side,
        size,
        pointer,
        frame,
        preedit,
    };
    painter.draw_agent(editor, editor_layout)?;
    Ok(painter.frame)
}

struct AgentPainter<'a> {
    ui: &'a mut Ui,
    config: &'a Config,
    side: Rect,
    size: WindowSize,
    pointer: Option<(u16, u16)>,
    frame: Panel,
    preedit: &'a str,
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

impl AgentPainter<'_> {
    fn draw_agent(&mut self, editor: Rect, editor_layout: EditorLines) -> io::Result<()> {
        let side = self.side;
        if side.cols < 12 || side.rows < 6 {
            self.frame.grid.text(
                side.x,
                side.y,
                side.cols,
                "Alt+a: close",
                self.config.foreground,
                self.config.background,
            );
            return Ok(());
        }
        let palette = Palette {
            bg: self.config.background,
            foreground: self.config.foreground,
            muted: mix_color(self.config.foreground, self.config.background, 180),
            user_bg: mix_color(self.config.foreground, self.config.background, 24),
            selection_bg: mix_color(self.config.foreground, self.config.background, 70),
            error: mix_color(0xe87575, self.config.foreground, 128),
        };
        let Palette {
            bg,
            muted,
            user_bg,
            error,
            ..
        } = palette;
        let chat_top = side.y + 1;
        if self.ui.agent.active().context.preview.is_some() {
            self.draw_preview(bg, muted);
        } else {
            let divider = mix_color(self.config.foreground, bg, 24);
            for row in side.y..side.y + side.rows {
                self.frame.grid.text(side.x, row, 1, "│", divider, bg);
            }
            self.frame.hits.push((side, AgentAction::Output));
            self.draw_agent_tabs(side, editor, bg);
            let attachment_count = self.ui.agent.active().context.basket.len();
            let attachment_rows = (attachment_count as u16)
                .min(3)
                .min(editor.y.saturating_sub(side.y + 6));
            let attachment_first = match self.ui.agent.keyboard {
                AgentAction::Inspect(index) | AgentAction::Remove(index) => {
                    index.saturating_sub(usize::from(attachment_rows.saturating_sub(1)))
                }
                _ => attachment_count.saturating_sub(usize::from(attachment_rows)),
            };
            let rewind = self.ui.agent.active().rewind_target.is_some();
            let hint_rows = if rewind { 3 } else { 0 };
            let chat_end = editor.y.saturating_sub(1 + attachment_rows + hint_rows);
            self.draw_transcript(palette, editor, chat_top, chat_end)?;
            if rewind {
                self.draw_rewind(
                    palette,
                    Rect {
                        y: editor.y.saturating_sub(1),
                        ..editor
                    },
                );
            }
            self.draw_attachments(palette, editor, chat_end, attachment_rows, attachment_first);
            self.frame.grid.text(
                side.x,
                editor.y.saturating_sub(1),
                side.cols,
                &format!("├{}", "─".repeat(usize::from(side.cols.saturating_sub(1)))),
                divider,
                bg,
            );
            self.draw_editor(palette, editor, editor_layout);
        }
        let hovered = self.pointer.and_then(|(x, y)| {
            self.frame
                .hits
                .iter()
                .rfind(|(rect, _)| rect.contains(x, y))
                .map(|(_, action)| *action)
        });
        for index in 0..self.frame.hits.len() {
            let (rect, action) = self.frame.hits[index];
            if hovered != Some(action)
                && !(self.ui.focus == Focus::Controls && self.ui.agent.keyboard == action)
            {
                continue;
            }
            if matches!(
                action,
                AgentAction::Session(_)
                    | AgentAction::Editor
                    | AgentAction::Output
                    | AgentAction::Copy(_)
            ) {
                continue;
            }
            if matches!(
                action,
                AgentAction::NewSession | AgentAction::CloseSession(_) | AgentAction::ClosePanel
            ) {
                let row = &mut self.frame.grid.rows[usize::from(rect.y - self.frame.grid.rect.y)];
                let start = usize::from(rect.x.saturating_sub(self.frame.grid.rect.x));
                let end = (start + usize::from(rect.cols)).min(row.cells.len());
                for cell in &mut row.cells[start..end] {
                    cell.fg = rgb(if matches!(action, AgentAction::CloseSession(_)) {
                        0xef5350
                    } else {
                        self.config.foreground
                    });
                }
            } else {
                self.frame.grid.recolor(rect, user_bg);
                if action == AgentAction::Stop {
                    let row =
                        &mut self.frame.grid.rows[usize::from(rect.y - self.frame.grid.rect.y)];
                    let start = usize::from(rect.x - self.frame.grid.rect.x);
                    let end = (start + usize::from(rect.cols)).min(row.cells.len());
                    for cell in &mut row.cells[start..end] {
                        cell.fg = rgb(error);
                    }
                }
            }
        }
        Ok(())
    }

    fn draw_transcript(
        &mut self,
        palette: Palette,
        editor: Rect,
        chat_top: u16,
        chat_end: u16,
    ) -> io::Result<()> {
        let Palette {
            bg,
            foreground,
            muted,
            user_bg,
            selection_bg,
            error,
        } = palette;
        let rows = usize::from(chat_end.saturating_sub(chat_top));
        let columns = usize::from(editor.cols);
        self.ui.agent.prepare_layout(columns)?;
        let session = self.ui.agent.active_mut();
        if let Some(target) = session.rewind_index()
            && let Some(line) = session.lines.iter().position(|line| {
                matches!(line, AgentLine::Text { message, .. } if *message as usize == target)
            })
        {
            let first = session.lines.len().saturating_sub(rows + session.model.scroll);
            if line < first {
                session.model.scroll = session.lines.len().saturating_sub(rows + line);
            } else if line >= first + rows {
                session.model.scroll = session.lines.len().saturating_sub(line + 1);
            }
        }
        session.model.scroll = session
            .model
            .scroll
            .min(session.lines.len().saturating_sub(rows));
        let first = session
            .lines
            .len()
            .saturating_sub(rows + session.model.scroll);
        self.frame.chat = Rect {
            x: editor.x,
            y: chat_top,
            cols: editor.cols,
            rows: rows as u16,
        };
        self.frame.first = first;
        for (row, line) in self
            .ui
            .agent
            .active()
            .lines
            .iter()
            .skip(first)
            .take(rows)
            .enumerate()
        {
            let y = chat_top + row as u16;
            let indent = match line {
                AgentLine::Role(_) => Some(3),
                AgentLine::Text { message, .. } => {
                    let message = &self.ui.agent.active().model.messages[*message as usize];
                    Some(if message.role == "You" {
                        1
                    } else if message.id.is_some() {
                        2
                    } else {
                        0
                    })
                }
                AgentLine::Blank => None,
            };
            if let Some(indent) = indent {
                self.frame.chat_rows.push((
                    first + row,
                    Rect {
                        x: editor.x + indent,
                        y,
                        cols: editor.cols.saturating_sub(indent),
                        rows: 1,
                    },
                ));
            }
            match line {
                AgentLine::Blank => (),
                AgentLine::Role(index) => {
                    let message = &self.ui.agent.active().model.messages[*index as usize];
                    let marker = if message
                        .id
                        .as_ref()
                        .is_some_and(|id| self.ui.agent.active().expanded_tools.contains(id))
                    {
                        "▾"
                    } else {
                        "›"
                    };
                    self.frame.grid.text(
                        editor.x,
                        y,
                        editor.cols,
                        &format!(" {marker} {} ", message.role),
                        muted,
                        bg,
                    );
                    if let Some(selected) = self.ui.agent.active().selection_columns(first + row) {
                        let from = column_offset(
                            &message.role,
                            0..message.role.len(),
                            selected.start as f32,
                        );
                        let to = column_offset(
                            &message.role,
                            0..message.role.len(),
                            selected.end as f32,
                        );
                        self.frame.grid.text(
                            editor.x + 3 + selected.start as u16,
                            y,
                            (selected.end - selected.start) as u16,
                            &message.role[from..to],
                            self.config.foreground,
                            selection_bg,
                        );
                    }
                    self.frame.hit(
                        editor.x,
                        y,
                        editor.cols.min(text_width(&message.role) as u16 + 4),
                        AgentAction::Tool(*index as usize),
                    );
                }
                AgentLine::Text {
                    message: index,
                    range,
                } => {
                    let message = &self.ui.agent.active().model.messages[*index as usize];
                    let user = message.role == "You";
                    let message_bg =
                        if user && self.ui.agent.active().rewind_target == Some(message.turn) {
                            selection_bg
                        } else if user {
                            user_bg
                        } else {
                            bg
                        };
                    let indent = if user {
                        1
                    } else if message.id.is_some() {
                        2
                    } else {
                        0
                    };
                    self.frame.grid.text(
                        editor.x,
                        y,
                        editor.cols,
                        "",
                        self.config.foreground,
                        message_bg,
                    );
                    let text = self.ui.agent.active().message_text(*index as usize);
                    let color =
                        if user && self.ui.agent.active().rewind_target == Some(message.turn) {
                            crate::panel::ACCENT
                        } else if message.role == "Error" {
                            error
                        } else if message.id.is_some() {
                            muted
                        } else {
                            foreground
                        };
                    let from = range.start as usize;
                    let to = range.end as usize;
                    if let Some(document) = self.ui.agent.active().markdown(*index as usize) {
                        let mut x = editor.x + indent;
                        for (span, style) in document.spans(from..to) {
                            if style & crate::agent::markdown::BOLD != 0 {
                                self.frame.grid.flags.insert(Flags::BOLD);
                            }
                            if style & crate::agent::markdown::ITALIC != 0 {
                                self.frame.grid.flags.insert(Flags::ITALIC);
                            }
                            let columns = text_width(&text[span.clone()]) as u16;
                            let column = x;
                            x += columns;
                            self.frame.grid.text(
                                column,
                                y,
                                columns,
                                &text[span],
                                crate::agent::markdown::color(style, color),
                                if style & crate::agent::markdown::CODE != 0 {
                                    user_bg
                                } else {
                                    message_bg
                                },
                            );
                            self.frame.grid.flags.remove(Flags::BOLD | Flags::ITALIC);
                        }
                    } else {
                        self.frame.grid.text(
                            editor.x + indent,
                            y,
                            editor.cols.saturating_sub(indent),
                            &text[from..to],
                            color,
                            message_bg,
                        );
                    }
                    if let Some(selected) = self.ui.agent.active().selection_columns(first + row) {
                        let text = &text[from..to];
                        let from = column_offset(text, 0..text.len(), selected.start as f32);
                        let to = column_offset(text, 0..text.len(), selected.end as f32);
                        self.frame.grid.text(
                            editor.x + indent + selected.start as u16,
                            y,
                            (selected.end - selected.start) as u16,
                            &text[from..to],
                            self.config.foreground,
                            selection_bg,
                        );
                    }
                    self.frame
                        .hit(editor.x, y, editor.cols, AgentAction::Copy(*index as usize));
                    if user
                        && columns >= crate::width::text_width(REWIND_LABEL) + 5
                        && (row + 1 == rows
                            || !matches!(
                                self.ui.agent.active().lines.get(first + row + 1),
                                Some(AgentLine::Text { message: next, .. }) if next == index
                            ))
                    {
                        let width = crate::width::text_width(REWIND_LABEL) as u16;
                        let x = editor.x + editor.cols.saturating_sub(width);
                        let hovered = self.pointer.is_some_and(|(column, row)| {
                            row == y && column >= x && column < x + width
                        });
                        self.frame.grid.text(
                            x,
                            y,
                            width,
                            REWIND_LABEL,
                            if hovered {
                                self.config.foreground
                            } else {
                                muted
                            },
                            message_bg,
                        );
                        self.frame
                            .hit(x, y, width, AgentAction::Rewind(*index as usize));
                    }
                }
            }
        }
        Ok(())
    }

    fn draw_rewind(&mut self, palette: Palette, editor: Rect) {
        let Palette { bg, muted, .. } = palette;
        self.frame.grid.text(
            editor.x,
            editor.y.saturating_sub(3),
            editor.cols,
            "j/k select · Enter confirm · Esc exits",
            muted,
            bg,
        );
        let session = self.ui.agent.active();
        let target =
            session.model.messages.iter().find(|message| {
                Some(message.turn) == session.rewind_target && message.role == "You"
            });
        let label = target.map_or(String::new(), |message| {
            format!(
                "Rewind {} turns: {}",
                session
                    .model
                    .messages
                    .iter()
                    .filter(|candidate| candidate.role == "You" && candidate.turn >= message.turn)
                    .count(),
                message.text.lines().next().unwrap_or("")
            )
        });
        self.frame.grid.text(
            editor.x,
            editor.y.saturating_sub(2),
            editor.cols,
            &label,
            self.config.foreground,
            bg,
        );
        self.frame.grid.text(
            editor.x,
            editor.y.saturating_sub(1),
            editor.cols,
            "Commands and file changes stay",
            self.config.foreground,
            bg,
        );
    }

    fn draw_attachments(
        &mut self,
        palette: Palette,
        editor: Rect,
        chat_end: u16,
        attachment_rows: u16,
        attachment_first: usize,
    ) {
        let Palette { bg, muted, .. } = palette;
        for (row, (index, item)) in self
            .ui
            .agent
            .active()
            .context
            .basket
            .iter()
            .enumerate()
            .skip(attachment_first)
            .take(usize::from(attachment_rows))
            .enumerate()
        {
            let y = chat_end + row as u16;
            self.frame.grid.text(
                editor.x,
                y,
                editor.cols.saturating_sub(3),
                &format!(" +{} ", item.label),
                self.config.foreground,
                bg,
            );
            self.frame.grid.text(
                editor.x + editor.cols.saturating_sub(3),
                y,
                3,
                " × ",
                muted,
                bg,
            );
            self.frame.hit(
                editor.x,
                y,
                editor
                    .cols
                    .saturating_sub(3)
                    .min(text_width(&item.label) as u16 + 3),
                AgentAction::Inspect(index),
            );
            self.frame.hit(
                editor.x + editor.cols.saturating_sub(3),
                y,
                3,
                AgentAction::Remove(index),
            );
        }
    }

    fn draw_editor(&mut self, palette: Palette, editor: Rect, layout: EditorLines) {
        let Palette {
            foreground,
            bg,
            selection_bg,
            error,
            ..
        } = palette;
        let mut display = std::borrow::Cow::Borrowed(self.ui.agent.active().model.input.as_str());
        let (mut lines, mut first) = (layout.lines, layout.first);
        let (mut caret_row, mut caret_column) = (layout.caret_row, layout.caret_column);
        if self.ui.focus == Focus::Editor && !self.preedit.is_empty() {
            let input = &self.ui.agent.active().model.input;
            let cursor = self.ui.agent.active().model.cursor;
            display = std::borrow::Cow::Owned(format!(
                "{}{}{}",
                &input[..cursor],
                self.preedit,
                &input[cursor..]
            ));
            let columns = usize::from(editor.cols)
                .saturating_sub(3 * usize::from(self.ui.agent.active().model.busy))
                .max(1);
            lines = wrap_context(&display, columns);
            let cursor = cursor + self.preedit.len();
            caret_row = lines
                .iter()
                .rposition(|range| range.start <= cursor)
                .unwrap_or(0);
            caret_column = text_width(&display[lines[caret_row].start..cursor]);
            if caret_column == columns {
                caret_row += 1;
                caret_column = 0;
                if caret_row == lines.len() {
                    lines.push(cursor..cursor);
                }
            }
            first = caret_row.saturating_sub(usize::from(editor.rows.saturating_sub(1)));
        }
        if self.ui.focus == Focus::Editor
            && (first..first + usize::from(editor.rows)).contains(&caret_row)
        {
            self.frame.cursor = Some((
                editor.x + caret_column as u16,
                editor.y + (caret_row - first) as u16,
            ));
        } else {
            self.frame.cursor = None;
        }
        for row in 0..editor.rows {
            let range = lines.get(first + usize::from(row));
            self.frame.grid.text(
                editor.x,
                editor.y + row,
                editor.cols,
                range.map_or("", |range| &display[range.clone()]),
                foreground,
                bg,
            );
            if self.preedit.is_empty()
                && let Some(range) = range
                && let Some(selected) = self.ui.agent.active().model.selection()
            {
                let start = range.start.max(selected.start);
                let end = range.end.min(selected.end);
                if start < end || selected.start <= range.end && selected.end > range.end {
                    let column = text_width(&display[range.start..start.min(range.end)]) as u16;
                    let width = if start < end {
                        text_width(&display[start..end])
                    } else {
                        0
                    };
                    self.frame.grid.recolor(
                        Rect {
                            x: editor.x + column,
                            y: editor.y + row,
                            cols: (width + usize::from(selected.end > range.end)) as u16,
                            rows: 1,
                        },
                        selection_bg,
                    );
                }
            }
        }
        self.frame.hits.push((editor, AgentAction::Editor));
        if self.ui.agent.active().model.busy {
            self.frame.grid.text(
                editor.x + editor.cols.saturating_sub(3),
                editor.y,
                3,
                " ■ ",
                error,
                bg,
            );
            self.frame.hit(
                editor.x + editor.cols.saturating_sub(3),
                editor.y,
                3,
                AgentAction::Stop,
            );
        }
        if self.ui.agent.active().context.menu {
            for (row, (label, action)) in [
                ("Selection", AgentAction::Selection),
                ("Visible screen", AgentAction::Screen),
            ]
            .into_iter()
            .enumerate()
            {
                let y = editor.y.saturating_sub(3) + row as u16;
                self.frame.grid.text(
                    editor.x,
                    y,
                    editor.cols,
                    &format!(" {label} "),
                    self.config.foreground,
                    bg,
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

    fn draw_preview(&mut self, bg: u32, muted: u32) {
        let Some(preview) = &mut self.ui.agent.active_mut().context.preview else {
            return;
        };
        let columns = self.size.num_cols.saturating_sub(4);
        let rows = usize::from(self.size.num_lines.saturating_sub(5));
        preview.prepare(usize::from(columns));
        let ContextPreview {
            item,
            scroll,
            lines,
            ..
        } = preview;
        *scroll = (*scroll).min(lines.len().saturating_sub(rows));
        self.frame
            .grid
            .text(2, 1, columns, &item.label, self.config.foreground, bg);
        self.frame
            .grid
            .text(self.size.num_cols.saturating_sub(4), 1, 3, " × ", muted, bg);
        for (row, range) in lines.iter().skip(*scroll).take(rows).enumerate() {
            self.frame.grid.text(
                2,
                row as u16 + 3,
                columns,
                &item.snapshot.text[range.clone()],
                self.config.foreground,
                bg,
            );
        }
        self.frame.grid.text(
            2,
            self.size.num_lines.saturating_sub(2),
            columns,
            " ⧉   Esc close",
            muted,
            bg,
        );
        self.frame.hit(
            self.size.num_cols.saturating_sub(4),
            1,
            3,
            AgentAction::ClosePreview,
        );
        self.frame.hit(
            2,
            self.size.num_lines.saturating_sub(2),
            text_width(" ⧉ ") as u16,
            AgentAction::CopyPreview,
        );
        self.frame.preview = Some(Rect {
            x: 2,
            y: 3,
            cols: columns,
            rows: rows as u16,
        });
    }

    fn tab_icon(
        &mut self,
        rect: Rect,
        label: &str,
        colors: (u32, u32),
        background: u32,
        hovered: bool,
    ) {
        let foreground = if hovered { colors.1 } else { colors.0 };
        self.frame
            .grid
            .text(rect.x, rect.y, rect.cols, "", foreground, background);
        self.frame
            .grid
            .text(rect.x + 1, rect.y, 1, label, foreground, background);
    }

    fn draw_agent_tabs(&mut self, side: Rect, editor: Rect, bg: u32) {
        let muted = mix_color(self.config.foreground, bg, 180);
        let mut line_bg = 0;
        for shift in [16, 8, 0] {
            line_bg |= ((bg >> shift & 255) * 4 / 5) << shift;
        }
        self.frame.grid.text(
            side.x,
            side.y,
            side.cols,
            "",
            self.config.foreground,
            line_bg,
        );
        self.frame.grid.text(
            side.x,
            side.y,
            1,
            "│",
            mix_color(self.config.foreground, bg, 24),
            line_bg,
        );
        let active_fg = if (bg >> 16 & 255) + (bg >> 8 & 255) + (bg & 255) > 384 {
            self.config.foreground
        } else {
            0xffffff
        };
        let mut x = editor.x;
        for (index, column, columns) in self.ui.agent.tabs(usize::from(side.cols.saturating_sub(1)))
        {
            x = editor.x + column as u16;
            let width = columns as u16;
            let active = index == self.ui.agent.current;
            let hovered = self
                .pointer
                .is_some_and(|(column, row)| row == side.y && (x..x + width).contains(&column));
            let model = &self.ui.agent.sessions[index].model;
            let status = if model.busy {
                0xfe9b30
            } else if model.messages.last().is_some_and(|last| {
                model
                    .messages
                    .iter()
                    .rev()
                    .take_while(|message| message.turn == last.turn)
                    .any(|message| matches!(message.role.as_str(), "Error" | "Blocked"))
            }) {
                0xef5350
            } else {
                0xb5d2a2
            };
            self.frame.grid.text(x, side.y, width, "", muted, line_bg);
            self.frame.grid.text(x, side.y, 1, "•", status, line_bg);
            self.frame.grid.flags.set(Flags::BOLD, active);
            self.frame.grid.text(
                x + 2,
                side.y,
                width.saturating_sub(7),
                self.ui.agent.sessions[index].title(),
                if active {
                    active_fg
                } else if hovered {
                    self.config.foreground
                } else {
                    muted
                },
                line_bg,
            );
            self.frame.grid.flags.remove(Flags::BOLD);
            self.frame
                .hit(x, side.y, width, AgentAction::Session(index));
            let close = Rect {
                x: x + width.saturating_sub(5),
                y: side.y,
                cols: 3,
                rows: 1,
            };
            let close_hovered = self.pointer.is_some_and(|(x, y)| close.contains(x, y));
            self.tab_icon(close, "×", (muted, 0xef5350), line_bg, close_hovered);
            self.frame
                .hits
                .push((close, AgentAction::CloseSession(index)));
            self.frame.grid.text(
                x + width - 2,
                side.y,
                1,
                "╱",
                mix_color(self.config.foreground, bg, 96),
                line_bg,
            );
            x += width;
        }
        self.frame
            .grid
            .text(x, side.y, 2, "+", self.config.foreground, line_bg);
        self.frame.hit(x, side.y, 2, AgentAction::NewSession);
        let close = Rect {
            x: side.x + side.cols.saturating_sub(3),
            y: side.y,
            cols: 3,
            rows: 1,
        };
        let hovered = self.pointer.is_some_and(|(x, y)| close.contains(x, y));
        self.tab_icon(
            close,
            "›",
            (muted, self.config.foreground),
            line_bg,
            hovered,
        );
        self.frame.hits.push((close, AgentAction::ClosePanel));
    }
}

#[derive(Clone)]
pub(crate) struct ContextItem {
    pub label: String,
    pub source: String,
    pub kind: String,
    pub snapshot: ContextSnapshot,
}

type ContextView = (String, String, String, String, bool);

impl ContextItem {
    fn view(&self) -> (&str, &str, &str, &str, bool) {
        (
            &self.label,
            &self.source,
            &self.kind,
            &self.snapshot.text,
            self.snapshot.incomplete,
        )
    }

    fn from_view((label, source, kind, text, incomplete): ContextView) -> Self {
        Self {
            label,
            source,
            kind,
            snapshot: ContextSnapshot { text, incomplete },
        }
    }
}

pub(crate) struct ContextPreview {
    pub item: ContextItem,
    pub scroll: usize,
    columns: usize,
    pub lines: Vec<Range<usize>>,
}

impl ContextPreview {
    pub fn new(item: ContextItem, scroll: usize) -> Self {
        Self {
            item,
            scroll,
            columns: 0,
            lines: Vec::new(),
        }
    }

    pub fn prepare(&mut self, columns: usize) {
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
}

// Agent bounds retained text at 256 KiB, so cached offsets fit in u32.
pub(crate) enum AgentLine {
    Role(u32),
    Text { message: u32, range: Range<u32> },
    Blank,
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
    document: Arc<crate::agent::markdown::Markdown>,
}

impl RenderedMarkdown {
    fn matches(&self, message: &crate::workspace::AgentMessage) -> bool {
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
    let notification = Arc::new(crate::eventfd::new()?);
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
                    let document = Arc::new(crate::agent::markdown::Markdown::render(&source, old));
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
    pub view_dirty: bool,
    markdown_worker: Option<Worker<MarkdownJob, MarkdownResult>>,
    next_render: u64,
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
            spare_grid: None,
        }
    }
}

impl AgentPanel {
    fn apply_model(&mut self, patch: &AgentModelPatch) -> io::Result<bool> {
        let mut value: serde_json::Value = serde_json::from_slice(&patch.recovery)?;
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == patch.session)
        else {
            return Ok(false);
        };
        let selection = session.selection_checkpoint();
        let rewind: Option<(u64, String, Vec<ContextView>)> = serde_json::from_value(
            value
                .get_mut("rewind_restore")
                .map_or(serde_json::Value::Null, serde_json::Value::take),
        )
        .map_err(io::Error::other)?;
        let failed: Option<(u64, String, Vec<ContextView>)> = serde_json::from_value(
            value
                .get_mut("failed_send")
                .map_or(serde_json::Value::Null, serde_json::Value::take),
        )
        .map_err(io::Error::other)?;
        let mut redraw = session.model.restore_patch(patch)?;
        if redraw {
            session.columns = 0;
            session.sync_rendered();
        }
        if let Some((turn, input, items)) = rewind
            && session.rewind_applied != Some(turn)
        {
            session.model.input = input;
            session.model.set_cursor(session.model.input.len(), false);
            session.model.scroll = 0;
            session.context.basket = items.into_iter().map(ContextItem::from_view).collect();
            session.context.preview = None;
            session.context.menu = false;
            session.editor_scroll = None;
            session.rewind_target = None;
            session.rewind_applied = Some(turn);
            self.view_dirty = true;
            redraw = true;
        }
        if let Some((revision, input, items)) = failed
            && session.failed_send_applied != Some(revision)
        {
            session.restore_draft(
                input,
                items.into_iter().map(ContextItem::from_view).collect(),
            );
            session.failed_send_applied = Some(revision);
            session.editor_scroll = None;
            self.view_dirty = true;
            redraw = true;
        }
        if selection.is_some() && !session.selection_matches(selection.as_ref()) {
            session.clear_selection();
            redraw = true;
        }
        Ok(redraw)
    }

    pub fn poll_remote(
        &mut self,
        client: &mut Client,
        config: &Config,
    ) -> io::Result<(bool, bool)> {
        if client.snapshot().is_none() {
            return Ok((false, false));
        }
        if self.remote == RemoteState::Unopened
            && !self.open
            && self
                .sessions
                .iter()
                .all(|session| session.model.pending.is_empty())
        {
            return Ok((false, false));
        }
        if self.remote == RemoteState::Unopened {
            client.send(crate::workspace::Action::AgentOpen)?;
            self.remote = RemoteState::Loading;
        }
        let mut changed = false;
        let mut messages = false;
        let mut models_changed = false;
        for event in client.take_agent_events() {
            match event {
                AgentEvent::Draft(payload) => {
                    self.view_dirty = false;
                    self.restore_view_state(serde_json::from_slice(&payload)?)?;
                    changed = true;
                }
                AgentEvent::Model(patch) => {
                    let redraw = self.apply_model(&patch)?;
                    changed |= redraw;
                    messages |= redraw;
                    models_changed = true;
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
        if self.remote == RemoteState::Ready && client.can_agent_input() {
            if self.view_dirty {
                client.send(crate::workspace::Action::AgentView(serde_json::to_vec(
                    &self.view_state(),
                )?))?;
                self.view_dirty = false;
            }
            for session in &mut self.sessions {
                for mut request in session.model.pending.drain(..) {
                    request["session"] = serde_json::json!(session.id);
                    client.send(crate::workspace::Action::AgentRequest(
                        serde_json::to_vec(&request).map_err(io::Error::other)?,
                    ))?;
                }
            }
        } else if self.remote == RemoteState::Ready {
            for session in &mut self.sessions {
                if !session.model.pending.is_empty() {
                    for mut request in std::mem::take(&mut session.model.pending) {
                        if request["action"] == "send" {
                            let input = serde_json::from_value(request["input"].take())?;
                            let items: Vec<ContextView> =
                                serde_json::from_value(request["basket"].take())?;
                            session.restore_draft(
                                input,
                                items.into_iter().map(ContextItem::from_view).collect(),
                            );
                            session.model.busy = false;
                            self.view_dirty = true;
                        }
                    }
                    changed = true;
                }
            }
        }
        Ok((changed, messages))
    }

    pub fn view_state(&self) -> serde_json::Value {
        let sessions = self.sessions.iter().map(|session| serde_json::json!({
            "id": session.id,
            "input": session.model.input,
            "rewind_applied": session.rewind_applied,
            "failed_send_applied": session.failed_send_applied,
            "basket": session.context.basket.iter().map(ContextItem::view).collect::<Vec<_>>(),
        })).collect();
        let mut view = serde_json::json!({"next_id": self.next_id});
        view["sessions"] = serde_json::Value::Array(sessions);
        view
    }

    pub fn restore_view_state(&mut self, mut value: serde_json::Value) -> io::Result<()> {
        let next_id = serde_json::from_value(value["next_id"].take())?;
        let values: Vec<serde_json::Value> = serde_json::from_value(value["sessions"].take())?;
        let current = self.active().id;
        let mut previous = std::mem::take(&mut self.sessions);
        for mut value in values {
            let id = serde_json::from_value(value["id"].take())?;
            let mut session = previous
                .iter()
                .position(|session| session.id == id)
                .map(|index| previous.swap_remove(index))
                .unwrap_or_else(|| AgentSession {
                    id,
                    ..AgentSession::default()
                });
            let input = serde_json::from_value(value["input"].take())?;
            let basket: Vec<ContextView> = serde_json::from_value(value["basket"].take())?;
            let basket = basket.into_iter().map(ContextItem::from_view).collect();
            if self.remote == RemoteState::Loading
                && (!session.model.input.is_empty() || !session.context.basket.is_empty())
            {
                session.restore_draft(input, basket);
                self.view_dirty = true;
            } else {
                if session.model.input != input {
                    session.model.input = input;
                    session.model.set_cursor(session.model.input.len(), false);
                    session.editor_scroll = None;
                }
                session.context.basket = basket;
            }
            session.rewind_applied = serde_json::from_value(value["rewind_applied"].take())?;
            session.failed_send_applied =
                serde_json::from_value(value["failed_send_applied"].take())?;
            self.sessions.push(session);
        }
        if self.remote == RemoteState::Loading {
            for session in previous {
                if !session.model.input.is_empty()
                    || !session.context.basket.is_empty()
                    || !session.model.pending.is_empty()
                {
                    self.sessions.push(session);
                    self.view_dirty = true;
                }
            }
        }
        self.current = self
            .sessions
            .iter()
            .position(|session| session.id == current)
            .unwrap_or(0);
        self.next_id = self.next_id.max(next_id);
        Ok(())
    }

    pub fn active(&self) -> &AgentSession {
        &self.sessions[self.current]
    }
    pub fn active_mut(&mut self) -> &mut AgentSession {
        &mut self.sessions[self.current]
    }

    pub fn prepare_layout(&mut self, columns: usize) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        // Commit with wrapping during draw so input keeps the displayed frame's geometry.
        self.apply_markdown();
        if self.active().render_request.is_none() {
            let session = self.active_mut();
            let selection = session.selection_prefixes().is_some();
            session.sync_rendered();
            // Cache invalidation only removes rendered text, so valid endpoints keep their prefixes.
            if selection && session.selection_prefixes().is_none() {
                session.clear_selection();
            }
        }
        self.request_markdown()?;
        self.active_mut().layout(columns);

        Ok(())
    }

    fn request_markdown(&mut self) -> io::Result<()> {
        if !self.open || self.active().render_request.is_some() {
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
            for session in &mut self.sessions {
                session.render_request = None;
            }
            self.next_render += 1;
            let job = MarkdownJob {
                session: self.active().id,
                request: self.next_render,
                messages,
            };
            self.active_mut().render_request = Some(job.request);
            self.markdown_worker.as_ref().unwrap().submit(job);
        }
        Ok(())
    }

    pub fn layout_fd(&self) -> RawFd {
        self.markdown_worker.as_ref().map_or(-1, Worker::fd)
    }

    pub fn poll_layout(&mut self) -> io::Result<bool> {
        let Some(worker) = &self.markdown_worker else {
            return Ok(false);
        };
        Ok(crate::eventfd::consume_notification(worker.fd())?
            && self.open
            && worker.ready(|_| true))
    }

    fn apply_markdown(&mut self) {
        let Some(result) = self.markdown_worker.as_mut().and_then(Worker::take) else {
            return;
        };
        let Some(session) = self.sessions.iter_mut().find(|session| {
            session.id == result.session && session.render_request == Some(result.request)
        }) else {
            return;
        };
        session.render_request = None;
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
        let selection = session.selection_checkpoint();
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
                        AgentLine::Text { message, range } if *message as usize == *index => {
                            Some((line, range.start as usize))
                        }
                        _ => None,
                    });
        }
        for (index, rendered) in result.messages {
            session.rendered[index] = Some(rendered);
        }
        if session.selection.is_some() && !session.selection_matches(selection.as_ref()) {
            session.clear_selection();
        }
        if session.append_from.is_none() {
            session.columns = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_layout(&mut self, columns: usize) {
        self.prepare_layout(columns).unwrap();
        while self.active().render_request.is_some() {
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

    pub fn new_session(&mut self) {
        let session = self.active_mut();
        session.end_selection();
        session.rewind_target = None;
        self.sessions.push(AgentSession {
            id: self.next_id,
            ..AgentSession::default()
        });
        self.next_id += 1;
        self.view_dirty = true;
        self.current = self.sessions.len() - 1;
        self.keyboard = AgentAction::Editor;
    }

    pub fn switch(&mut self, index: usize) {
        if index < self.sessions.len() {
            let session = self.active_mut();
            session.end_selection();
            session.rewind_target = None;
            self.current = index;
            self.keyboard = AgentAction::Editor;
        }
    }

    pub fn close(&mut self, index: usize) {
        if index >= self.sessions.len() {
            return;
        }
        let session = self.active_mut();
        session.end_selection();
        session.rewind_target = None;
        self.sessions.remove(index);
        self.view_dirty = true;
        if self.sessions.is_empty() {
            self.sessions.push(AgentSession {
                id: self.next_id,
                ..AgentSession::default()
            });
            self.next_id += 1;
        }
        self.current = if index < self.current {
            self.current - 1
        } else {
            self.current.min(self.sessions.len() - 1)
        };
        self.keyboard = AgentAction::Editor;
    }

    pub fn insert(&mut self, id: u64, text: &str) -> Result<(), String> {
        let session = self
            .sessions
            .iter_mut()
            .find(|session| session.id == id)
            .ok_or_else(|| "Conversation was closed; paste discarded".to_string())?;
        let range = session
            .model
            .selection()
            .unwrap_or(session.model.cursor..session.model.cursor);
        let changed = &session.model.input[range] != text;
        session.model.insert(text)?;
        session.editor_scroll = None;
        self.view_dirty |= changed;
        Ok(())
    }

    pub fn clear_input(&mut self) {
        self.view_dirty |= !self.active().model.input.is_empty();
        self.active_mut().model.clear_input();
        self.active_mut().prompt_index = None;
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
        let capacity = (6
            + self.sessions.len()
            + usize::from(session.model.busy)
            + session.context.basket.len() * 2
            + session.model.messages.len())
        .next_power_of_two();
        let mut actions = Vec::with_capacity(capacity);
        actions.extend([AgentAction::Editor, AgentAction::Output]);
        if session.model.busy {
            actions.push(AgentAction::Stop);
        }
        actions.push(AgentAction::ContextMenu);
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
        let available = columns.saturating_sub(6);
        if available == 0 {
            return Vec::new();
        }
        let count = self.sessions.len().min((available / 10).max(1));
        let start = self
            .current
            .saturating_sub(count - 1)
            .min(self.sessions.len() - count);
        // Reuse the column slot for desired widths while fitting the tabs.
        let mut tabs: Vec<_> = (start..start + count)
            .map(|index| {
                (
                    index,
                    (crate::width::text_width(self.sessions[index].title()) + 7).max(10),
                    0,
                )
            })
            .collect();
        let mut remaining = available;
        loop {
            let pending = tabs.iter().filter(|(_, _, width)| *width == 0).count();
            if pending == 0 {
                break;
            }
            let share = remaining / pending;
            let mut fitted = false;
            for (_, desired, width) in &mut tabs {
                if *width == 0 && *desired <= share {
                    *width = *desired;
                    remaining -= *desired;
                    fitted = true;
                }
            }
            if !fitted {
                let mut extra = remaining % pending;
                for (_, _, width) in &mut tabs {
                    if *width == 0 {
                        *width = share + usize::from(extra > 0);
                        extra = extra.saturating_sub(1);
                    }
                }
                break;
            }
        }
        let mut x = 0;
        for (_, column, width) in &mut tabs {
            *column = x;
            x += *width;
        }
        tabs
    }
}

pub(crate) fn wrap_context(text: &str, columns: usize) -> Vec<Range<usize>> {
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
            let mut width = 0;
            for (index, c) in line.char_indices() {
                let index = offset + index;
                let c = if c.is_control() { ' ' } else { c };
                let next = usize::from(scalar_width(c).min(2));
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
    let mut used = 0.;
    for (index, c) in text[range.clone()].char_indices() {
        let width = f32::from(scalar_width(if c.is_control() { ' ' } else { c }).min(2));
        if column < used + width * 0.5 {
            return range.start + index;
        }
        used += width;
    }
    range.end
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
    pub expanded_tools: HashSet<String>,
    pub lines: Vec<AgentLine>,
    pub columns: usize,
    append_from: Option<(usize, usize)>,
    rendered: Vec<Option<RenderedMarkdown>>,
    render_request: Option<u64>,
    pub editor_scroll: Option<usize>,
    pub rewind_target: Option<u64>,
    escape_pending: bool,
    prompt_index: Option<usize>,
    prompt_draft: String,
    selection: Option<ChatSelection>,
    rewind_applied: Option<u64>,
    failed_send_applied: Option<u64>,
}

impl AgentSession {
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

    pub fn rewind_index(&self) -> Option<usize> {
        let turn = self.rewind_target?;
        self.model
            .messages
            .iter()
            .position(|message| message.turn == turn && message.role == "You")
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

    pub fn markdown(&self, index: usize) -> Option<&crate::agent::markdown::Markdown> {
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
        match self.lines.get(index)? {
            AgentLine::Role(message) => {
                let text = &self.model.messages.get(*message as usize)?.role;
                Some((
                    TranscriptPoint {
                        message: *message as usize,
                        part: 0,
                        offset: 0,
                    },
                    0..text.len(),
                    text,
                ))
            }
            AgentLine::Text { message, range } => {
                self.model.messages.get(*message as usize)?;
                let text = self.message_text(*message as usize);
                let range = range.start as usize..range.end as usize;
                text.get(range.clone())?;
                Some((
                    TranscriptPoint {
                        message: *message as usize,
                        part: 1,
                        offset: 0,
                    },
                    range,
                    text,
                ))
            }
            AgentLine::Blank => None,
        }
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

    fn selection_checkpoint(&self) -> Option<[(u64, Option<String>, String); 2]> {
        self.selection_prefixes().map(|prefixes| {
            prefixes.map(|(turn, id, text)| (turn, id.map(str::to_owned), text.to_owned()))
        })
    }

    fn selection_matches(&self, previous: Option<&[(u64, Option<String>, String); 2]>) -> bool {
        self.selection_prefixes().is_some_and(|current| {
            previous.is_none_or(|previous| {
                current
                    == previous
                        .each_ref()
                        .map(|(turn, id, text)| (*turn, id.as_deref(), text.as_str()))
            })
        })
    }

    fn selection_prefixes(&self) -> Option<[(u64, Option<&str>, &str); 2]> {
        let selection = self.selection.as_ref()?;
        let checkpoint = |point: TranscriptPoint| {
            let message = self.model.messages.get(point.message)?;
            let text = if point.part == 0 {
                &message.role
            } else {
                self.message_text(point.message)
            };
            Some((
                message.turn,
                message.id.as_deref(),
                text.get(..point.offset)?,
            ))
        };
        Some([checkpoint(selection.anchor)?, checkpoint(selection.head)?])
    }

    fn selected_range(
        selection: &ChatSelection,
        mut first: TranscriptPoint,
        range: Range<usize>,
    ) -> Option<Range<usize>> {
        let (start, end) = (
            selection.anchor.min(selection.head),
            selection.anchor.max(selection.head),
        );
        first.offset = range.start;
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

    pub fn selection_columns(&self, line: usize) -> Option<Range<usize>> {
        let selection = self.selection.as_ref()?;
        let (point, range, text) = self.line_text(line)?;
        let selected = Self::selected_range(selection, point, range.clone())?;
        let start = crate::width::text_width(&text[range.start..selected.start]);
        Some(start..start + crate::width::text_width(&text[selected]))
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

    pub fn editor_layout(&self, columns: usize, rows: usize) -> EditorLines {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let prefix = &self.model.input[..self.model.cursor];
        let mut lines = wrap_context(&self.model.input, columns);
        let (caret_lines, caret_range) = if self.model.cursor == self.model.input.len() {
            (lines.len(), lines.last().cloned().unwrap_or(0..0))
        } else {
            let row = lines.partition_point(|range| range.end < self.model.cursor);
            // A caret inside CRLF falls between adjacent displayed ranges.
            let row = row.saturating_sub(usize::from(lines[row].start > self.model.cursor));
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
        let text = &prefix[caret_range];
        let width = if text.is_ascii() {
            text.len()
        } else {
            text.chars()
                .map(|c| usize::from(scalar_width(if c.is_control() { ' ' } else { c }).min(2)))
                .sum::<usize>()
        };
        let caret_row = caret_lines.saturating_sub(1) + usize::from(width >= columns);
        if lines.is_empty() {
            lines.push(0..0);
        }
        if caret_row >= lines.len() {
            lines.push(self.model.input.len()..self.model.input.len());
        }
        let first = self
            .editor_scroll
            .unwrap_or_else(|| caret_row.saturating_sub(rows - 1))
            .min(lines.len().saturating_sub(rows));
        EditorLines {
            lines,
            first,
            caret_row,
            caret_column: width % columns,
        }
    }
    pub fn move_vertical(&mut self, layout: EditorLines, up: bool, extend: bool) {
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
        let row = row.floor().clamp(0., rows.saturating_sub(1) as f32) as usize;
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
    pub fn add_context(&mut self, item: ContextItem) -> Result<(), &'static str> {
        if self.context.basket.iter().any(|old| {
            old.source == item.source && old.label == item.label && old.snapshot == item.snapshot
        }) {
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
    pub fn toggle_tool(&mut self, index: usize) {
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
    pub fn send(&mut self, client: &Client) -> io::Result<bool> {
        if self.model.busy || self.model.input.trim().is_empty() {
            return Ok(false);
        }
        let pane = client
            .snapshot()
            .ok_or_else(|| io::Error::other("No workspace"))?
            .focused;
        if !client.can_agent_input() {
            return Err(io::Error::other("Workspace is read-only"));
        }
        let mut request = serde_json::json!({
            "action": "send",
            "pane": pane,
        });
        request["basket"] = serde_json::Value::Array(
            self.context
                .basket
                .drain(..)
                .map(|item| {
                    serde_json::Value::Array(vec![
                        item.label.into(),
                        item.source.into(),
                        item.kind.into(),
                        item.snapshot.text.into(),
                        item.snapshot.incomplete.into(),
                    ])
                })
                .collect(),
        );
        request["input"] = serde_json::Value::String(std::mem::take(&mut self.model.input));
        self.model.pending.push(request);
        self.model.clear_input();
        self.model.busy = true;
        self.model.scroll = 0;
        self.clear_selection();
        self.context.preview = None;
        self.context.menu = false;
        self.columns = 0;
        self.editor_scroll = None;
        Ok(true)
    }

    pub fn attach_screen(
        &mut self,
        client: &Client,
        pane: PaneId,
        selection: bool,
    ) -> Result<(), &'static str> {
        let screen = client.screens().get(&pane).ok_or("No active pane")?;
        let range = if selection {
            Some(
                screen
                    .selection_range()
                    .ok_or("Select text in the terminal first")?,
            )
        } else {
            None
        };
        self.add_context(ContextItem {
            label: if selection {
                "Visible selection"
            } else {
                "Visible screen"
            }
            .into(),
            kind: if selection { "Selection" } else { "Screen" }.into(),
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
                    self.line_text(index).map(|(mut point, range, _)| {
                        point.offset = range.start;
                        (point, last - index)
                    })
                })
            })
            .flatten();
        let mut lines = std::mem::take(&mut self.lines);
        if let Some((line, _)) = append {
            lines.truncate(line);
        } else {
            lines.clear();
        }
        self.expanded_tools.retain(|id| {
            self.model
                .messages
                .iter()
                .any(|message| message.id.as_ref() == Some(id))
        });
        let first = append.map_or(0, |_| self.model.messages.len() - 1);
        let offset = append.map_or(0, |(_, offset)| offset);
        for (index, message) in self.model.messages.iter().enumerate().skip(first) {
            let tool = message.id.is_some();
            if tool {
                lines.push(AgentLine::Role(index as u32));
            }
            if !tool
                || message
                    .id
                    .as_ref()
                    .is_some_and(|id| self.expanded_tools.contains(id))
            {
                let reserved = if message.role == "You"
                    && columns >= crate::width::text_width(REWIND_LABEL) + 5
                {
                    1 + crate::width::text_width(REWIND_LABEL)
                } else if tool || message.role == "You" {
                    2
                } else {
                    0
                };
                wrap_context_into(
                    &self.message_text(index)[offset..],
                    columns.saturating_sub(reserved),
                    |range| {
                        lines.push(AgentLine::Text {
                            message: index as u32,
                            range: (offset + range.start) as u32..(offset + range.end) as u32,
                        });
                    },
                );
            }
            lines.push(AgentLine::Blank);
        }
        self.lines = lines;
        if let Some((anchor, gap)) = anchor {
            if let Some(index) = self.lines.iter().position(|line| match line {
                AgentLine::Role(message) => *message as usize == anchor.message && anchor.part == 0,
                AgentLine::Text { message, range } => {
                    *message as usize == anchor.message
                        && anchor.part == 1
                        && (range.contains(&(anchor.offset as u32))
                            || range.is_empty() && range.start as usize == anchor.offset)
                }
                AgentLine::Blank => false,
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
    pub fn agent_tab(&mut self, reverse: bool) {
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
        if let AgentAction::Tool(index) | AgentAction::Copy(index) | AgentAction::Rewind(index) =
            self.agent.keyboard
            && let Some(line) = self.agent.sessions[self.agent.current]
                .lines
                .iter()
                .rposition(|line| match line {
                    AgentLine::Role(i) | AgentLine::Text { message: i, .. } => *i as usize == index,
                    AgentLine::Blank => false,
                })
        {
            self.agent.sessions[self.agent.current].model.scroll = self.agent.sessions
                [self.agent.current]
                .lines
                .len()
                .saturating_sub(line + 1);
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
            AgentAction::Rewind(_) => "History · j/k selects · Esc exits · Enter confirms",
        };
        self.message(hint);
    }
    pub fn agent_key(&mut self, symbol: u32, shift: bool, control: bool) -> Option<AgentAction> {
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
                if self.agent.sessions[self.agent.current].context.menu {
                    self.agent.sessions[self.agent.current].context.menu = false;
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
            AgentAction::Stop => self
                .agent
                .active_mut()
                .model
                .pending
                .push(serde_json::json!({"action":"abort"})),
            AgentAction::ContextMenu => {
                self.agent.sessions[self.agent.current].context.menu =
                    !self.agent.sessions[self.agent.current].context.menu;
                self.focus = Focus::Controls;
                self.agent.keyboard = if self.agent.sessions[self.agent.current].context.menu {
                    AgentAction::Selection
                } else {
                    AgentAction::ContextMenu
                };
            }
            AgentAction::Selection | AgentAction::Screen => {
                if let Some(pane) = client.snapshot().map(|s| s.focused) {
                    let count = self.agent.active().context.basket.len();
                    self.agent
                        .active_mut()
                        .attach_screen(client, pane, action == AgentAction::Selection)
                        .map_err(io::Error::other)?;
                    self.agent.view_dirty |= count != self.agent.active().context.basket.len();
                }
                if !self.agent.open {
                    self.toggle_agent();
                }
                self.focus = Focus::Editor;
            }
            AgentAction::Inspect(index) => {
                if let Some(item) = self.agent.sessions[self.agent.current]
                    .context
                    .basket
                    .get(index)
                {
                    self.agent.sessions[self.agent.current].context.preview =
                        Some(ContextPreview::new(item.clone(), 0));
                    self.agent.keyboard = AgentAction::CopyPreview;
                    self.focus = Focus::Controls;
                }
            }
            AgentAction::Remove(index) => {
                if index < self.agent.sessions[self.agent.current].context.basket.len() {
                    self.agent.sessions[self.agent.current]
                        .context
                        .basket
                        .remove(index);
                    self.agent.view_dirty = true;
                }
                self.agent.keyboard = AgentAction::ContextMenu;
            }
            AgentAction::Tool(index) => {
                self.focus = Focus::Output;
                self.agent.active_mut().toggle_tool(index);
            }
            AgentAction::Copy(index) => {
                self.focus = Focus::Output;
                return Ok(self.agent.sessions[self.agent.current]
                    .model
                    .messages
                    .get(index)
                    .map(|message| message.text.clone()));
            }
            AgentAction::CopyPreview => {
                return Ok(self.agent.sessions[self.agent.current]
                    .context
                    .preview
                    .as_ref()
                    .map(|preview| preview.item.snapshot.text.clone()));
            }
            AgentAction::ClosePreview => {
                self.agent.sessions[self.agent.current].context.preview = None;
                self.focus = Focus::Editor;
            }
        }
        Ok(None)
    }
    fn select_agent_rewind(&mut self, index: usize) {
        let session = self.agent.active_mut();
        let Some(message) = session
            .model
            .messages
            .get(index)
            .filter(|message| message.role == "You")
        else {
            return;
        };
        session.rewind_target = Some(message.turn);
        session.clear_selection();
        if let Some(line) = session.lines.iter().position(
            |line| matches!(line, AgentLine::Text { message, .. } if *message as usize == index),
        ) {
            session.model.scroll = session.lines.len().saturating_sub(line + 1);
        }
        self.agent.keyboard = AgentAction::Rewind(index);
        self.focus = Focus::Controls;
    }

    fn previous_agent_rewind(&mut self) {
        let session = self.agent.active();
        let before = session
            .rewind_index()
            .unwrap_or(session.model.messages.len());
        if let Some(index) = session.model.messages[..before]
            .iter()
            .rposition(|message| message.role == "You")
        {
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

    pub(crate) fn confirm_agent_rewind(&mut self) {
        let turn = self.agent.active().rewind_target;
        self.cancel_rewind();
        self.focus = Focus::Editor;
        if let Some(turn) = turn {
            self.agent
                .active_mut()
                .model
                .pending
                .push(serde_json::json!({"action":"rewind", "turn":turn}));
        }
    }

    pub fn agent_rewind_key(&mut self, symbol: u32) -> bool {
        if !self.focus.is_agent() {
            self.cancel_rewind();
            return false;
        }
        if !self.agent.open
            || self.agent.active().context.menu
            || self.agent.active().context.preview.is_some()
        {
            return false;
        }
        if self.agent.active().rewind_target.is_some() {
            match symbol {
                keysym::ESCAPE => self.cancel_rewind(),
                keysym::RETURN | keysym::KP_ENTER => self.confirm_agent_rewind(),
                0x6b => self.previous_agent_rewind(),
                0x6a => {
                    let session = self.agent.active();
                    if let Some(index) = session.rewind_index().and_then(|index| {
                        session.model.messages[index + 1..]
                            .iter()
                            .position(|message| message.role == "You")
                            .map(|offset| index + 1 + offset)
                    }) {
                        self.select_agent_rewind(index);
                    }
                }
                _ => (),
            }
            return true;
        }
        if symbol == keysym::ESCAPE {
            if self.agent.active().escape_pending {
                self.agent.active_mut().escape_pending = false;
                self.previous_agent_rewind();
            } else {
                self.agent.active_mut().escape_pending = true;
            }
            return true;
        }
        self.agent.active_mut().escape_pending = false;
        false
    }

    /// Handles a key aimed at the Agent panel; `None` passes it to the next region.
    pub(crate) fn agent_key_input(
        &mut self,
        client: &mut Client,
        key: &crate::input::Key<'_>,
        viewport: (usize, usize),
    ) -> io::Result<Option<crate::input::KeyResult>> {
        use crate::input::KeyResult;
        let (control, shift, letter, paste) =
            (key.control(), key.shift(), key.letter(), key.paste());
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
        if self.agent.active().context.preview.is_some() {
            if matches!(key.symbol, keysym::TAB | keysym::LEFT_TAB)
                || self.focus == Focus::Controls
                    && matches!(key.symbol, keysym::RETURN | keysym::KP_ENTER | 32)
            {
                if let Some(action) = self.agent_key(key.symbol, shift, control) {
                    return self.activate_agent(action, client).map(|copy| {
                        Some(copy.map_or(KeyResult::Handled, |text| KeyResult::Copy(text, false)))
                    });
                }
            } else if let Some(preview) = &mut self.agent.active_mut().context.preview {
                match key.symbol {
                    keysym::ESCAPE => self.agent.active_mut().context.preview = None,
                    keysym::UP | keysym::PAGE_UP => {
                        preview.scroll = preview
                            .scroll
                            .saturating_sub(if key.symbol == keysym::PAGE_UP { 10 } else { 1 })
                    }
                    keysym::DOWN | keysym::PAGE_DOWN => {
                        preview.scroll =
                            preview
                                .scroll
                                .saturating_add(if key.symbol == keysym::PAGE_DOWN {
                                    10
                                } else {
                                    1
                                })
                    }
                    _ => (),
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
        if self.agent.open && self.focus.is_agent() {
            if matches!(key.symbol, keysym::TAB | keysym::LEFT_TAB)
                || control && key.symbol == 32
                || self.focus == Focus::Controls
            {
                if let Some(action) = self.agent_key(key.symbol, shift, control) {
                    return self
                        .activate_agent(action, client)
                        .map(|copy| {
                            copy.map_or(KeyResult::Handled, |text| KeyResult::Copy(text, false))
                        })
                        .map(Some);
                }
                return Ok(Some(KeyResult::Handled));
            }
            if self.focus == Focus::Output {
                let model = &mut self.agent.active_mut().model;
                match key.symbol {
                    keysym::ESCAPE => {
                        self.agent.active_mut().clear_selection();
                        self.focus = Focus::Terminal;
                    }
                    keysym::END => model.scroll = 0,
                    keysym::UP | keysym::PAGE_UP => {
                        model.scroll = model
                            .scroll
                            .saturating_add(if key.symbol == keysym::PAGE_UP { 10 } else { 1 })
                    }
                    keysym::DOWN | keysym::PAGE_DOWN => {
                        model.scroll =
                            model
                                .scroll
                                .saturating_sub(if key.symbol == keysym::PAGE_DOWN {
                                    10
                                } else {
                                    1
                                })
                    }
                    _ => (),
                }
                return Ok(Some(KeyResult::Handled));
            }
            if !(control && matches!(letter, Some('h' | 'j' | 'k' | 'l'))) {
                self.agent.active_mut().prompt_index = None;
            }
            if paste {
                return Ok(Some(KeyResult::Paste));
            }
            if control {
                match letter {
                    Some('a') => self.agent.active_mut().model.select_all(),
                    Some('x') => {
                        if let Some(range) = self.agent.active().model.selection() {
                            return Ok(Some(KeyResult::Copy(
                                self.agent.active().model.input[range].to_owned(),
                                true,
                            )));
                        }
                    }
                    Some('u') => self.agent.clear_input(),
                    Some('h' | 'l') => self.agent.active_mut().model.key(
                        if letter == Some('h') {
                            keysym::LEFT
                        } else {
                            keysym::RIGHT
                        },
                        false,
                    ),
                    Some('j' | 'k') => {
                        let up = letter == Some('k');
                        let layout = self
                            .agent
                            .active()
                            .editor_layout(viewport.0.max(1), viewport.1.max(1));
                        if (up && layout.caret_row == 0)
                            || (!up && layout.caret_row + 1 == layout.lines.len())
                        {
                            let session = self.agent.active_mut();
                            let prompts: Vec<_> = session
                                .model
                                .messages
                                .iter()
                                .filter(|message| message.role == "You")
                                .map(|message| message.text.clone())
                                .collect();
                            let current = session.prompt_index.unwrap_or(prompts.len());
                            if session.prompt_index.is_none() {
                                session.prompt_draft = session.model.input.clone();
                            }
                            let index = if up {
                                current.saturating_sub(1)
                            } else {
                                (current + 1).min(prompts.len())
                            };
                            session.model.input = prompts
                                .get(index)
                                .cloned()
                                .unwrap_or_else(|| session.prompt_draft.clone());
                            session.prompt_index = (index < prompts.len()).then_some(index);
                            session
                                .model
                                .set_cursor(if up { 0 } else { session.model.input.len() }, false);
                            self.agent.view_dirty = true;
                        } else {
                            self.agent.active_mut().move_vertical(layout, up, false);
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
                    keysym::RETURN | keysym::KP_ENTER if shift => {
                        if let Err(error) = self.agent.insert(self.agent.active().id, "\n") {
                            self.message(error);
                        }
                    }
                    keysym::RETURN | keysym::KP_ENTER => {
                        match self.agent.active_mut().send(client) {
                            Ok(changed) => self.agent.view_dirty |= changed,
                            Err(error) => self.message(format!("Agent: {error}")),
                        }
                    }
                    keysym::UP | keysym::DOWN => {
                        let layout = self
                            .agent
                            .active()
                            .editor_layout(viewport.0.max(1), viewport.1.max(1));
                        self.agent.active_mut().move_vertical(
                            layout,
                            key.symbol == keysym::UP,
                            shift,
                        );
                    }
                    _ => {
                        let length = self.agent.active().model.input.len();
                        self.agent.active_mut().model.key(key.symbol, shift);
                        self.agent.view_dirty |= self.agent.active().model.input.len() != length;
                        if !key.text.is_empty()
                            && let Err(error) = self.agent.insert(self.agent.active().id, key.text)
                        {
                            self.message(error);
                        }
                    }
                }
            }
            self.agent.active_mut().editor_scroll = None;
            return Ok(Some(key.text_result()));
        }
        Ok(None)
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

    #[test]
    fn tab_dot_tracks_agent_outcome_without_treating_tool_errors_as_failure() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        let config = Config::default();
        let size = WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 0,
            cell_height: 0,
        };
        let side = Rect {
            x: 40,
            y: 1,
            cols: 40,
            rows: 22,
        };
        for (role, busy, expected) in [
            ("Agent", false, 0xb5d2a2),
            ("Error", true, 0xfe9b30),
            ("Error", false, 0xef5350),
            ("Blocked", false, 0xef5350),
            ("shell error", false, 0xb5d2a2),
        ] {
            let model = &mut ui.agent.active_mut().model;
            model.busy = busy;
            model.messages = vec![crate::workspace::AgentMessage {
                turn: 1,
                role: role.into(),
                text: "result".into(),
                id: None,
            }];
            let panel = draw(&mut ui, &config, side, size, None, "").unwrap();
            let row = &panel.grid.rows[0];
            assert_eq!(row.cells[1].c, '•');
            assert_eq!(row.cells[1].fg, rgb(expected));
            assert!(row.cells[3].flags.contains(Flags::BOLD));
            assert_eq!(row.cells[3].fg, rgb(0xffffff));
            assert!(
                row.cells
                    .iter()
                    .all(|cell| !cell.flags.contains(Flags::UNDERLINE))
            );
            assert!(row.cells[1..].iter().all(|cell| cell.bg == rgb(0x191919)));
            assert!(row.cells.iter().any(|cell| cell.c == '╱'));
        }
    }

    #[test]
    fn shared_editor_preserves_composition_and_only_emits_changed_rows() {
        let mut ui = Ui::new().unwrap();
        ui.agent.open = true;
        ui.focus = Focus::Editor;
        ui.agent.active_mut().model.insert("e\u{301}中").unwrap();
        let config = Config::default();
        let size = WindowSize {
            num_cols: 80,
            num_lines: 24,
            cell_width: 0,
            cell_height: 0,
        };
        let side = Rect {
            x: 40,
            y: 1,
            cols: 40,
            rows: 22,
        };
        let first = draw(&mut ui, &config, side, size, None, "").unwrap();
        assert_eq!(first.grid.runs(Some(&first.grid), &[]).count(), 0);
        let composing = draw(&mut ui, &config, side, size, None, "文").unwrap();
        assert_eq!(ui.agent.active().model.input, "e\u{301}中");
        let editor = editor(&ui, side).0;
        assert_eq!(composing.cursor, Some((editor.x + 5, editor.y)));
        assert!(
            composing
                .grid
                .runs(Some(&first.grid), &[])
                .all(|run| run.y >= editor.y)
        );
        let mut output = Vec::new();
        composing.grid.ansi(&mut output, None, &[]).unwrap();
        let mut terminal = crate::core::Core::new(size, config.foreground, config.background);
        crate::vt::Parser::new().advance(&mut terminal, &output);
        for (row, cells) in composing.grid.rows.iter().enumerate() {
            let observed = terminal.row(usize::from(composing.grid.rect.y) + row);
            for (column, cell) in cells.cells.iter().enumerate() {
                assert_eq!(
                    observed.cells[usize::from(composing.grid.rect.x) + column],
                    *cell
                );
                assert_eq!(
                    observed
                        .extra(usize::from(composing.grid.rect.x) + column)
                        .map_or(&[][..], |extra| extra.zerowidth.as_slice()),
                    cells
                        .extra(column)
                        .map_or(&[][..], |extra| extra.zerowidth.as_slice()),
                );
            }
        }
    }

    #[test]
    fn first_agent_restore_preserves_typing_while_loading() {
        for saved in ["", "saved"] {
            let mut remote = AgentPanel::default();
            remote.active_mut().model.input = saved.into();
            let mut local = AgentPanel {
                remote: RemoteState::Loading,
                ..AgentPanel::default()
            };
            local.active_mut().model.input = "typed".into();
            local.active_mut().model.cursor = 5;
            local.restore_view_state(remote.view_state()).unwrap();
            assert_eq!(
                local.active().model.input,
                if saved.is_empty() {
                    "typed"
                } else {
                    "saved\ntyped"
                }
            );
            assert_eq!(
                local.active().model.cursor,
                local.active().model.input.len()
            );
            assert!(local.view_dirty);
        }
    }

    #[test]
    fn initial_idle_model_keeps_a_send_queued_while_loading_busy() {
        use crate::wire::{Peer, Response};
        use crate::workspace::{Action, Snapshot};

        let path = std::env::temp_dir().join(format!(
            "nextty-ui-loading-send-{}.sock",
            std::process::id()
        ));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path).unwrap();
        let (stream, _) = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        let mut peer = Peer::new(stream).unwrap();
        peer.send_response(Response::State(
            Snapshot {
                name: "loading".into(),
                controller: true,
                focused: 1,
                active_tab: 1,
                tabs: Vec::new(),
                panes: Vec::new(),
            },
            Vec::new(),
            0,
        ))
        .unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();

        let saved = AgentPanel::default();
        let mut local = AgentPanel {
            open: true,
            remote: RemoteState::Loading,
            ..AgentPanel::default()
        };
        let item = ContextItem {
            label: "attachment".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "registers".into(),
                incomplete: false,
            },
        };
        local.active_mut().context.basket.push(item.clone());
        local.active_mut().model.insert("first draft").unwrap();
        assert!(local.active_mut().send(&client).unwrap());
        peer.send_response(Response::Agent(AgentEvent::Draft(
            serde_json::to_vec(&saved.view_state()).unwrap(),
        )))
        .unwrap();
        peer.send_response(Response::Agent(AgentEvent::Model(
            saved.model_snapshot().remove(0),
        )))
        .unwrap();
        peer.send_response(Response::Agent(AgentEvent::Ready))
            .unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();
        local.poll_remote(&mut client, &Config::default()).unwrap();
        assert!(local.active().model.pending.is_empty());
        local.active_mut().model.insert("next draft").unwrap();
        assert!(!local.active_mut().send(&client).unwrap());
        assert!(local.active().model.busy);
        assert_eq!(local.active().model.input, "next draft");
        client.flush().unwrap();
        let mut requests = Vec::new();
        while let Some(action) = peer.next_action().unwrap() {
            if let Action::AgentRequest(payload) = action {
                requests.push(serde_json::from_slice::<serde_json::Value>(&payload).unwrap());
            }
        }
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["input"], "first draft");
        assert_eq!(requests[0]["basket"], serde_json::json!([item.view()]));

        peer.send_response(Response::Agent(AgentEvent::Model(
            saved.model_snapshot().remove(0),
        )))
        .unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();
        local.poll_remote(&mut client, &Config::default()).unwrap();
        assert!(!local.active().model.busy);
        assert_eq!(local.active().model.input, "next draft");
    }

    impl AgentPanel {
        fn model_snapshot(&self) -> Vec<AgentModelPatch> {
            self.sessions
                .iter()
                .map(|session| AgentModelPatch {
                    session: session.id,
                    busy: session.model.busy,
                    revision: session.model.revision,
                    messages: session.model.messages.clone(),
                    recovery: b"{}".to_vec(),
                    ..AgentModelPatch::default()
                })
                .collect()
        }

        fn restore_models(&mut self, patches: Vec<AgentModelPatch>) -> io::Result<bool> {
            let mut redraw = false;
            for patch in patches {
                redraw |= self.apply_model(&patch)?;
            }
            self.request_markdown()?;
            Ok(redraw)
        }
    }

    #[test]
    fn failed_send_restores_once_and_preserves_newer_edits() {
        let mut panel = AgentPanel::default();
        let item = ContextItem {
            label: "old attachment".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "registers".into(),
                incomplete: false,
            },
        };
        panel.active_mut().model.insert("next draft").unwrap();
        panel.active_mut().model.anchor = Some(0);
        panel.active_mut().context.basket.push(item.clone());
        let mut models = panel.model_snapshot();
        models[0].recovery = serde_json::to_vec(
            &serde_json::json!({"failed_send":[3, "failed draft", [item.view()]]}),
        )
        .unwrap();
        panel.restore_models(models.clone()).unwrap();
        assert_eq!(panel.active().model.input, "failed draft\nnext draft");
        assert_eq!(
            panel.active().model.cursor,
            "failed draft\nnext draft".len()
        );
        assert_eq!(panel.active().model.anchor, Some("failed draft\n".len()));
        assert_eq!(panel.active().context.basket.len(), 2);
        panel.restore_models(models.clone()).unwrap();
        assert_eq!(panel.active().model.input, "failed draft\nnext draft");
        let view = panel.view_state();
        let mut empty = AgentPanel::default();
        empty.restore_models(models.clone()).unwrap();
        assert_eq!(empty.active().model.input, "failed draft");
        assert_eq!(empty.active().model.cursor, "failed draft".len());
        assert_eq!(empty.active().context.basket.len(), 1);
        let mut attached = AgentPanel::default();
        attached.restore_view_state(view).unwrap();
        attached.restore_models(models).unwrap();
        assert_eq!(attached.active().model.input, "failed draft\nnext draft");
        assert_eq!(attached.active().context.basket.len(), 2);
    }

    #[test]
    fn read_only_stop_and_rewind_report_rejection_without_sticking_busy() {
        let path =
            std::env::temp_dir().join(format!("nextty-ui-readonly-{}.sock", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = Client::connect(&path).unwrap();
        let (stream, _) = listener.accept().unwrap();
        std::fs::remove_file(path).unwrap();
        let mut peer = crate::wire::Peer::new(stream).unwrap();
        peer.send_response(crate::wire::Response::State(
            crate::workspace::Snapshot {
                name: "readonly".into(),
                controller: false,
                focused: 1,
                active_tab: 1,
                tabs: Vec::new(),
                panes: Vec::new(),
            },
            Vec::new(),
            0,
        ))
        .unwrap();
        peer.flush().unwrap();
        client.poll().unwrap();
        let mut ui = Ui::new().unwrap();
        ui.agent.remote = RemoteState::Ready;
        ui.agent
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 7,
                role: "You".into(),
                text: "earlier".into(),
                id: None,
            });
        ui.agent.active_mut().rewind_target = Some(7);
        ui.confirm_agent_rewind();
        ui.activate_agent(AgentAction::Stop, &client).unwrap();
        assert_eq!(ui.agent.active().model.pending[0]["turn"], 7);
        assert!(!ui.agent.active().model.busy);
        ui.agent.active_mut().model.pending.push(serde_json::json!({
            "action":"send", "input":"rejected draft", "basket":[],
        }));
        ui.agent.active_mut().model.busy = true;
        ui.agent.active_mut().model.insert("newer draft").unwrap();
        assert_eq!(
            ui.poll_agent(&mut client, &Config::default()).unwrap(),
            (true, false)
        );
        assert!(ui.agent.active().model.pending.is_empty());
        assert!(!ui.agent.active().model.busy);
        assert_eq!(ui.status.as_ref().unwrap().0, "Workspace is read-only");
        assert_eq!(ui.agent.active().model.input, "rejected draft\nnewer draft");
    }

    #[test]
    fn trimming_an_expanded_tool_marks_the_view_without_user_input() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "shell · done".into(),
                text: "output".into(),
                id: Some("old-tool".into()),
            });
        panel.active_mut().expanded_tools.insert("old-tool".into());
        panel.prepare_layout(40).unwrap();
        panel.view_dirty = false;
        let mut models = panel.model_snapshot();
        models[0].messages.clear();
        panel.restore_models(models).unwrap();
        assert!(!panel.view_dirty);
        panel.prepare_layout(40).unwrap();
        assert!(panel.active().expanded_tools.is_empty());
        assert!(!panel.view_dirty);
        assert_eq!(panel.active().model.scroll, 0);
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
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "You".into(),
                text: "previous".into(),
                id: None,
            });
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
        assert_eq!(
            ui.agent.active().model.pending.last().unwrap()["action"],
            "abort"
        );
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
    fn markdown_completion_wakes_the_final_frame_and_uses_the_current_width() {
        let mut panel = AgentPanel::default();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "Agent".into(),
                text: "**done** 中文".into(),
                id: None,
            });
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.layout_fd(), -1);
        assert!(panel.active().lines.is_empty());
        panel.open = true;
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "");
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        assert!(panel.poll_layout().unwrap());
        assert_eq!(panel.active().message_text(0), "");
        panel.prepare_layout(6).unwrap();
        assert_eq!(panel.active().message_text(0), "done 中文");
        assert_eq!(panel.active().lines.len(), 3);
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().lines.len(), 2);
        assert!(panel.active().render_request.is_none());
    }

    #[test]
    fn markdown_appends_request_work_without_redrawing_until_completion() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "Agent".into(),
                text: "**first**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        panel.wait_layout(40);
        assert!(!panel.poll_layout().unwrap());
        owner.active_mut().model.messages[0].text.push_str(" next");
        assert!(!panel.restore_models(owner.model_snapshot()).unwrap());
        assert!(panel.active().render_request.is_some());
        assert_eq!(panel.active().message_text(0), "first");
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        assert!(panel.poll_layout().unwrap());
        assert_eq!(panel.active().message_text(0), "first");
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "first next");

        owner.active_mut().model.messages[0].text = "**other** next".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        panel.wait_layout(40);
        assert_eq!(panel.active().message_text(0), "other next");

        owner.active_mut().model.busy = true;
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.busy = false;
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        let message = &mut owner.active_mut().model.messages[0];
        message.role = "run · running".into();
        message.id = Some("tool".into());
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.messages[0].text = "tool output".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
        owner.active_mut().model.messages[0].role = "run · done".into();
        assert!(panel.restore_models(owner.model_snapshot()).unwrap());
    }

    #[test]
    fn streaming_markdown_progress_survives_new_text_before_the_next_frame() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "Agent".into(),
                text: "**first**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.restore_models(owner.model_snapshot()).unwrap();
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        owner.active_mut().model.messages[0]
            .text
            .push_str("\n\nnext");
        panel.restore_models(owner.model_snapshot()).unwrap();
        assert!(panel.poll_layout().unwrap());
        panel.prepare_layout(40).unwrap();
        assert_eq!(panel.active().message_text(0), "first");
        panel.wait_layout(40);
        assert_eq!(panel.active().message_text(0), "first\n\nnext");
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
                .push(crate::workspace::AgentMessage {
                    turn: 1,
                    role: "Agent".into(),
                    text: text.into(),
                    id: None,
                });
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
    fn markdown_results_ignore_outdated_requests_and_sources() {
        for restore in [false, true] {
            let mut owner = AgentPanel::default();
            owner
                .active_mut()
                .model
                .messages
                .push(crate::workspace::AgentMessage {
                    turn: 1,
                    role: "Agent".into(),
                    text: "**old".into(),
                    id: None,
                });
            let mut panel = AgentPanel {
                open: true,
                ..AgentPanel::default()
            };
            panel.restore_models(owner.model_snapshot()).unwrap();
            let mut fd = libc::pollfd {
                fd: panel.layout_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
            if restore {
                owner.active_mut().model.messages[0].text = "**new**".into();
                panel.restore_models(owner.model_snapshot()).unwrap();
            } else {
                panel.active_mut().model.messages[0].text = "**new**".into();
            }
            assert!(panel.poll_layout().unwrap());
            panel.prepare_layout(40).unwrap();
            assert_eq!(panel.active().message_text(0), "");
            panel.wait_layout(40);
            assert_eq!(panel.active().message_text(0), "new");
        }
    }

    #[test]
    fn rewinding_discards_a_completed_markdown_result() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "Agent".into(),
                text: "**discarded reply**".into(),
                id: None,
            });
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.restore_models(owner.model_snapshot()).unwrap();
        let mut fd = libc::pollfd {
            fd: panel.layout_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 5000) }, 1);
        owner.active_mut().model.messages.clear();
        let mut models = owner.model_snapshot();
        models[0].recovery =
            serde_json::to_vec(&serde_json::json!({"rewind_restore":[1, "restored draft", []]}))
                .unwrap();
        panel.restore_models(models).unwrap();
        assert!(panel.poll_layout().unwrap());
        panel.prepare_layout(40).unwrap();
        assert!(panel.active().model.messages.is_empty());
        assert!(panel.active().rendered.is_empty());
        assert!(panel.active().lines.is_empty());
        assert_eq!(panel.active().model.input, "restored draft");
    }

    #[test]
    fn transcript_selection_preserves_unicode_wraps_and_visible_tool_headers() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.active_mut().model.messages = vec![
            crate::workspace::AgentMessage {
                role: "You".into(),
                text: "a中🙂e\u{301}\nnext".into(),
                id: None,
                turn: 1,
            },
            crate::workspace::AgentMessage {
                role: "run · done".into(),
                text: "hidden tool output".into(),
                id: Some("tool".into()),
                turn: 1,
            },
            crate::workspace::AgentMessage {
                role: "Agent".into(),
                text: "answer".into(),
                id: None,
                turn: 1,
            },
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
        assert_eq!(session.selection_columns(0), Some(1..3));
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
            crate::workspace::AgentMessage {
                turn: 1,
                role: "Agent".into(),
                text: "keep this\n\n**draft".into(),
                id: None,
            },
            crate::workspace::AgentMessage {
                turn: 1,
                role: "run · running".into(),
                text: "running".into(),
                id: Some("cmd".into()),
            },
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
            .push(crate::workspace::AgentMessage {
                role: "Agent".into(),
                text: "**中文".into(),
                id: None,
                turn: 1,
            });
        panel.wait_layout(20);
        let session = panel.active_mut();
        session.begin_selection(session.point(0, 2).unwrap(), false);
        session.extend_selection(session.point(0, 4).unwrap());
        panel.active_mut().model.messages[0].text.push_str("**");
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "中文");
        assert_eq!(panel.active().selected_text(), None);
    }

    #[test]
    fn full_layout_invalidation_overrides_pending_stream_update() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        for text in ["**previous**", "reply"] {
            panel
                .active_mut()
                .model
                .messages
                .push(crate::workspace::AgentMessage {
                    role: "Agent".into(),
                    text: text.into(),
                    id: None,
                    turn: 1,
                });
        }
        panel.wait_layout(20);
        panel.active_mut().model.messages[1].text.push_str(" next");
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "previous");
        assert_eq!(panel.active().message_text(1), "reply next");
        panel.active_mut().model.messages.remove(0);
        panel.active_mut().columns = 0;
        panel.wait_layout(20);
        assert_eq!(panel.active().message_text(0), "reply next");
        assert_eq!(panel.active().rendered.len(), 1);
    }

    #[test]
    fn view_updates_preserve_models_and_apply_rewind_once() {
        let mut owner = AgentPanel::default();
        owner
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                role: "You".into(),
                text: "earlier".into(),
                id: None,
                turn: 4,
            });
        owner.active_mut().model.revision = 3;
        let item = ContextItem {
            label: "registers".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "rax 0x1234".into(),
                incomplete: false,
            },
        };
        let mut view = owner.view_state();
        let canonical = owner.model_snapshot();
        view["sessions"][0]["input"] = serde_json::json!("typing");
        owner.restore_view_state(view.clone()).unwrap();
        assert_eq!(owner.model_snapshot(), canonical);
        assert_eq!(owner.active().model.input, "typing");
        let mut frontend = AgentPanel::default();
        frontend.active_mut().model.insert("next draft").unwrap();
        frontend.restore_models(owner.model_snapshot()).unwrap();
        assert_eq!(frontend.active().model.input, "next draft");
        let canonical = frontend.model_snapshot();
        frontend.restore_view_state(view.clone()).unwrap();
        assert_eq!(frontend.model_snapshot(), canonical);
        let mut models = owner.model_snapshot();
        models[0].recovery = serde_json::to_vec(
            &serde_json::json!({"rewind_restore":[4, "earlier", [item.view()]]}),
        )
        .unwrap();
        frontend.restore_models(models.clone()).unwrap();
        assert_eq!(frontend.active().model.input, "earlier");
        assert_eq!(frontend.active().context.basket.len(), 1);
        frontend.active_mut().model.insert(" edited").unwrap();
        frontend.restore_models(models.clone()).unwrap();
        assert_eq!(frontend.active().model.input, "earlier edited");
        let view = frontend.view_state();
        let mut attached = AgentPanel::default();
        attached.restore_view_state(view).unwrap();
        attached.restore_models(models).unwrap();
        assert_eq!(attached.active().model.input, "earlier edited");
        assert_eq!(attached.active().context.basket.len(), 1);
        attached.restore_models(owner.model_snapshot()).unwrap();
        assert_eq!(attached.active().model.input, "earlier edited");
        assert_eq!(attached.active().context.basket.len(), 1);
    }

    #[test]
    fn agent_view_roundtrip_preserves_drafts_and_canonical_models() {
        let mut panel = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        panel.new_session();
        panel.keyboard = AgentAction::Inspect(0);
        let item = ContextItem {
            label: "寄存器".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "rax 0x1234\n终端内容".into(),
                incomplete: true,
            },
        };
        for session in &mut panel.sessions {
            session.model.insert("中文🙂 draft").unwrap();
            session.model.cursor = "中文".len();
            session.model.anchor = Some("中".len());
            session.model.scroll = 7;
            session.context.basket.push(item.clone());
            session.context.menu = true;
            session.context.preview = Some(ContextPreview::new(item.clone(), 3));
            session.expanded_tools.insert("tool-11".into());
            session.editor_scroll = Some(2);
            session.rewind_target = Some(11);
            session.selection = Some(ChatSelection {
                anchor: TranscriptPoint {
                    message: 0,
                    part: 0,
                    offset: 0,
                },
                head: TranscriptPoint {
                    message: 0,
                    part: 0,
                    offset: 6,
                },
                dragging: true,
            });
        }
        let view = panel.view_state();
        assert_eq!(panel.view_state(), view);
        panel.active_mut().model.cursor = 0;
        panel.active_mut().context.menu = false;
        panel.active_mut().context.preview = None;
        assert_eq!(panel.view_state(), view);
        assert!(view["sessions"][0].get("cursor").is_none());
        assert!(view["sessions"][0].get("messages").is_none());
        let mut restored = AgentPanel::default();
        restored.new_session();
        for session in &mut restored.sessions {
            session.model.messages.push(crate::workspace::AgentMessage {
                role: "Agent".into(),
                text: format!("canonical model {}", session.id),
                id: None,
                turn: 11,
            });
            session.model.busy = true;
            session.model.revision = 99;
            session.layout(40);
        }
        restored.sessions.swap(0, 1);
        restored.restore_view_state(view.clone()).unwrap();
        assert_eq!(restored.view_state(), view);
        for session in &restored.sessions {
            assert_eq!(
                session.model.messages[0].text,
                format!("canonical model {}", session.id)
            );
            assert!(session.model.busy);
            assert_eq!(session.model.revision, 99);
            assert!(session.selection.is_none());
            assert!(!session.context.menu);
            assert!(session.context.preview.is_none());
            assert_eq!(session.model.scroll, 0);
            assert_eq!(session.model.cursor, session.model.input.len());
        }
        restored.new_session();
        assert_eq!(restored.active().id, 2);
    }

    #[test]
    fn restored_stream_updates_markdown_selection_and_trimmed_messages() {
        let mut owner = AgentPanel::default();
        for text in ["**earlier**", "**中文"] {
            owner
                .active_mut()
                .model
                .messages
                .push(crate::workspace::AgentMessage {
                    role: "Agent".into(),
                    text: text.into(),
                    id: None,
                    turn: 1,
                });
        }
        let mut frontend = AgentPanel {
            open: true,
            ..AgentPanel::default()
        };
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        let session = frontend.active_mut();
        let line = session
            .lines
            .iter()
            .position(|line| matches!(line, AgentLine::Text { message: 1, .. }))
            .unwrap();
        session.begin_selection(session.point(line, 2).unwrap(), false);
        session.extend_selection(session.point(line, 4).unwrap());
        assert!(session.selected_text().is_some());
        owner.active_mut().model.messages[1].text.push_str("**");
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        assert_eq!(frontend.active().message_text(0), "earlier");
        assert_eq!(frontend.active().message_text(1), "中文");
        assert_eq!(frontend.active().selected_text(), None);
        owner.active_mut().model.messages.remove(0);
        owner.active_mut().model.revision += 1;
        frontend.restore_models(owner.model_snapshot()).unwrap();
        frontend.wait_layout(40);
        assert_eq!(frontend.active().message_text(0), "中文");
        assert_eq!(frontend.active().rendered.len(), 1);
    }

    #[test]
    fn conversation_tabs_keep_drafts_context_and_scroll_independent() {
        let mut panel = AgentPanel::default();
        panel.active_mut().model.insert("first draft").unwrap();
        panel.active_mut().model.scroll = 7;
        panel.active_mut().context.basket.push(ContextItem {
            label: "Screen".into(),
            source: "pane 1".into(),
            kind: "Screen".into(),
            snapshot: ContextSnapshot {
                text: "original snapshot".into(),
                incomplete: false,
            },
        });
        panel.new_session();
        panel.active_mut().model.insert("second draft").unwrap();
        assert!(panel.active().context.basket.is_empty());
        panel.switch(0);
        assert_eq!(panel.active().model.input, "first draft");
        assert_eq!(panel.active().model.scroll, 7);
        assert_eq!(
            panel.active().context.basket[0].snapshot.text,
            "original snapshot"
        );
        panel.close(1);
        assert_eq!(panel.active().model.input, "first draft");
        for _ in 0..12 {
            panel.new_session();
        }
        let tabs = panel.tabs(30);
        assert!(tabs.iter().any(|(index, _, _)| *index == panel.current));
        assert!(tabs.iter().all(|(_, x, width)| x + width <= 24));
    }

    #[test]
    fn conversation_titles_use_panel_space_and_redistribute_short_tab_width() {
        let mut panel = AgentPanel::default();
        let title = "使用 pwndbg 分析 sudo 的启动与权限检查";
        panel
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 1,
                role: "You".into(),
                text: title.into(),
                id: None,
            });
        let wide = panel.tabs(100);
        assert_eq!(wide[0].2, crate::width::text_width(title) + 7);
        assert!(wide[0].2 > 18);
        let narrow = panel.tabs(24);
        assert_eq!(narrow[0].2, 18);
        assert!(wide[0].2 > narrow[0].2);
        panel.new_session();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 2,
                role: "You".into(),
                text: "第二个长会话标题".repeat(8),
                id: None,
            });
        panel.new_session();
        panel
            .active_mut()
            .model
            .messages
            .push(crate::workspace::AgentMessage {
                turn: 3,
                role: "You".into(),
                text: "a".into(),
                id: None,
            });
        let tabs = panel.tabs(80);
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[2].2, 10);
        assert!(tabs[0].2 > 18 && tabs[1].2 > 18);
        assert_eq!(tabs.iter().map(|tab| tab.2).sum::<usize>(), 74);
        let tiny = panel.tabs(12);
        assert!(tiny.iter().any(|tab| tab.0 == panel.current));
        assert!(tiny.iter().all(|tab| tab.1 + tab.2 <= 6));
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
        session.model.messages.push(crate::workspace::AgentMessage {
            turn: 1,
            role: "You".into(),
            text: "0123456789".repeat(50),
            id: None,
        });
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
        session.model.messages.push(crate::workspace::AgentMessage {
            turn: 2,
            role: "shell · done".into(),
            text: "hidden output".repeat(500),
            id: Some("tool".into()),
        });
        session.columns = 0;
        session.layout(40);
        let last = session.lines.len() - session.model.scroll - 1;
        assert!(session.line_text(last).unwrap().1.contains(&anchor));
    }
}
