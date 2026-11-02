use crate::core::{Cell, Flags, Row};
use crate::vt::{Color, Rgb};
use crate::width::{fit_label, scalar_width};
use crate::workspace::Rect;
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

pub(crate) struct Grid {
    pub rect: Rect,
    pub rows: Vec<Row>,
    pub flags: Flags,
}

pub(crate) struct TextRun {
    pub x: u16,
    pub y: u16,
    pub columns: u16,
    pub text: String,
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

    /// Clears a recycled grid in place, reallocating only when its size changed.
    #[cfg(feature = "agent")]
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
            row.extras = None;
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
        row.cells[start..end].fill(blank);
        if let Some(extras) = &mut row.extras {
            std::sync::Arc::make_mut(extras).retain(|(column, _)| !(start..end).contains(column));
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
        row.used = row.used.max(end);
    }

    fn edit_region(&mut self, rect: Rect, mut edit: impl FnMut(&mut Row, std::ops::Range<usize>)) {
        let start = usize::from(rect.x.saturating_sub(self.rect.x).min(self.rect.cols));
        let end = usize::from(
            (rect.x + rect.cols)
                .saturating_sub(self.rect.x)
                .min(self.rect.cols),
        );
        for y in rect.y.max(self.rect.y)..(rect.y + rect.rows).min(self.rect.y + self.rect.rows) {
            let row = &mut self.rows[usize::from(y - self.rect.y)];
            edit(row, start..end);
        }
    }

    pub fn recolor(&mut self, rect: Rect, background: u32) {
        let background = rgb(background);
        self.edit_region(rect, |row, columns| {
            for cell in &mut row.cells[columns] {
                cell.bg = background;
            }
        });
    }

    pub fn runs<'a>(
        &'a self,
        previous: Option<&'a Self>,
        damage: &'a [bool],
    ) -> impl Iterator<Item = TextRun> + 'a {
        self.rows
            .iter()
            .enumerate()
            .filter(move |(index, row)| {
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
            })
            .flat_map(move |(index, row)| {
                let mut cells = row.cells_with_extras(0..row.cells.len()).peekable();
                std::iter::from_fn(move || {
                    let &(start, cell, extra) = cells.peek()?;
                    let style = cell.flags - (Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER);
                    let underline_color = if style.contains(Flags::UNDERLINE) {
                        extra.and_then(|extra| extra.underline_color)
                    } else {
                        None
                    };
                    let mut text = String::with_capacity(row.cells.len() - start);
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
                    Some(TextRun {
                        x: self.rect.x + start as u16,
                        y: self.rect.y + index as u16,
                        columns: (end - start) as u16,
                        text,
                        foreground: packed(cell.fg),
                        background: packed(cell.bg),
                        flags: style,
                        underline_color: underline_color.map(packed),
                    })
                })
            })
    }

    pub fn ansi(
        &self,
        output: &mut Vec<u8>,
        previous: Option<&Self>,
        damage: &[bool],
    ) -> io::Result<()> {
        let mut cursor = None;
        let mut style = None;
        for run in self.runs(previous, damage) {
            if cursor != Some((run.x, run.y)) {
                write!(output, "\x1b[{};{}H", run.y + 1, run.x + 1)?;
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
                write!(
                    output,
                    "\x1b[0;38;2;{};{};{};48;2;{};{};{}m",
                    run.foreground >> 16 & 255,
                    run.foreground >> 8 & 255,
                    run.foreground & 255,
                    run.background >> 16 & 255,
                    run.background >> 8 & 255,
                    run.background & 255
                )?;
                if run.flags.contains(Flags::BOLD) {
                    output.extend_from_slice(b"\x1b[1m");
                }
                if run.flags.contains(Flags::ITALIC) {
                    output.extend_from_slice(b"\x1b[3m");
                }
                if run.flags.contains(Flags::UNDERLINE) {
                    let color = run.underline_color.unwrap_or(run.foreground);
                    write!(
                        output,
                        "\x1b[4;58;2;{};{};{}m",
                        color >> 16 & 255,
                        color >> 8 & 255,
                        color & 255
                    )?;
                }
            }
            output.extend_from_slice(run.text.as_bytes());
        }
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
