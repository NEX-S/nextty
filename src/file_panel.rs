use crate::config::Config;
use crate::files::FileNav;
use crate::panel::Grid;
use crate::ui::mix_color;
use crate::workspace::Rect;

pub(crate) struct Layout {
    pub body: Rect,
    pub search: Option<Rect>,
    pub first: usize,
}

impl Layout {
    pub fn new(rect: Rect, files: &FileNav) -> Self {
        let search = files.is_search().then_some(Rect {
            x: rect.x + 1,
            y: rect.y,
            cols: rect.cols.saturating_sub(2),
            rows: 1,
        });
        let body = Rect {
            x: rect.x,
            y: rect.y + u16::from(search.is_some()),
            cols: rect.cols.saturating_sub(1),
            rows: rect.rows.saturating_sub(u16::from(search.is_some())),
        };
        let first = files
            .selection()
            .saturating_sub(usize::from(body.rows.saturating_sub(1)));
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
        (index < files.entries().len()).then_some(index)
    }
}

pub(crate) fn preview_rect(canvas: Rect) -> Rect {
    let cols = (canvas.cols.saturating_mul(7) / 10)
        .max(12)
        .min(canvas.cols);
    let rows = (canvas.rows.saturating_mul(7) / 10).max(5).min(canvas.rows);
    Rect {
        x: canvas.x + (canvas.cols - cols) / 2,
        y: canvas.y + (canvas.rows - rows) / 2,
        cols,
        rows,
    }
}

pub(crate) fn list(files: &FileNav, rect: Rect, preedit: &str, config: &Config) -> Grid {
    let layout = Layout::new(rect, files);
    let foreground = mix_color(config.foreground, config.background, 176);
    let mut grid = Grid::new(rect, foreground, config.background);
    if rect.cols == 0 {
        return grid;
    }
    let divider = mix_color(config.foreground, config.background, 24);
    for row in rect.y..rect.y + rect.rows {
        grid.text(
            rect.x + rect.cols - 1,
            row,
            1,
            "│",
            divider,
            config.background,
        );
    }
    if let Some(search) = layout.search {
        grid.text(
            search.x,
            search.y,
            search.cols,
            &format!("⌕ {}{preedit}", files.query()),
            0xb5d2a2,
            config.background,
        );
    }
    for (row, entry) in files
        .entries()
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
            &files.entry_label(entry),
            if selected {
                config.foreground
            } else {
                foreground
            },
            config.background,
        );
        if selected {
            grid.text(rect.x, y, 1, "▎", crate::panel::ACCENT, config.background);
            grid.recolor(
                Rect {
                    y,
                    rows: 1,
                    ..layout.body
                },
                mix_color(config.foreground, config.background, 24),
            );
        }
    }
    if let Some(error) = files.error() {
        grid.text(
            rect.x + 1,
            rect.y + rect.rows.saturating_sub(1),
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
    let mut grid = Grid::new(rect, config.foreground, background);
    let columns = rect.cols.saturating_sub(2);
    let title = files
        .selected()
        .map(|entry| entry.display_name())
        .unwrap_or_default();
    grid.text(
        rect.x + 1,
        rect.y,
        columns,
        &title,
        config.foreground,
        background,
    );
    let error = files.preview_error().map(ToString::to_string);
    for row in 0..rect.rows.saturating_sub(2) {
        let text = match (row, files.preview_loading(), error.as_deref()) {
            (0, true, _) => "Loading…",
            (0, _, Some(error)) => error,
            _ => match files
                .preview_lines()
                .get(files.preview_scroll() + usize::from(row))
            {
                Some(text) => text,
                None => break,
            },
        };
        grid.text(
            rect.x + 1,
            rect.y + row + 1,
            columns,
            text,
            config.foreground,
            background,
        );
    }
    grid.text(
        rect.x + 1,
        rect.y + rect.rows.saturating_sub(1),
        columns,
        if files.preview_truncated() {
            "Preview truncated · Esc close · Enter copy path"
        } else {
            "Esc close · Enter copy path"
        },
        mix_color(config.foreground, config.background, 144),
        background,
    );
    grid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, WindowSize};
    use std::time::{Duration, Instant};

    #[test]
    fn loaded_files_share_list_hits_search_rows_and_preview_text() {
        let directory =
            std::env::temp_dir().join(format!("nextty-file-grid-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("中.txt"), "hello 中文\n").unwrap();
        let mut files = FileNav::new(directory.clone()).unwrap();
        let finish = |files: &mut FileNav| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while files.searching() {
                let mut fds = [libc::pollfd {
                    fd: files.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }];
                crate::poll::poll(&mut fds, 1000).unwrap();
                files.drain_notification().unwrap();
                files.poll();
                assert!(Instant::now() < deadline);
            }
        };
        finish(&mut files);
        let rect = Rect {
            x: 0,
            y: 1,
            cols: 24,
            rows: 15,
        };
        let before = Layout::new(rect, &files);
        assert_eq!(before.entry_at(&files, 1, before.body.y), Some(0));
        assert_eq!(before.entry_at(&files, 23, before.body.y), None);
        files.set_query("中".into());
        finish(&mut files);
        let layout = Layout::new(rect, &files);
        assert_eq!(layout.body.y, before.body.y + 1);
        assert_eq!(layout.entry_at(&files, 1, layout.body.y), Some(0));
        let config = Config {
            foreground: 0x123456,
            background: 0xf0e0d0,
            ..Config::default()
        };
        let list = list(&files, rect, "文", &config);
        assert_eq!(
            list.rows[0].cells[0].bg,
            crate::panel::rgb(config.background)
        );
        let search = layout.search.unwrap();
        assert!(
            list.rows[usize::from(search.y - rect.y)]
                .cells
                .iter()
                .any(|cell| cell.c == '文')
        );
        assert!(files.focus_preview());
        finish(&mut files);
        let canvas = Rect {
            x: 0,
            y: 1,
            cols: 80,
            rows: 22,
        };
        let preview = preview(&files, preview_rect(canvas), &config);
        assert_eq!(
            preview.rows[0].cells[0].fg,
            crate::panel::rgb(config.foreground)
        );
        let mut bytes = Vec::new();
        preview.ansi(&mut bytes, None, &[]).unwrap();
        let mut terminal = Core::new(
            WindowSize {
                num_cols: 80,
                num_lines: 24,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(&mut terminal, &bytes);
        let row = terminal.row(usize::from(preview.rect.y + 1));
        assert!(
            row.cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .contains("hello")
        );
        drop(files);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
