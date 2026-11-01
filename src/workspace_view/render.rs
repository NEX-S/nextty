use super::*;
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
            if controller { MUTED } else { 0xfe9b30ff },
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
            match self.config.tab_style {
                crate::config::TabStyle::Angled => {
                    self.renderer
                        .tab((x, 0.), (w, cell.1), cell.0, (background << 8) | 255)
                }
                crate::config::TabStyle::Flat => {
                    self.renderer
                        .rect(x, 0., w, cell.1, (background << 8) | 255, false)
                }
            }
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
                if active || hovered {
                    (self.config.foreground << 8) | 255
                } else {
                    MUTED
                },
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
                if close_hovered { INK } else { MUTED },
            )?;
            self.control_hits.push(GuiHit {
                bounds: (close_x - cell.0, 0., cell.0 * 3., layout.top),
                action: GuiAction::CloseTab(tab.id),
            });
            if active && self.config.tab_style == crate::config::TabStyle::Flat {
                self.renderer.rect(x, cell.1 - 2., w, 1., ACCENT, false);
            }
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
    fn control_feedback(&self, bounds: (f32, f32, f32, f32), action: GuiAction) {
        let hovered = GuiHit { bounds, action }.contains(self.pointer);
        if hovered {
            let underline = match action {
                GuiAction::CloseTab(_) | GuiAction::Close => Some(MUTED),
                GuiAction::NewTab
                | GuiAction::Tab(_)
                | GuiAction::Minimize
                | GuiAction::Maximize => Some(ACCENT),
                _ => None,
            };
            if let Some(color) = underline {
                let padding = self.cell.0.min((bounds.2 - self.cell.0).max(0.) * 0.5);
                self.renderer.rect(
                    bounds.0 + padding,
                    bounds.1 + bounds.3 - 1.,
                    bounds.2 - padding * 2.,
                    1.,
                    color,
                    false,
                );
                return;
            }
            self.renderer.rect(
                bounds.0,
                bounds.1,
                bounds.2,
                bounds.3,
                (crate::panel::HOVER << 8) | 255,
                false,
            );
        }
    }
    pub(super) fn draw_scrollbars(&mut self) -> Result<()> {
        self.renderer.clip(0., 0., 0., 0.)?;
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
    fn draw_resources(&mut self, layout: GuiLayout, summary_y: f32) -> Result<()> {
        let top = layout.footer + 6.;
        let columns = ((layout.width - PADDING * 2.) / self.cell.0).max(0.) as u16;
        let rows = ((summary_y - 6. - top) / self.cell.1).max(0.) as u16;
        let view = self.ui.resources.view(columns, rows);
        let body_left = PADDING + f32::from(view.body_x) * self.cell.0;
        let body_top = top;
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
            for series in plot.series {
                let color = (crate::monitor::series_color(series.name) << 8) | 255;
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
            let background = match text.emphasis(self.ui.focus == Focus::Monitor) {
                crate::monitor::Emphasis::Selected => Some(crate::panel::SELECTED),
                crate::monitor::Emphasis::Highlighted => Some(crate::panel::HOVER),
                crate::monitor::Emphasis::None => None,
            };
            if let Some(background) = background {
                self.renderer
                    .rect(x, y, width, self.cell.1, (background << 8) | 255, false);
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
        self.renderer.clip(
            0.,
            layout.footer,
            layout.width,
            layout.height - layout.footer,
        )?;
        self.renderer.clear((crate::panel::BACKGROUND << 8) | 255)?;
        self.renderer
            .rect(0., layout.footer, layout.width, 1., BORDER, false);
        let summary_y = layout.height - self.cell.1 - 6.;
        if self.ui.resources.open {
            self.draw_resources(layout, summary_y)?;
            self.renderer
                .rect(0., summary_y - 6., layout.width, 1., BORDER, false);
        }
        let columns = (layout.width / self.cell.0).max(0.) as u16;
        let origin = layout.width - f32::from(columns) * self.cell.0;
        let view = self
            .ui
            .resources
            .footer(columns, self.controller(), self.client.is_remote());
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
                let action = GuiAction::Monitor(hit.action);
                self.control_feedback(bounds, action);
                self.control_hits.push(GuiHit { bounds, action });
            }
            if text.emphasis(self.ui.focus == Focus::Monitor) != crate::monitor::Emphasis::None {
                self.renderer.rect(
                    x,
                    summary_y,
                    bounds.2,
                    self.cell.1,
                    (crate::panel::SELECTED << 8) | 255,
                    false,
                );
            }
            self.text.draw(
                &self.renderer,
                &text.text,
                x,
                summary_y,
                (text.color << 8) | 255,
            )?;
        }
        let right = origin + f32::from(left) * self.cell.0;
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
        let cols = ((layout.width / self.cell.0).floor() as u16)
            .saturating_sub(4)
            .min(78);
        let rows = (self.ui.help_lines().count() as u16 + 4)
            .min(((layout.height / self.cell.1).floor() as u16).saturating_sub(2));
        let rect = crate::workspace::Rect {
            x: ((layout.width / self.cell.0).floor() as u16 - cols) / 2,
            y: ((layout.height / self.cell.1).floor() as u16 - rows) / 2,
            cols,
            rows,
        };
        self.renderer.clip(0., 0., layout.width, layout.height)?;
        self.renderer
            .rect(0., 0., layout.width, layout.height, 0x00000088, false);
        let grid = self.ui.help_view(rect, &self.config);
        self.draw_panel(&grid)?;
        self.control_hits.push(GuiHit {
            bounds: (
                f32::from(rect.x) * self.cell.0,
                f32::from(rect.y + rows.saturating_sub(1)) * self.cell.1,
                f32::from(cols) * self.cell.0,
                self.cell.1,
            ),
            action: GuiAction::DismissHelp,
        });
        Ok(())
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
        let (label, caret) = self.ui.search.display(&self.preedit, usize::from(columns));
        self.text.draw(
            &self.renderer,
            &fit_label(&label, columns),
            x + self.cell.0,
            y,
            INK,
        )?;
        self.renderer.rect(x, y, width, 1., BORDER, false);
        self.window.cursor_rect(
            (x + self.cell.0 * (1 + caret.min(usize::from(columns))) as f32) as i32,
            y as i32,
            self.cell.0 as i32,
            self.cell.1 as i32,
        );
        self.renderer.clip(0., 0., 0., 0.)
    }
    pub(super) fn draw_close(&mut self, width: f32, height: f32) -> Result<()> {
        let prompt = self.ui.close_prompt.unwrap();
        let rects = close_layout(width, height, self.cell);
        let (x, y, w, h) = rects[0];
        let padding = self.cell.0.min(w * 0.1);
        let columns = ((w - padding * 2.) / self.cell.0) as usize;
        self.renderer
            .rect(x, y, w, h, (crate::panel::BORDER << 8) | 255, false);
        self.renderer.rect(
            x + 1.,
            y + 1.,
            w - 2.,
            h - 2.,
            (self.config.background << 8) | 255,
            false,
        );
        self.renderer.clip(x + 1., y + 1., w - 2., h - 2.)?;
        let name = self.client.snapshot().map_or("", |s| s.name.as_str());
        self.text.draw(
            &self.renderer,
            &fit(
                &format!(
                    "{} · {name}",
                    if prompt.tab.is_some() {
                        "Close tab"
                    } else {
                        "Close window"
                    }
                ),
                columns,
            ),
            x + padding,
            y + h * 0.08,
            (self.config.foreground << 8) | 255,
        )?;
        self.text.draw(
            &self.renderer,
            &fit(
                if prompt.tab.is_some() {
                    "Programs may still be running in this tab"
                } else {
                    "End session to stop tasks and delete history"
                },
                columns,
            ),
            x + padding,
            y + h * 0.22,
            MUTED,
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
                &if prompt.tab.is_some() {
                    "Closing terminates all panes in this tab".into()
                } else {
                    format!("Resume with: nextty attach {name}")
                },
                columns,
            ),
            x + padding,
            y + h * 0.36,
            MUTED,
        )?;
        for ((terminate, label), &(bx, by, bw, bh)) in [
            (
                true,
                if prompt.tab.is_some() {
                    "Close tab"
                } else {
                    "End session"
                },
            ),
            (
                false,
                if prompt.tab.is_some() {
                    "Cancel"
                } else {
                    "Keep running"
                },
            ),
        ]
        .into_iter()
        .zip(&rects[1..])
        {
            let selected = prompt.terminate == terminate;
            let hovered = self.pointer.0 >= bx
                && self.pointer.0 < bx + bw
                && self.pointer.1 >= by
                && self.pointer.1 < by + bh;
            let ink = match (terminate, selected || hovered) {
                (true, true) => 0xff8a94ff,
                (true, false) => 0xe05b65ff,
                (false, true) => ACCENT,
                (false, false) => INK,
            };
            if selected || hovered {
                self.renderer.rect(
                    bx,
                    by,
                    bw,
                    bh,
                    if terminate {
                        0x633534ff
                    } else {
                        (crate::panel::HOVER << 8) | 255
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
            MUTED,
        )?;
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

impl WorkspaceView {
    pub(super) fn draw_panel(&mut self, grid: &crate::panel::Grid) -> Result<()> {
        self.renderer.clip(0., 0., 0., 0.)?;
        for run in grid.runs(None, &[]) {
            let x = f32::from(run.x) * self.cell.0;
            let y = f32::from(run.y) * self.cell.1;
            self.renderer.rect(
                x,
                y,
                f32::from(run.columns) * self.cell.0,
                self.cell.1,
                (run.background << 8) | 255,
                false,
            );
            if run.flags.contains(crate::core::Flags::UNDERLINE) {
                self.renderer.rect(
                    x,
                    y + self.cell.1 - 2.,
                    f32::from(run.columns) * self.cell.0,
                    1.,
                    (run.underline_color.unwrap_or(run.foreground) << 8) | 255,
                    false,
                );
            }
            self.text.draw_flags(
                &self.renderer,
                &run.text,
                x,
                y,
                (run.foreground << 8) | 255,
                u32::from(run.flags.contains(crate::core::Flags::BOLD))
                    | (u32::from(run.flags.contains(crate::core::Flags::ITALIC)) << 1),
            )?;
        }
        Ok(())
    }

    pub(super) fn draw_files(&mut self, layout: GuiLayout) -> Result<()> {
        let Some(files) = &self.ui.files else {
            return Ok(());
        };
        let grid =
            crate::file_panel::list(files, self.file_rect(layout), &self.preedit, &self.config);
        let preview = (files.focus() == FileFocus::Preview).then(|| {
            crate::file_panel::preview(files, self.file_preview_layout(layout), &self.config)
        });
        self.draw_panel(&grid)?;
        if let Some(grid) = preview {
            self.draw_panel(&grid)?;
        }
        Ok(())
    }
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
