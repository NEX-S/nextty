use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentScroll {
    Transcript,
    Editor,
    Preview,
}

pub(super) struct EditorViewport {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) columns: usize,
    pub(super) rows: usize,
}

#[derive(Default)]
pub(super) struct AgentState {
    pub(super) drag: Option<f32>,
    pub(super) divider_hover: bool,
    selecting_editor: bool,
    chat_rows: Vec<(usize, crate::workspace::Rect)>,
    tool_click: Option<usize>,
}

impl WorkspaceView {
    pub(super) fn agent_point(&self, clamp: bool) -> Option<crate::agent::panel::TranscriptPoint> {
        let (line, rect) = if clamp {
            self.agent
                .chat_rows
                .iter()
                .rfind(|(_, rect)| f32::from(rect.y) * self.cell.1 <= self.pointer.1)
                .or_else(|| self.agent.chat_rows.first())?
        } else {
            self.agent.chat_rows.iter().find(|(_, rect)| {
                let y = f32::from(rect.y) * self.cell.1;
                (y..y + self.cell.1).contains(&self.pointer.1)
            })?
        };
        let column = ((self.pointer.0 - f32::from(rect.x) * self.cell.0) / self.cell.0) as usize;
        self.ui
            .agent
            .active()
            .point(*line, column.min(usize::from(rect.cols)))
    }

    pub(super) fn editor_viewport(&self) -> Option<EditorViewport> {
        let hit = self
            .control_hits
            .iter()
            .find(|hit| hit.action == GuiAction::Agent(AgentAction::Editor))?;
        let columns = self
            .ui
            .agent
            .active()
            .editor_columns((hit.bounds.2 / self.cell.0) as usize);
        let rows = (hit.bounds.3 / self.cell.1).round().max(1.) as usize;
        Some(EditorViewport {
            x: hit.bounds.0,
            y: hit.bounds.1,
            columns,
            rows,
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
            && let Some(action) = self
                .control_hits
                .iter()
                .rfind(|hit| {
                    hit.contains(self.pointer)
                        && matches!(
                            hit.action,
                            GuiAction::Agent(AgentAction::ClosePreview | AgentAction::CopyPreview)
                        )
                })
                .map(|hit| hit.action)
        {
            self.gui_action(action)?;
        }
        Ok(())
    }
    pub(super) fn agent_control_button(
        &mut self,
        button: u32,
        action: GuiAction,
        modifiers: u32,
    ) -> Result<bool> {
        match (button, action) {
            (3, GuiAction::Agent(AgentAction::Copy(index) | AgentAction::Tool(index))) => {
                if let Some(text) = self.ui.agent.active().selected_text() {
                    self.window.selection(false, &text);
                    self.message("Copied");
                } else {
                    self.gui_action(GuiAction::Agent(AgentAction::Copy(index)))?;
                }
            }
            (3, GuiAction::Agent(AgentAction::Editor)) => {
                self.gui_action(GuiAction::Agent(AgentAction::ContextMenu))?
            }
            (1, GuiAction::Agent(AgentAction::Copy(_) | AgentAction::Tool(_))) => {
                if self.composition_finished() {
                    self.ui.focus = Focus::Output;
                    if let Some(point) = self.agent_point(false) {
                        self.ui
                            .agent
                            .active_mut()
                            .begin_selection(point, modifiers & native::SHIFT != 0);
                        self.agent.tool_click = match action {
                            GuiAction::Agent(AgentAction::Tool(index)) => Some(index),
                            _ => None,
                        };
                    }
                    self.dirty = true;
                }
            }
            (1 | 2, GuiAction::Agent(AgentAction::Editor)) => {
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
        let side = crate::workspace::Rect {
            x: (layout.side / self.cell.0) as u16,
            y: 1,
            cols: ((layout.width - layout.side) / self.cell.0) as u16,
            rows: ((layout.footer - layout.top) / self.cell.1).max(1.) as u16,
        };
        let size = WindowSize {
            num_cols: (layout.width / self.cell.0) as u16,
            num_lines: (layout.height / self.cell.1) as u16,
            cell_width: self.cell.0 as u16,
            cell_height: self.cell.1 as u16,
        };
        let pointer = Some((
            (self.pointer.0 / self.cell.0) as u16,
            (self.pointer.1 / self.cell.1) as u16,
        ));
        let panel = crate::agent::panel::draw(
            &mut self.ui,
            &self.config,
            side,
            size,
            pointer,
            &self.preedit,
        )?;
        if let Some(backdrop) = &panel.backdrop {
            self.draw_panel(backdrop)?;
        }
        self.draw_panel(&panel.grid)?;
        self.agent.chat_rows = panel.chat_rows;
        for (rect, action) in panel.hits {
            self.control_hits.push(GuiHit {
                bounds: (
                    f32::from(rect.x) * self.cell.0,
                    f32::from(rect.y) * self.cell.1,
                    f32::from(rect.cols) * self.cell.0,
                    f32::from(rect.rows) * self.cell.1,
                ),
                action: GuiAction::Agent(action),
            });
        }
        let editor = self.ui.agent.active().editor_layout(
            self.ui
                .agent
                .active()
                .editor_columns(usize::from(panel.editor.cols)),
            usize::from(panel.editor.rows),
        );
        let chat = panel.chat;
        let mut bars = vec![
            (
                ScrollTarget::Agent(AgentScroll::Transcript),
                chat,
                self.ui.agent.active().lines.len(),
                self.ui.agent.active().model.scroll,
            ),
            (
                ScrollTarget::Agent(AgentScroll::Editor),
                panel.editor,
                editor.lines.len(),
                editor.first,
            ),
        ];
        if let Some(rect) = panel.preview
            && let Some(preview) = &self.ui.agent.active().context.preview
        {
            bars.push((
                ScrollTarget::Agent(AgentScroll::Preview),
                rect,
                preview.lines.len(),
                preview.scroll,
            ));
        }
        for (target, rect, total, offset) in bars {
            if let Some(bar) = Scrollbar::new(
                target,
                (
                    f32::from(rect.x + rect.cols) * self.cell.0 - 3.,
                    f32::from(rect.y) * self.cell.1,
                    3.,
                    f32::from(rect.rows) * self.cell.1,
                ),
                total,
                usize::from(rect.rows),
                offset,
            ) {
                self.scrollbars.push(bar);
            }
        }
        if let Some((column, row)) = panel.cursor {
            let x = f32::from(column) * self.cell.0;
            let y = f32::from(row) * self.cell.1;
            self.renderer.rect(x, y, 2., self.cell.1, INK, false);
            self.window
                .cursor_rect(x as i32, y as i32, self.cell.0 as i32, self.cell.1 as i32);
        }
        self.ui.agent.spare_grid = Some(panel.grid);
        Ok(())
    }
    pub(super) fn poll_agent_events(&mut self, pollfds: &[libc::pollfd]) -> Result<()> {
        let revision = self.ui.agent.active().model.revision;
        let (changed, messages_changed) = self.ui.poll_agent(&mut self.client, &self.config)?;
        let layout_changed = pollfds
            .iter()
            .any(|source| source.fd == self.ui.agent.layout_fd() && source.revents != 0)
            && self.ui.agent.poll_layout()?;
        if revision != self.ui.agent.active().model.revision {
            self.agent.chat_rows.clear();
            self.agent.tool_click = None;
        }
        if messages_changed {
            self.control_hits.retain(|hit| {
                !matches!(
                    hit.action,
                    GuiAction::Agent(
                        AgentAction::Copy(_) | AgentAction::Tool(_) | AgentAction::Rewind(_)
                    )
                )
            });
        }
        self.dirty |= changed && self.ui.agent.open || layout_changed;
        Ok(())
    }

    pub(super) fn agent_event(&mut self, event: &Event) -> bool {
        match event.kind {
            native::TEXT if self.ui.agent.active().rewind_target.is_some() => {
                self.ui.cancel_rewind()
            }
            native::PREEDIT
                if self.ui.agent.open
                    && matches!(self.ui.focus, Focus::Output | Focus::Controls) =>
            {
                return true;
            }
            _ => (),
        }
        false
    }

    pub(super) fn agent_action(&mut self, action: AgentAction) -> Result<bool> {
        if !matches!(
            action,
            AgentAction::Stop
                | AgentAction::Remove(_)
                | AgentAction::ClosePreview
                | AgentAction::CopyPreview
        ) && !self.composition_finished()
        {
            return Ok(false);
        }
        let open = self.ui.agent.open;
        match self.ui.activate_agent(action, &self.client) {
            Ok(Some(copy)) => {
                self.window.selection(false, &copy);
                self.message("Copied");
            }
            Ok(None) => (),
            Err(error) => self.message(error.to_string()),
        }
        if open != self.ui.agent.open {
            self.resize()?;
        }
        self.agent.selecting_editor = false;
        self.agent.tool_click = None;
        self.agent.chat_rows.clear();
        Ok(true)
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

    pub(super) fn agent_scroll(&mut self, amount: f32) -> bool {
        let delta = if amount > 0. { -3 } else { 3 };
        if let Some(crate::agent::panel::ContextPreview { scroll, .. }) =
            &mut self.ui.agent.active_mut().context.preview
        {
            *scroll = scroll.saturating_add_signed(delta);
            self.dirty = true;
            return true;
        }
        if self.ui.resources.open && self.pointer.1 >= self.layout().footer {
            return false;
        }
        if self.ui.agent.open && self.pointer.0 >= self.layout().side {
            if self.control_hits.iter().any(|hit| {
                hit.action == GuiAction::Agent(AgentAction::Editor) && hit.contains(self.pointer)
            }) {
                if let Some(layout) = self.editor_viewport() {
                    let editor = self
                        .ui
                        .agent
                        .active()
                        .editor_layout(layout.columns, layout.rows);
                    self.ui.agent.active_mut().editor_scroll = Some(
                        editor
                            .first
                            .saturating_add_signed(delta)
                            .min(editor.lines.len().saturating_sub(layout.rows)),
                    );
                }
                self.dirty = true;
                return true;
            }
            let scroll = &mut self.ui.agent.active_mut().model.scroll;
            *scroll = scroll.saturating_add_signed(-delta);
            self.dirty = true;
            return true;
        }
        false
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
