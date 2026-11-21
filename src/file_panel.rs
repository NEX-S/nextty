use crate::config::Config;
use crate::files::FileNav;
use crate::panel::{Grid, mix_color, rect};
use crate::workspace::Rect;

pub(crate) struct Layout {
    pub body: Rect,
    pub search: Option<Rect>,
    pub first: usize,
}

impl Layout {
    pub fn new(area: Rect, files: &mut FileNav) -> Self {
        let search =
            files
                .is_search()
                .then_some(rect(area.x + 1, area.y, area.cols.saturating_sub(2), 1));
        let body = rect(
            area.x,
            area.y + u16::from(search.is_some()),
            area.cols.saturating_sub(1),
            area.rows.saturating_sub(u16::from(search.is_some())),
        );
        let first = files.follow_selection(usize::from(body.rows));
        Self {
            body,
            search,
            first,
        }
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
    let foreground = mix_color(config.foreground, config.background, 176);
    let mut grid = Grid::new(rect, foreground, config.background);
    if rect.cols == 0 {
        return grid;
    }
    let divider = crate::panel::rgb(mix_color(config.foreground, config.background, 24));
    for row in &mut grid.rows {
        let cell = row.cells.last_mut().unwrap();
        cell.c = '│';
        cell.fg = divider;
        row.used = row.cells.len();
    }
    if let (Some(search), Some(query)) = (layout.search, &files.query) {
        let (before, after) = query.text.split_at(query.cursor);
        grid.text(
            search.x,
            search.y,
            search.cols,
            &["⌕ ", before, preedit, "▏", after].concat(),
            0xb5d2a2,
            config.background,
        );
    }
    for (row, entry) in files
        .scan
        .entries
        .iter()
        .skip(layout.first)
        .take(usize::from(layout.body.rows))
        .enumerate()
    {
        let y = layout.body.y + row as u16;
        let selected = layout.first + row == files.selection();
        grid.text(
            rect.x + 1,
            y,
            rect.cols.saturating_sub(2),
            &crate::width::fit_middle(&files.entry_label(entry), rect.cols.saturating_sub(2)),
            if selected && focused {
                config.foreground
            } else {
                foreground
            },
            config.background,
        );
        if selected {
            grid.text(
                rect.x,
                y,
                1,
                "▎",
                if focused {
                    crate::panel::ACCENT
                } else {
                    foreground
                },
                config.background,
            );
            grid.recolor(
                Rect {
                    y,
                    rows: 1,
                    ..layout.body
                },
                mix_color(
                    config.foreground,
                    config.background,
                    if focused { 24 } else { 12 },
                ),
            );
        }
    }
    if files.scan.entries.is_empty() && files.scan.error.is_none() {
        let message = if files.searching() {
            "Loading…"
        } else if files.is_search() {
            "No matches"
        } else {
            "Empty directory"
        };
        grid.text(
            layout.body.x + 1,
            layout.body.y,
            layout.body.cols.saturating_sub(1),
            message,
            foreground,
            config.background,
        );
    }
    if let Some(error) = &files.scan.error {
        grid.text(
            rect.x + 1,
            if files.scan.entries.is_empty() {
                layout.body.y
            } else {
                rect.y + rect.rows.saturating_sub(1)
            },
            rect.cols.saturating_sub(2),
            &error.to_string(),
            0xe87575,
            config.background,
        );
    }
    grid
}

pub(crate) fn preview(files: &FileNav, rect: Rect, config: &Config) -> Grid {
    let background = mix_color(config.foreground, config.background, 9);
    let muted = mix_color(config.foreground, config.background, 176);
    let title = files
        .selected()
        .map(|entry| entry.display_name())
        .unwrap_or_default();
    let mut grid = Grid::popup(rect, config.foreground, config.background, &title);
    let body = crate::panel::popup_body(rect);
    let columns = body.cols;
    let error = files.preview.error.as_ref().map(ToString::to_string);
    let message = if files.preview_pending {
        Some(("Loading…", muted))
    } else if let Some(error) = &error {
        Some((
            error.as_str(),
            crate::panel::diff_colors(config.foreground).1,
        ))
    } else if files.selected().is_none() {
        Some(("Select a file", muted))
    } else if files.selected().is_some_and(|entry| entry.is_dir) {
        Some(("Directory", muted))
    } else if files.preview.lines.is_empty() {
        Some(("Empty file", muted))
    } else {
        None
    };
    if let Some((text, color)) = message {
        grid.text(rect.x + 2, body.y, columns, text, color, background);
    }
    for (row, line) in files
        .preview
        .lines
        .iter()
        .skip(files.preview_scroll)
        .take(usize::from(body.rows))
        .enumerate()
    {
        let y = body.y + row as u16;
        grid.text(rect.x + 2, y, columns, line, config.foreground, background);
    }
    grid.text(
        rect.x + 2,
        rect.y + rect.rows.saturating_sub(2),
        columns,
        if files.preview.truncated {
            "Preview truncated · Esc close · Enter insert path"
        } else {
            "Esc close · Enter insert path"
        },
        muted,
        background,
    );
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_files_render_the_search_row_and_preview_text() {
        let directory =
            std::env::temp_dir().join(format!("nextty-file-grid-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("中.txt"), "hello 中文\n").unwrap();
        let mut files = FileNav::new(directory.clone()).unwrap();
        let finish = |files: &mut FileNav| {
            while files.searching() {
                let mut fds = [crate::sys::watch(files.fd(), libc::POLLIN)];
                crate::sys::poll(&mut fds, 5000).unwrap();
                files.drain_notification().unwrap();
                files.poll();
            }
        };
        finish(&mut files);
        files.insert_query("中");
        finish(&mut files);
        let area = rect(0, 1, 24, 15);
        let layout = Layout::new(area, &mut files);
        assert_eq!(layout.entry_at(&files, 1, layout.body.y), Some(0));
        let config = Config::default();
        let list = list(&mut files, area, "文", true, &config);
        let row = &list.rows[usize::from(layout.search.unwrap().y - area.y)];
        assert!(row.cells.iter().any(|cell| cell.c == '文'));
        files.focus_preview();
        finish(&mut files);
        let preview = preview(&files, preview_rect(rect(0, 1, 80, 22)), &config);
        let body = crate::panel::popup_body(preview.rect);
        let first = &preview.rows[usize::from(body.y - preview.rect.y)];
        let text: String = first.cells.iter().map(|cell| cell.c).collect();
        assert!(text.contains("hello"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
