use super::*;
use crate::config::TabStyle;
use crate::core::Flags;
use crate::ui::TabPart;
use crate::width::fit_label;

impl WorkspaceView {
    pub(super) fn chrome(&mut self, layout: GuiLayout) {
        self.painter.clip(0., 0., layout.width, layout.top);
        let alpha = (self.config.tab_bar_opacity * 255.).round() as u32;
        let row = crate::ui::tab_row_color(&self.config);
        self.painter.clear((row << 8) | alpha);
        let Some(snapshot) = self.client.snapshot() else {
            return;
        };
        let (cell, style) = (self.cell, self.config.tab_style);
        let columns = ((layout.width - self.control_width()) / cell.0) as u16;
        let status = if self.client.is_remote() { "SSH" } else { "" };
        let pointer = (self.pointer.1 < layout.top).then(|| self.cell().0);
        let painter = &mut self.painter;
        let config = &self.config;
        self.ui.paint_tabs(
            snapshot,
            columns,
            status,
            pointer,
            config,
            |part| match part {
                TabPart::Tab { x, width, color } => {
                    let (x, width) = (f32::from(x) * cell.0, f32::from(width) * cell.0);
                    match style {
                        TabStyle::Angled => {
                            painter.tab((x, 0.), (width, cell.1), cell.0, opaque(color))
                        }
                        TabStyle::Flat => painter.rect(x, 0., width, cell.1, opaque(color)),
                    }
                }
                TabPart::Text {
                    x,
                    width,
                    text,
                    color,
                    ..
                } => {
                    let x = f32::from(x) * cell.0;
                    painter.text(&fit_label(text, width), x, 0., opaque(color));
                }
                TabPart::Underline { x, width, color } => {
                    let (x, width) = (f32::from(x) * cell.0, f32::from(width) * cell.0);
                    painter.rect(x, cell.1 - 2., width, 1., opaque(color));
                }
            },
        );
        self.painter.clip(0., 0., 0., 0.)
    }

    pub(super) fn control_width(&self) -> f32 {
        self.cell.0 * 18.
    }
    pub(super) fn draw_window_controls(&mut self, layout: GuiLayout) {
        self.painter.clip(0., 0., layout.width, layout.top);
        let one_tab = self
            .client
            .snapshot()
            .is_none_or(|snapshot| !snapshot.controller || snapshot.tabs.len() < 2);
        let mut x = layout.width - self.control_width();
        for (cells, action, label) in [
            (3., GuiAction::Tab(TabHit::Scroll(-1)), "‹"),
            (3., GuiAction::Tab(TabHit::Scroll(1)), "›"),
            (4., GuiAction::Minimize, "−"),
            (4., GuiAction::Maximize, "☐"),
            (4., GuiAction::Close, "×"),
        ] {
            let w = self.cell.0 * cells;
            let hit = GuiHit {
                bounds: (x, 0., w, layout.top),
                action,
            };
            let disabled = matches!(action, GuiAction::Tab(_)) && one_tab;
            let hovered = !disabled && hit.contains(self.pointer);
            let color = match action {
                _ if disabled => 0x505850ff,
                GuiAction::Close if hovered => DANGER,
                _ => INK,
            };
            if hovered {
                let padding = self.cell.0.min((w - self.cell.0).max(0.) * 0.5);
                let y = layout.top - 1.;
                self.painter
                    .rect(x + padding, y, w - padding * 2., 1., color);
            }
            if !disabled {
                self.control_hits.push(hit);
            }
            let y = (layout.top - self.cell.1) * 0.5;
            self.painter
                .text(label, x + (w - self.cell.0) * 0.5, y, color);
            x += w;
        }
        self.painter.clip(0., 0., 0., 0.)
    }
    pub(super) fn draw_scrollbars(&mut self) {
        self.painter.clip(0., 0., 0., 0.);
        if self.ui.files_open {
            self.scrollbars.clear();
            return;
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
            let (x, y, w, h) = bar.bounds;
            let (top, height) = bar.thumb();
            if active {
                self.painter.rect(x, y, w, h, 0x343834ff);
            }
            let thumb = if active { 0xa2aaa0ff } else { 0x60685fff };
            self.painter.rect(x, top, w, height, thumb);
        }
    }
    fn draw_resources(&mut self, layout: GuiLayout, summary_y: f32, pointer: Option<(u16, u16)>) {
        let top = layout.footer + 6.;
        let columns = ((layout.width - PADDING * 2.) / self.cell.0) as u16;
        let rows = ((summary_y - 6. - top) / self.cell.1) as u16;
        let view = self.ui.resources.view(columns, rows, pointer);
        let focused = self.ui.focus == Focus::Monitor;
        let body_left = PADDING + f32::from(view.body_x) * self.cell.0;
        self.painter.clip(
            body_left,
            top,
            (layout.width - body_left - PADDING).max(0.),
            f32::from(rows) * self.cell.1,
        );
        for plot in view.plots {
            let x = PADDING + f32::from(plot.x) * self.cell.0;
            let y = top + plot.y as f32 * self.cell.1;
            let width = f32::from(plot.width.saturating_sub(1)) * self.cell.0;
            let height = f32::from(plot.height) * self.cell.1 - 2.;
            for fraction in [0., 0.5, 1.] {
                self.painter
                    .rect(x, y + fraction * height, width, 1., BORDER);
            }
            for series in plot.series {
                let color = opaque(crate::monitor::series_color(series.name));
                let mut previous: Option<(f32, f32)> = None;
                for (age, value) in series.points {
                    let Some(value) = value else {
                        previous = None;
                        continue;
                    };
                    let point = (
                        x + (1. - age / self.ui.resources.range as f32).clamp(0., 1.) * width,
                        y + (1. - value.clamp(0., plot.maximum) / plot.maximum) * height,
                    );
                    if let Some(from) = previous {
                        if from.0.floor() == point.0.floor() {
                            continue;
                        }
                        self.painter.stroke(from, point, 1.5, color);
                    }
                    previous = Some(point);
                }
                if let Some(point) = previous {
                    self.painter.stroke(point, point, 3., color);
                }
            }
        }
        self.painter.clip(
            0.,
            layout.footer,
            layout.width,
            (summary_y - 6. - layout.footer).max(0.),
        );
        for text in view.texts {
            let x = PADDING + f32::from(text.x) * self.cell.0;
            let y = top + text.y as f32 * self.cell.1;
            let width = f32::from(text.width) * self.cell.0;
            self.painter.clip(x, y, width, self.cell.1);
            if let Some(background) = text.background(focused) {
                self.painter
                    .rect(x, y, width, self.cell.1, opaque(background));
            }
            self.painter
                .text(&fit_label(&text.text, text.width), x, y, opaque(text.color));
        }
        self.painter.clip(
            0.,
            layout.footer,
            layout.width,
            layout.height - layout.footer,
        )
    }
    pub(super) fn footer(&mut self, layout: GuiLayout) {
        self.painter.clip(
            0.,
            layout.footer,
            layout.width,
            layout.height - layout.footer,
        );
        self.painter.clear(opaque(crate::panel::BACKGROUND));
        self.painter
            .rect(0., layout.footer, layout.width, 1., BORDER);
        let summary_y = layout.height - self.cell.1 - 6.;
        let (area, column, row, _) = self.locate(self.pointer);
        if self.ui.resources.open {
            let pointer = (area == Area::Monitor).then_some((column, row));
            self.draw_resources(layout, summary_y, pointer);
            self.painter
                .rect(0., summary_y - 6., layout.width, 1., BORDER);
        }
        let columns = (layout.width / self.cell.0) as u16;
        let origin = layout.width - f32::from(columns) * self.cell.0;
        let pointer = (area == Area::Footer).then_some(column);
        let (controller, remote) = (self.client.controller(), self.client.is_remote());
        let focused = self.ui.focus == Focus::Monitor;
        let footer = self
            .ui
            .resources
            .footer(columns, controller, remote, pointer);
        for text in &footer.texts {
            let x = origin + f32::from(text.x) * self.cell.0;
            if let Some(background) = text.background(focused) {
                let width = f32::from(text.width) * self.cell.0;
                let background = opaque(background);
                self.painter
                    .rect(x, summary_y, width, self.cell.1, background);
            }
            self.painter
                .text(&text.text, x, summary_y, opaque(text.color));
        }
        let (label, columns) = self.ui.footer_label(&self.client, &footer);
        let label = crate::width::fit_middle(&label, columns);
        self.painter.text(&label, PADDING, summary_y, MUTED);
        self.painter.clip(0., 0., 0., 0.)
    }

    pub(super) fn draw_search(&mut self, layout: GuiLayout) {
        let Some(pane) = self
            .client
            .snapshot()
            .and_then(|snapshot| snapshot.pane(self.ui.search.pane))
        else {
            return;
        };
        let (grid_x, grid_y) = layout.origin(self.cell);
        let x = grid_x + f32::from(pane.rect.x) * self.cell.0;
        let y = grid_y + f32::from(pane.rect.y + pane.rect.rows.saturating_sub(1)) * self.cell.1;
        let width = f32::from(pane.rect.cols) * self.cell.0;
        let columns = pane.rect.cols.saturating_sub(2);
        self.painter.clip(x, y, width, self.cell.1);
        self.painter.rect(x, y, width, self.cell.1, CHROME);
        let (label, caret) = self.ui.search.display(&self.preedit, usize::from(columns));
        self.painter.text(&label, x + self.cell.0, y, INK);
        self.painter.rect(x, y, width, 1., BORDER);
        self.window.cursor_rect(
            (x + self.cell.0 * (1 + caret.min(usize::from(columns))) as f32) as i32,
            y as i32,
            self.cell.0 as i32,
            self.cell.1 as i32,
        );
        self.painter.clip(0., 0., 0., 0.)
    }

    pub(super) fn draw_panel(&mut self, grid: &crate::panel::Grid) {
        self.painter.clip(0., 0., 0., 0.);
        let Ok(()) = grid.for_each_run::<std::convert::Infallible>(None, false, |run| {
            let x = f32::from(run.x) * self.cell.0;
            let y = f32::from(run.y) * self.cell.1;
            let width = f32::from(run.columns) * self.cell.0;
            self.painter
                .rect(x, y, width, self.cell.1, opaque(run.background));
            if run.flags.contains(Flags::UNDERLINE) {
                self.painter.rect(
                    x,
                    y + self.cell.1 - 2.,
                    width,
                    1.,
                    opaque(run.underline_color.unwrap_or(run.foreground)),
                );
            }
            self.painter.text_flags(
                run.text,
                x,
                y,
                opaque(run.foreground),
                u32::from((run.flags & (Flags::BOLD | Flags::ITALIC)).bits()),
            );
            Ok(())
        });
    }

    pub(super) fn draw_files(&mut self, layout: GuiLayout) {
        let list = self.file_rect(layout);
        let preview = self.file_preview_layout(layout);
        let (preedit, config) = (&self.preedit, &self.config);
        let (list, preview) = self.ui.file_grids(list, preview, preedit, config);
        self.draw_panel(&list);
        if let Some(preview) = preview {
            self.draw_panel(&preview);
        }
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
        let (config, preedit) = (&self.config, &self.preedit);
        let panel = crate::agent::panel::draw(&mut self.ui, config, side, size, pointer, preedit)?;
        if let Some(backdrop) = &panel.backdrop {
            self.draw_panel(backdrop);
        }
        self.draw_panel(&panel.grid);
        let agent = &self.ui.agent;
        let session = agent.active();
        let editor = session.editor_layout(
            session.editor_columns(usize::from(agent.editor.cols)),
            usize::from(agent.editor.rows),
        );
        let cell = self.cell;
        let mut bar = |target, rect, total, offset| {
            let bar = Scrollbar::in_rect(target, rect, cell, total, offset);
            self.scrollbars.extend(bar);
        };
        let transcript = (session.lines.len(), session.model.scroll);
        bar(
            ScrollTarget::Transcript,
            agent.chat,
            transcript.0,
            transcript.1,
        );
        let lines = editor.lines.len();
        bar(ScrollTarget::Editor, agent.editor, lines, editor.first);
        if let Some(rect) = panel.preview
            && let Some(preview) = &session.context.preview
        {
            bar(
                ScrollTarget::Preview,
                rect,
                preview.lines.len(),
                preview.scroll,
            );
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
}
