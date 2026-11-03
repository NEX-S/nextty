use crate::config::Config;
use crate::files::FileNav;
use crate::panel::{Grid, mix_color};
use crate::workspace::Rect;

pub(crate) struct Layout {
    pub body: Rect,
    pub search: Option<Rect>,
    pub first: usize,
}

impl Layout {
    pub fn new(rect: Rect, files: &mut FileNav) -> Self {
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
    if files.entries().is_empty() && files.error().is_none() {
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
    if let Some(error) = files.error() {
        grid.text(
            rect.x + 1,
            if files.entries().is_empty() {
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
        &crate::width::fit_middle(&title, columns),
        config.foreground,
        background,
    );
    let error = files.preview_error().map(ToString::to_string);
    let mut lines = files.preview_lines().skip(files.preview_scroll());
    for row in 0..rect.rows.saturating_sub(2) {
        let line = lines.next();
        let (text, color) = match (row, files.preview_loading(), error.as_deref()) {
            (0, true, _) => ("Loading…", muted),
            (0, _, Some(error)) => (error, 0xe87575),
            (0, false, None) if files.preview_lines().len() == 0 => ("Empty file", muted),
            _ => match line {
                Some(text) => (text, config.foreground),
                None => break,
            },
        };
        grid.text(
            rect.x + 1,
            rect.y + row + 1,
            columns,
            text,
            color,
            background,
        );
    }
    grid.text(
        rect.x + 1,
        rect.y + rect.rows.saturating_sub(1),
        columns,
        if files.preview_truncated() {
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
    use crate::core::{Core, WindowSize};
    use std::time::{Duration, Instant};

    #[test]
    fn loaded_files_share_list_hits_search_rows_and_preview_text() {
        let directory =
            std::env::temp_dir().join(format!("nextty-file-grid-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("中.txt"), "hello 中文\nsecond\nthird\n").unwrap();
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
        let before = Layout::new(rect, &mut files);
        assert_eq!(before.entry_at(&files, 1, before.body.y), Some(0));
        assert_eq!(before.entry_at(&files, 23, before.body.y), None);
        files.set_query("中".into());
        finish(&mut files);
        let layout = Layout::new(rect, &mut files);
        assert_eq!(layout.body.y, before.body.y + 1);
        assert_eq!(layout.entry_at(&files, 1, layout.body.y), Some(0));
        let config = Config {
            foreground: 0x123456,
            background: 0xf0e0d0,
            ..Config::default()
        };
        let list = list(&mut files, rect, "文", true, &config);
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
        files.scroll(1, 1);
        let scrolled = super::preview(
            &files,
            Rect {
                rows: 3,
                ..preview.rect
            },
            &config,
        );
        assert!(
            scrolled.rows[1]
                .cells
                .iter()
                .map(|cell| cell.c)
                .collect::<String>()
                .contains("second")
        );
        drop(files);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
