use crate::native::{Line, Renderer, Run};
use crate::{
    core::{Flags, Point},
    error::Result,
    screen::Screen,
    vt::{Color, CursorShape, NamedColor},
    width::scalar_width,
};
use std::num::NonZeroU32;

pub fn text_columns(text: &str) -> Vec<u16> {
    // Printable ASCII columns already match the native shaper's byte offsets.
    let Some(prefix) = text.bytes().position(|byte| !(b' '..=b'~').contains(&byte)) else {
        return Vec::new();
    };
    let mut result = Vec::with_capacity(text.len());
    result.extend((0..prefix).map(|column| column as u16));
    let (mut column, mut previous) = (prefix, prefix.saturating_sub(1));
    for ch in text[prefix..].chars() {
        let width = usize::from(scalar_width(ch).min(2));
        result.extend(std::iter::repeat_n(
            if width == 0 {
                previous as u16
            } else {
                column as u16
            },
            ch.len_utf8(),
        ));
        if width != 0 {
            previous = column;
        }
        column += width;
    }
    result
}

struct StyleRun {
    text: Run,
    start: u16,
    end: u16,
    background: Option<NonZeroU32>,
    underline: Option<(u32, bool)>,
    strike: bool,
}

struct Row {
    index: usize,
    text: String,
    runs: Vec<StyleRun>,
    columns: Vec<u16>,
}

struct CachedRow {
    text: String,
    columns: Vec<u16>,
    line: Option<Line>,
    runs: Vec<StyleRun>,
}

impl CachedRow {
    fn matches(&self, row: &Row) -> bool {
        // Native shaping merges adjacent runs with the same foreground and font.
        let connected = |a: &StyleRun, b: &StyleRun| {
            a.text.color == b.text.color && a.text.flags == b.text.flags
        };
        let shaping = |runs: &[StyleRun]| {
            let first = runs[0].text;
            (
                first.start,
                runs.last().unwrap().text.end,
                first.color,
                first.flags,
            )
        };
        self.text == row.text
            && self.columns == row.columns
            && self
                .runs
                .chunk_by(connected)
                .map(shaping)
                .eq(row.runs.chunk_by(connected).map(shaping))
    }
}

#[derive(Default)]
pub struct LineCache {
    rows: Vec<CachedRow>,
    columns: usize,
    block_cursor: Option<(usize, usize)>,
}

pub struct Frame {
    rows: Vec<Row>,
    pub background: u32,
    cursor: Option<(usize, usize, CursorShape, usize)>,
    cursor_color: u32,
    columns: usize,
    row_count: usize,
    block_cursor: Option<(usize, usize)>,
}

impl Frame {
    pub fn new(term: &mut Screen, cache: &LineCache, focused: bool, preedit: bool) -> Self {
        let row_count = term.screen_lines();
        let columns = term.columns();
        let selection = term.selection_range();
        let full = cache.rows.len() != row_count || cache.columns != columns;
        if full {
            term.dirty.fill(true);
        }
        let background = term.color(Color::Named(NamedColor::Background)).rgba();
        let cursor_color = term.color(Color::Named(NamedColor::Cursor)).rgba();
        let (cursor_point, cursor_shape) = term.cursor();
        let cursor_row = cursor_point.line + term.display_offset() as i32;
        let cursor = (!preedit
            && cursor_shape != CursorShape::Hidden
            && (0..row_count as i32).contains(&cursor_row))
        .then(|| {
            let width = if term.row(cursor_row as usize).cells[cursor_point.column]
                .flags
                .contains(Flags::WIDE_CHAR)
            {
                2
            } else {
                1
            };
            (
                cursor_point.column,
                cursor_row as usize,
                cursor_shape,
                width,
            )
        });
        let block_cursor = cursor
            .filter(|(_, _, shape, _)| focused && *shape == CursorShape::Block)
            .map(|(_, row, _, _)| (row, cursor_point.column));
        // The filled cursor changes cached text colors; focus and overlays can change
        // its visibility without any PTY damage.
        if !full && cache.block_cursor != block_cursor {
            for (row, _) in cache.block_cursor.into_iter().chain(block_cursor) {
                if let Some(dirty) = term.dirty.get_mut(row) {
                    *dirty = true;
                }
            }
        }
        let mut rows = Vec::with_capacity(term.dirty_rows().count());
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
            let previous = cache.rows.get(index);
            let mut text =
                String::with_capacity(previous.map_or(end, |row| end.max(row.text.len())));
            let previous_runs = previous.map_or(0, |row| row.runs.len());
            let mut runs: Vec<StyleRun> = Vec::with_capacity(previous_runs.min(end));
            let mut column_map = Vec::new();
            let mut previous_style = None;
            for (column, cell, extra) in data.cells_with_extras(0..end) {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                let p = Point { line, column };
                let selected = selection.as_ref().is_some_and(|s| s.contains(p))
                    && !cell.flags.contains(Flags::PROMPT);
                let block_cursor =
                    p == cursor_point && !preedit && focused && cursor_shape == CursorShape::Block;
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
                let style = (
                    cell.fg,
                    cell.bg,
                    cell.flags
                        & (Flags::BOLD
                            | Flags::ITALIC
                            | Flags::DIM
                            | Flags::INVERSE
                            | Flags::ALL_UNDERLINES
                            | Flags::STRIKEOUT),
                    extra.and_then(|e| e.underline_color),
                    selected,
                    block_cursor,
                );
                if previous_style == Some(style)
                    && runs
                        .last()
                        .is_some_and(|run| usize::from(run.end) == column)
                {
                    let run = runs.last_mut().unwrap();
                    run.text.end = text.len() as u32;
                    run.end = (column + width) as u16;
                    continue;
                }
                previous_style = Some(style);
                let mut fg = term.color(cell.fg);
                if cell.flags.contains(Flags::DIM) {
                    fg = fg.dimmed();
                }
                let mut fg = fg.rgba();
                let mut bg = term.color(cell.bg).rgba();
                let mut default_bg = cell.bg == Color::Named(NamedColor::Background);
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                    default_bg = false;
                }
                if selected {
                    bg = 0x303030ff;
                    fg = 0xccccccff;
                    default_bg = false;
                }
                if block_cursor {
                    bg = cursor_color;
                    fg = background;
                    default_bg = false;
                }
                runs.push(StyleRun {
                    text: Run {
                        start: start as u32,
                        end: text.len() as u32,
                        color: fg,
                        flags: u32::from(cell.flags.contains(Flags::BOLD))
                            | (u32::from(cell.flags.contains(Flags::ITALIC)) << 1),
                    },
                    start: column as u16,
                    end: (column + width) as u16,
                    background: NonZeroU32::new(if default_bg { 0 } else { bg }),
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES).then(|| {
                        (
                            extra
                                .and_then(|e| e.underline_color)
                                .map_or(fg, |c| term.color(c).rgba()),
                            cell.flags.contains(Flags::UNDERCURL),
                        )
                    }),
                    strike: cell.flags.contains(Flags::STRIKEOUT),
                });
            }
            while let Some(run) = runs.last() {
                if run.background.is_some() || run.underline.is_some() || run.strike {
                    break;
                }
                let start = run.text.start as usize;
                let trimmed = text[start..].trim_end_matches(' ').len();
                text.truncate(start + trimmed);
                column_map.truncate(text.len());
                runs.last_mut().unwrap().text.end = text.len() as u32;
                if trimmed != 0 {
                    break;
                }
                runs.pop();
            }
            rows.push(Row {
                index,
                text,
                runs,
                columns: column_map,
            });
        }
        term.reset_damage();
        Self {
            rows,
            background,
            cursor,
            cursor_color,
            columns,
            row_count,
            block_cursor,
        }
    }

    fn align_cache(&self, cache: &mut LineCache) {
        if cache.rows.len() != self.row_count || cache.columns != self.columns {
            return;
        }
        for rows in self.rows.chunk_by(|a, b| a.index + 1 == b.index) {
            if rows.len() == 1 {
                continue;
            }
            let cached = &mut cache.rows[rows[0].index..rows[0].index + rows.len()];
            let mut changed = rows
                .iter()
                .enumerate()
                .filter(|(new, row)| !cached[*new].matches(row))
                .peekable();
            let found = changed.next().and_then(|(new, row)| {
                if let Some(old) = cached.iter().position(|cached| cached.matches(row)) {
                    return Some((new, old));
                }
                changed.peek()?;
                let mut indexed: Vec<_> = cached.iter().enumerate().collect();
                // Preserve the first matching cache entry when text repeats.
                indexed.sort_unstable_by_key(|(index, row)| (&row.text, *index));
                changed.find_map(|(new, row)| {
                    let start = indexed.partition_point(|(_, cached)| cached.text < row.text);
                    indexed[start..]
                        .iter()
                        .take_while(|(_, cached)| cached.text == row.text)
                        .find(|(_, cached)| cached.matches(row))
                        .map(|(old, _)| (new, *old))
                })
            });
            if let Some((new, old)) = found {
                let count = cached.len();
                cached.rotate_left((old + count - new) % count);
            }
        }
    }

    pub fn paint(
        self,
        cell: (f32, f32),
        focused: bool,
        cache: &mut LineCache,
        renderer: &Renderer,
        origin: (f32, f32),
        size: (f32, f32),
    ) -> Result<()> {
        self.align_cache(cache);
        cache.rows.truncate(self.row_count);
        let mut native_runs = Vec::new();
        for row in self.rows {
            if cache
                .rows
                .get(row.index)
                .is_some_and(|cached| cached.matches(&row))
            {
                cache.rows[row.index].runs = row.runs;
                continue;
            }
            native_runs.clear();
            native_runs.extend(row.runs.iter().map(|run| run.text));
            let line = (!row.text.is_empty())
                .then(|| renderer.shape(&row.text, &native_runs, &row.columns))
                .transpose()?;
            let cached = CachedRow {
                text: row.text,
                columns: row.columns,
                line,
                runs: row.runs,
            };
            if row.index < cache.rows.len() {
                cache.rows[row.index] = cached;
            } else {
                cache.rows.push(cached);
            }
        }
        cache.columns = self.columns;
        cache.block_cursor = self.block_cursor;
        for (row, data) in cache.rows.iter().enumerate() {
            let y = origin.1 + cell.1 * row as f32;
            for runs in data
                .runs
                .chunk_by(|a, b| a.background == b.background && a.end == b.start)
            {
                let run = &runs[0];
                if let Some(color) = run.background {
                    let x = origin.0 + cell.0 * run.start as f32;
                    let end = runs.last().unwrap().end;
                    let width = if usize::from(end) == self.columns {
                        cell.0 * (end - run.start) as f32
                            + (size.0 - cell.0 * self.columns as f32).max(0.)
                    } else {
                        cell.0 * (end - run.start) as f32
                    };
                    let height = if row + 1 == self.row_count {
                        cell.1 + (size.1 - cell.1 * self.row_count as f32).max(0.)
                    } else {
                        cell.1
                    };
                    renderer.rect(x, y, width, height, color.get(), false);
                }
            }
            if let Some(line) = &data.line {
                renderer.line(line, origin.0, y);
            }
            for run in &data.runs {
                let x = origin.0 + cell.0 * run.start as f32;
                let width = cell.0 * (run.end - run.start) as f32;
                if let Some((color, wave)) = run.underline {
                    renderer.rect(
                        x,
                        y + cell.1 - if wave { 4. } else { 2. },
                        width,
                        if wave { 3. } else { 1. },
                        color,
                        wave,
                    );
                }
                if run.strike {
                    renderer.rect(x, y + cell.1 * 0.55, width, 1., run.text.color, false);
                }
            }
        }
        if let Some((column, row, shape, width)) = self.cursor {
            let x = origin.0 + cell.0 * column as f32;
            let y = origin.1 + cell.1 * row as f32;
            let width = cell.0 * width as f32;
            if !focused {
                renderer.rect(x, y, width, 1., self.cursor_color, false);
                renderer.rect(x, y + cell.1 - 1., width, 1., self.cursor_color, false);
                renderer.rect(x, y, 1., cell.1, self.cursor_color, false);
                renderer.rect(x + width - 1., y, 1., cell.1, self.cursor_color, false);
            } else {
                match shape {
                    CursorShape::Beam => renderer.rect(x, y, 2., cell.1, self.cursor_color, false),
                    CursorShape::Underline => {
                        renderer.rect(x, y + cell.1 - 2., width, 2., self.cursor_color, false)
                    }
                    _ => (),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::width::text_width;
    use crate::{
        config::Config,
        core::{Core, WindowSize},
        screen::Update,
        vt::Parser,
    };

    fn frame(core: &Core) -> Frame {
        let mut screen = Screen::default();
        screen.apply(Update::capture(core, true)).unwrap();
        Frame::new(&mut screen, &LineCache::default(), true, false)
    }

    #[test]
    fn dimmed_foreground_becomes_the_background_after_inverse() {
        let mut core = Core::new(
            WindowSize {
                num_cols: 4,
                num_lines: 1,
                cell_width: 8,
                cell_height: 16,
            },
            0xffffff,
            0,
        );
        Parser::new().advance(&mut core, b"\x1b[2;7;38;2;255;0;0;48;2;32;48;64mX");
        let frame = frame(&core);
        let style = &frame.rows[0].runs[0];
        assert_eq!(style.text.color, 0x203040ff);
        assert_eq!(style.background.unwrap().get(), 0xa80000ff);
    }

    #[test]
    fn ui_text_columns_keep_wide_and_combining_text_on_the_terminal_grid() {
        assert_eq!(text_columns("中a\u{301}"), [0, 0, 0, 2, 2, 2]);
        assert_eq!(text_width("中a\u{301}"), 3);
        for text in ["👩‍💻a", "لاا", "\u{17d8}a"] {
            assert!(
                text_columns(text)
                    .iter()
                    .all(|&column| usize::from(column) < text_width(text))
            );
        }
    }

    #[test]
    fn partial_scroll_reuses_shaping_without_moving_undamaged_rows() {
        let mut core = Core::new(
            WindowSize {
                num_cols: 20,
                num_lines: 8,
                cell_width: 8,
                cell_height: 16,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = Parser::new();
        parser.advance(
            &mut core,
            b"\x1b[?25lheader\r\nguard\r\nA\r\nB\r\nC\r\nD\r\nE\r\nfooter\x1b[3;7r\x1b[7;1H",
        );
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true)).unwrap();
        core.reset_damage();
        let initial = Frame::new(&mut screen, &LineCache::default(), true, false);
        let mut cache = LineCache {
            rows: initial
                .rows
                .into_iter()
                .map(|row| CachedRow {
                    text: row.text,
                    columns: row.columns,
                    line: None,
                    runs: row.runs,
                })
                .collect(),
            columns: initial.columns,
            block_cursor: initial.block_cursor,
        };
        parser.advance(&mut core, b"\x1b7\x1b[H#\x1b8\r\nF");
        screen.apply(Update::capture(&core, false)).unwrap();
        let scrolled = Frame::new(&mut screen, &cache, true, false);
        scrolled.align_cache(&mut cache);
        assert_eq!(
            scrolled
                .rows
                .iter()
                .filter(|row| !cache.rows[row.index].matches(row))
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            ["#eader", "F"]
        );
        assert_eq!(cache.rows[1].text, "guard");
        assert_eq!(cache.rows[7].text, "footer");
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
            let mut core = Core::new(
                WindowSize {
                    num_cols: 12,
                    num_lines: 2,
                    cell_width: 10,
                    cell_height: 22,
                },
                config.foreground,
                config.background,
            );
            Parser::new().advance(&mut core, format!("\x1b[?25l{input}").as_bytes());
            let frame = frame(&core);
            assert_eq!(frame.rows[0].text, expected);
            assert_eq!(
                frame.rows[0].runs.last().unwrap().text.end as usize,
                expected.len()
            );
            assert!(
                frame.rows[0].columns.is_empty() || frame.rows[0].columns.len() == expected.len()
            );
        }
    }

    #[test]
    fn blank_tail_keeps_multiline_selection_and_the_cursor_at_the_edge() {
        use crate::core::{Point, SelectionType, Side};
        let mut core = Core::new(
            WindowSize {
                num_cols: 21,
                num_lines: 3,
                cell_width: 8,
                cell_height: 16,
            },
            0xcccccc,
            0x202020,
        );
        let mut parser = Parser::new();
        parser.advance(&mut core, b"\x1b[?25l");
        core.selection_start(
            Point { line: 0, column: 8 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(Point { line: 2, column: 4 }, Side::Right);
        let selected = frame(&core);
        for (row, expected) in selected.rows.iter().zip([(8, 21), (0, 21), (0, 5)]) {
            let runs: Vec<_> = row
                .runs
                .iter()
                .filter(|run| run.background == NonZeroU32::new(0x303030ff))
                .map(|run| (run.start, run.end))
                .collect();
            assert_eq!(runs, [expected]);
        }
        core.clear_selection();
        parser.advance(&mut core, b"\x1b[?25h\x1b[1;21H");
        let edge = frame(&core);
        assert_eq!(edge.rows[0].text.len(), 21);
        assert_eq!(
            (
                edge.rows[0].runs.last().unwrap().start,
                edge.rows[0].runs.last().unwrap().end
            ),
            (20, 21)
        );
    }

    #[test]
    #[cfg(feature = "tui")]
    fn prompt_cells_do_not_gain_selection_background() {
        use crate::core::{Point, SelectionType, Side};
        let config = Config::default();
        let mut core = Core::new(
            WindowSize {
                num_cols: 16,
                num_lines: 2,
                cell_width: 10,
                cell_height: 22,
            },
            config.foreground,
            config.background,
        );
        Parser::new().advance(
            &mut core,
            "\x1b[?25l\x1b]133;A\x07 • \x1b]133;B\x07echo".as_bytes(),
        );
        core.selection_start(
            Point { line: 0, column: 0 },
            Side::Left,
            SelectionType::Simple,
        );
        core.selection_update(Point { line: 0, column: 6 }, Side::Right);
        let frame = frame(&core);
        let selected: Vec<_> = frame.rows[0]
            .runs
            .iter()
            .filter(|run| run.background == NonZeroU32::new(0x303030ff))
            .collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].start, 3);
        assert_eq!(
            &frame.rows[0].text[selected[0].text.start as usize..selected[0].text.end as usize],
            "echo"
        );
    }

    #[test]
    #[cfg(feature = "tui")]
    fn right_prompt_shaping_and_decorations_keep_terminal_columns() {
        let mut core = Core::new(
            WindowSize {
                num_cols: 21,
                num_lines: 2,
                cell_width: 10,
                cell_height: 22,
            },
            0xcccccc,
            0x202020,
        );
        Parser::new().advance(&mut core, "\x1b]133;A\x07 • \x1b]133;B\x07cmd\x1b[1;19H\x1b[44;4m\x1b]133;P;2\x07中a\u{301}\x1b]133;P;0\x07\x1b[0m\x1b[1;4H".as_bytes());
        let frame = frame(&core);
        let row = &frame.rows[0];
        assert_eq!(&row.columns[row.columns.len() - 3..], [20, 20, 20]);
        let run = row.runs.last().unwrap();
        assert_eq!((run.start, run.end), (18, 21));
        assert!(run.background.is_some());
        assert!(run.underline.is_some());
        assert_eq!(row.columns[1], 1);
        assert_eq!(frame.cursor.unwrap().0, 3);
    }
}
