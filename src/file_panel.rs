use crate::config::Config;
use crate::files::FileNav;
use crate::panel::{ACCENT, Grid, mix_color, rect};
use crate::workspace::Rect;

/// The list rows below the search row, if any, and the first entry they show.
pub(crate) struct Layout {
    pub body: Rect,
    pub first: usize,
}

impl Layout {
    pub fn new(area: Rect, files: &mut FileNav) -> Self {
        let top = u16::from(files.is_search());
        let body = rect(
            area.x,
            area.y + top,
            area.cols.saturating_sub(1),
            area.rows.saturating_sub(top),
        );
        let first = files.follow_selection(usize::from(body.rows));
        Self { body, first }
    }

    pub fn entry_at(&self, files: &FileNav, column: u16, row: u16) -> Option<usize> {
        if !self.body.contains(column, row) {
            return None;
        }
        let index = self.first + usize::from(row - self.body.y);
        (index < files.scan.entries.len()).then_some(index)
    }
}

pub(crate) fn sidebar_width(columns: u16) -> u16 {
    32.min(columns.saturating_sub(12))
}

pub(crate) fn preview_rect(canvas: Rect) -> Rect {
    let cols = (canvas.cols.saturating_mul(7) / 10)
        .max(12)
        .min(canvas.cols);
    let rows = (canvas.rows.saturating_mul(7) / 10).max(6).min(canvas.rows);
    rect(
        canvas.x + (canvas.cols - cols) / 2,
        canvas.y + (canvas.rows - rows) / 2,
        cols,
        rows,
    )
}

pub(crate) fn list(
    files: &mut FileNav,
    rect: Rect,
    preedit: &str,
    focused: bool,
    config: &Config,
) -> Grid {
    let layout = Layout::new(rect, files);
    let (foreground, background) = (config.foreground, config.background);
    let muted = mix_color(foreground, background, 176);
    let mut grid = Grid::new(rect, muted, background);
    if rect.cols == 0 {
        return grid;
    }
    let divider = crate::panel::rgb(mix_color(foreground, background, 24));
    for row in &mut grid.rows {
        let cell = row.cells.last_mut().unwrap();
        cell.c = '│';
        cell.fg = divider;
        row.used = row.cells.len();
    }
    let (x, width) = (rect.x + 1, rect.cols.saturating_sub(2));
    if let Some(query) = &files.query {
        let (text, _) = query.label(preedit, usize::from(width));
        grid.text(x, rect.y, width, &text, ACCENT, background);
    }
    let rows = usize::from(layout.body.rows);
    let entries = files.scan.entries.iter().skip(layout.first).take(rows);
    for (index, entry) in (layout.first..).zip(entries) {
        let y = layout.body.y + (index - layout.first) as u16;
        let label = files.entry_label(entry);
        let label = crate::width::fit_middle(&label, width);
        if index != files.selection() {
            grid.text(x, y, width, &label, muted, background);
            continue;
        }
        let (text, marker, weight) = if focused {
            (foreground, ACCENT, 24)
        } else {
            (muted, muted, 12)
        };
        grid.text(x, y, width, &label, text, background);
        grid.text(rect.x, y, 1, "▎", marker, background);
        let row = Rect {
            y,
            rows: 1,
            ..layout.body
        };
        grid.recolor(row, mix_color(foreground, background, weight));
    }
    let message = match &files.scan.error {
        Some(error) => Some((error.to_string(), 0xe87575)),
        None if !files.scan.entries.is_empty() => None,
        None if files.searching() => Some(("Loading…".into(), muted)),
        None if files.is_search() => Some(("No matches".into(), muted)),
        None => Some(("Empty directory".into(), muted)),
    };
    if let Some((text, color)) = message {
        let y = if files.scan.entries.is_empty() {
            layout.body.y
        } else {
            rect.y + rect.rows.saturating_sub(1)
        };
        grid.text(x, y, width, &text, color, background);
    }
    grid
}

pub(crate) fn preview(files: &FileNav, rect: Rect, config: &Config) -> Grid {
    let background = mix_color(config.foreground, config.background, 9);
    let muted = mix_color(config.foreground, config.background, 176);
    // The popup only opens for a selected file, and every rescan returns the focus to the list.
    let title = files.selected().unwrap().display_name();
    let mut grid = Grid::popup(rect, config.foreground, config.background, &title);
    let body = crate::panel::popup_body(rect);
    let preview = &files.preview;
    let message = match &preview.error {
        _ if files.preview_pending => Some(("Loading…".into(), muted)),
        Some(error) => Some((
            error.to_string(),
            crate::panel::diff_colors(config.foreground).1,
        )),
        None if preview.lines.is_empty() => Some(("Empty file".into(), muted)),
        None => None,
    };
    if let Some((text, color)) = message {
        grid.text(body.x, body.y, body.cols, &text, color, background);
    }
    let lines = preview.lines.iter().skip(files.preview_scroll);
    for (y, line) in (body.y..).zip(lines.take(usize::from(body.rows))) {
        grid.text(body.x, y, body.cols, line, config.foreground, background);
    }
    let hint = if preview.truncated {
        "Preview truncated · Esc close · Enter insert path"
    } else {
        "Esc close · Enter insert path"
    };
    let y = rect.y + rect.rows.saturating_sub(2);
    grid.text(body.x, y, body.cols, hint, muted, background);
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_files_render_the_search_row_and_preview_text() {
        use crate::files::tests::{Fixture, finish};
        let fixture = Fixture::new();
        std::fs::write(fixture.file("中.txt"), "hello 中文\n").unwrap();
        let mut files = FileNav::new(fixture.0.clone()).unwrap();
        finish(&mut files);
        files.insert_query("中");
        finish(&mut files);
        let area = rect(0, 1, 24, 15);
        let layout = Layout::new(area, &mut files);
        assert_eq!(layout.entry_at(&files, 1, layout.body.y), Some(0));
        let config = Config::default();
        let list = list(&mut files, area, "文", true, &config);
        assert!(list.rows[0].cells.iter().any(|cell| cell.c == '文'));
        files.focus_preview();
        finish(&mut files);
        let preview = preview(&files, preview_rect(rect(0, 1, 80, 22)), &config);
        let body = crate::panel::popup_body(preview.rect);
        let first = &preview.rows[usize::from(body.y - preview.rect.y)];
        let text: String = first.cells.iter().map(|cell| cell.c).collect();
        assert!(text.contains("hello"));
    }
}
