use super::*;

pub(super) struct EditorViewport {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) columns: usize,
    pub(super) rows: usize,
}

#[derive(Default)]
pub(super) struct AgentState {
    pub(super) drag: Option<f32>,
    selecting_editor: bool,
    chat_rows: Vec<(usize, crate::workspace::Rect)>,
    pub(super) hits: Vec<(crate::workspace::Rect, AgentAction)>,
    tool_click: Option<usize>,
}

impl WorkspaceView {
    pub(super) fn agent_point(&self, clamp: bool) -> Option<crate::agent::panel::TranscriptPoint> {
        let (column, row) = self.cell();
        crate::ui::transcript_point(
            self.ui.agent.active(),
            &self.agent.chat_rows,
            column,
            row,
            clamp,
        )
    }

    pub(super) fn editor_viewport(&self) -> Option<EditorViewport> {
        let (rect, _) = self
            .agent
            .hits
            .iter()
            .find(|(_, action)| *action == AgentAction::Editor)?;
        let columns = self
            .ui
            .agent
            .active()
            .editor_columns(usize::from(rect.cols));
        Some(EditorViewport {
            x: f32::from(rect.x) * self.cell.0,
            y: f32::from(rect.y) * self.cell.1,
            columns,
            rows: usize::from(rect.rows).max(1),
        })
    }

    pub(super) fn editor_pointer(&mut self, extend: bool) {
        if self.paste.is_some() {
            return;
        }
        let Some(layout) = self.editor_viewport() else {
            return;
        };
        let editor = self
            .ui
            .agent
            .active()
            .editor_layout(layout.columns, layout.rows);
        let row = (self.pointer.1 - layout.y) / self.cell.1;
        let column = ((self.pointer.0 - layout.x) / self.cell.0).clamp(0., layout.columns as f32);
        self.ui
            .agent
            .active_mut()
            .select_editor(editor, layout.rows, (column, row), extend);
        self.dirty = true;
    }

    /// Clicks while a context preview covers the window.
    pub(super) fn preview_button(&mut self, button: u32) -> Result<()> {
        if button == 1
            && let Some(action) =
                crate::ui::agent_hit(&self.agent.hits, self.cell().0, self.cell().1).filter(
                    |action| matches!(action, AgentAction::ClosePreview | AgentAction::CopyPreview),
                )
        {
            self.gui_action(GuiAction::Agent(action))?;
        }
        Ok(())
    }
    pub(super) fn agent_control_button(
        &mut self,
        button: u32,
        action: AgentAction,
        modifiers: u32,
    ) -> Result<bool> {
        match (button, action) {
            (3, AgentAction::Copy(index) | AgentAction::Tool(index)) => {
                if let Some(text) = self.ui.agent.active().selected_text() {
                    self.window.selection(false, &text);
                    self.message("Copied");
                } else {
                    self.gui_action(GuiAction::Agent(AgentAction::Copy(index)))?;
                }
            }
            (3, AgentAction::Editor) => {
                self.gui_action(GuiAction::Agent(AgentAction::ContextMenu))?
            }
            (1, AgentAction::Copy(_) | AgentAction::Tool(_)) => {
                if self.composition_finished() {
                    self.ui.focus = Focus::Output;
                    if let Some(point) = self.agent_point(false) {
                        self.ui
                            .agent
                            .active_mut()
                            .begin_selection(point, modifiers & native::SHIFT != 0);
                        self.agent.tool_click = match action {
                            AgentAction::Tool(index) => Some(index),
                            _ => None,
                        };
                    }
                    self.dirty = true;
                }
            }
            (1 | 2, AgentAction::Editor) => {
                if self.composition_finished() {
                    self.ui.focus = Focus::Editor;
                    self.ui.cancel_rewind();
                    self.ui.agent.active_mut().context.menu = false;
                    self.editor_pointer(modifiers & native::SHIFT != 0);
                    self.agent.selecting_editor = button == 1;
                    if button == 2 {
                        self.request_paste(PasteTarget::Agent(self.ui.agent.active().id), true);
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
    pub(super) fn draw_agent(&mut self, layout: GuiLayout) -> Result<()> {
        if !self.ui.agent.open {
            return Ok(());
        }
        let side = rect(
            (layout.side / self.cell.0) as u16,
            1,
            ((layout.width - layout.side) / self.cell.0) as u16,
            ((layout.footer - layout.top) / self.cell.1).max(1.) as u16,
        );
        let size = WindowSize {
            num_cols: (layout.width / self.cell.0) as u16,
            num_lines: (layout.height / self.cell.1) as u16,
            cell_width: self.cell.0 as u16,
            cell_height: self.cell.1 as u16,
        };
        let pointer = Some(self.cell());
        let panel = crate::agent::panel::draw(
            &mut self.ui,
            &self.config,
            side,
            size,
            pointer,
            &self.preedit,
        )?;
        if let Some(backdrop) = &panel.backdrop {
            self.draw_panel(backdrop);
        }
        self.draw_panel(&panel.grid);
        self.agent.chat_rows = panel.chat_rows;
        self.agent.hits = panel.hits;
        let (cell, session) = (self.cell, self.ui.agent.active());
        let editor = session.editor_layout(
            session.editor_columns(usize::from(panel.editor.cols)),
            usize::from(panel.editor.rows),
        );
        let transcript = ScrollTarget::Transcript;
        let bars = [
            (
                transcript,
                panel.chat,
                session.lines.len(),
                session.model.scroll,
            ),
            (
                ScrollTarget::Editor,
                panel.editor,
                editor.lines.len(),
                editor.first,
            ),
        ];
        for (target, rect, total, offset) in bars {
            self.scrollbars
                .extend(Scrollbar::in_rect(target, rect, cell, total, offset));
        }
        if let Some(rect) = panel.preview
            && let Some(preview) = &session.context.preview
        {
            let target = ScrollTarget::Preview;
            self.scrollbars.extend(Scrollbar::in_rect(
                target,
                rect,
                cell,
                preview.lines.len(),
                preview.scroll,
            ));
        }
        if let Some((column, row)) = panel.cursor {
            let x = f32::from(column) * self.cell.0;
            let y = f32::from(row) * self.cell.1;
            self.painter.rect(x, y, 2., self.cell.1, INK);
            self.window
                .cursor_rect(x as i32, y as i32, self.cell.0 as i32, self.cell.1 as i32);
        }
        self.ui.agent.spare_grid = Some(panel.grid);
        Ok(())
    }
    pub(super) fn agent_refresh(&mut self, revision: u64, messages_changed: bool) {
        if revision != self.ui.agent.active().model.revision {
            self.agent.chat_rows.clear();
            self.agent.tool_click = None;
        }
        if messages_changed {
            self.agent.hits.retain(|(_, action)| {
                !matches!(
                    action,
                    AgentAction::Copy(_) | AgentAction::Tool(_) | AgentAction::Rewind(_)
                )
            });
        }
    }

    pub(super) fn agent_action(&mut self, action: AgentAction) -> bool {
        if !matches!(
            action,
            AgentAction::Stop
                | AgentAction::Remove(_)
                | AgentAction::ClosePreview
                | AgentAction::CopyPreview
        ) && !self.composition_finished()
        {
            return false;
        }
        match self.ui.activate_agent(action, &self.client) {
            Ok(Some(copy)) => {
                self.window.selection(false, &copy);
                self.message("Copied");
            }
            Ok(None) => (),
            Err(error) => self.message(error.to_string()),
        }
        self.agent.selecting_editor = false;
        self.agent.tool_click = None;
        self.agent.chat_rows.clear();
        true
    }

    pub(super) fn agent_button_release(&mut self, button: u32) -> Result<bool> {
        if button != 1 {
            return Ok(false);
        }
        if self.ui.agent.active().selecting() {
            if let Some(point) = self.agent_point(true) {
                self.ui.agent.active_mut().extend_selection(point);
            }
            self.ui.agent.active_mut().end_selection();
            if let Some(text) = self.ui.agent.active().selected_text() {
                self.window.selection(true, &text);
            } else if let Some(index) = self.agent.tool_click.take() {
                self.gui_action(GuiAction::Agent(AgentAction::Tool(index)))?;
            }
            self.agent.tool_click = None;
            self.dirty = true;
            return Ok(true);
        }
        if self.ui.resources.dragging {
            return Ok(false);
        }
        if std::mem::take(&mut self.agent.selecting_editor) {
            if let Some(range) = self.ui.agent.active().model.selection() {
                self.window
                    .selection(true, &self.ui.agent.active().model.input[range]);
            }
            self.dirty = true;
            return Ok(true);
        }
        Ok(self.scrollbar_drag.is_none() && self.agent.drag.take().is_some())
    }

    pub(super) fn agent_motion(&mut self, previous: (f32, f32), layout: GuiLayout) -> Result<bool> {
        if self.ui.agent.open
            && self.pointer.0 >= layout.side
            && (previous.1 / self.cell.1).floor() != (self.pointer.1 / self.cell.1).floor()
        {
            self.dirty = true;
        }
        if self.ui.resources.dragging {
            return Ok(false);
        }
        if self.ui.agent.active().selecting() {
            if let Some(point) = self.agent_point(true) {
                self.ui.agent.active_mut().extend_selection(point);
            }
            self.dirty = true;
            return Ok(true);
        }
        if self.agent.selecting_editor {
            self.editor_pointer(true);
            return Ok(true);
        }
        if self.scrollbar_drag.is_some() || self.preview_open() {
            return Ok(false);
        }
        if let Some(grab) = self.agent.drag {
            let minimum = self.cell.0 * 24. + PADDING * 2.;
            let agent_width = (layout.width - self.pointer.0 + grab)
                .clamp(minimum, (layout.width * 0.62).max(minimum));
            if Some(agent_width) != self.ui.agent.width {
                let previous = layout.size(self.cell);
                self.ui.agent.width = Some(agent_width);
                if self.layout().size(self.cell) != previous {
                    self.resize()?;
                }
                self.dirty = true;
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// `down` is the wheel travel in lines, positive towards newer content.
    pub(super) fn agent_scroll(&mut self, down: isize) -> bool {
        let layout = self.layout();
        let in_panel = self.ui.agent.open
            && self.pointer.0 >= layout.side
            && !(self.ui.resources.open && self.pointer.1 >= layout.footer);
        if !self.preview_open() && !in_panel {
            return false;
        }
        let (column, row) = self.cell();
        let editor = (crate::ui::agent_hit(&self.agent.hits, column, row)
            == Some(AgentAction::Editor))
        .then(|| self.editor_viewport())
        .flatten()
        .map(|viewport| {
            let editor = self
                .ui
                .agent
                .active()
                .editor_layout(viewport.columns, viewport.rows);
            (editor.first, editor.lines.len(), viewport.rows)
        });
        self.ui.agent_wheel(down, editor);
        self.dirty = true;
        true
    }
    pub(super) fn agent_button(&mut self, button: u32, layout: GuiLayout) -> bool {
        if self.ui.agent.active().context.menu {
            self.ui.agent.active_mut().context.menu = false;
            self.dirty = true;
        }
        if layout.terminal_contains(self.pointer) && self.ui.focus != Focus::Terminal {
            if !self.composition_finished() {
                return false;
            }
            self.ui.focus = Focus::Terminal;
            self.ui.cancel_rewind();
            self.dirty = true;
        }
        if button == 1
            && self.ui.agent.open
            && self.pointer.0 >= layout.side
            && (layout.top..layout.footer).contains(&self.pointer.1)
            && self.composition_finished()
        {
            self.ui.focus = Focus::Output;
            self.ui.agent.active_mut().clear_selection();
            self.dirty = true;
        }
        true
    }
}
