use super::*;

#[derive(Default)]
pub(super) struct AgentState {
    pub hits: Vec<(Rect, AgentAction)>,
    pub chat: Rect,
    pub chat_rows: Vec<(usize, Rect)>,
    pub selecting_editor: bool,
}

impl Frontend {
    pub(super) fn agent_width(&self) -> u16 {
        (self.size.num_cols / 2)
            .clamp(20, 80)
            .min(self.size.num_cols.saturating_sub(2))
    }

    pub(super) fn agent_rect(&self) -> Rect {
        let width = self.agent_width();
        let rows = self.view_size().num_lines.saturating_sub(1);
        rect(self.size.num_cols - width, 1, width, rows)
    }

    fn editor_pointer(&mut self, pointer: PointerEvent, extend: bool) {
        let (rect, layout) = editor(&self.ui, self.agent_rect());
        let row = f32::from(pointer.row) - f32::from(rect.y);
        let column = pointer.col.saturating_sub(rect.x).min(rect.cols) as f32;
        let rows = usize::from(rect.rows);
        let session = self.ui.agent.active_mut();
        session.select_editor(layout, rows, (column, row), extend);
    }

    pub(super) fn agent_captures_mouse(&self) -> bool {
        let session = self.ui.agent.active();
        session.context.preview.is_some() || self.agent.selecting_editor || session.selecting()
    }

    fn chat_point(&self, pointer: PointerEvent) -> Option<crate::agent::panel::TranscriptPoint> {
        let session = self.ui.agent.active();
        let rows = &self.agent.chat_rows;
        crate::ui::transcript_point(session, rows, pointer.col, pointer.row, true)
    }

    pub(super) fn agent_mouse(&mut self, pointer: PointerEvent) -> io::Result<bool> {
        let rect = self.agent_rect();
        if !self.agent_captures_mouse() && !(self.ui.agent.open && pointer.col >= rect.x) {
            if pointer.press(0) && self.ui.agent.active().rewind_target.is_some() {
                self.ui.cancel_rewind();
                self.dirty = true;
            }
            return Ok(false);
        }
        self.dirty = true;
        let released = pointer.kind == PointerKind::Release;
        let shift = pointer.mods & crate::input::SHIFT != 0;
        if self.ui.agent.active().selecting() {
            let chat = self.agent.chat;
            let session = self.ui.agent.active_mut();
            if pointer.row < chat.y {
                session.model.scroll = session.model.scroll.saturating_add(1);
            } else if pointer.row >= chat.y + chat.rows {
                session.model.scroll = session.model.scroll.saturating_sub(1);
            }
            if let Some(point) = self.chat_point(pointer) {
                self.ui.agent.active_mut().extend_selection(point);
            }
            if released {
                self.ui.agent.active_mut().end_selection();
            }
            return Ok(true);
        }
        if self.agent.selecting_editor {
            if released {
                self.agent.selecting_editor = false;
            } else {
                self.editor_pointer(pointer, true);
            }
            return Ok(true);
        }
        match pointer.kind {
            PointerKind::Release | PointerKind::Motion => return Ok(true),
            PointerKind::Scroll => {
                let (rect, layout) = editor(&self.ui, rect);
                self.ui.agent_wheel(
                    pointer.wheel(),
                    rect.contains(pointer.col, pointer.row)
                        .then(|| (layout.first, layout.lines.len(), usize::from(rect.rows))),
                );
                return Ok(true);
            }
            PointerKind::Press => (),
        }
        let hit = crate::ui::agent_hit(&self.agent.hits, pointer.col, pointer.row);
        let right = pointer.button == 2;
        if right
            && self.agent.chat.contains(pointer.col, pointer.row)
            && self.ui.agent.active().context.preview.is_none()
            && let Some(text) = self.ui.agent.active().selected_text()
        {
            self.copy_text(text, false);
            return Ok(true);
        }
        if !right && let Some(AgentAction::Copy(_) | AgentAction::Tool(_)) = hit {
            let marker =
                matches!(hit, Some(AgentAction::Tool(_))) && pointer.col < self.agent.chat.x + 2;
            if !marker && let Some(point) = self.chat_point(pointer) {
                self.ui.cancel_rewind();
                self.ui.agent.active_mut().begin_selection(point, shift);
                self.ui.focus = Focus::Output;
                return Ok(true);
            }
        }
        match hit {
            Some(AgentAction::Editor) if right => self.activate_agent(AgentAction::ContextMenu),
            Some(AgentAction::Editor) => {
                self.activate_agent(AgentAction::Editor);
                self.editor_pointer(pointer, shift);
                self.agent.selecting_editor = true;
            }
            Some(AgentAction::Tool(index) | AgentAction::Copy(index)) if right => {
                self.activate_agent(AgentAction::Copy(index));
            }
            Some(AgentAction::Copy(_)) => self.ui.focus = Focus::Output,
            Some(_) if right => (),
            Some(action) => self.activate_agent(action),
            None => (),
        }
        Ok(true)
    }

    fn activate_agent(&mut self, action: AgentAction) {
        match self.ui.activate_agent(action, &self.client) {
            Ok(Some(text)) => self.copy_text(text, false),
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
    }

    pub(super) fn draw_agent(&mut self, frame: &mut Grid) -> io::Result<Option<(u16, u16)>> {
        let side = self.agent_rect();
        let panel = crate::agent::panel::draw(
            &mut self.ui,
            &self.config,
            side,
            self.size,
            self.pointer,
            "",
        )?;
        if let Some(backdrop) = &panel.backdrop {
            frame.blit(backdrop);
        }
        frame.blit(&panel.grid);
        self.agent.hits = panel.hits;
        self.agent.chat = panel.chat;
        self.agent.chat_rows = panel.chat_rows;
        self.ui.agent.spare_grid = Some(panel.grid);
        Ok(panel.cursor)
    }
}
