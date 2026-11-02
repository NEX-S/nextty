use super::*;

#[derive(Default)]
pub(super) struct AgentState {
    pub hits: Vec<(Rect, AgentAction)>,
    pub chat: Rect,
    pub first: usize,
    pub frame: Option<crate::panel::Grid>,
    pub dirty: bool,
    pub selecting_editor: bool,
    pub last_click: Option<(Instant, PaneId, Point)>,
}

impl Frontend {
    pub(super) fn agent_width(&self) -> u16 {
        (self.size.num_cols / 2)
            .clamp(20, 80)
            .min(self.size.num_cols.saturating_sub(2))
    }

    pub(super) fn agent_rect(&self) -> Rect {
        let width = self.agent_width();
        Rect {
            x: self.size.num_cols - width,
            y: 1,
            cols: width,
            rows: self.view_size().num_lines.saturating_sub(1),
        }
    }

    pub(super) fn editor_pointer(&mut self, mouse: Mouse, extend: bool) {
        let (rect, layout) = editor(&self.ui, self.agent_rect());
        let row = f32::from(mouse.row) - f32::from(rect.y);
        let column = mouse.column.saturating_sub(rect.x).min(rect.cols) as f32;
        self.ui.agent.active_mut().select_editor(
            layout,
            usize::from(rect.rows),
            (column, row),
            extend,
        );
        self.dirty = true;
        self.agent.dirty = true;
    }

    pub(super) fn agent_mouse(&mut self, mouse: Mouse) -> io::Result<bool> {
        if self.ui.agent.active().context.preview.is_none()
            && !self.agent.selecting_editor
            && !self.ui.agent.active().selecting()
            && !(self.ui.agent.open && mouse.column >= self.agent_rect().x)
        {
            if !mouse.release
                && mouse.code & 0b1110_0011 == 0
                && self.ui.agent.active().rewind_target.is_some()
            {
                self.ui.cancel_rewind();
                self.agent.dirty = true;
                self.dirty = true;
            }
            if self.ui.agent.open
                && self
                    .pointer
                    .is_some_and(|(column, row)| self.agent_rect().contains(column, row))
            {
                self.agent.dirty = true;
                self.dirty = true;
            }
            return Ok(false);
        }
        let pointer = Some((mouse.column, mouse.row));
        if self.pointer != pointer {
            self.pointer = pointer;
            self.agent.dirty = true;
            self.dirty = true;
        }
        if self.ui.agent.active().selecting() {
            self.chat_pointer(mouse);
            if mouse.release {
                self.ui.agent.active_mut().end_selection();
            }
            return Ok(true);
        }
        if self.agent.selecting_editor {
            if mouse.release {
                self.agent.selecting_editor = false;
            } else {
                self.editor_pointer(mouse, true);
            }
            return Ok(true);
        }
        if mouse.release {
            return Ok(true);
        }
        if mouse.code & 64 != 0 {
            let up = mouse.code & 1 == 0;
            if let Some(crate::agent::panel::ContextPreview { scroll, .. }) =
                &mut self.ui.agent.active_mut().context.preview
            {
                *scroll = if up {
                    scroll.saturating_sub(3)
                } else {
                    scroll.saturating_add(3)
                };
            } else {
                let (rect, layout) = editor(&self.ui, self.agent_rect());
                if rect.contains(mouse.column, mouse.row) {
                    let (lines, first) = (layout.lines, layout.first);
                    self.ui.agent.active_mut().editor_scroll = Some(if up {
                        first.saturating_sub(3)
                    } else {
                        first
                            .saturating_add(3)
                            .min(lines.len().saturating_sub(usize::from(rect.rows)))
                    });
                } else {
                    self.ui.agent.active_mut().model.scroll = if up {
                        self.ui.agent.active().model.scroll.saturating_add(3)
                    } else {
                        self.ui.agent.active().model.scroll.saturating_sub(3)
                    };
                }
            }
            self.dirty = true;
            self.agent.dirty = true;
            return Ok(true);
        }
        if mouse.code & 32 != 0 {
            return Ok(true);
        }
        let hit = self
            .agent
            .hits
            .iter()
            .rfind(|(rect, _)| rect.contains(mouse.column, mouse.row))
            .map(|(_, action)| *action);
        let right = mouse.code & 3 == 2;
        if right
            && self.agent.chat.contains(mouse.column, mouse.row)
            && self.ui.agent.active().context.preview.is_none()
            && let Some(text) = self.ui.agent.active().selected_text()
        {
            self.copy_text(text, false);
            return Ok(true);
        }
        if !right && let Some(AgentAction::Copy(_) | AgentAction::Tool(_)) = hit {
            let marker =
                matches!(hit, Some(AgentAction::Tool(_))) && mouse.column < self.agent.chat.x + 2;
            if !marker && let Some(point) = self.chat_point(mouse) {
                self.ui.cancel_rewind();
                self.ui
                    .agent
                    .active_mut()
                    .begin_selection(point, mouse.code & 4 != 0);
                self.ui.focus = Focus::Output;
                self.dirty = true;
                self.agent.dirty = true;
                return Ok(true);
            }
        }
        if let Some(action) = hit {
            if right
                && !matches!(
                    action,
                    AgentAction::Editor | AgentAction::Copy(_) | AgentAction::Tool(_)
                )
            {
                return Ok(true);
            }
            match action {
                AgentAction::Editor if right => {
                    self.activate_agent(AgentAction::ContextMenu)?;
                }
                AgentAction::Editor => {
                    self.activate_agent(AgentAction::Editor)?;
                    self.editor_pointer(mouse, mouse.code & 4 != 0);
                    self.agent.selecting_editor = true;
                }
                AgentAction::Tool(index) | AgentAction::Copy(index) if right => {
                    self.activate_agent(AgentAction::Copy(index))?;
                }
                AgentAction::Copy(_) => self.ui.focus = Focus::Output,
                action => self.activate_agent(action)?,
            }
        }
        self.dirty = true;
        self.agent.dirty = true;
        Ok(true)
    }

    pub(super) fn chat_point(&self, mouse: Mouse) -> Option<crate::agent::panel::TranscriptPoint> {
        let session = self.ui.agent.active();
        let row = mouse
            .row
            .saturating_sub(self.agent.chat.y)
            .min(self.agent.chat.rows.saturating_sub(1));
        let line = self.agent.first + usize::from(row);
        if !matches!(
            session.lines.get(line),
            Some(AgentLine::Role(_) | AgentLine::Text { .. })
        ) {
            return (0..line.min(session.lines.len()))
                .rev()
                .find_map(|line| session.point(line, usize::MAX));
        }
        let indent = match session.lines.get(line)? {
            AgentLine::Role(_) => 3,
            AgentLine::Text { message, .. } => {
                let message = session.model.messages.get(*message as usize)?;
                if message.role == "You" {
                    1
                } else if message.id.is_some() {
                    2
                } else {
                    0
                }
            }
            AgentLine::Blank => return None,
        };
        session.point(
            line,
            usize::from(mouse.column.saturating_sub(self.agent.chat.x + indent)),
        )
    }

    pub(super) fn chat_pointer(&mut self, mouse: Mouse) {
        if mouse.row < self.agent.chat.y {
            self.ui.agent.active_mut().model.scroll =
                self.ui.agent.active().model.scroll.saturating_add(1);
        } else if mouse.row >= self.agent.chat.y + self.agent.chat.rows {
            self.ui.agent.active_mut().model.scroll =
                self.ui.agent.active().model.scroll.saturating_sub(1);
        }
        if let Some(point) = self.chat_point(mouse) {
            self.ui.agent.active_mut().extend_selection(point);
        }
        self.agent.dirty = true;
        self.dirty = true;
    }

    pub(super) fn activate_agent(&mut self, action: AgentAction) -> io::Result<()> {
        match self.ui.activate_agent(action, &self.client) {
            Ok(Some(text)) => {
                self.copy_text(text, false);
            }
            Ok(None) => (),
            Err(error) => self.message(error.to_string()),
        }
        if matches!(
            action,
            AgentAction::ClosePreview
                | AgentAction::ClosePanel
                | AgentAction::NewSession
                | AgentAction::Session(_)
                | AgentAction::CloseSession(_)
        ) {
            self.agent.selecting_editor = false;
            if !matches!(self.paste, Some(PasteTarget::Agent(_))) {
                self.panel_utf8.clear();
            }
        }
        self.ui.agent.active_mut().columns = 0;
        self.agent.dirty = true;
        self.dirty = true;
        if action == AgentAction::ClosePanel {
            self.resize()?;
        }
        Ok(())
    }

    pub(super) fn draw_agent(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let side = self.agent_rect();
        let panel = crate::agent::panel::draw(
            &mut self.ui,
            &self.config,
            side,
            self.size,
            self.pointer,
            "",
        )?;
        let previous = if self.full {
            None
        } else {
            self.agent.frame.as_ref()
        };
        panel.grid.ansi(output, previous, &self.damage)?;
        self.agent.hits = panel.hits;
        self.agent.chat = panel.chat;
        self.agent.first = panel.first;
        self.ui.agent.spare_grid = self.agent.frame.replace(panel.grid);
        Ok(())
    }
}
