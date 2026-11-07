use crate::core::{Cell, Flags, Row};
use crate::vt::{Color, Rgb};
use crate::width::{fit_label, scalar_width};
use crate::workspace::Rect;
#[cfg(any(feature = "tui", test))]
use std::io::{self, Write};

pub(crate) const BACKGROUND: u32 = 0x202020;
pub(crate) const CHROME: u32 = 0x191919;
pub(crate) const BORDER: u32 = 0x303030;
pub(crate) const INK: u32 = 0xcccccc;
pub(crate) const MUTED: u32 = 0x969696;
pub(crate) const SELECTED: u32 = 0x303830;
pub(crate) const HOVER: u32 = 0x343434;
pub(crate) const ACCENT: u32 = 0xb5d2a2;

pub(crate) fn mix_color(foreground: u32, background: u32, weight: u32) -> u32 {
    [0, 8, 16].into_iter().fold(0, |rgb, shift| {
        rgb | ((((foreground >> shift & 255) * weight
            + (background >> shift & 255) * (255 - weight))
            / 255)
            << shift)
    })
}

pub(crate) fn expand_icon(expanded: bool) -> &'static str {
    if expanded { "" } else { "" }
}

pub(crate) fn diff_colors(foreground: u32) -> (u32, u32) {
    if (foreground >> 16 & 255) + (foreground >> 8 & 255) + (foreground & 255) < 384 {
        (0x35552a, 0xa32e36)
    } else {
        (0xb5d2a2, 0xe87575)
    }
}

pub(crate) fn popup_close(rect: Rect) -> Rect {
    Rect {
        x: rect.x + rect.cols.saturating_sub(5),
        y: rect.y + 1,
        cols: 3,
        rows: 1,
    }
}

pub(crate) fn popup_body(rect: Rect) -> Rect {
    Rect {
        x: rect.x + 2,
        y: rect.y + 3,
        cols: rect.cols.saturating_sub(4),
        rows: rect.rows.saturating_sub(5),
    }
}

pub(crate) struct Grid {
    pub rect: Rect,
    pub rows: Vec<Row>,
    pub flags: Flags,
}

pub(crate) struct TextRun<'a> {
    pub x: u16,
    pub y: u16,
    pub columns: u16,
    pub text: &'a str,
    pub foreground: u32,
    pub background: u32,
    pub flags: Flags,
    pub underline_color: Option<u32>,
}

pub(crate) fn rgb(color: u32) -> Color {
    Color::Spec(Rgb::from_packed(color))
}

fn packed(color: Color) -> u32 {
    let Color::Spec(color) = color else {
        unreachable!()
    };
    color.packed()
}

#[cfg(any(feature = "tui", test))]
pub(crate) fn sgr_rgb(output: &mut Vec<u8>, color: Rgb) {
    let mut bytes = [0; 12];
    let mut end = 0;
    for value in [color.r, color.g, color.b] {
        bytes[end] = b';';
        end += 1;
        if value >= 100 {
            bytes[end] = b'0' + value / 100;
            end += 1;
        }
        if value >= 10 {
            bytes[end] = b'0' + value / 10 % 10;
            end += 1;
        }
        bytes[end] = b'0' + value % 10;
        end += 1;
    }
    output.extend_from_slice(&bytes[..end]);
}

#[cfg(any(feature = "tui", test))]
pub(crate) fn cursor_position(output: &mut impl Write, row: u16, column: u16) -> io::Result<()> {
    let mut bytes = [0; 14];
    bytes[..2].copy_from_slice(b"\x1b[");
    let mut end = 2;
    for (mut value, suffix) in [(row, b';'), (column, b'H')] {
        let start = end;
        loop {
            bytes[end] = b'0' + (value % 10) as u8;
            end += 1;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        bytes[start..end].reverse();
        bytes[end] = suffix;
        end += 1;
    }
    output.write_all(&bytes[..end])
}

impl Grid {
    pub fn new(rect: Rect, foreground: u32, background: u32) -> Self {
        let blank = Cell {
            fg: rgb(foreground),
            bg: rgb(background),
            ..Cell::default()
        };
        Self {
            rect,
            rows: (0..rect.rows)
                .map(|_| Row::new(usize::from(rect.cols), blank))
                .collect(),
            flags: Flags::empty(),
        }
    }

    pub fn popup(rect: Rect, foreground: u32, background: u32, title: &str) -> Self {
        let background = mix_color(foreground, background, 9);
        let border = mix_color(foreground, background, 64);
        let muted = mix_color(foreground, background, 176);
        let mut grid = Self::new(rect, foreground, background);
        let edge = Cell {
            c: '─',
            fg: rgb(border),
            bg: rgb(background),
            ..Cell::default()
        };
        let bottom = grid.rows.len().saturating_sub(1);
        for (index, row) in grid.rows.iter_mut().enumerate() {
            if index == 0 || index == bottom {
                row.cells.fill(edge);
            }
            let (left, right) = if index == bottom {
                ('╰', '╯')
            } else if index == 0 {
                ('╭', '╮')
            } else {
                ('│', '│')
            };
            if let Some((first, rest)) = row.cells.split_first_mut() {
                *first = Cell { c: left, ..edge };
                if let Some(last) = rest.last_mut() {
                    *last = Cell { c: right, ..edge };
                }
            }
            row.used = row.cells.len();
        }
        let close = popup_close(rect);
        let columns = close.x.saturating_sub(rect.x + 3);
        grid.text(
            rect.x + 2,
            rect.y + 1,
            columns,
            &crate::width::fit_middle(title, columns),
            foreground,
            background,
        );
        grid.text(close.x, close.y, close.cols, " × ", muted, background);
        grid
    }

    /// Clears a recycled grid in place, reallocating only when its size changed.
    pub fn reset(&mut self, rect: Rect, foreground: u32, background: u32) {
        if (self.rect.cols, self.rect.rows) != (rect.cols, rect.rows) {
            *self = Self::new(rect, foreground, background);
            return;
        }
        let blank = Cell {
            fg: rgb(foreground),
            bg: rgb(background),
            ..Cell::default()
        };
        self.rect = rect;
        self.flags = Flags::empty();
        for row in &mut self.rows {
            row.cells.fill(blank);
            row.wrapped = false;
            row.used = 0;
            if !row.extras().is_empty() {
                row.clear_all_extras();
            }
        }
    }

    pub fn text(
        &mut self,
        x: u16,
        y: u16,
        columns: u16,
        text: &str,
        foreground: u32,
        background: u32,
    ) {
        if y < self.rect.y || y >= self.rect.y + self.rect.rows || x >= self.rect.x + self.rect.cols
        {
            return;
        }
        let row = &mut self.rows[usize::from(y - self.rect.y)];
        let start = usize::from(x.saturating_sub(self.rect.x));
        let end = (start + usize::from(columns)).min(row.cells.len());
        let blank = Cell {
            c: ' ',
            fg: rgb(foreground),
            bg: rgb(background),
            flags: self.flags,
        };
        if start < end && row.cells[start].flags.contains(Flags::WIDE_CHAR_SPACER) {
            row.cells[start - 1] = blank;
        }
        if end > start
            && end < row.cells.len()
            && row.cells[end - 1].flags.contains(Flags::WIDE_CHAR)
        {
            row.cells[end] = blank;
        }
        if !row.extras().is_empty() {
            row.clear_extras(start, end);
        }
        let text = fit_label(text, (end - start) as u16);
        let mut column = start;
        for c in text.chars() {
            let width = usize::from(scalar_width(c).min(2));
            if width == 0 {
                if column > start {
                    let previous = row.leading_column(column - 1);
                    row.extra_mut(previous).zerowidth.push(c);
                }
                continue;
            }
            row.cells[column] = Cell { c, ..blank };
            if width == 2 {
                row.cells[column].flags.insert(Flags::WIDE_CHAR);
                row.cells[column + 1] = blank.wide_spacer();
            }
            column += width;
        }
        row.cells[column..end].fill(blank);
        row.used = row.used.max(end);
    }

    pub fn recolor(&mut self, rect: Rect, background: u32) {
        let background = rgb(background);
        let start = usize::from(rect.x.saturating_sub(self.rect.x).min(self.rect.cols));
        let end = usize::from(
            (rect.x + rect.cols)
                .saturating_sub(self.rect.x)
                .min(self.rect.cols),
        );
        for y in rect.y.max(self.rect.y)..(rect.y + rect.rows).min(self.rect.y + self.rect.rows) {
            let row = &mut self.rows[usize::from(y - self.rect.y)];
            for cell in &mut row.cells[start..end] {
                cell.bg = background;
            }
        }
    }

    pub fn for_each_run<E>(
        &self,
        previous: Option<&Self>,
        damage: &[bool],
        mut emit: impl FnMut(TextRun<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut text = String::new();
        for (index, row) in self.rows.iter().enumerate().filter(|(index, row)| {
            damage
                .get(usize::from(self.rect.y) + index)
                .copied()
                .unwrap_or(false)
                || previous.is_none_or(|previous| {
                    previous.rect != self.rect
                        || previous.rows.get(*index).is_none_or(|old| {
                            old.cells != row.cells || old.extras() != row.extras()
                        })
                })
        }) {
            text.reserve(row.cells.len().saturating_sub(text.len()));
            let mut cells = row.cells_with_extras(0..row.cells.len()).peekable();
            while let Some(&(start, cell, extra)) = cells.peek() {
                let style = cell.flags - (Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER);
                let underline_color = if style.contains(Flags::UNDERLINE) {
                    extra.and_then(|extra| extra.underline_color)
                } else {
                    None
                };
                text.clear();
                while let Some((_, next, extra)) = cells.next_if(|(_, next, extra)| {
                    next.fg == cell.fg
                        && next.bg == cell.bg
                        && next.flags - (Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) == style
                        && (!style.contains(Flags::UNDERLINE)
                            || extra.and_then(|extra| extra.underline_color) == underline_color)
                }) {
                    if !next.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        text.push(next.c);
                        if let Some(extra) = extra {
                            text.extend(extra.zerowidth.iter());
                        }
                    }
                }
                let end = cells
                    .peek()
                    .map_or(row.cells.len(), |(column, _, _)| *column);
                emit(TextRun {
                    x: self.rect.x + start as u16,
                    y: self.rect.y + index as u16,
                    columns: (end - start) as u16,
                    text: &text,
                    foreground: packed(cell.fg),
                    background: packed(cell.bg),
                    flags: style,
                    underline_color: underline_color.map(packed),
                })?;
            }
        }
        Ok(())
    }

    #[cfg(any(feature = "tui", test))]
    pub fn ansi(
        &self,
        output: &mut Vec<u8>,
        previous: Option<&Self>,
        damage: &[bool],
    ) -> io::Result<()> {
        let mut cursor = None;
        let mut style = None;
        self.for_each_run::<io::Error>(previous, damage, |run| {
            if cursor != Some((run.x, run.y)) {
                cursor_position(output, run.y + 1, run.x + 1)?;
            }
            cursor = Some((run.x + run.columns, run.y));
            let next_style = (
                run.foreground,
                run.background,
                run.flags,
                run.underline_color,
            );
            if style != Some(next_style) {
                style = Some(next_style);
                output.extend_from_slice(b"\x1b[0;38;2");
                sgr_rgb(output, Rgb::from_packed(run.foreground));
                output.extend_from_slice(b";48;2");
                sgr_rgb(output, Rgb::from_packed(run.background));
                output.push(b'm');
                if run.flags.contains(Flags::BOLD) {
                    output.extend_from_slice(b"\x1b[1m");
                }
                if run.flags.contains(Flags::ITALIC) {
                    output.extend_from_slice(b"\x1b[3m");
                }
                if run.flags.contains(Flags::UNDERLINE) {
                    let color = run.underline_color.unwrap_or(run.foreground);
                    output.extend_from_slice(b"\x1b[4;58;2");
                    sgr_rgb(output, Rgb::from_packed(color));
                    output.push(b'm');
                }
            }
            output.extend_from_slice(run.text.as_bytes());
            Ok(())
        })?;
        output.extend_from_slice(b"\x1b[0m");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, WindowSize};

    #[test]
    fn ansi_preserves_styles_and_wide_cells_across_the_right_margin() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 8,
            rows: 3,
        };
        let mut grid = Grid::new(rect, 0xcccccc, 0x202020);
        grid.text(0, 0, 8, "ab中cdef", 0xcccccc, 0x202020);
        grid.text(0, 1, 8, "12345678", 0xcccccc, 0x202020);
        grid.flags = Flags::BOLD;
        grid.text(0, 2, 2, "xy", 0xb5d2a2, 0x303030);
        grid.flags = Flags::UNDERLINE | Flags::ITALIC;
        grid.text(2, 2, 2, "中", 0xb5d2a2, 0x303030);
        grid.text(4, 2, 2, "uv", 0xb5d2a2, 0x303030);
        for column in 2..6 {
            grid.rows[2].extra_mut(column).underline_color =
                Some(rgb(if column < 4 { 0x123456 } else { 0x654321 }));
        }
        let mut bytes = Vec::new();
        grid.ansi(&mut bytes, None, &[]).unwrap();
        let mut core = Core::new(
            WindowSize {
                num_cols: 8,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(&mut core, &bytes);
        for (y, row) in grid.rows.iter().enumerate() {
            for (x, cell) in row.cells.iter().enumerate() {
                if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    assert_eq!(core.row(y).cells[x], *cell, "cell ({x}, {y})");
                    if let Some(color) = row.extra(x).and_then(|extra| extra.underline_color) {
                        assert_eq!(core.row(y).extra(x).unwrap().underline_color, Some(color));
                    }
                }
            }
        }
    }
}
