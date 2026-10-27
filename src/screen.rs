//! Visible grid snapshots shared by desktop and workspace frontends.
#[cfg(feature = "tui")]
use crate::core::{BlockMarker, Flags};
use crate::core::{Core, Mode, Point, Row, SelectionRange};
use crate::vt::{Color, CursorShape, NamedColor, Rgb};

#[derive(Clone, Debug)]
pub struct Screen {
    pub(crate) columns: usize,
    pub(crate) rows: Vec<Row>,
    pub(crate) dirty: Vec<bool>,
    pub(crate) cursor: (Point, CursorShape),
    pub(crate) mode: Mode,
    pub(crate) palette: [Rgb; 269],
    pub(crate) selection: Option<SelectionRange>,
    pub(crate) offset: usize,
    pub(crate) history: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Update {
    pub columns: usize,
    pub lines: usize,
    pub rows: Vec<(usize, Row)>,
    pub cursor: (Point, CursorShape),
    pub mode: Mode,
    pub palette: Option<Box<[Rgb; 269]>>,
    pub selection: Option<SelectionRange>,
    pub offset: usize,
    pub history: usize,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            columns: 0,
            rows: Vec::new(),
            dirty: Vec::new(),
            cursor: (Point::default(), CursorShape::Hidden),
            mode: Mode::NONE,
            palette: [Rgb::default(); 269],
            selection: None,
            offset: 0,
            history: 0,
        }
    }
}

impl Update {
    pub fn capture(core: &Core, full: bool) -> Self {
        let rows: Vec<_> = if full {
            (0..core.screen_lines())
                .map(|index| (index, core.row(index).clone()))
                .collect()
        } else {
            core.dirty_rows()
                .map(|index| (index, core.row(index).clone()))
                .collect()
        };
        // Every Core palette mutation damages all rows. Sparse updates can reuse
        // the frontend's existing palette, without a second palette or revision.
        let palette = (full || rows.len() == core.screen_lines()).then(|| {
            let mut palette = [Rgb::default(); 269];
            for (index, color) in palette.iter_mut().enumerate() {
                *color = if index < 256 {
                    core.color(Color::Indexed(index as u8))
                } else {
                    core.color(Color::Named(NamedColor::from_index(index as u16).unwrap()))
                };
            }
            Box::new(palette)
        });
        Self {
            columns: core.columns(),
            lines: core.screen_lines(),
            rows,
            cursor: core.cursor(),
            mode: core.mode(),
            palette,
            selection: core.selection_range(),
            offset: core.display_offset(),
            history: core.history_size(),
        }
    }
}

impl Screen {
    pub(crate) fn apply(&mut self, update: Update) -> std::io::Result<()> {
        if self.columns != update.columns || self.rows.len() != update.lines {
            if update.palette.is_none()
                || update.rows.len() != update.lines
                || update.rows.iter().enumerate().any(|(i, (n, _))| i != *n)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "resize requires a complete screen and palette",
                ));
            }
            self.rows = update.rows.into_iter().map(|(_, row)| row).collect();
            self.dirty = vec![true; update.lines];
            self.columns = update.columns;
        } else {
            for (index, row) in update.rows {
                self.rows[index] = row;
                self.dirty[index] = true;
            }
        }
        if self.offset != update.offset
            || update
                .palette
                .as_deref()
                .is_some_and(|palette| self.palette != *palette)
            || self.mode.contains(Mode::ALT_SCREEN) != update.mode.contains(Mode::ALT_SCREEN)
        {
            self.dirty.fill(true);
        } else if self.selection != update.selection {
            let span = |selection: Option<SelectionRange>, line| {
                selection.and_then(|selection| {
                    let start = selection.start.max(Point { line, column: 0 });
                    let end = selection.end.min(Point {
                        line,
                        column: self.columns - 1,
                    });
                    (start <= end).then_some((start, end))
                })
            };
            for (row, dirty) in self.dirty.iter_mut().enumerate() {
                let line = row as i32 - self.offset as i32;
                if !*dirty && span(self.selection, line) != span(update.selection, line) {
                    *dirty = true;
                }
            }
        }
        if self.cursor != update.cursor {
            self.damage_cursor();
            self.cursor = update.cursor;
            self.damage_cursor();
        }
        self.mode = update.mode;
        if let Some(palette) = update.palette {
            self.palette = *palette;
        }
        self.selection = update.selection;
        self.offset = update.offset;
        self.history = update.history;
        Ok(())
    }
    #[cfg(feature = "tui")]
    pub fn row_output<'a>(
        &self,
        pane: &'a crate::workspace::PaneView,
        row: usize,
    ) -> Option<&'a crate::workspace::BlockMarker> {
        if pane.blocks.is_empty() || self.mode.contains(Mode::ALT_SCREEN) || row >= self.rows.len()
        {
            return None;
        }
        let line = row as i32 - self.offset as i32;
        let cells = &self.rows[row];
        // Styled cells can extend beyond used; scan that tail only for matching output.
        let mut prompt_cells = cells.cells.iter();
        if prompt_cells
            .by_ref()
            .take(cells.used())
            .any(|cell| cell.flags.contains(Flags::PROMPT))
        {
            return None;
        }
        let block = output_block(&pane.blocks, line, cells.used())?;
        (!prompt_cells.any(|cell| cell.flags.contains(Flags::PROMPT))).then_some(block)
    }
    #[cfg(feature = "tui")]
    pub(crate) fn output_changed(
        &self,
        old: &crate::workspace::PaneView,
        new: &crate::workspace::PaneView,
        row: usize,
    ) -> bool {
        if self.mode.contains(Mode::ALT_SCREEN) || row >= self.rows.len() {
            return false;
        }
        let line = row as i32 - self.offset as i32;
        let cells = &self.rows[row];
        if cells.cells[0].flags.contains(Flags::PROMPT) {
            return false;
        }
        let output = |blocks: &[BlockMarker]| {
            output_block(blocks, line, cells.used()).map(|block| (block.id, block.status))
        };
        output(&old.blocks) != output(&new.blocks)
            && !cells.cells[1..]
                .iter()
                .any(|cell| cell.flags.contains(Flags::PROMPT))
    }
    pub fn columns(&self) -> usize {
        self.columns
    }
    pub fn screen_lines(&self) -> usize {
        self.rows.len()
    }
    pub fn row(&self, index: usize) -> &Row {
        &self.rows[index]
    }
    pub fn cursor(&self) -> (Point, CursorShape) {
        self.cursor
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn display_offset(&self) -> usize {
        self.offset
    }
    pub fn history_size(&self) -> usize {
        self.history
    }
    pub fn selection_range(&self) -> Option<SelectionRange> {
        self.selection
    }
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty
            .iter()
            .enumerate()
            .filter_map(|(i, dirty)| dirty.then_some(i))
    }
    pub fn reset_damage(&mut self) {
        self.dirty.fill(false);
    }
    pub fn damage_cursor(&mut self) {
        if self.cursor.1 == CursorShape::Hidden {
            return;
        }
        let line = self.cursor.0.line + self.offset as i32;
        if let Some(dirty) = usize::try_from(line)
            .ok()
            .and_then(|line| self.dirty.get_mut(line))
        {
            *dirty = true;
        }
    }
    pub fn color(&self, color: Color) -> Rgb {
        match color {
            Color::Spec(rgb) => rgb,
            Color::Indexed(index) => self.palette[index as usize],
            Color::Named(name) => self.palette[name as usize],
        }
    }
}

#[cfg(feature = "tui")]
fn output_block(blocks: &[BlockMarker], line: i32, used: usize) -> Option<&BlockMarker> {
    blocks.iter().find(|block| {
        let Some((start, end)) = block.output else {
            return false;
        };
        start < end
            && line >= start.line
            && (line > start.line || start.column == 0)
            && (line < end.line || (line == end.line && end.column > 0 && used <= end.column))
    })
}

#[test]
fn selection_damage_matches_visible_highlight_changes() {
    for columns in [1, 8] {
        let mut core = Core::new(
            crate::core::WindowSize {
                num_cols: columns,
                num_lines: 4,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        let mut initial = Screen::default();
        initial.apply(Update::capture(&core, true)).unwrap();
        core.reset_damage();
        let last = usize::from(columns - 1);
        let range = |(line, column), (end_line, end_column)| {
            Some(SelectionRange {
                start: Point { line, column },
                end: Point {
                    line: end_line,
                    column: end_column,
                },
            })
        };
        let selections = [
            None,
            range((0, 0), (0, last)),
            range((0, last), (2, 0)),
            range((1, 0), (3, last)),
            range((-4, 0), (-2, last)),
            range((-2, last), (2, 0)),
            range((8, 0), (9, last)),
        ];
        for offset in [0, 2, 8] {
            for old in selections {
                for new in selections {
                    let mut screen = initial.clone();
                    screen.selection = old;
                    screen.offset = offset;
                    screen.reset_damage();
                    screen.dirty[3] = true;
                    let mut update = Update::capture(&core, false);
                    update.selection = new;
                    update.offset = offset;
                    screen.apply(update).unwrap();
                    for row in 0..4 {
                        let changed = (0..usize::from(columns)).any(|column| {
                            let point = Point {
                                line: row as i32 - offset as i32,
                                column,
                            };
                            old.is_some_and(|range| range.contains(point))
                                != new.is_some_and(|range| range.contains(point))
                        });
                        assert_eq!(
                            screen.dirty[row],
                            changed || row == 3,
                            "{columns} columns, offset {offset}, {old:?} -> {new:?}, row {row}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;
    use crate::core::WindowSize;
    use crate::workspace::{PaneView, Rect};
    fn state(source: &[u8]) -> (Screen, PaneView) {
        let mut core = Core::new(
            WindowSize {
                num_cols: 21,
                num_lines: 6,
                cell_width: 0,
                cell_height: 0,
            },
            0xcccccc,
            0x202020,
        );
        crate::vt::Parser::new().advance(&mut core, source);
        let pane = PaneView {
            process: Default::default(),
            id: 1,
            rect: Rect {
                x: 0,
                y: 1,
                cols: 21,
                rows: 6,
            },
            title: String::new(),
            cwd: None,
            blocks: core.visible_blocks(),
            command_blocks: core.recent_command_blocks(),
        };
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true)).unwrap();
        (screen, pane)
    }
    #[test]
    fn hover_covers_only_pure_output_and_half_open_rows() {
        let (screen, pane) = state(b"\x1b]133;A\x07 > \x1b]133;B\x07echo\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07\x1b]133;A\x07 > \x1b]133;B\x07");
        assert_eq!(
            pane.blocks[0].output,
            Some((Point { line: 1, column: 0 }, Point { line: 2, column: 0 }))
        );
        assert!(screen.row_output(&pane, 0).is_none());
        assert!(screen.row_output(&pane, 1).is_some());
        assert!(screen.row_output(&pane, 2).is_none());
        let (screen, pane) = state(b"\x1b]133;A\x07 > \x1b]133;B\x07echo\r\n\x1b]133;C\x07output\x1b]133;D;0\x07\x1b]133;A\x07 > \x1b]133;B\x07");
        assert!(
            screen.row_output(&pane, 1).is_none(),
            "same-line prompt must stay attached to output"
        );
        let (screen, pane) =
            state(b"\x1b]133;A\x07 > \x1b]133;B\x07cmd\x1b]133;C\x07output\x1b]133;D;0\x07");
        assert!(
            screen.row_output(&pane, 0).is_none(),
            "output starting after command is mixed"
        );
    }
    #[test]
    fn output_damage_ignores_prompt_padding_and_unrelated_metadata() {
        let (mut screen, old) = state(
            b"\x1b]133;A\x07 > \x1b]133;B\x07cmd\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07",
        );
        let mut new = old.clone();
        new.blocks[0].status = Some(1);
        assert!(screen.output_changed(&old, &new, 1));
        for column in [0, screen.columns() - 1] {
            screen.rows[1].cells[column].flags.insert(Flags::PROMPT);
            assert!(!screen.output_changed(&old, &new, 1));
            screen.rows[1].cells[column].flags.remove(Flags::PROMPT);
        }
        new.blocks[0].status = old.blocks[0].status;
        new.blocks[0].complete = !old.blocks[0].complete;
        assert!(!screen.output_changed(&old, &new, 1));
    }
    #[test]
    fn running_output_is_hoverable_and_alternate_screen_is_not() {
        let (screen, pane) = state(b"\x1b]133;A\x07 > \x1b]133;B\x07echo\r\n\x1b]133;C\x07running");
        assert!(screen.row_output(&pane, 1).is_some());
        let (screen, pane) =
            state(b"\x1b]133;A\x07 > \x1b]133;B\x07echo\r\n\x1b]133;C\x07out\x1b[?1049hALT");
        assert!(screen.mode().contains(Mode::ALT_SCREEN));
        assert!(screen.row_output(&pane, 0).is_none());
    }
}
