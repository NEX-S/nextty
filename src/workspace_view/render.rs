use super::*;
#[cfg(feature = "agent")]
use crate::ui::{AgentLine, wrap_context};
use crate::width::{fit_label, scalar_width};
use std::borrow::Cow;

impl WorkspaceView {
    pub(super) fn chrome(&mut self, layout: GuiLayout) -> Result<()> {
        self.renderer.clip(0., 0., layout.width, layout.top)?;
        let mut tabline_background = 0;
        for shift in [16, 8, 0] {
            tabline_background |= ((self.config.background >> shift & 255) * 9 / 10) << shift;
        }
        self.renderer.clear(
            (tabline_background << 8) | (self.config.tab_bar_opacity * 255.).round() as u32,
        )?;
        let Some(snapshot) = self.client.snapshot() else {
            return Ok(());
        };
        let controller = snapshot.controller;
        let cell = self.cell;
        self.tab_hits.clear();
        let label = if self.client.is_remote() { "SSH" } else { "" };
        let columns = ((layout.width - self.control_width()) / cell.0).max(0.) as u16;
        self.text.draw(
            &self.renderer,
            "›",
            cell.0,
            0.,
            if controller { 0x9c8fdcff } else { 0xfe9b30ff },
        )?;
        let (tabs, label, before, after) = crate::workspace::tab_chrome(snapshot, columns, label);
        let mut end = 3;
        for (index, column, columns) in tabs {
            let tab = &snapshot.tabs[index];
            let active = tab.id == snapshot.active_tab;
            let x = f32::from(column) * cell.0;
            let w = f32::from(columns) * cell.0;
            let hovered =
                (x..x + w).contains(&self.pointer.0) && (0. ..layout.top).contains(&self.pointer.1);
            let mut background = self.config.background;
            if !active && !hovered {
                background = 0;
                for shift in [16, 8, 0] {
                    background |= ((self.config.background >> shift & 255) * 4 / 5) << shift;
                }
            }
            self.renderer
                .tab((x, 0.), (w, cell.1), cell.0, (background << 8) | 255);
            self.text.draw(
                &self.renderer,
                "•",
                x + cell.0 * 2.,
                0.,
                (self
                    .ui
                    .tab_color(tab, self.config.foreground, self.config.background)
                    << 8)
                    | 255,
            )?;
            self.text.draw(
                &self.renderer,
                &fit(&tab.title, usize::from(columns - 8)),
                x + cell.0 * 4.,
                0.,
                if active { 0x949494ff } else { 0x606060ff },
            )?;
            let close_x = x + w - cell.0 * 3.;
            let close_hovered = self.pointer.0 >= close_x - cell.0
                && self.pointer.0 < close_x + cell.0 * 2.
                && self.pointer.1 < layout.top;
            self.control_feedback(
                (close_x - cell.0, 0., cell.0 * 3., layout.top),
                GuiAction::CloseTab(tab.id),
            );
            self.text.draw(
                &self.renderer,
                "×",
                close_x,
                0.,
                if close_hovered {
                    0xe87575ff
                } else if active {
                    0x9c8fdcff
                } else {
                    0x404040ff
                },
            )?;
            self.control_hits.push(GuiHit {
                bounds: (close_x - cell.0, 0., cell.0 * 3., layout.top),
                action: GuiAction::CloseTab(tab.id),
            });
            self.tab_hits.push((x, x + w, index));
            end = column + columns + 1;
        }
        if end.saturating_add(3) < columns.saturating_sub(text_width(&label) as u16 + 2) {
            let x = f32::from(end) * cell.0;
            self.header_button(x - cell.0, cell.0 * 3., layout, GuiAction::NewTab)?;
        }
        let label = fit_label(&label, columns.saturating_sub(end + 4));
        let label_x = (f32::from(columns) - text_width(&label) as f32 - 2.) * cell.0;
        self.overflow_hits(label_x, 0., before, after, text_width(&label));
        self.text.draw(
            &self.renderer,
            &label,
            label_x,
            0.,
            if controller { 0xb8b8b8ff } else { 0xfe9b30ff },
        )?;
        self.renderer.clip(0., 0., 0., 0.)
    }

    #[cfg(feature = "agent")]
    fn gui_button(&mut self, label: &str, x: f32, y: f32, action: GuiAction) -> Result<f32> {
        let w = (text_width(label) as f32 + 2.) * self.cell.0;
        let h = self.cell.1 + 6.;
        let layout = self.layout();
        let (left, top, bottom) = if y >= layout.footer {
            (0., layout.footer, layout.height)
        } else {
            (layout.side, layout.top, layout.footer)
        };
        let clipped_x = x.max(left);
        let clipped_y = y.max(top);
        let clipped_w = (x + w).min(layout.width) - clipped_x;
        let clipped_h = (y + h).min(bottom) - clipped_y;
        let hit = GuiHit {
            bounds: (clipped_x, clipped_y, clipped_w, clipped_h),
            action,
        };
        self.control_feedback(hit.bounds, action);
        self.text.draw(
            &self.renderer,
            label,
            x + self.cell.0,
            y + 3.,
            if hit.contains(self.pointer) {
                INK
            } else {
                ACCENT
            },
        )?;
        if clipped_w > 0. && clipped_h > 0. {
            self.control_hits.push(hit);
        }
        Ok(x + w + self.cell.0)
    }
    pub(super) fn control_width(&self) -> f32 {
        self.cell.0 * 18.
    }
    fn header_button(
        &mut self,
        x: f32,
        w: f32,
        layout: GuiLayout,
        action: GuiAction,
    ) -> Result<()> {
        let hit = GuiHit {
            bounds: (x, 0., w, layout.top),
            action,
        };
        let label = match action {
            GuiAction::NewTab => "+",
            GuiAction::Minimize => "−",
            GuiAction::Maximize => "☐",
            GuiAction::Close => "×",
            GuiAction::Tab(-1) => "‹",
            GuiAction::Tab(1) => "›",
            _ => unreachable!("header controls only"),
        };
        let disabled = matches!(action, GuiAction::Tab(_))
            && (!self.controller()
                || self
                    .client
                    .snapshot()
                    .is_none_or(|snapshot| snapshot.tabs.len() < 2));
        let color = if disabled {
            0x505850ff
        } else if hit.contains(self.pointer) {
            if action == GuiAction::Close {
                0xe87575ff
            } else if action == GuiAction::NewTab {
                ACCENT
            } else {
                INK
            }
        } else if action == GuiAction::NewTab {
            0x606060ff
        } else {
            INK
        };
        if !disabled {
            self.control_feedback(hit.bounds, action);
        }
        self.text.draw(
            &self.renderer,
            label,
            x + (w - self.cell.0) * 0.5,
            (layout.top - self.cell.1) * 0.5,
            color,
        )?;
        if !disabled {
            self.control_hits.push(hit);
        }
        Ok(())
    }
    pub(super) fn draw_window_controls(&mut self, layout: GuiLayout) -> Result<()> {
        self.renderer.clip(0., 0., layout.width, layout.top)?;
        let w = self.cell.0 * 4.;
        let start = layout.width - self.control_width();
        for (i, direction) in [-1, 1].into_iter().enumerate() {
            self.header_button(
                start + i as f32 * self.cell.0 * 3.,
                self.cell.0 * 3.,
                layout,
                GuiAction::Tab(direction),
            )?;
        }
        for (i, action) in [GuiAction::Minimize, GuiAction::Maximize, GuiAction::Close]
            .into_iter()
            .enumerate()
        {
            self.header_button(layout.width - w * 3. + i as f32 * w, w, layout, action)?;
        }
        self.renderer.clip(0., 0., 0., 0.)
    }
    #[cfg(feature = "agent")]
    pub(super) fn draw_agent(&mut self, layout: GuiLayout) -> Result<()> {
        if !self.ui.agent.open {
            return Ok(());
        }
        let padding = 12.;
        let x = layout.side + padding;
        let width = (layout.width - x - padding).max(self.cell.0);
        let columns = (width / self.cell.0).floor().max(1.) as usize;
        let background = (self.config.background << 8) | 255;
        let mut user_rgb = 0;
        for shift in [16, 8, 0] {
            user_rgb = (user_rgb << 8) | (((self.config.background >> shift) & 255) + 16).min(255);
        }
        let user_background = (user_rgb << 8) | 255;
        self.renderer.clip(
            layout.side,
            layout.top,
            layout.width - layout.side,
            layout.footer - layout.top,
        )?;
        self.renderer.clear(background)?;
        self.chat_rows.clear();
        let tab_height = self.cell.1 + 8.;
        let tab_cell = self.cell.0;
        let tab_columns = ((layout.width - layout.side) / tab_cell).max(1.) as usize;
        let mut tabline_background = 0;
        for shift in [16, 8, 0] {
            tabline_background |= ((self.config.background >> shift & 255) * 4 / 5) << shift;
        }
        self.renderer.rect(
            layout.side,
            layout.top,
            layout.width - layout.side,
            tab_height,
            (tabline_background << 8) | 255,
            false,
        );
        let tabs = self.ui.agent.tabs(tab_columns);
        let plus_column = tabs.last().map_or(0, |(_, x, width)| x + width);
        for (index, column, cells) in tabs {
            let tx = layout.side + column as f32 * tab_cell;
            let tab_width = cells as f32 * tab_cell;
            let action = GuiAction::Agent(crate::ui::AgentAction::Session(index));
            let close = GuiAction::Agent(crate::ui::AgentAction::CloseSession(index));
            let active = index == self.ui.agent.current;
            let close_hovered = self.hovered_control == Some(close);
            let mut tab_background = self.config.background;
            if !active {
                tab_background = 0;
                for shift in [16, 8, 0] {
                    tab_background |= ((self.config.background >> shift & 255) * 9 / 10) << shift;
                }
            }
            self.renderer.rect(
                tx,
                layout.top,
                tab_width,
                tab_height,
                (tab_background << 8) | 255,
                false,
            );
            if active {
                self.renderer.rect(
                    tx,
                    layout.top + tab_height - 1.,
                    tab_width,
                    1.,
                    ACCENT,
                    false,
                );
            }
            self.text.draw(
                &self.renderer,
                &fit(
                    self.ui.agent.sessions[index].title(),
                    cells.saturating_sub(4),
                ),
                tx + tab_cell,
                layout.top + 4.,
                if active { INK } else { 0x606060ff },
            )?;
            self.control_hits.push(GuiHit {
                bounds: (tx, layout.top, tab_width, tab_height),
                action,
            });
            let cx = tx + tab_width - tab_cell * 2.;
            self.control_feedback(
                (cx - tab_cell, layout.top, tab_cell * 3., tab_height),
                close,
            );
            self.text.draw(
                &self.renderer,
                "×",
                cx,
                layout.top + 4.,
                if close_hovered {
                    0xe87575ff
                } else if active {
                    MUTED
                } else {
                    0x404040ff
                },
            )?;
            self.control_hits.push(GuiHit {
                bounds: (cx - tab_cell, layout.top, tab_cell * 3., tab_height),
                action: close,
            });
        }
        for (column, label, action) in [
            (plus_column, "+", crate::ui::AgentAction::NewSession),
            (
                tab_columns.saturating_sub(3),
                "×",
                crate::ui::AgentAction::ClosePanel,
            ),
        ] {
            let button_width = tab_cell * 3.;
            let button_x = if action == crate::ui::AgentAction::ClosePanel {
                layout.width - button_width
            } else {
                layout.side + column as f32 * tab_cell
            };
            if label == "+" && column + 3 > tab_columns.saturating_sub(3) {
                continue;
            }
            self.control_feedback(
                (button_x, layout.top, button_width, tab_height),
                GuiAction::Agent(action),
            );
            self.text.draw(
                &self.renderer,
                label,
                button_x + tab_cell,
                layout.top + 4.,
                if self.hovered_control == Some(GuiAction::Agent(action)) {
                    if action == crate::ui::AgentAction::NewSession {
                        ACCENT
                    } else {
                        0xe87575ff
                    }
                } else {
                    INK
                },
            )?;
            self.control_hits.push(GuiHit {
                bounds: (button_x, layout.top, button_width, tab_height),
                action: GuiAction::Agent(action),
            });
        }
        self.renderer.rect(
            layout.side,
            layout.top,
            1.,
            layout.footer - layout.top,
            BORDER,
            false,
        );
        let editor_y =
            (layout.footer - self.cell.1 * 3. - padding).max(layout.top + self.cell.1 * 3.);
        let mut chips = Vec::new();
        let (mut column, mut row) = (0, 0);
        for (index, item) in self.ui.agent.active().context.basket.iter().enumerate() {
            let label = format!(" +{} ", fit(&item.label, columns.saturating_sub(7).min(18)));
            let cells = text_width(&label) + 3;
            if column > 0 && column + cells > columns {
                row += 1;
                column = 0;
            }
            chips.push((index, column, row, label, cells));
            column += cells + 1;
        }
        let attachment_rows = if chips.is_empty() { 0 } else { row + 1 };
        let chip_top = editor_y - self.cell.1 * (attachment_rows as f32 + 0.5);
        let picking_rewind = self.ui.agent.active().rewind_index().is_some();
        let chat_end = chip_top - self.cell.1 * (0.5 + f32::from(picking_rewind));
        self.ui.agent.prepare_layout(columns)?;
        let top = layout.top + tab_height + padding;
        let rows = ((chat_end - top) / self.cell.1).max(0.) as usize;
        let previous_scroll = self.ui.agent.active().model.scroll;
        if let Some(target) = self.ui.agent.active().rewind_index()
            && let Some(line) = self.ui.agent.active().lines.iter().position(|line| {
                matches!(line, AgentLine::Text { message, .. } if *message as usize == target)
            })
        {
            let session = self.ui.agent.active_mut();
            let first = session.lines.len().saturating_sub(rows + session.model.scroll);
            if line < first {
                session.model.scroll = session.lines.len().saturating_sub(rows + line);
            } else if line >= first + rows {
                session.model.scroll = session.lines.len().saturating_sub(line + 1);
            }
        }
        self.ui.agent.active_mut().model.scroll = self
            .ui
            .agent
            .active()
            .model
            .scroll
            .min(self.ui.agent.active().lines.len().saturating_sub(rows));
        self.ui.agent.view_dirty |= previous_scroll != self.ui.agent.active().model.scroll;
        let start = self
            .ui
            .agent
            .active()
            .lines
            .len()
            .saturating_sub(rows + self.ui.agent.active().model.scroll);
        self.renderer.clip(
            layout.side + 6.,
            top,
            layout.width - layout.side - 12.,
            (chat_end - top).max(0.),
        )?;
        for (row, line) in self
            .ui
            .agent
            .active()
            .lines
            .iter()
            .skip(start)
            .take(rows)
            .enumerate()
        {
            let py = top + row as f32 * self.cell.1;
            let (text, color, indent) = match line {
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
                    let bounds = (
                        x,
                        py,
                        width.min((text_width(&message.role) + 4) as f32 * self.cell.0),
                        self.cell.1,
                    );
                    let action = GuiAction::Agent(AgentAction::Tool(*index as usize));
                    self.control_feedback(bounds, action);
                    self.control_hits.push(GuiHit { bounds, action });
                    (
                        Cow::Owned(format!(
                            "{marker} {}",
                            fit(&message.role, columns.saturating_sub(4))
                        )),
                        if self.hovered_control
                            == Some(GuiAction::Agent(AgentAction::Tool(*index as usize)))
                        {
                            INK
                        } else {
                            MUTED
                        },
                        self.cell.0,
                    )
                }
                AgentLine::Text { message, range } => {
                    let index = *message as usize;
                    let message = &self.ui.agent.active().model.messages[index];
                    let user = message.role == "You";
                    if user {
                        self.renderer.rect(
                            x - 6.,
                            py,
                            width + 12.,
                            self.cell.1,
                            user_background,
                            false,
                        );
                        if self.ui.agent.active().rewind_target == Some(message.turn) {
                            self.renderer.rect(
                                x - 6.,
                                py,
                                width + 12.,
                                self.cell.1,
                                0x3d4938ff,
                                false,
                            );
                            self.renderer
                                .rect(x - 6., py, 2., self.cell.1, ACCENT, false);
                        }
                    }
                    self.control_hits.push(GuiHit {
                        bounds: (x - 6., py, width + 12., self.cell.1),
                        action: GuiAction::Agent(AgentAction::Copy(index)),
                    });
                    (
                        fit(
                            &self.ui.agent.active().message_text(index)
                                [range.start as usize..range.end as usize],
                            usize::MAX,
                        ),
                        if message.role == "Error" {
                            0xd49790ff
                        } else if message.id.is_some() {
                            MUTED
                        } else {
                            INK
                        },
                        if user {
                            self.cell.0
                        } else if message.id.is_some() {
                            self.cell.0 * 2.
                        } else {
                            0.
                        },
                    )
                }
                AgentLine::Blank => (Cow::Borrowed(""), INK, 0.),
            };
            if !matches!(line, AgentLine::Blank) {
                let text_x = x
                    + indent
                    + if matches!(line, AgentLine::Role(_)) {
                        self.cell.0 * 2.
                    } else {
                        0.
                    };
                self.chat_rows.push(ChatRow {
                    line: start + row,
                    x: text_x,
                    y: py,
                    columns: columns.saturating_sub(((text_x - x) / self.cell.0) as usize),
                });
                if let AgentLine::Text { message, range } = line
                    && let Some(document) = self.ui.agent.active().markdown(*message as usize)
                {
                    let from = range.start as usize;
                    for (span, style) in document.spans(from..range.end as usize) {
                        if style & crate::markdown::CODE != 0 {
                            self.renderer.rect(
                                text_x
                                    + text_width(&document.text[from..span.start]) as f32
                                        * self.cell.0,
                                py,
                                text_width(&document.text[span]) as f32 * self.cell.0,
                                self.cell.1,
                                0x2b302dff,
                                false,
                            );
                        }
                    }
                }
                if let Some(range) = self.ui.agent.active().selection_columns(start + row) {
                    self.renderer.rect(
                        text_x + range.start as f32 * self.cell.0,
                        py,
                        (range.end - range.start) as f32 * self.cell.0,
                        self.cell.1,
                        0x414a51ff,
                        false,
                    );
                }
            }
            if let AgentLine::Text { message, range } = line
                && let Some(document) = self.ui.agent.active().markdown(*message as usize)
            {
                let from = range.start as usize;
                for (span, style) in document.spans(from..range.end as usize) {
                    self.text.draw_flags(
                        &self.renderer,
                        &document.text[span.clone()],
                        x + indent
                            + text_width(&document.text[from..span.start]) as f32 * self.cell.0,
                        py,
                        (crate::markdown::color(style, color >> 8) << 8) | 255,
                        u32::from(style & 3),
                    )?;
                }
            } else {
                self.text
                    .draw(&self.renderer, &text, x + indent, py, color)?;
            }
            if let AgentLine::Text { message, .. } = line {
                let index = *message as usize;
                if self.ui.agent.active().model.messages[index].role == "You"
                    && columns >= crate::width::text_width(crate::ui::REWIND_LABEL) + 5
                    && (row + 1 == rows
                        || !matches!(
                            self.ui.agent.active().lines.get(start + row + 1),
                            Some(AgentLine::Text { message: next, .. }) if *next as usize == index
                        ))
                {
                    let button_width =
                        crate::width::text_width(crate::ui::REWIND_LABEL) as f32 * self.cell.0;
                    let rx = x + width - button_width;
                    let action = GuiAction::Agent(crate::ui::AgentAction::Rewind(index));
                    let button_background = if self.ui.agent.active().rewind_target
                        == Some(self.ui.agent.active().model.messages[index].turn)
                    {
                        0x3d4938ff
                    } else {
                        user_background
                    };
                    self.renderer
                        .rect(rx, py, button_width, self.cell.1, button_background, false);
                    self.control_feedback((rx, py, button_width, self.cell.1), action);
                    let icon_scale = 20. / self.config.font_size_px;
                    self.text.draw_at(
                        &self.renderer,
                        "←",
                        (
                            rx + (button_width - self.cell.0 * icon_scale) * 0.5,
                            py + self.cell.1 * (1. - icon_scale) * 0.5,
                            icon_scale,
                        ),
                        if self.hovered_control == Some(action) {
                            ACCENT
                        } else {
                            MUTED
                        },
                        0,
                    )?;
                    self.control_hits.push(GuiHit {
                        bounds: (rx, py, button_width, self.cell.1),
                        action,
                    });
                }
            }
        }
        if let Some(bar) = Scrollbar::new(
            ScrollTarget::Agent,
            (layout.width - 3., top, 3., (chat_end - top).max(0.)),
            self.ui.agent.active().lines.len(),
            rows,
            self.ui.agent.active().model.scroll,
        ) {
            self.scrollbars.push(bar);
        }
        self.renderer.clip(
            layout.side,
            layout.top,
            layout.width - layout.side,
            layout.footer - layout.top,
        )?;
        if picking_rewind {
            let hint = "Esc older · Enter confirm · click input cancel";
            let hint = if text_width(hint) <= columns {
                hint
            } else {
                "Esc prev · Enter OK · click cancel"
            };
            self.text.draw(
                &self.renderer,
                &fit(hint, columns),
                x,
                chip_top - self.cell.1,
                MUTED,
            )?;
        }
        for (index, column, row, label, cells) in chips {
            let cx = x + column as f32 * self.cell.0;
            let cy = chip_top + row as f32 * self.cell.1;
            let text_cells = cells - 3;
            self.control_feedback(
                (cx, cy, text_cells as f32 * self.cell.0, self.cell.1),
                GuiAction::Agent(AgentAction::Inspect(index)),
            );
            self.text.draw(&self.renderer, &label, cx, cy, ACCENT)?;
            self.control_hits.push(GuiHit {
                bounds: (cx, cy, text_cells as f32 * self.cell.0, self.cell.1),
                action: GuiAction::Agent(AgentAction::Inspect(index)),
            });
            let remove_x = cx + (text_cells + 1) as f32 * self.cell.0;
            let remove_bounds = (remove_x - self.cell.0, cy, self.cell.0 * 3., self.cell.1);
            self.control_feedback(remove_bounds, GuiAction::Agent(AgentAction::Remove(index)));
            self.text.draw(
                &self.renderer,
                "×",
                remove_x,
                cy,
                if self.hovered_control == Some(GuiAction::Agent(AgentAction::Remove(index))) {
                    INK
                } else {
                    MUTED
                },
            )?;
            self.control_hits.push(GuiHit {
                bounds: remove_bounds,
                action: GuiAction::Agent(AgentAction::Remove(index)),
            });
        }
        self.renderer.rect(
            x - 6.,
            editor_y - 6.,
            width + 12.,
            self.cell.1 * 3. + 12.,
            user_background,
            false,
        );
        self.control_hits.push(GuiHit {
            bounds: (x - 6., editor_y - 6., width + 12., self.cell.1 * 3. + 12.),
            action: GuiAction::Agent(AgentAction::Editor),
        });
        let editor_columns = columns.saturating_sub(self.agent_editor_reserved()).max(1);
        let editor = self.ui.agent.active().editor_layout(editor_columns, 3);
        let input = &self.ui.agent.active().model.input;
        let mut display = Cow::Borrowed(input.as_str());
        let lines = if self.ui.focus == Focus::Editor && !self.preedit.is_empty() {
            let cursor = self.ui.agent.active().model.cursor;
            let mut text = String::with_capacity(input.len() + self.preedit.len());
            text.push_str(&input[..cursor]);
            text.push_str(&self.preedit);
            text.push_str(&input[cursor..]);
            display = Cow::Owned(text);
            Cow::Owned(wrap_context(&display, editor_columns))
        } else {
            // The caret can need an extra empty row that has no text to draw.
            let count = editor.lines.len()
                - usize::from(!input.ends_with('\n') && editor.lines.last().unwrap().is_empty());
            Cow::Borrowed(&editor.lines[..count])
        };
        let caret_row = editor.caret_row;
        let caret_width = editor.caret_column;
        let total = lines.len().max(caret_row + 1);
        let first = self
            .ui
            .agent
            .active()
            .editor_scroll
            .unwrap_or_else(|| caret_row.saturating_sub(2))
            .min(total.saturating_sub(3));
        self.renderer.clip(
            x,
            editor_y,
            editor_columns as f32 * self.cell.0,
            self.cell.1 * 3.,
        )?;
        if display.is_empty() {
            self.text
                .draw(&self.renderer, "Message…", x, editor_y, MUTED)?;
        }
        for (row, range) in lines.iter().skip(first).take(3).enumerate() {
            if self.preedit.is_empty()
                && let Some(selected) = self.ui.agent.active().model.selection()
            {
                let start = range.start.max(selected.start);
                let end = range.end.min(selected.end);
                if start < end || selected.start <= range.end && selected.end > range.end {
                    let left = text_width(&fit(
                        &display[range.start..start.min(range.end)],
                        usize::MAX,
                    ));
                    let cells = if start < end {
                        text_width(&fit(&display[start..end], usize::MAX))
                    } else {
                        0
                    };
                    let newline = usize::from(selected.end > range.end);
                    self.renderer.rect(
                        x + left as f32 * self.cell.0,
                        editor_y + row as f32 * self.cell.1,
                        (cells + newline) as f32 * self.cell.0,
                        self.cell.1,
                        0x52634cff,
                        false,
                    );
                }
            }
            self.text.draw(
                &self.renderer,
                &fit(&display[range.clone()], usize::MAX),
                x,
                editor_y + row as f32 * self.cell.1,
                INK,
            )?;
        }
        if self
            .ui
            .agent
            .active()
            .editor_scroll
            .is_some_and(|scroll| scroll != first)
        {
            self.ui.agent.active_mut().editor_scroll = Some(first);
            self.ui.agent.view_dirty = true;
        }
        if (self.ui.focus == Focus::Editor) && (first..first + 3).contains(&caret_row) {
            let cx = x + (caret_width % editor_columns) as f32 * self.cell.0;
            let cy = editor_y + (caret_row - first) as f32 * self.cell.1;
            self.renderer.rect(cx, cy, 1., self.cell.1, ACCENT, false);
            self.window
                .cursor_rect(cx as i32, cy as i32, self.cell.0 as i32, self.cell.1 as i32);
        }
        self.renderer.clip(
            layout.side,
            layout.top,
            layout.width - layout.side,
            layout.footer - layout.top,
        )?;
        if let Some(bar) = Scrollbar::new(
            ScrollTarget::Editor,
            (layout.width - 3., editor_y - 6., 3., self.cell.1 * 3. + 12.),
            total,
            3,
            total.saturating_sub(3) - first,
        ) {
            self.scrollbars.push(bar);
        }
        if self.ui.agent.active().model.busy {
            let stop_x = x + width - self.cell.0 * 3.;
            self.control_feedback(
                (stop_x, editor_y, self.cell.0 * 3., self.cell.1),
                GuiAction::Agent(AgentAction::Stop),
            );
            self.text.draw(
                &self.renderer,
                "■",
                stop_x + self.cell.0,
                editor_y,
                0xe87575ff,
            )?;
            self.control_hits.push(GuiHit {
                bounds: (stop_x, editor_y, self.cell.0 * 3., self.cell.1),
                action: GuiAction::Agent(AgentAction::Stop),
            });
        }
        if self.ui.agent.active().context.menu {
            let menu_y = editor_y - self.cell.1 * 4. - 12.;
            let menu_w = (self.cell.0 * 21.).min(width);
            self.renderer
                .rect(x, menu_y, menu_w, self.cell.1 * 4. + 8., BORDER, false);
            self.renderer.rect(
                x + 1.,
                menu_y + 1.,
                menu_w - 2.,
                self.cell.1 * 4. + 6.,
                background,
                false,
            );
            self.gui_button(
                "Selection",
                x,
                menu_y + self.cell.1 * 0.7,
                GuiAction::Agent(AgentAction::Selection),
            )?;
            self.gui_button(
                "Visible screen",
                x,
                menu_y + self.cell.1 * 2.1,
                GuiAction::Agent(AgentAction::Screen),
            )?;
        }
        self.renderer.clip(0., 0., 0., 0.)
    }
    #[cfg(feature = "agent")]
    pub(super) fn draw_context_preview(&mut self, layout: GuiLayout) -> Result<()> {
        let close_highlighted = self.hovered_control
            == Some(GuiAction::Agent(AgentAction::ClosePreview))
            || self.ui.focus == Focus::Controls
                && self.ui.agent.keyboard == crate::ui::AgentAction::ClosePreview;
        let copy_highlighted = self.hovered_control
            == Some(GuiAction::Agent(AgentAction::CopyPreview))
            || self.ui.focus == Focus::Controls
                && self.ui.agent.keyboard == crate::ui::AgentAction::CopyPreview;
        let Some(preview) = &mut self.ui.agent.active_mut().context.preview else {
            return Ok(());
        };
        let padding = 16.;
        let w = (self.cell.0 * 52. + padding * 2.).min(layout.width * 0.85);
        let h = (self.cell.1 * 20. + padding * 2.).min((layout.footer - layout.top) * 0.9);
        let x = (layout.width - w) * 0.5;
        let y = layout.top + (layout.footer - layout.top - h) * 0.5;
        let columns = ((w - padding * 2.) / self.cell.0).max(1.) as usize;
        preview.prepare(columns);
        let crate::ui::ContextPreview {
            item,
            scroll,
            lines,
            ..
        } = preview;
        self.renderer.rect(
            0.,
            layout.top,
            layout.width,
            layout.footer - layout.top,
            0x00000066,
            false,
        );
        self.renderer.rect(x, y, w, h, BORDER, false);
        self.renderer
            .rect(x + 1., y + 1., w - 2., h - 2., PANEL, false);
        self.renderer.clip(x + 1., y + 1., w - 2., h - 2.)?;
        self.text.draw(
            &self.renderer,
            &fit(&format!("[ {} ]", item.label), columns.saturating_sub(3)),
            x + padding,
            y + padding,
            ACCENT,
        )?;
        let close_x = x + w - padding - self.cell.0;
        if close_highlighted {
            self.renderer.rect(
                close_x - self.cell.0,
                y + padding,
                self.cell.0 * 3.,
                self.cell.1,
                0x353e35ff,
                false,
            );
        }
        self.text.draw(
            &self.renderer,
            "×",
            close_x,
            y + padding,
            if self.hovered_control == Some(GuiAction::Agent(AgentAction::ClosePreview)) {
                INK
            } else {
                MUTED
            },
        )?;
        self.control_hits.push(GuiHit {
            bounds: (
                close_x - self.cell.0,
                y + padding,
                self.cell.0 * 3.,
                self.cell.1,
            ),
            action: GuiAction::Agent(AgentAction::ClosePreview),
        });
        self.text.draw(
            &self.renderer,
            &fit(&item.source, columns),
            x + padding,
            y + padding + self.cell.1 * 1.5,
            MUTED,
        )?;
        let top = y + padding + self.cell.1 * 3.;
        let bottom = y + h - padding - self.cell.1 * 2.;
        let rows = ((bottom - top) / self.cell.1).max(1.) as usize;
        let previous_scroll = *scroll;
        *scroll = (*scroll).min(lines.len().saturating_sub(rows));
        let scroll_changed = previous_scroll != *scroll;
        if let Some(bar) = Scrollbar::new(
            ScrollTarget::Preview,
            (x + w - 3., top, 3., (bottom - top).max(0.)),
            lines.len(),
            rows,
            lines.len().saturating_sub(rows) - *scroll,
        ) {
            self.scrollbars.push(bar);
        }

        self.renderer
            .clip(x + padding, top, w - padding * 2., (bottom - top).max(0.))?;
        for (row, range) in lines.iter().skip(*scroll).take(rows).enumerate() {
            self.text.draw(
                &self.renderer,
                &fit(&item.snapshot.text[range.clone()], usize::MAX),
                x + padding,
                top + row as f32 * self.cell.1,
                INK,
            )?;
        }
        self.renderer.clip(x + 1., y + 1., w - 2., h - 2.)?;
        let by = y + h - padding - self.cell.1;
        let copy_label = " ⧉ ";
        let copy_cells = text_width(copy_label);
        if copy_highlighted {
            self.renderer.rect(
                x + padding,
                by,
                self.cell.0 * copy_cells as f32,
                self.cell.1,
                0x353e35ff,
                false,
            );
        }
        self.text
            .draw(&self.renderer, copy_label, x + padding, by, ACCENT)?;
        self.control_hits.push(GuiHit {
            bounds: (
                x + padding,
                by,
                self.cell.0 * copy_cells as f32,
                self.cell.1,
            ),
            action: GuiAction::Agent(AgentAction::CopyPreview),
        });
        self.text.draw(
            &self.renderer,
            &fit(
                if item.snapshot.incomplete {
                    "partial · Esc back"
                } else {
                    "Esc back"
                },
                columns.saturating_sub(copy_cells + 2),
            ),
            x + padding + self.cell.0 * (copy_cells + 2) as f32,
            by,
            MUTED,
        )?;
        self.ui.agent.view_dirty |= scroll_changed;
        self.renderer.clip(0., 0., 0., 0.)
    }
    fn control_feedback(&self, bounds: (f32, f32, f32, f32), action: GuiAction) {
        let hovered = GuiHit { bounds, action }.contains(self.pointer);
        #[cfg(feature = "agent")]
        let focused =
            self.ui.focus == Focus::Controls && action == GuiAction::Agent(self.ui.agent.keyboard);
        #[cfg(not(feature = "agent"))]
        let focused = false;
        if hovered || focused {
            let tab_control = matches!(
                action,
                GuiAction::NewTab
                    | GuiAction::CloseTab(_)
                    | GuiAction::Tab(_)
                    | GuiAction::Minimize
                    | GuiAction::Maximize
                    | GuiAction::Close
            );
            #[cfg(feature = "agent")]
            let tab_control = tab_control
                || matches!(
                    action,
                    GuiAction::Agent(
                        crate::ui::AgentAction::CloseSession(_)
                            | crate::ui::AgentAction::NewSession
                            | crate::ui::AgentAction::ClosePanel
                    )
                );
            if tab_control {
                let padding = self.cell.0.min((bounds.2 - self.cell.0).max(0.) * 0.5);
                self.renderer.rect(
                    bounds.0 + padding,
                    bounds.1 + bounds.3 - 1.,
                    bounds.2 - padding * 2.,
                    1.,
                    {
                        let close = matches!(action, GuiAction::CloseTab(_) | GuiAction::Close);
                        #[cfg(feature = "agent")]
                        let close = close
                            || matches!(
                                action,
                                GuiAction::Agent(
                                    crate::ui::AgentAction::CloseSession(_)
                                        | crate::ui::AgentAction::ClosePanel
                                )
                            );
                        if close { 0xe87575ff } else { ACCENT }
                    },
                    false,
                );
                return;
            }
            let color = match action {
                GuiAction::Close => 0x633534ff,
                #[cfg(feature = "agent")]
                GuiAction::Agent(AgentAction::Stop) => 0x633534ff,
                _ => 0x353e35ff,
            };
            self.renderer
                .rect(bounds.0, bounds.1, bounds.2, bounds.3, color, false);
        }
    }
    pub(super) fn draw_scrollbars(&mut self) -> Result<()> {
        self.renderer.clip(0., 0., 0., 0.)?;
        #[cfg(feature = "files")]
        if self.ui.files_open {
            self.scrollbars.clear();
            return Ok(());
        }
        for bar in &self.scrollbars {
            let mut bar = *bar;
            if let Some(drag) = &self.scrollbar_drag
                && drag.bar.target == bar.target
                && let Some(offset) = drag.requested
            {
                bar.offset = if offset == drag.bar.maximum() {
                    bar.maximum()
                } else {
                    offset.min(bar.maximum())
                };
            }
            if !self.scrollbar_visible(bar.target) {
                continue;
            }
            let active = bar.contains(self.pointer)
                || self
                    .scrollbar_drag
                    .as_ref()
                    .is_some_and(|drag| drag.bar.target == bar.target);
            let (top, height) = bar.thumb();
            if active {
                self.renderer.rect(
                    bar.bounds.0,
                    bar.bounds.1,
                    bar.bounds.2,
                    bar.bounds.3,
                    0x343834ff,
                    false,
                );
            }
            self.renderer.rect(
                bar.bounds.0,
                top,
                bar.bounds.2,
                height,
                if active { 0xa2aaa0ff } else { 0x60685fff },
                false,
            );
        }
        Ok(())
    }
    #[cfg(feature = "monitor")]
    fn draw_resources(&mut self, layout: GuiLayout, summary_y: f32) -> Result<()> {
        let top = layout.footer + 6.;
        let columns = ((layout.width - PADDING * 2.) / self.cell.0).max(0.) as u16;
        let rows = ((summary_y - 6. - top) / self.cell.1).max(0.) as u16;
        let view = self.ui.resources.view(columns, rows);
        let body_left = PADDING + f32::from(view.body_x) * self.cell.0;
        let body_top = top + self.cell.1 * 2.;
        let bottom = top + f32::from(rows) * self.cell.1;
        self.renderer.clip(
            body_left,
            body_top,
            (layout.width - body_left - PADDING).max(0.),
            (bottom - body_top).max(0.),
        )?;
        for plot in view.plots {
            let x = PADDING + f32::from(plot.x) * self.cell.0;
            let y = top + plot.y as f32 * self.cell.1;
            let width = f32::from(plot.width.saturating_sub(1)) * self.cell.0;
            let height = f32::from(plot.height) * self.cell.1 - 2.;
            for fraction in [0., 0.5, 1.] {
                self.renderer
                    .rect(x, y + fraction * height, width, 1., BORDER, false);
            }
            for (index, series) in plot.series.into_iter().enumerate() {
                let color = (if index == 0 {
                    crate::monitor::GREEN
                } else {
                    crate::monitor::PURPLE
                } << 8)
                    | 255;
                let mut previous = None;
                for (age, value) in series.points {
                    let Some(value) = value else {
                        previous = None;
                        continue;
                    };
                    let point = (
                        x + (1. - age / self.ui.resources.seconds() as f32).clamp(0., 1.) * width,
                        y + (1. - value.clamp(0., plot.maximum) / plot.maximum) * height,
                    );
                    if let Some(from) = previous {
                        self.renderer.stroke(from, point, 1.5, color);
                    }
                    previous = Some(point);
                }
                if let Some(point) = previous {
                    self.renderer.stroke(point, point, 3., color);
                }
            }
        }
        self.renderer.clip(
            0.,
            layout.footer,
            layout.width,
            (summary_y - 6. - layout.footer).max(0.),
        )?;
        for text in view.texts {
            let x = PADDING + f32::from(text.x) * self.cell.0;
            let y = top + text.y as f32 * self.cell.1;
            let width = f32::from(text.width) * self.cell.0;
            self.renderer.clip(x, y, width, self.cell.1)?;
            if text.selected || text.highlighted || text.focused && self.ui.focus == Focus::Monitor
            {
                self.renderer
                    .rect(x, y, width, self.cell.1, 0x29322bff, false);
            }
            self.text.draw(
                &self.renderer,
                &fit_label(&text.text, text.width),
                x,
                y,
                (text.color << 8) | 255,
            )?;
        }
        for hit in view.hits {
            self.control_hits.push(GuiHit {
                bounds: (
                    PADDING + f32::from(hit.x) * self.cell.0,
                    top + hit.y as f32 * self.cell.1,
                    f32::from(hit.width) * self.cell.0,
                    self.cell.1,
                ),
                action: GuiAction::Monitor(hit.action),
            });
        }
        self.renderer.clip(
            0.,
            layout.footer,
            layout.width,
            layout.height - layout.footer,
        )
    }
    pub(super) fn footer(&mut self, layout: GuiLayout) -> Result<()> {
        #[cfg(not(feature = "monitor"))]
        let layout = GuiLayout {
            footer: layout.height - self.cell.1 - 12.,
            ..layout
        };
        #[cfg(not(feature = "monitor"))]
        if self.ui.status.is_none() {
            return Ok(());
        }
        self.renderer.clip(
            0.,
            layout.footer,
            layout.width,
            layout.height - layout.footer,
        )?;
        self.renderer.clear(CHROME)?;
        self.renderer
            .rect(0., layout.footer, layout.width, 1., BORDER, false);
        let summary_y = layout.height - self.cell.1 - 6.;
        #[cfg(feature = "monitor")]
        if self.ui.resources.open {
            self.draw_resources(layout, summary_y)?;
            self.renderer
                .rect(0., summary_y - 6., layout.width, 1., BORDER, false);
        }
        #[cfg(feature = "monitor")]
        let columns = ((layout.width - PADDING * 2.) / self.cell.0).max(0.) as u16;
        #[cfg(feature = "monitor")]
        let right = {
            if self.ui.resources.open {
                let label = if self.controller() {
                    "Local system · 1s"
                } else {
                    "Read-only · local system · 1s"
                };
                let stats = fit_label(label, (columns / 2).saturating_sub(4));
                let arrow_x = layout.width - PADDING - self.cell.0 * 3.;
                let right = arrow_x - self.cell.0 * (text_width(&stats) as f32 + 1.);
                self.text
                    .draw(&self.renderer, &stats, right, summary_y, INK)?;
                let bounds = (arrow_x, summary_y - 6., self.cell.0 * 3., self.cell.1 + 12.);
                self.control_feedback(bounds, GuiAction::Resources);
                self.text
                    .draw(&self.renderer, "⌄", arrow_x + self.cell.0, summary_y, INK)?;
                self.control_hits.push(GuiHit {
                    bounds,
                    action: GuiAction::Resources,
                });
                right
            } else {
                let label =
                    self.ui
                        .resources
                        .stats
                        .label(self.controller(), None, self.client.is_remote());
                let stats = fit_label(&label, columns / 2);
                let right = layout.width - PADDING - text_width(&stats) as f32 * self.cell.0;
                let bounds = (
                    right - self.cell.0,
                    summary_y - 6.,
                    (text_width(&stats) as f32 + 2.) * self.cell.0,
                    self.cell.1 + 12.,
                );
                self.control_feedback(bounds, GuiAction::Resources);
                self.text.draw(
                    &self.renderer,
                    &stats,
                    right,
                    summary_y,
                    if self.hovered_control == Some(GuiAction::Resources) {
                        INK
                    } else {
                        MUTED
                    },
                )?;
                self.control_hits.push(GuiHit {
                    bounds,
                    action: GuiAction::Resources,
                });
                right
            }
        };
        #[cfg(not(feature = "monitor"))]
        let right = layout.width - PADDING;
        let label = self
            .client
            .snapshot()
            .map(|snapshot| self.ui.footer(snapshot, self.client.is_remote()))
            .unwrap_or_default();
        self.text.draw(
            &self.renderer,
            &fit(
                &label,
                ((right - PADDING - self.cell.0 * 2.) / self.cell.0).max(0.) as usize,
            ),
            PADDING,
            summary_y,
            MUTED,
        )?;
        self.renderer.clip(0., 0., 0., 0.)
    }

    fn overflow_hits(&mut self, x: f32, y: f32, before: usize, after: usize, visible: usize) {
        let mut column = 0;
        for (count, direction) in [(before, -1), (after, 1)] {
            if count == 0 {
                continue;
            }
            let width = count.ilog10() as usize + 2;
            if column + width <= visible {
                let bounds = (
                    x + column as f32 * self.cell.0,
                    y,
                    width as f32 * self.cell.0,
                    self.cell.1,
                );
                self.control_feedback(bounds, GuiAction::Tab(direction));
                self.control_hits.push(GuiHit {
                    bounds,
                    action: GuiAction::Tab(direction),
                });
            }
            column += width + 1;
        }
    }
    pub(super) fn draw_help(&mut self, layout: GuiLayout) -> Result<()> {
        let lines = self.ui.help_lines();
        let padding = self.cell.0;
        let w = (self.cell.0 * 72.).min((layout.width - padding * 2.).max(self.cell.0));
        let columns = ((w - padding * 2.) / self.cell.0).max(1.) as u16;
        let rows = ((layout.height - self.cell.1 * 4.) / self.cell.1).max(1.) as usize;
        let count = lines.len().min(rows);
        self.ui.help_scroll = self.ui.help_scroll.min(lines.len().saturating_sub(count));
        let h = self.cell.1 * (count + 3) as f32;
        let x = (layout.width - w) * 0.5;
        let y = (layout.height - h) * 0.5;
        self.renderer.clip(0., 0., layout.width, layout.height)?;
        self.renderer
            .rect(0., 0., layout.width, layout.height, 0x00000088, false);
        self.renderer.rect(x, y, w, h, CHROME, false);
        self.renderer.clip(x, y, w, h)?;
        self.text.draw(
            &self.renderer,
            "KEYS",
            x + padding,
            y + self.cell.1 * 0.5,
            ACCENT,
        )?;
        for (row, line) in lines
            .into_iter()
            .skip(self.ui.help_scroll)
            .take(count)
            .enumerate()
        {
            self.text.draw(
                &self.renderer,
                &fit_label(line, columns),
                x + padding,
                y + self.cell.1 * (row as f32 + 1.5),
                INK,
            )?;
        }
        let label = " ↑↓ scroll · Esc close ";
        let close_w = text_width(label) as f32 * self.cell.0;
        self.control_feedback(
            (x + w - close_w - padding, y, close_w, self.cell.1 * 1.5),
            GuiAction::DismissHelp,
        );
        self.text.draw(
            &self.renderer,
            label,
            x + w - close_w - padding,
            y + self.cell.1 * 0.5,
            MUTED,
        )?;
        self.control_hits.push(GuiHit {
            bounds: (x + w - close_w - padding, y, close_w, self.cell.1 * 1.5),
            action: GuiAction::DismissHelp,
        });
        self.renderer.clip(0., 0., 0., 0.)
    }
    pub(super) fn draw_search(&mut self, layout: GuiLayout) -> Result<()> {
        let Some(pane) = self.client.snapshot().and_then(|snapshot| {
            snapshot
                .panes
                .iter()
                .find(|pane| pane.id == self.ui.search.pane)
        }) else {
            return Ok(());
        };
        let (grid_x, grid_y) = layout.origin(self.cell);
        let x = grid_x + f32::from(pane.rect.x) * self.cell.0;
        let y = grid_y + f32::from(pane.rect.y + pane.rect.rows.saturating_sub(1)) * self.cell.1;
        let width = f32::from(pane.rect.cols) * self.cell.0;
        let columns = pane.rect.cols.saturating_sub(2);
        self.renderer.clip(x, y, width, self.cell.1)?;
        self.renderer.rect(x, y, width, self.cell.1, CHROME, false);
        let label = self.ui.search.label(&self.preedit);
        self.text.draw(
            &self.renderer,
            &fit_label(&label, columns),
            x + self.cell.0,
            y,
            INK,
        )?;
        self.renderer.rect(x, y, width, 1., BORDER, false);
        let caret = text_width("⌕ ")
            + text_width(&self.ui.search.query[..self.ui.search.cursor])
            + text_width(&self.preedit);
        self.window.cursor_rect(
            (x + self.cell.0 * (1 + caret.min(usize::from(columns))) as f32) as i32,
            y as i32,
            self.cell.0 as i32,
            self.cell.1 as i32,
        );
        self.renderer.clip(0., 0., 0., 0.)
    }
    pub(super) fn draw_close(&mut self, width: f32, height: f32) -> Result<()> {
        let rects = close_layout(width, height, self.cell);
        let (x, y, w, h) = rects[0];
        let padding = self.cell.0.min(w * 0.1);
        let columns = ((w - padding * 2.) / self.cell.0) as usize;
        self.renderer.rect(x, y, w, h, 0x404040ff, false);
        self.renderer
            .rect(x + 1., y + 1., w - 2., h - 2., 0x202020ff, false);
        self.renderer.clip(x + 1., y + 1., w - 2., h - 2.)?;
        let name = self.client.snapshot().map_or("", |s| s.name.as_str());
        self.text.draw(
            &self.renderer,
            &fit(
                &format!(
                    "{} · {name}",
                    if self.ui.close_tab.is_some() {
                        "Close tab"
                    } else {
                        "Close window"
                    }
                ),
                columns,
            ),
            x + padding,
            y + h * 0.08,
            0xccccccff,
        )?;
        self.text.draw(
            &self.renderer,
            &fit(
                if self.ui.close_tab.is_some() {
                    "Programs may still be running in this tab"
                } else {
                    "End session to stop tasks and delete history"
                },
                columns,
            ),
            x + padding,
            y + h * 0.22,
            0x888888ff,
        )?;
        let name = if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
        {
            name.to_owned()
        } else {
            format!("'{}'", name.replace('\'', "'\\''"))
        };
        self.text.draw(
            &self.renderer,
            &fit(
                &if self.ui.close_tab.is_some() {
                    "Closing terminates all panes in this tab".into()
                } else {
                    format!("Resume with: nextty attach {name}")
                },
                columns,
            ),
            x + padding,
            y + h * 0.36,
            0x888888ff,
        )?;
        for ((choice, label), &(bx, by, bw, bh)) in [
            (
                CloseChoice::Terminate,
                if self.ui.close_tab.is_some() {
                    "Close tab"
                } else {
                    "End session"
                },
            ),
            (
                CloseChoice::Background,
                if self.ui.close_tab.is_some() {
                    "Cancel (default)"
                } else {
                    "Keep running (default)"
                },
            ),
        ]
        .into_iter()
        .zip(&rects[1..])
        {
            let selected = self.closing == Some(choice);
            let hovered = self.pointer.0 >= bx
                && self.pointer.0 < bx + bw
                && self.pointer.1 >= by
                && self.pointer.1 < by + bh;
            let ink = match choice {
                CloseChoice::Terminate => {
                    if selected || hovered {
                        0xff8a94ff
                    } else {
                        0xe05b65ff
                    }
                }
                CloseChoice::Background => {
                    if selected || hovered {
                        ACCENT
                    } else {
                        INK
                    }
                }
            };
            if selected || hovered {
                self.renderer.rect(
                    bx,
                    by,
                    bw,
                    bh,
                    if choice == CloseChoice::Terminate {
                        0x633534ff
                    } else {
                        0x353e35ff
                    },
                    false,
                );
            }
            let label = fit(label, (bw / self.cell.0).floor() as usize);
            self.text.draw(
                &self.renderer,
                &label,
                bx + (bw - text_width(&label) as f32 * self.cell.0) * 0.5,
                by + (bh - self.cell.1) * 0.5,
                ink,
            )?;
        }
        self.text.draw(
            &self.renderer,
            &fit("Enter: confirm · Tab/←/→: select · Esc: cancel", columns),
            x + padding,
            y + h * 0.83,
            0x666666ff,
        )?;
        self.renderer.clip(0., 0., 0., 0.)
    }
    #[cfg(feature = "files")]
    pub(super) fn draw_files(&mut self) -> Result<()> {
        let Some(files) = &self.ui.files else {
            return Ok(());
        };
        let layout = self.file_layout();
        let FileLayout {
            x,
            y,
            w,
            h,
            split,
            top,
            rows,
        } = layout;
        let preview_focus = files.focus() == FileFocus::Preview;
        self.renderer
            .rounded_rect((x - 1., y - 1.), (w + 2., h + 2.), 7., 0x404040ff);
        self.renderer.rounded_rect((x, y), (w, h), 6., 0x202020ff);
        let directory = std::env::var_os("HOME")
            .and_then(|home| {
                files
                    .directory()
                    .strip_prefix(home)
                    .ok()
                    .map(|suffix| Cow::Owned(format!("~/{}", suffix.to_string_lossy())))
            })
            .unwrap_or_else(|| files.directory().to_string_lossy());
        let header_cols = ((w / self.cell.0) as usize).saturating_sub(3);
        let title = format!(" {} ", fit(&directory, header_cols.saturating_sub(2)));
        self.renderer.rect(
            x + self.cell.0,
            y - self.cell.1 * 0.5,
            text_width(&title) as f32 * self.cell.0,
            self.cell.1,
            0x202020ff,
            false,
        );
        self.text.draw(
            &self.renderer,
            &title,
            x + self.cell.0,
            y - self.cell.1 * 0.5,
            0x606060ff,
        )?;
        self.renderer.clip(x, y + 1., w, h - 1.)?;
        if files.is_search() {
            self.text.draw(
                &self.renderer,
                "⌕",
                x + self.cell.0,
                y + self.cell.1 * 0.5,
                0xafc460ff,
            )?;
            self.text.draw(
                &self.renderer,
                &fit(
                    &format!("{}{}", files.query(), self.preedit),
                    header_cols.saturating_sub(2),
                ),
                x + self.cell.0 * 3.,
                y + self.cell.1 * 0.5,
                0xccccccff,
            )?;
        }
        let content_h = y + h - top - self.cell.1 * 0.3;
        if preview_focus {
            self.renderer.rect(
                split + 1.,
                top,
                x + w - split - self.cell.0 * 0.5,
                content_h,
                0x262626ff,
                false,
            );
        }
        self.renderer
            .rect(split, top, 1., content_h, 0x303030ff, false);
        let list_width = split - x - self.cell.0;
        self.renderer.clip(x + 1., top, split - x - 2., content_h)?;
        self.file_hits.clear();
        let start = files
            .selection()
            .saturating_sub(rows / 2)
            .min(files.entries().len().saturating_sub(rows));
        for (index, entry) in files.entries().iter().enumerate().skip(start).take(rows) {
            let ry = top + (index - start) as f32 * self.cell.1;
            if index == files.selection() {
                self.renderer.rect(
                    x + self.cell.0 * 0.5,
                    ry,
                    split - x - self.cell.0,
                    self.cell.1,
                    if preview_focus {
                        0x232323ff
                    } else {
                        0x262626ff
                    },
                    false,
                );
            }
            let label = if files.is_search() {
                entry.display_path(files.directory())
            } else {
                entry.display_name()
            };
            self.text.draw(
                &self.renderer,
                &fit(&label, (list_width / self.cell.0) as usize),
                x + self.cell.0,
                ry,
                0x666666ff,
            )?;
            self.file_hits.push((ry, ry + self.cell.1, index));
        }
        if files.entries().is_empty() {
            let label = if let Some(error) = files.error() {
                error.to_string()
            } else if files.searching() {
                "Loading…".into()
            } else {
                "Empty directory".into()
            };
            self.text.draw(
                &self.renderer,
                &fit(&label, (list_width / self.cell.0) as usize),
                x + self.cell.0,
                top,
                0x606060ff,
            )?;
        }
        let preview_x = split + self.cell.0;
        let preview_w = x + w - preview_x - self.cell.0;
        self.renderer
            .clip(split + 1., top, x + w - split - 2., content_h)?;
        if let Some(error) = files.preview_error() {
            self.text.draw(
                &self.renderer,
                &fit(&error.to_string(), (preview_w / self.cell.0) as usize),
                preview_x,
                top,
                0x888888ff,
            )?;
        } else {
            for (row, line) in files
                .preview_lines()
                .iter()
                .skip(files.preview_scroll())
                .take(rows)
                .enumerate()
            {
                self.text.draw(
                    &self.renderer,
                    &fit(line, (preview_w / self.cell.0) as usize),
                    preview_x,
                    top + row as f32 * self.cell.1,
                    0x888888ff,
                )?;
            }
        }
        if files.is_search() && !preview_focus && self.closing.is_none() {
            let column = text_width(files.query()).min(header_cols.saturating_sub(3));
            self.window.cursor_rect(
                (x + self.cell.0 * (3 + column) as f32) as i32,
                (y + self.cell.1 * 0.5) as i32,
                self.cell.0 as i32,
                self.cell.1 as i32,
            );
        }
        self.renderer.clip(0., 0., 0., 0.)
    }
}

fn fit(text: &str, limit: usize) -> Cow<'_, str> {
    // With widths capped at two columns, UTF-8 byte length is an upper bound.
    if text.len() <= limit && !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut result = String::with_capacity(text.len().min(limit));
    let mut width = 0;
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        let w = usize::from(scalar_width(c).min(2));
        if width + w > limit {
            if limit > 0 {
                while width >= limit {
                    let Some(previous) = result.pop() else {
                        break;
                    };
                    width -= usize::from(scalar_width(previous).min(2));
                }
                result.push('…');
            }
            break;
        }
        result.push(c);
        width += w;
    }
    Cow::Owned(result)
}

#[cfg(test)]
mod tests {
    use crate::workspace::{Snapshot, TabView};

    #[test]
    fn overflow_keeps_active_tab_and_counts_both_hidden_sides() {
        let snapshot = Snapshot {
            name: "dev".into(),
            controller: true,
            focused: 1,
            active_tab: 6,
            tabs: (1..=12)
                .map(|id| TabView {
                    activity: crate::workspace::TabActivity::Idle,
                    id,
                    title: format!("tab-{id}"),
                })
                .collect(),
            panes: Vec::new(),
        };
        let (tabs, label, before, after) = crate::workspace::tab_chrome(&snapshot, 80, "SSH");
        assert!(
            tabs.iter()
                .any(|(index, _, _)| snapshot.tabs[*index].id == snapshot.active_tab)
        );
        assert_eq!(before + tabs.len() + after, snapshot.tabs.len());
        assert!(before > 0 && after > 0);
        assert!(label.contains('‹') && label.contains('›') && label.ends_with("SSH"));
    }
}
