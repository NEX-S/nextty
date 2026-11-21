use super::*;
use crate::core::Flags;
use crate::width::fit_label;

impl WorkspaceView {
    pub(super) fn chrome(&mut self, layout: GuiLayout) {
        self.painter.clip(0., 0., layout.width, layout.top);
        let mut tabline_background = 0;
        for shift in [16, 8, 0] {
            tabline_background |= ((self.config.background >> shift & 255) * 9 / 10) << shift;
        }
        self.painter
            .clear((tabline_background << 8) | (self.config.tab_bar_opacity * 255.).round() as u32);
        let Some(snapshot) = self.client.snapshot() else {
            return;
        };
        let cell = self.cell;
        let bar = self.tab_bar(snapshot, layout);
        let hit = (self.pointer.1 < layout.top)
            .then(|| bar.at(snapshot, self.cell().0))
            .flatten();
        let prompt = if snapshot.controller { MUTED } else { WARNING };
        self.painter.text("›", cell.0, 0., prompt);
        for &(index, column, columns) in &bar.tabs {
            let tab = &snapshot.tabs[index];
            let active = tab.id == snapshot.active_tab;
            let x = f32::from(column) * cell.0;
            let w = f32::from(columns) * cell.0;
            let close_hovered = hit == Some(TabHit::Close(tab.id));
            let hovered = hit == Some(TabHit::Select(index)) || close_hovered;
            let background = if active {
                self.config.background
            } else {
                mix_color(self.config.background, 0, 204)
            };
            match self.config.tab_style {
                crate::config::TabStyle::Angled => {
                    self.painter
                        .tab((x, 0.), (w, cell.1), cell.0, opaque(background))
                }
                crate::config::TabStyle::Flat => {
                    self.painter.rect(x, 0., w, cell.1, opaque(background))
                }
            }
            self.painter
                .text("•", x + cell.0 * 2., 0., opaque(crate::ui::tab_color(tab)));
            self.painter.text(
                &fit_label(&tab.title, columns - 8),
                x + cell.0 * 4.,
                0.,
                if active || hovered {
                    opaque(self.config.foreground)
                } else {
                    MUTED
                },
            );
            let close_x = x + w - cell.0 * 3.;
            if close_hovered {
                self.painter.rect(close_x, cell.1 - 1., cell.0, 1., DANGER);
            }
            let color = if close_hovered { DANGER } else { MUTED };
            self.painter.text("×", close_x, 0., color);
            if active && self.config.tab_style == crate::config::TabStyle::Flat {
                self.painter.rect(x, cell.1 - 2., w, 1., ACCENT);
            }
        }
        if let Some(plus) = bar.plus {
            let x = f32::from(plus + 1) * cell.0;
            let hovered = hit == Some(TabHit::New);
            if hovered {
                self.painter.rect(x, cell.1 - 1., cell.0, 1., ACCENT);
            }
            self.painter
                .text("+", x, 0., if hovered { ACCENT } else { 0x606060ff });
        }
        if let Some(TabHit::Scroll(direction)) = hit
            && let Some((column, columns)) = bar.overflow[usize::from(direction > 0)]
        {
            self.painter.rect(
                f32::from(column) * cell.0,
                cell.1 - 1.,
                f32::from(columns) * cell.0,
                1.,
                INK,
            );
        }
        let color = if snapshot.controller {
            0xb8b8b8ff
        } else {
            WARNING
        };
        self.painter
            .text(&bar.label, f32::from(bar.label_x) * cell.0, 0., color);
        self.painter.clip(0., 0., 0., 0.)
    }

    pub(super) fn control_width(&self) -> f32 {
        self.cell.0 * 18.
    }
    fn header_button(
        &mut self,
        x: f32,
        w: f32,
        layout: GuiLayout,
        (action, label): (GuiAction, &str),
    ) {
        let hit = GuiHit {
            bounds: (x, 0., w, layout.top),
            action,
        };
        let disabled = matches!(action, GuiAction::Tab(_))
            && self
                .client
                .snapshot()
                .is_none_or(|snapshot| !snapshot.controller || snapshot.tabs.len() < 2);
        let color = if disabled {
            0x505850ff
        } else if action == GuiAction::Close && hit.contains(self.pointer) {
            DANGER
        } else {
            INK
        };
        if !disabled {
            self.control_feedback(hit);
            self.control_hits.push(hit);
        }
        let y = (layout.top - self.cell.1) * 0.5;
        self.painter
            .text(label, x + (w - self.cell.0) * 0.5, y, color)
    }
    pub(super) fn draw_window_controls(&mut self, layout: GuiLayout) {
        self.painter.clip(0., 0., layout.width, layout.top);
        let mut x = layout.width - self.control_width();
        for (cells, button) in [
            (3., (GuiAction::Tab(TabHit::Scroll(-1)), "‹")),
            (3., (GuiAction::Tab(TabHit::Scroll(1)), "›")),
            (4., (GuiAction::Minimize, "−")),
            (4., (GuiAction::Maximize, "☐")),
            (4., (GuiAction::Close, "×")),
        ] {
            let w = self.cell.0 * cells;
            self.header_button(x, w, layout, button);
            x += w;
        }
        self.painter.clip(0., 0., 0., 0.)
    }
    fn control_feedback(&self, hit: GuiHit) {
        if hit.contains(self.pointer) {
            let (x, y, w, h) = hit.bounds;
            let underline = match hit.action {
                GuiAction::Close => Some(DANGER),
                GuiAction::Tab(_) | GuiAction::Minimize | GuiAction::Maximize => Some(INK),
                _ => None,
            };
            if let Some(color) = underline {
                let padding = self.cell.0.min((w - self.cell.0).max(0.) * 0.5);
                self.painter
                    .rect(x + padding, y + h - 1., w - padding * 2., 1., color);
            } else {
                self.painter.rect(x, y, w, h, opaque(crate::panel::HOVER));
            }
        }
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
            let (top, height) = bar.thumb();
            if active {
                self.painter.rect(
                    bar.bounds.0,
                    bar.bounds.1,
                    bar.bounds.2,
                    bar.bounds.3,
                    0x343834ff,
                );
            }
            self.painter.rect(
                bar.bounds.0,
                top,
                bar.bounds.2,
                height,
                if active { 0xa2aaa0ff } else { 0x60685fff },
            );
        }
    }
    fn draw_resources(&mut self, layout: GuiLayout, summary_y: f32) {
        let top = layout.footer + 6.;
        let columns = ((layout.width - PADDING * 2.) / self.cell.0) as u16;
        let rows = ((summary_y - 6. - top) / self.cell.1) as u16;
        let view = self.ui.resources.view(columns, rows);
        let body_left = PADDING + f32::from(view.body_x) * self.cell.0;
        let body_top = top;
        let bottom = top + f32::from(rows) * self.cell.1;
        self.painter.clip(
            body_left,
            body_top,
            (layout.width - body_left - PADDING).max(0.),
            (bottom - body_top).max(0.),
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
            let background = match text.emphasis(self.ui.focus == Focus::Monitor) {
                crate::monitor::Emphasis::Selected => Some(crate::panel::SELECTED),
                crate::monitor::Emphasis::Highlighted => Some(crate::panel::HOVER),
                crate::monitor::Emphasis::None => None,
            };
            if let Some(background) = background {
                self.painter
                    .rect(x, y, width, self.cell.1, opaque(background));
            }
            self.painter
                .text(&fit_label(&text.text, text.width), x, y, opaque(text.color));
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
        if self.ui.resources.open {
            self.draw_resources(layout, summary_y);
            self.painter
                .rect(0., summary_y - 6., layout.width, 1., BORDER);
        }
        let columns = (layout.width / self.cell.0) as u16;
        let origin = layout.width - f32::from(columns) * self.cell.0;
        let view =
            self.ui
                .resources
                .footer(columns, self.client.controller(), self.client.is_remote());
        let left = view
            .texts
            .iter()
            .map(|text| text.x)
            .min()
            .unwrap_or(columns);
        for text in view.texts {
            let x = origin + f32::from(text.x) * self.cell.0;
            let bounds = (
                x,
                summary_y - 6.,
                f32::from(text.width) * self.cell.0,
                self.cell.1 + 12.,
            );
            if let Some(hit) = view.hits.iter().find(|hit| hit.x == text.x) {
                let hit = GuiHit {
                    bounds,
                    action: GuiAction::Monitor(hit.action),
                };
                self.control_feedback(hit);
                self.control_hits.push(hit);
            }
            if text.emphasis(self.ui.focus == Focus::Monitor) != crate::monitor::Emphasis::None {
                self.painter.rect(
                    x,
                    summary_y,
                    bounds.2,
                    self.cell.1,
                    opaque(crate::panel::SELECTED),
                );
            }
            self.painter
                .text(&text.text, x, summary_y, opaque(text.color));
        }
        let right = origin + f32::from(left) * self.cell.0;
        let label = self
            .client
            .snapshot()
            .map(|snapshot| self.ui.footer(snapshot, self.client.is_remote()))
            .unwrap_or_default();
        self.painter.text(
            &crate::width::fit_middle(
                &label,
                ((right - PADDING - self.cell.0 * 2.) / self.cell.0) as u16,
            ),
            PADDING,
            summary_y,
            MUTED,
        );
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
        let rect = self.file_rect(layout);
        let preview_rect = self.file_preview_layout(layout);
        let Some(files) = &mut self.ui.files else {
            return;
        };
        let focused = self.ui.focus == Focus::Files && files.focus() == FileFocus::List;
        let grid = crate::file_panel::list(files, rect, &self.preedit, focused, &self.config);
        let preview = (files.focus() == FileFocus::Preview)
            .then(|| crate::file_panel::preview(files, preview_rect, &self.config));
        self.draw_panel(&grid);
        if let Some(preview) = preview {
            self.draw_panel(&preview);
        }
    }
}
