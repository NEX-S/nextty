use crate::native::{Line, Renderer, Run};
use crate::{
    core::{Flags, Point},
    screen::Screen,
    vt::{Color, CursorShape, NamedColor},
    width::scalar_width,
};
use std::num::NonZeroU32;

pub fn text_columns(text: &str) -> Vec<u16> {
    // Printable ASCII columns already match the native shaper's byte offsets.
    let prefix = crate::width::printable_ascii_prefix(text.as_bytes());
    if prefix == text.len() {
        return Vec::new();
    }
    let mut result: Vec<u16> = (0..prefix as u16).collect();
    let (mut column, mut previous) = (prefix, prefix.saturating_sub(1));
    for ch in text[prefix..].chars() {
        let width = usize::from(scalar_width(ch));
        let owner = if width == 0 { previous } else { column };
        result.extend(std::iter::repeat_n(owner as u16, ch.len_utf8()));
        if width != 0 {
            previous = column;
            column += width;
        }
    }
    result
}

struct StyleRun {
    text: Run,
    start: u16,
    end: u16,
    background: Option<NonZeroU32>,
    underline: Option<NonZeroU32>,
    decorations: Flags,
}

fn text_runs(runs: &[StyleRun]) -> impl Iterator<Item = Run> + '_ {
    runs.chunk_by(|a, b| a.text.color == b.text.color && a.text.flags == b.text.flags)
        .map(|runs| Run {
            end: runs.last().unwrap().text.end,
            ..runs[0].text
        })
}

/// A screen row as text and style runs, and the line shaped from them.
struct Row {
    text: String,
    runs: Vec<StyleRun>,
    columns: Vec<u16>,
    line: Option<Line>,
}

/// A pane's rows; a damaged row keeps its shaped line while its text and colors stay the same.
#[derive(Default)]
pub struct LineCache {
    rows: Vec<Row>,
    columns: usize,
    focused: bool,
    cursor: Option<(usize, usize, CursorShape, usize)>,
    cursor_color: u32,
    block_cursor: Option<(usize, usize)>,
    ime_cursor: Option<Point>,
}

impl LineCache {
    /// The cell an IME composes at: the cursor, or where it last showed while a program hides it.
    pub fn ime_position(&mut self, screen: &Screen) -> (usize, usize) {
        let (cursor, shape) = screen.cursor();
        // Hidden cursors can visit drawing cells; retain this pane's insertion point.
        if shape != CursorShape::Hidden {
            self.ime_cursor = Some(cursor);
        }
        let cursor = self.ime_cursor.unwrap_or(cursor);
        (
            cursor.column.min(screen.columns() - 1),
            (cursor.line + screen.display_offset() as i32)
                .clamp(0, screen.screen_lines() as i32 - 1) as usize,
        )
    }

    /// Rebuilds the damaged rows of `term` and returns its default background. `hide_cursor`
    /// leaves the cursor to an input field or IME preedit drawn over the pane.
    pub fn update(&mut self, term: &mut Screen, focused: bool, hide_cursor: bool) -> u32 {
        let row_count = term.screen_lines();
        let columns = term.columns();
        let selection = term.selection_range();
        // Lines that scrolled through since the previous frame; the cache shifts with them.
        let scroll = std::mem::take(&mut term.scrolled).min(row_count);
        if self.rows.len() != row_count || self.columns != columns {
            term.dirty.fill(true);
        }
        let background = term.color(Color::Named(NamedColor::Background)).rgba();
        let (cursor_point, cursor_shape) = term.cursor();
        let cursor_row = cursor_point.line + term.display_offset() as i32;
        let cursor = (!hide_cursor
            && cursor_shape != CursorShape::Hidden
            && (0..row_count as i32).contains(&cursor_row))
        .then(|| {
            (
                cursor_point.column,
                cursor_row as usize,
                cursor_shape,
                term.row(cursor_row as usize).cells[cursor_point.column].width(),
            )
        });
        let block_cursor = cursor
            .filter(|(_, _, shape, _)| focused && *shape == CursorShape::Block)
            .map(|(column, row, _, _)| (row, column));
        // The filled cursor changes cached text colors; focus and overlays can change
        // its visibility without any PTY damage.
        let previous = self
            .block_cursor
            .map(|(row, column)| (row.saturating_sub(scroll), column));
        if previous != block_cursor {
            for (row, _) in previous.into_iter().chain(block_cursor) {
                if let Some(dirty) = term.dirty.get_mut(row) {
                    *dirty = true;
                }
            }
        }
        self.rows.truncate(row_count);
        let scroll = scroll.min(self.rows.len());
        self.rows.rotate_left(scroll);
        let text_style = Flags::BOLD
            | Flags::ITALIC
            | Flags::DIM
            | Flags::INVERSE
            | Flags::ALL_UNDERLINES
            | Flags::STRIKEOUT;
        let mut native_runs = smallvec::SmallVec::<[Run; 4]>::new();
        for index in term.dirty_rows() {
            let line = index as i32 - term.display_offset() as i32;
            let data = term.row(index);
            let mut end = data
                .non_default_len()
                .max(data.extras().last().map_or(0, |(column, _)| column + 1));
            if let Some(selection) = &selection
                && (selection.start.line..=selection.end.line).contains(&line)
            {
                end = end.max(if line == selection.end.line {
                    selection.end.column + 1
                } else {
                    columns
                });
            }
            if let Some((row, column)) = block_cursor
                && row == index
            {
                end = end.max(column + 1);
            }
            let end = end.min(columns);
            let mut text = String::with_capacity(end);
            let mut runs: Vec<StyleRun> = Vec::new();
            let mut column_map = Vec::new();
            let mut previous_style = None;
            for (column, cell, extra) in data.cells_with_extras(0..end) {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let width = cell.width();
                let p = Point { line, column };
                let selected = selection.as_ref().is_some_and(|s| s.contains(p))
                    && !cell.flags.contains(Flags::PROMPT);
                let block_cursor = block_cursor == Some((index, column));
                let start = text.len();
                if cell.flags.contains(Flags::HIDDEN) {
                    text.extend(std::iter::repeat_n(' ', width));
                } else {
                    text.push(cell.c);
                    if let Some(extra) = extra {
                        text.extend(&extra.zerowidth);
                    }
                }
                if !column_map.is_empty() || start != column || text.len() - start != 1 {
                    if column_map.is_empty() {
                        column_map.reserve(text.capacity());
                        column_map.extend((0..start).map(|x| x as u16));
                    }
                    column_map.extend(std::iter::repeat_n(column as u16, text.len() - start));
                }
                let flags = cell.flags & text_style;
                let underline_color = extra.and_then(|extra| extra.underline_color);
                let style = (
                    cell.fg,
                    cell.bg,
                    flags,
                    underline_color,
                    selected,
                    block_cursor,
                );
                if previous_style == Some(style)
                    && let Some(run) = runs.last_mut()
                    && usize::from(run.end) == column
                {
                    run.text.end = text.len() as u32;
                    run.end = (column + width) as u16;
                    continue;
                }
                previous_style = Some(style);
                let (fg, bg, default_bg) = if selected {
                    (0xccccccff, 0x303030ff, false)
                } else {
                    let mut fg = term.color(cell.fg);
                    if flags.contains(Flags::DIM) {
                        fg = fg.dimmed();
                    }
                    let fg = fg.rgba();
                    let bg = term.color(cell.bg).rgba();
                    if flags.contains(Flags::INVERSE) {
                        (bg, fg, false)
                    } else {
                        (fg, bg, cell.bg == Color::Named(NamedColor::Background))
                    }
                };
                runs.push(StyleRun {
                    text: Run {
                        start: start as u32,
                        end: text.len() as u32,
                        color: if block_cursor { background } else { fg },
                        flags: u32::from((flags & (Flags::BOLD | Flags::ITALIC)).bits()),
                    },
                    start: column as u16,
                    end: (column + width) as u16,
                    background: NonZeroU32::new(if default_bg { 0 } else { bg }),
                    underline: NonZeroU32::new(if flags.intersects(Flags::ALL_UNDERLINES) {
                        underline_color.map_or(fg, |c| term.color(c).rgba())
                    } else {
                        0
                    }),
                    decorations: flags & (Flags::UNDERCURL | Flags::STRIKEOUT),
                });
            }
            while let Some(run) = runs.last_mut()
                && run.background.is_none()
                && run.underline.is_none()
                && !run.decorations.contains(Flags::STRIKEOUT)
                && !block_cursor.is_some_and(|(row, column)| {
                    row == index && (usize::from(run.start)..usize::from(run.end)).contains(&column)
                })
            {
                let start = run.text.start as usize;
                let trimmed = text[start..].trim_end_matches(' ').len();
                text.truncate(start + trimmed);
                column_map.truncate(text.len());
                run.text.end = text.len() as u32;
                if trimmed != 0 {
                    break;
                }
                runs.pop();
            }
            if let Some(cached) = self.rows.get_mut(index)
                && cached.text == text
                && cached.columns == column_map
                && text_runs(&cached.runs).eq(text_runs(&runs))
            {
                cached.runs = runs;
                continue;
            }
            native_runs.clear();
            native_runs.extend(text_runs(&runs));
            let row = Row {
                line: (!text.is_empty()).then(|| Line::new(&text, &native_runs, &column_map)),
                text,
                runs,
                columns: column_map,
            };
            if let Some(slot) = self.rows.get_mut(index) {
                *slot = row;
            } else {
                self.rows.push(row);
            }
        }
        term.reset_damage();
        self.columns = columns;
        self.focused = focused;
        self.cursor = cursor;
        self.cursor_color = term.color(Color::Named(NamedColor::Cursor)).rgba();
        self.block_cursor = block_cursor;
        background
    }

    /// Draws the rows at `origin`; backgrounds of the last row and column fill `size`.
    pub fn paint(
        &self,
        renderer: &Renderer,
        cell: (f32, f32),
        origin: (f32, f32),
        size: (f32, f32),
    ) {
        let row_count = self.rows.len();
        for (row, data) in self.rows.iter().enumerate() {
            let y = origin.1 + cell.1 * row as f32;
            for runs in data
                .runs
                .chunk_by(|a, b| a.background == b.background && a.end == b.start)
            {
                let run = &runs[0];
                if let Some(color) = run.background {
                    let x = origin.0 + cell.0 * run.start as f32;
                    let end = runs.last().unwrap().end;
                    let mut width = cell.0 * (end - run.start) as f32;
                    if usize::from(end) == self.columns {
                        width += (size.0 - cell.0 * self.columns as f32).max(0.);
                    }
                    let mut height = cell.1;
                    if row + 1 == row_count {
                        height += (size.1 - cell.1 * row_count as f32).max(0.);
                    }
                    renderer.rect(x, y, width, height, color.get());
                }
            }
            // Cell backgrounds fill the pane's remainder; the cursor stays on the grid.
            if let Some((column, cursor_row, CursorShape::Block, width)) = self.cursor
                && self.focused
                && row == cursor_row
            {
                renderer.rect(
                    origin.0 + cell.0 * column as f32,
                    y,
                    cell.0 * width as f32,
                    cell.1,
                    self.cursor_color,
                );
            }
            if let Some(line) = &data.line {
                renderer.line(line, origin.0, y);
            }
            for run in &data.runs {
                let x = origin.0 + cell.0 * run.start as f32;
                let width = cell.0 * (run.end - run.start) as f32;
                if let Some(color) = run.underline {
                    if run.decorations.contains(Flags::UNDERCURL) {
                        renderer.undercurl(x, y + cell.1 - 4., width, 3., color.get());
                    } else {
                        renderer.rect(x, y + cell.1 - 2., width, 1., color.get());
                    }
                }
                if run.decorations.contains(Flags::STRIKEOUT) {
                    renderer.rect(x, y + cell.1 * 0.55, width, 1., run.text.color);
                }
            }
        }
        if let Some((column, row, shape, width)) = self.cursor {
            let x = origin.0 + cell.0 * column as f32;
            let y = origin.1 + cell.1 * row as f32;
            let width = cell.0 * width as f32;
            if !self.focused {
                renderer.rect(x, y, width, 1., self.cursor_color);
                renderer.rect(x, y + cell.1 - 1., width, 1., self.cursor_color);
                renderer.rect(x, y, 1., cell.1, self.cursor_color);
                renderer.rect(x + width - 1., y, 1., cell.1, self.cursor_color);
            } else {
                match shape {
                    CursorShape::Beam => renderer.rect(x, y, 2., cell.1, self.cursor_color),
                    CursorShape::Underline => {
                        renderer.rect(x, y + cell.1 - 2., width, 2., self.cursor_color)
                    }
                    _ => (),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        core::{Core, SelectionType, Side, WindowSize},
        screen::Update,
        vt::Parser,
    };

    fn core(num_cols: u16, num_lines: u16, foreground: u32, background: u32) -> Core {
        let size = WindowSize {
            num_cols,
            num_lines,
            cell_width: 8,
            cell_height: 16,
        };
        Core::new(size, foreground, background)
    }

    fn rows(core: &Core, focused: bool) -> (LineCache, u32) {
        let mut screen = Screen::default();
        screen.apply(Update::capture(core, true));
        let mut cache = LineCache::default();
        let background = cache.update(&mut screen, focused, false);
        (cache, background)
    }

    #[test]
    fn bottom_right_block_cursor_is_separate_from_the_cell_background() {
        let mut core = core(4, 2, 0xcccccc, 0x202020);
        Parser::new().advance(&mut core, b"\x1b[2;4HX\x1b[2;4H\x1b[2 q");
        let (focused, background) = rows(&core, true);
        assert_eq!(focused.cursor, Some((3, 1, CursorShape::Block, 1)));
        let run = focused.rows[1].runs.last().unwrap();
        assert_eq!(run.text.color, background);
        assert!(run.background.is_none());

        let (unfocused, _) = rows(&core, false);
        let run = unfocused.rows[1].runs.last().unwrap();
        assert_eq!(run.text.color, 0xccccccff);
        assert!(run.background.is_none());
    }

    #[test]
    fn ui_text_columns_keep_wide_and_combining_text_on_the_terminal_grid() {
        assert_eq!(text_columns("中a\u{301}"), [0, 0, 0, 2, 2, 2]);
        for text in ["👩‍💻a", "لاا", "\u{17d8}a"] {
            let width = crate::width::text_width(text);
            assert!(text_columns(text).iter().all(|&c| usize::from(c) < width));
        }
    }

    #[test]
    fn blank_tail_preserves_visible_decorations_and_wide_text() {
        for (input, expected) in [
            ("abc\x1b[31m  ", "abc"),
            ("abc\x1b[44m  \x1b[0m", "abc  "),
            ("abc\x1b[4m  \x1b[0m", "abc  "),
            ("abc\x1b[9m  \x1b[0m", "abc  "),
            ("中a\u{301}  ", "中a\u{301}"),
            (" \u{301}", " \u{301}"),
            ("abc\u{3000}  ", "abc\u{3000}"),
        ] {
            let config = Config::default();
            let mut core = core(12, 2, config.foreground, config.background);
            Parser::new().advance(&mut core, format!("\x1b[?25l{input}").as_bytes());
            let row = &rows(&core, true).0.rows[0];
            assert_eq!(row.text, expected);
            assert_eq!(row.runs.last().unwrap().text.end as usize, expected.len());
            assert!(row.columns.is_empty() || row.columns.len() == expected.len());
        }
    }

    #[test]
    fn selection_skips_prompt_cells_and_fills_blank_tails() {
        let mut core = core(21, 3, 0xcccccc, 0x202020);
        Parser::new().advance(
            &mut core,
            "\x1b[?25l\x1b]133;A\x07 • \x1b]133;B\x07echo".as_bytes(),
        );
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(Point { line: 2, column: 4 }, Side::Right);
        let (cache, _) = rows(&core, true);
        let selected = |row: &Row| -> Vec<_> {
            row.runs
                .iter()
                .filter(|run| run.background == NonZeroU32::new(0x303030ff))
                .map(|run| (run.start, run.end))
                .collect()
        };
        assert_eq!(selected(&cache.rows[0]), [(3, 21)]);
        assert_eq!(selected(&cache.rows[1]), [(0, 21)]);
        assert_eq!(selected(&cache.rows[2]), [(0, 5)]);
    }

    #[test]
    fn ime_anchor_stays_with_each_pane_when_tui_cursors_are_hidden() {
        let mut core = core(40, 12, 0xcccccc, 0x202020);
        let mut parser = Parser::new();
        let mut screen = Screen::default();
        let (mut left, mut right) = (LineCache::default(), LineCache::default());
        parser.advance(&mut core, b"\x1b[3;5H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(left.ime_position(&screen), (4, 2));
        parser.advance(&mut core, b"\x1b[9;7H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(right.ime_position(&screen), (6, 8));

        parser.advance(&mut core, b"\x1b[?25l\x1b[12;40H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(left.ime_position(&screen), (4, 2));
        assert_eq!(right.ime_position(&screen), (6, 8));

        parser.advance(&mut core, b"\x1b[?25h\x1b[10;8H");
        screen.apply(Update::capture(&core, true));
        assert_eq!(right.ime_position(&screen), (7, 9));
    }

    #[test]
    fn right_prompt_shaping_and_decorations_keep_terminal_columns() {
        let mut core = core(21, 2, 0xcccccc, 0x202020);
        Parser::new().advance(&mut core, "\x1b]133;A\x07 • \x1b]133;B\x07cmd\x1b[1;19H\x1b[44;4m\x1b]133;P;2\x07中a\u{301}\x1b]133;P;0\x07\x1b[0m\x1b[1;4H".as_bytes());
        let (cache, _) = rows(&core, true);
        let row = &cache.rows[0];
        assert_eq!(&row.columns[row.columns.len() - 3..], [20, 20, 20]);
        let run = row.runs.last().unwrap();
        assert_eq!((run.start, run.end), (18, 21));
        assert!(run.background.is_some());
        assert!(run.underline.is_some());
        assert_eq!(row.columns[1], 1);
        assert_eq!(cache.cursor.unwrap().0, 3);
    }
}
