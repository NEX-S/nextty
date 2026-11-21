use crate::history::{
    BATCH, HOT_ROWS, History, HistoryBudget, HistoryCopy, HistoryCopyRequest, HistoryCursor,
    HistoryPage, HistoryRead, HistorySearch, Reply,
};
use crate::vt::{
    ClearMode, Color, CursorShape, LineClearMode, NamedColor, PALETTE_SIZE, Rgb, StandardCharset,
};
use crate::{base64::decode_utf8, width::scalar_width};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMarker {
    pub id: u64,
    pub line: i32,
    pub complete: bool,
    pub status: Option<i32>,
    pub output: Option<(Point, Point)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandBlock {
    pub id: u64,
    pub complete: bool,
    pub status: Option<i32>,
    pub has_output: bool,
    pub elapsed_ms: Option<u64>,
}

#[derive(Default)]
struct Block {
    id: u64,
    prompt: BlockPoint,
    command: Option<BlockPoint>,
    output: Option<BlockPoint>,
    end: Option<BlockPoint>,
    status: Option<i32>,
    incomplete: bool,
    started: Option<std::time::Instant>,
    finished: Option<std::time::Instant>,
}

impl Block {
    fn is_complete(&self, first: u64) -> bool {
        self.end.is_some()
            && !self.incomplete
            && self.prompt.row >= first
            && self.output.is_none_or(|point| point.row >= first)
    }

    fn points_mut(&mut self) -> impl Iterator<Item = &mut BlockPoint> {
        std::iter::once(&mut self.prompt).chain(
            [&mut self.command, &mut self.output, &mut self.end]
                .into_iter()
                .flatten(),
        )
    }

    fn metadata(&self, first: u64) -> CommandBlock {
        CommandBlock {
            id: self.id,
            complete: self.is_complete(first),
            status: self.status,
            has_output: self.output.is_some(),
            elapsed_ms: self.started.map(|start| {
                self.finished
                    .unwrap_or_else(std::time::Instant::now)
                    .duration_since(start)
                    .as_millis() as u64
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct BlockPoint {
    row: u64,
    column: usize,
}

impl BlockPoint {
    fn at(self, base: u64) -> Point {
        Point {
            line: line_offset(self.row, base),
            column: self.column,
        }
    }
}

/// Line of absolute row `row` in a view whose first screen line is row `base`.
fn line_offset(row: u64, base: u64) -> i32 {
    (i128::from(row) - i128::from(base)).clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
}

type Request = Box<dyn FnOnce(&Core) + Send>;

struct DiskHistory {
    io: History,
    first: u64,
    hot_start: u64,
    // Rows that left the hot area and are not with the writer yet.
    spool: VecDeque<Row>,
    requests: VecDeque<Request>,
    epoch: u64,
    layout_epoch: u64,
    page_start: u64,
    page: Vec<Row>,
    requested: Option<Range<u64>>,
    blank: Row,
    // The archive is being emptied; replies about its old contents are stale.
    clearing: bool,
    // The archive failed and was dropped; rows that leave the hot area are discarded.
    failed: bool,
}

const DEFAULT_PALETTE: [Rgb; 256] = {
    const ANSI: [u32; 16] = [
        0x252932, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, 0x5c6370,
        0xff7a85, 0xb1df90, 0xffd68a, 0x80c7ff, 0xda98ee, 0x70d0dc, 0xf0f3f8,
    ];
    let mut colors = [Rgb { r: 0, g: 0, b: 0 }; 256];
    let mut index = 0;
    while index < 256 {
        colors[index] = if index < 16 {
            Rgb::from_packed(ANSI[index])
        } else if index < 232 {
            let levels = [0, 95, 135, 175, 215, 255];
            let n = index - 16;
            Rgb {
                r: levels[n / 36],
                g: levels[n / 6 % 6],
                b: levels[n % 6],
            }
        } else {
            let c = 8 + (index as u8 - 232) * 10;
            Rgb { r: c, g: c, b: c }
        };
        index += 1;
    }
    colors
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub num_cols: u16,
    pub num_lines: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Point {
    pub line: i32,
    pub column: usize,
}

macro_rules! bitset {
    ($name:ident, $bits:ty) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        #[repr(transparent)]
        pub struct $name($bits);

        impl $name {
            pub const fn bits(self) -> $bits {
                self.0
            }
            pub const fn from_bits(bits: $bits) -> Self {
                Self(bits)
            }
            pub const fn contains(self, other: Self) -> bool {
                self.0 & other.0 == other.0
            }
            pub const fn intersects(self, other: Self) -> bool {
                self.0 & other.0 != 0
            }
            pub fn insert(&mut self, other: Self) {
                self.0 |= other.0;
            }
            pub fn remove(&mut self, other: Self) {
                self.0 &= !other.0;
            }
            pub fn set(&mut self, other: Self, enabled: bool) {
                if enabled {
                    self.insert(other);
                } else {
                    self.remove(other);
                }
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }
        }
    };
}

bitset!(Flags, u16);
bitset!(Mode, u32);

impl Flags {
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const DIM: Self = Self(1 << 2);
    pub const INVERSE: Self = Self(1 << 3);
    pub const UNDERLINE: Self = Self(1 << 4);
    pub const DOUBLE_UNDERLINE: Self = Self(1 << 5);
    pub const UNDERCURL: Self = Self(1 << 6);
    pub const DOTTED_UNDERLINE: Self = Self(1 << 7);
    pub const DASHED_UNDERLINE: Self = Self(1 << 8);
    pub const STRIKEOUT: Self = Self(1 << 9);
    pub const HIDDEN: Self = Self(1 << 10);
    pub const WIDE_CHAR: Self = Self(1 << 11);
    pub const WIDE_CHAR_SPACER: Self = Self(1 << 12);
    pub const PROMPT: Self = Self(1 << 13);
    pub const ALL_UNDERLINES: Self = Self(
        Self::UNDERLINE.0
            | Self::DOUBLE_UNDERLINE.0
            | Self::UNDERCURL.0
            | Self::DOTTED_UNDERLINE.0
            | Self::DASHED_UNDERLINE.0,
    );
    pub const fn empty() -> Self {
        Self(0)
    }
}

impl Mode {
    pub const NONE: Self = Self(0);
    pub const APP_CURSOR: Self = Self(1 << 0);
    pub const FOCUS_IN_OUT: Self = Self(1 << 1);
    pub const BRACKETED_PASTE: Self = Self(1 << 2);
    pub const MOUSE_CLICK: Self = Self(1 << 3);
    pub const MOUSE_DRAG: Self = Self(1 << 4);
    pub const MOUSE_MOTION: Self = Self(1 << 5);
    pub const MOUSE_MODE: Self =
        Self(Self::MOUSE_CLICK.0 | Self::MOUSE_DRAG.0 | Self::MOUSE_MOTION.0);
    pub const SGR_MOUSE: Self = Self(1 << 6);
    pub const ALT_SCREEN: Self = Self(1 << 7);
    pub const ALTERNATE_SCROLL: Self = Self(1 << 8);
    pub const INSERT: Self = Self(1 << 9);
    pub const ORIGIN: Self = Self(1 << 10);
    pub const LINE_WRAP: Self = Self(1 << 11);
    pub const SHOW_CURSOR: Self = Self(1 << 12);
    pub const LINE_FEED_NEW_LINE: Self = Self(1 << 13);
    pub const APP_KEYPAD: Self = Self(1 << 14);
    pub const SYNC_UPDATE: Self = Self(1 << 15);
}

fn ansi_mode(mode: u16) -> Option<Mode> {
    match mode {
        4 => Some(Mode::INSERT),
        20 => Some(Mode::LINE_FEED_NEW_LINE),
        _ => None,
    }
}

fn private_mode(mode: u16) -> Option<Mode> {
    Some(match mode {
        1 => Mode::APP_CURSOR,
        7 => Mode::LINE_WRAP,
        25 => Mode::SHOW_CURSOR,
        1000 => Mode::MOUSE_CLICK,
        1002 => Mode::MOUSE_DRAG,
        1003 => Mode::MOUSE_MOTION,
        1004 => Mode::FOCUS_IN_OUT,
        1006 => Mode::SGR_MOUSE,
        1007 => Mode::ALTERNATE_SCROLL,
        2004 => Mode::BRACKETED_PASTE,
        2026 => Mode::SYNC_UPDATE,
        _ => return None,
    })
}

impl std::ops::BitAnd for Flags {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl std::ops::Sub for Flags {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: Flags,
}

impl Cell {
    pub(crate) fn width(self) -> usize {
        1 + usize::from(self.flags.contains(Flags::WIDE_CHAR))
    }

    pub(crate) fn wide_spacer(self) -> Self {
        Self {
            c: ' ',
            flags: (self.flags - Flags::WIDE_CHAR) | Flags::WIDE_CHAR_SPACER,
            ..self
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    // Four chars fit inline without enlarging the container on 64-bit targets.
    pub zerowidth: smallvec::SmallVec<[char; 4]>,
    pub underline_color: Option<Color>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
    pub(crate) used: usize,
    pub(crate) revision: u64,
    // Column order avoids scanning unrelated extras for cell access and erasure.
    pub(crate) extras: Option<Arc<Vec<(usize, Extra)>>>,
}

impl Row {
    pub fn from_parts(
        cells: Vec<Cell>,
        wrapped: bool,
        used: usize,
        extras: Vec<(usize, Extra)>,
    ) -> Self {
        Self {
            cells,
            wrapped,
            used,
            revision: 0,
            extras: (!extras.is_empty()).then(|| Arc::new(extras)),
        }
    }
    pub fn used(&self) -> usize {
        self.used
    }
    pub(crate) fn leading_column(&self, column: usize) -> usize {
        column
            - usize::from(column > 0 && self.cells[column].flags.contains(Flags::WIDE_CHAR_SPACER))
    }
    pub(crate) fn non_default_len(&self) -> usize {
        self.cells
            .iter()
            .rposition(|cell| *cell != Cell::default())
            .map_or(0, |column| column + 1)
    }
    pub fn extras(&self) -> &[(usize, Extra)] {
        self.extras.as_deref().map_or(&[], Vec::as_slice)
    }

    pub(crate) fn extras_mut(&mut self) -> &mut Vec<(usize, Extra)> {
        Arc::make_mut(self.extras.get_or_insert_default())
    }

    pub(crate) fn clear_all_extras(&mut self) {
        self.extras = None;
    }

    pub(crate) fn cells_with_extras(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (usize, &Cell, Option<&Extra>)> {
        let extras = self.extras();
        let first = if range.start == 0 {
            0
        } else {
            extras.partition_point(|(column, _)| *column < range.start)
        };
        let mut extras = &extras[first..];
        let cells = &self.cells[range.start..];
        range.zip(cells).map(move |(column, cell)| {
            let extra = match extras.split_first() {
                Some(((index, extra), rest)) if *index == column => {
                    extras = rest;
                    Some(extra)
                }
                _ => None,
            };
            (column, cell, extra)
        })
    }

    pub(crate) fn characters(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (usize, char)> {
        self.cells_with_extras(range)
            .filter(|(_, cell, _)| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .flat_map(|(column, cell, extra)| {
                let hidden = cell.flags.contains(Flags::HIDDEN);
                let zerowidth = extra
                    .filter(|_| !hidden)
                    .map_or(&[][..], |extra| extra.zerowidth.as_slice());
                std::iter::once(if hidden { ' ' } else { cell.c })
                    .chain((hidden && cell.flags.contains(Flags::WIDE_CHAR)).then_some(' '))
                    .chain(zerowidth.iter().copied())
                    .map(move |ch| (column, ch))
            })
    }

    /// Text of columns `start..end`: clipped to the used cells and widened back to the lead
    /// cell of a wide character, optionally without prompt cells.
    pub(crate) fn text(
        &self,
        start: usize,
        end: usize,
        skip_prompt: bool,
    ) -> impl Iterator<Item = char> {
        let start = if start < self.cells.len() {
            self.leading_column(start)
        } else {
            start
        };
        let end = end.min(self.used);
        self.characters(start.min(end)..end)
            .filter(move |(column, _)| {
                !skip_prompt || !self.cells[*column].flags.contains(Flags::PROMPT)
            })
            .map(|(_, ch)| ch)
    }

    pub(crate) fn new(columns: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; columns],
            wrapped: false,
            used: 0,
            revision: 0,
            extras: None,
        }
    }

    pub fn extra(&self, column: usize) -> Option<&Extra> {
        let extras = self.extras();
        extras
            .binary_search_by_key(&column, |(column, _)| *column)
            .ok()
            .map(|index| &extras[index].1)
    }

    pub(crate) fn extra_mut(&mut self, column: usize) -> &mut Extra {
        let extras = self.extras_mut();
        let index = extras
            .binary_search_by_key(&column, |(column, _)| *column)
            .unwrap_or_else(|index| {
                extras.insert(index, (column, Extra::default()));
                index
            });
        &mut extras[index].1
    }

    pub(crate) fn clear_extras(&mut self, start: usize, end: usize) {
        let extras = self.extras();
        let first = extras.partition_point(|(column, _)| *column < start);
        let last = extras.partition_point(|(column, _)| *column < end);
        if first != last {
            self.extras_mut().drain(first..last);
        }
    }

    fn clear(&mut self, blank: Cell) {
        if blank.bg == Color::Named(NamedColor::Background) {
            self.cells[..self.used].fill(blank);
            // A wide character can wrap before the last cell without consuming it.
            *self.cells.last_mut().unwrap() = blank;
            self.used = 0;
        } else {
            self.cells.fill(blank);
            self.used = self.cells.len();
        }
        self.wrapped = false;
        self.clear_all_extras();
    }

    fn clear_range(&mut self, mut start: usize, end: usize, blank: Cell) {
        let mut end = end.min(self.cells.len());
        if start >= end {
            return;
        }
        start = self.leading_column(start);
        if self.cells[end - 1].flags.contains(Flags::WIDE_CHAR) && end < self.cells.len() {
            end += 1;
        }
        self.cells[start..end].fill(blank);
        self.clear_extras(start, end);
        if blank.bg != Color::Named(NamedColor::Background) {
            self.used = self.used.max(end);
        } else if end >= self.used {
            self.used = self.used.min(start);
        }
        if end == self.cells.len() {
            self.wrapped = false;
        }
    }

    #[inline(always)]
    fn prepare_write(&mut self, column: usize, width: usize, bg: Color) {
        let blank = Cell {
            bg,
            ..Cell::default()
        };
        let start = self.leading_column(column);
        let mut end = column + width;
        if start != column {
            self.cells[start] = blank;
        }
        if self.cells[end - 1].flags.contains(Flags::WIDE_CHAR) && end < self.cells.len() {
            self.cells[end] = blank;
            end += 1;
        }
        self.clear_extras(start, end);
        self.used = if bg == Color::Named(NamedColor::Background) && end >= self.used {
            column + width
        } else {
            self.used.max(end)
        };
        if end == self.cells.len() {
            self.wrapped = false;
        }
    }

    #[inline(always)]
    fn write(&mut self, column: usize, cell: Cell, underline_color: Option<Color>) {
        let width = cell.width();
        self.prepare_write(column, width, cell.bg);
        self.cells[column] = cell;
        if width == 2 {
            self.cells[column + 1] = cell.wide_spacer();
        }
        if let Some(color) = underline_color {
            self.extra_mut(column).underline_color = Some(color);
        }
    }

    fn repair_wide(&mut self, start: usize, blank: Cell) {
        let end = self.non_default_len();
        if start >= end {
            self.used = self.used.max(end);
            return;
        }
        for x in start..end {
            let flags = self.cells[x].flags;
            let invalid = flags.contains(Flags::WIDE_CHAR)
                && (x + 1 == self.cells.len()
                    || !self.cells[x + 1].flags.contains(Flags::WIDE_CHAR_SPACER))
                || flags.contains(Flags::WIDE_CHAR_SPACER)
                    && (x == 0 || !self.cells[x - 1].flags.contains(Flags::WIDE_CHAR));
            if invalid {
                self.cells[x] = blank;
                self.clear_extras(x, x + 1);
            }
        }
        // An edit can move a colored wrap gap from the last cell into the row.
        if self.used < end {
            self.used = self.used.max(self.non_default_len());
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Scroll {
    Delta(i32),
    Top,
    Bottom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Side {
    Left = 0,
    Right = 1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SelectionType {
    Simple = 0,
    Semantic = 1,
    Lines = 2,
}

fn semantic_group(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if matches!(
        c,
        ',' | '│' | '`' | '|' | ':' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
    ) {
        1
    } else {
        2
    }
}

pub(crate) fn expand_selection<R: std::borrow::Borrow<Row>, E>(
    kind: SelectionType,
    start: &mut (i64, usize),
    end: &mut (i64, usize),
    mut row: impl FnMut(i64) -> Result<Option<R>, E>,
) -> Result<(), E> {
    if kind == SelectionType::Lines {
        while row(start.0 - 1)?.is_some_and(|row| row.borrow().wrapped) {
            start.0 -= 1;
        }
        while row(end.0)?.is_some_and(|row| row.borrow().wrapped) && row(end.0 + 1)?.is_some() {
            end.0 += 1;
        }
        start.1 = 0;
        if let Some(row) = row(end.0)? {
            end.1 = row.borrow().cells.len() - 1;
        }
    } else if kind == SelectionType::Semantic {
        if let Some(mut current) = row(start.0)? {
            start.1 = start.1.min(current.borrow().cells.len() - 1);
            start.1 = current.borrow().leading_column(start.1);
            let class = semantic_group(current.borrow().cells[start.1].c);
            loop {
                let mut previous = *start;
                if previous.1 > 0 {
                    previous.1 -= 1;
                } else if let Some(previous_row) = row(previous.0 - 1)? {
                    if !previous_row.borrow().wrapped {
                        break;
                    }
                    previous.0 -= 1;
                    previous.1 = previous_row.borrow().used().saturating_sub(1);
                    current = previous_row;
                } else {
                    break;
                }
                let current = current.borrow();
                previous.1 = current.leading_column(previous.1);
                if semantic_group(current.cells[previous.1].c) != class {
                    break;
                }
                *start = previous;
            }
        }
        if let Some(mut current) = row(end.0)? {
            end.1 = end.1.min(current.borrow().cells.len() - 1);
            end.1 = current.borrow().leading_column(end.1);
            let class = semantic_group(current.borrow().cells[end.1].c);
            loop {
                let mut next = *end;
                next.1 += current.borrow().cells[end.1].width();
                if current.borrow().wrapped && next.1 >= current.borrow().used() {
                    let Some(next_row) = row(next.0 + 1)? else {
                        break;
                    };
                    if semantic_group(next_row.borrow().cells[0].c) != class {
                        break;
                    }
                    next.0 += 1;
                    next.1 = 0;
                    current = next_row;
                } else if next.1 >= current.borrow().cells.len()
                    || semantic_group(current.borrow().cells[next.1].c) != class
                {
                    break;
                }
                *end = next;
            }
        }
    }
    Ok(())
}

pub(crate) fn object_selection<R: std::borrow::Borrow<Row>>(
    point: (i64, usize),
    row: impl Fn(i64) -> Option<R>,
) -> Option<[(i64, usize); 2]> {
    // Bound work on one user gesture, even for output containing enormous soft-wrapped lines.
    const LIMIT: usize = 4096;
    let anchor_row = row(point.0)?;
    if point.1 >= anchor_row.borrow().used() {
        return None;
    }
    let mut first = point.0;
    let mut before = point.1;
    while before < LIMIT {
        let Some(previous) = row(first - 1) else {
            break;
        };
        let previous = previous.borrow();
        if !previous.wrapped {
            break;
        }
        before += previous.used();
        first -= 1;
    }
    let mut cells = Vec::new();
    let mut anchor = None;
    let mut line = first;
    let right_limited = 'rows: loop {
        let Some(current) = row(line) else {
            break false;
        };
        let current = current.borrow();
        let begin = if line == first {
            before.saturating_sub(LIMIT)
        } else {
            0
        };
        for (column, cell) in current
            .cells
            .iter()
            .enumerate()
            .take(current.used())
            .skip(begin)
        {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if (line, column) == point && !cells.is_empty() {
                    anchor = Some(cells.len() - 1);
                }
                continue;
            }
            if (line, column) == point {
                anchor = Some(cells.len());
            }
            cells.push((cell.c, (line, column)));
            if anchor.is_some_and(|anchor| cells.len() - anchor > LIMIT) {
                break 'rows true;
            }
        }
        if !current.wrapped {
            break false;
        }
        line += 1;
    };
    let anchor = anchor?;
    let mut quote = None;
    let mut escaped = false;
    for (index, &(ch, _)) in cells.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if let Some((delimiter, start)) = quote {
            if ch == delimiter {
                if (start..=index).contains(&anchor) {
                    return Some([cells[start].1, cells[index].1]);
                }
                quote = None;
            }
        } else if matches!(ch, '\'' | '"' | '`')
            && (index == 0 && before < LIMIT
                || index > 0
                    && (cells[index - 1].0.is_whitespace()
                        || matches!(cells[index - 1].0, '=' | '(' | '[' | '{' | ',' | ':')))
        {
            quote = Some((ch, index));
        }
    }
    let boundary = |ch: char| {
        ch.is_whitespace()
            || matches!(ch, ',' | '|' | '"' | '\'' | '`' | '<' | '>')
            || matches!(ch, '，' | '。' | '；' | '！' | '？' | '、' | '：')
            || matches!(ch, '（' | '）' | '【' | '】' | '《' | '》')
            || matches!(ch, '\u{2500}'..='\u{257f}')
    };
    if boundary(cells[anchor].0) {
        return Some([cells[anchor].1; 2]);
    }
    let mut start = anchor;
    let mut end = anchor + 1;
    while start > 0 && !boundary(cells[start - 1].0) {
        start -= 1;
    }
    while end < cells.len() && !boundary(cells[end].0) {
        end += 1;
    }
    if start == 0 && before >= LIMIT || end == cells.len() && right_limited {
        return None;
    }
    if !cells[start..end].iter().any(|&(ch, _)| ch == '/')
        && let Some(register_start) = (start..=anchor).rfind(|&index| cells[index].0 == '%')
    {
        let register_end = register_start
            + 1
            + cells[register_start + 1..end]
                .iter()
                .take_while(|(ch, _)| ch.is_ascii_alphanumeric())
                .count();
        if anchor < register_end {
            return Some([cells[register_start].1, cells[register_end - 1].1]);
        }
    }
    while start < end && matches!(cells[start].0, '(' | '[' | '{' | '=') {
        start += 1;
    }
    if let Some(equal) = (start..end).find(|&index| cells[index].0 == '=')
        && equal > start
        && cells[start..equal]
            .iter()
            .all(|&(ch, _)| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        if anchor > equal {
            start = equal + 1;
        } else {
            end = equal;
        }
    }
    while end > start && matches!(cells[end - 1].0, '.' | ';') {
        end -= 1;
    }
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
        let mut balance = cells[start..end].iter().fold(0i32, |balance, &(ch, _)| {
            balance + i32::from(ch == open) - i32::from(ch == close)
        });
        while end > start && balance < 0 && cells[end - 1].0 == close {
            end -= 1;
            balance += 1;
        }
    }
    if !(start..end).contains(&anchor) {
        return Some([cells[anchor].1; 2]);
    }
    Some([cells[start].1, cells[end - 1].1])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRange {
    pub start: Point,
    pub end: Point,
}
impl SelectionRange {
    pub fn contains(&self, point: Point) -> bool {
        self.start <= point && point <= self.end
    }
}

#[derive(Clone, Copy, Debug)]
struct Selection {
    start: Point,
    start_side: Side,
    end: Point,
    end_side: Side,
    kind: SelectionType,
}

#[derive(Debug)]
pub enum Event {
    Write(Vec<u8>),
    Title(Option<String>),
    ClipboardStore { primary: bool, text: String },

    HistoryError(io::Error),
}

#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    point: Point,
    template: Cell,
    underline_color: Option<Color>,
    charsets: [StandardCharset; 4],
    charset: usize,
    wrap_pending: bool,
    origin: bool,
}

impl Cursor {
    fn logical_point(&self) -> Point {
        Point {
            column: self.point.column + usize::from(self.wrap_pending),
            ..self.point
        }
    }
}

struct Screen {
    rows: VecDeque<Row>,
    cursor: Cursor,
    saved: Cursor,
    offset: usize,
}
impl Screen {
    fn new(columns: usize, lines: usize) -> Self {
        Self {
            rows: vec![Row::new(columns, Cell::default()); lines].into(),
            cursor: Cursor::default(),
            saved: Cursor::default(),
            offset: 0,
        }
    }

    fn clear(&mut self) {
        for row in &mut self.rows {
            row.clear(Cell::default());
        }
        self.cursor = Cursor::default();
        self.saved = Cursor::default();
        self.offset = 0;
    }

    fn resize_alternate(&mut self, columns: usize, lines: usize) {
        for row in &mut self.rows {
            if row.cells.len() != columns {
                row.cells.resize(columns, Cell::default());
                row.used = row.used.min(columns);
                if row
                    .extras()
                    .last()
                    .is_some_and(|(column, _)| *column >= columns)
                {
                    row.extras_mut().retain(|(column, _)| *column < columns);
                }
                row.repair_wide(0, Cell::default());
            }
            row.wrapped = false;
        }
        self.rows.truncate(lines);
        while self.rows.len() < lines {
            self.rows.push_back(Row::new(columns, Cell::default()));
        }
        for cursor in [&mut self.cursor, &mut self.saved] {
            cursor.point.line = cursor.point.line.min(lines as i32 - 1);
            cursor.point.column = cursor.point.column.min(columns - 1);
            cursor.wrap_pending = false;
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SearchAnchor {
    base: u64,
    top: u64,
    alternate: bool,
    columns: usize,
    selection: Option<Selection>,
}

impl SearchAnchor {
    fn selection_at(self, base: u64, columns: usize) -> Option<Selection> {
        if self.columns != columns {
            return None;
        }
        self.selection.map(|mut selection| {
            let delta = line_offset(base, self.base);
            selection.start.line = selection.start.line.saturating_sub(delta);
            selection.end.line = selection.end.line.saturating_sub(delta);
            selection
        })
    }
}

pub struct Core {
    size: WindowSize,
    active: Screen,
    inactive: Screen,
    mode: Mode,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    revision: u64,
    revision_pending: bool,
    full_revision: u64,
    selection: Option<Selection>,
    cursor_shape: CursorShape,
    foreground: u32,
    background: u32,
    colors: [Option<Rgb>; PALETTE_SIZE],
    palette_revision: u64,
    events: Vec<Event>,
    title: Option<String>,
    titles: Vec<Option<Box<str>>>,
    history: Option<DiskHistory>,
    // Rows that left the hot area, kept to be cleared and scrolled in again.
    recycled: VecDeque<Row>,
    blocks: VecDeque<Block>,
    next_block: u64,
    prompt_scope: Flags,
    block_origin: u64,
    directory: Option<PathBuf>,
    input_generation: u64,
    prompt_dirty: bool,
}

impl Core {
    pub fn new(size: WindowSize, foreground: u32, background: u32) -> Self {
        assert!(size.num_cols > 0 && size.num_lines > 0);
        let columns = usize::from(size.num_cols);
        let lines = usize::from(size.num_lines);
        Self {
            size,
            active: Screen::new(columns, lines),
            inactive: Screen::new(columns, lines),
            mode: Mode::LINE_WRAP | Mode::SHOW_CURSOR | Mode::ALTERNATE_SCROLL,
            scroll_top: 0,
            scroll_bottom: lines,
            tabs: (0..columns).map(|x| x != 0 && x % 8 == 0).collect(),
            revision: 0,
            revision_pending: true,
            full_revision: 0,
            selection: None,
            cursor_shape: CursorShape::Block,
            foreground,
            background,
            colors: [None; PALETTE_SIZE],
            palette_revision: 0,
            events: Vec::new(),
            title: None,
            titles: Vec::new(),
            history: None,
            recycled: VecDeque::new(),
            blocks: VecDeque::new(),
            next_block: 1,
            prompt_scope: Flags::empty(),
            block_origin: 0,
            directory: None,
            input_generation: 0,
            prompt_dirty: false,
        }
    }

    pub fn columns(&self) -> usize {
        usize::from(self.size.num_cols)
    }
    pub fn screen_lines(&self) -> usize {
        usize::from(self.size.num_lines)
    }
    fn hot_history_size(&self) -> usize {
        self.active.rows.len().saturating_sub(self.screen_lines())
    }
    pub fn history_size(&self) -> usize {
        if !self.mode.contains(Mode::ALT_SCREEN)
            && let Some(history) = &self.history
        {
            return (history.hot_start - history.first) as usize + self.hot_history_size();
        }
        self.hot_history_size()
    }
    pub fn display_offset(&self) -> usize {
        self.active.offset
    }
    pub fn mode(&self) -> Mode {
        let mut mode = self.mode;
        mode.set(Mode::ORIGIN, self.active.cursor.origin);
        mode
    }
    pub fn row(&self, viewport_row: usize) -> &Row {
        let line = viewport_row as i32 - self.display_offset() as i32;
        self.absolute_row(line)
            .or_else(|| self.history.as_ref().map(|history| &history.blank))
            .expect("visible row outside the screen")
    }

    pub(crate) fn screen_row(&self, line: usize) -> &Row {
        &self.active.rows[self.hot_history_size() + line]
    }
    fn main_screen(&self) -> &Screen {
        if self.mode.contains(Mode::ALT_SCREEN) {
            &self.inactive
        } else {
            &self.active
        }
    }
    pub fn cursor(&self) -> (Point, CursorShape) {
        (
            self.active.cursor.point,
            if self.mode.contains(Mode::SHOW_CURSOR) {
                self.cursor_shape
            } else {
                CursorShape::Hidden
            },
        )
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }
    pub(crate) fn window_size(&self) -> WindowSize {
        self.size
    }
    pub(crate) fn row_versions(&self) -> Vec<u64> {
        (0..self.screen_lines())
            .map(|index| self.row(index).revision)
            .collect()
    }
    pub fn reset_damage(&mut self) {
        self.revision_pending = true;
    }
    fn damage_palette(&mut self) {
        self.palette_revision = self.next_revision();
        self.damage_all();
    }
    pub(crate) fn palette_revision(&self) -> u64 {
        self.palette_revision
    }
    pub(crate) fn full_revision(&self) -> u64 {
        self.full_revision
    }
    #[inline(always)]
    fn next_revision(&mut self) -> u64 {
        if self.revision_pending {
            self.revision += 1;
            self.revision_pending = false;
        }
        self.revision
    }
    #[inline(always)]
    fn damage_all(&mut self) {
        self.full_revision = self.next_revision();
    }
    // Scrolled back, the live screen's last `display_offset` lines are outside the view.
    #[inline(always)]
    fn damage_range(&mut self, start: usize, end: usize) {
        let end = end.min(self.screen_lines().saturating_sub(self.display_offset()));
        if start < end {
            let revision = self.next_revision();
            let base = self.hot_history_size();
            for row in self.active.rows.range_mut(base + start..base + end) {
                row.revision = revision;
            }
        }
    }
    #[inline(always)]
    fn damage_line(&mut self, line: usize) {
        if line + self.display_offset() < self.screen_lines() {
            let revision = self.next_revision();
            self.screen_row_mut(line).revision = revision;
        }
    }
    fn screen_row_mut(&mut self, line: usize) -> &mut Row {
        let index = self.hot_history_size() + line;
        &mut self.active.rows[index]
    }
    fn absolute_row(&self, line: i32) -> Option<&Row> {
        let index = self.hot_history_size() as i64 + i64::from(line);
        if let Ok(index) = usize::try_from(index) {
            return self.active.rows.get(index);
        }

        if !self.mode.contains(Mode::ALT_SCREEN)
            && let Some(history) = &self.history
        {
            let id = history.hot_start.checked_add_signed(index)?;
            return id
                .checked_sub(history.page_start)
                .and_then(|offset| history.page.get(offset as usize));
        }
        None
    }
    fn text_template(&self) -> Cell {
        let mut cell = self.active.cursor.template;

        if !self.mode.contains(Mode::ALT_SCREEN) {
            cell.flags.insert(self.prompt_scope);
        }

        cell
    }
    fn blank(&self) -> Cell {
        Cell {
            bg: self.active.cursor.template.bg,
            ..Cell::default()
        }
    }
    fn move_cursor(&mut self, line: usize, column: usize) {
        let line = line.min(self.screen_lines() - 1) as i32;
        let column = column.min(self.columns() - 1);
        let cursor = &mut self.active.cursor;
        cursor.point = Point { line, column };
        cursor.wrap_pending = false;
    }
    fn shift_selection(&mut self, top: usize, bottom: usize, delta: i32, history: bool) {
        let low = -(self.history_size() as i32);
        if let Some(selection) = &mut self.selection {
            let mut invalid = false;
            for point in [&mut selection.start, &mut selection.end] {
                if (history || point.line >= top as i32) && point.line < bottom as i32 {
                    point.line += delta;
                    invalid |= !history && (point.line < top as i32 || point.line >= bottom as i32);
                }
            }
            if invalid || selection.start.line < low || selection.end.line < low {
                self.selection = None;
            }
        }
    }
    fn scroll_region(&mut self, top: usize, bottom: usize, count: usize, down: bool) {
        let count = count.min(bottom - top);
        let blank = self.blank();
        let history = !down && top == 0 && !self.mode.contains(Mode::ALT_SCREEN);
        for _ in 0..count {
            if history {
                let mut recycled = self
                    .recycled
                    .pop_front()
                    .filter(|row| row.cells.len() == self.columns())
                    .unwrap_or_else(|| Row::new(self.columns(), blank));
                recycled.clear(blank);
                let base = self.hot_history_size();
                self.active.rows.insert(base + bottom, recycled);
                if self.active.offset > 0 {
                    self.active.offset = (self.active.offset + 1).min(self.history_size());
                }
            } else {
                let base = self.hot_history_size();
                let from = if down { bottom - 1 } else { top };
                let to = if down { top } else { bottom - 1 };
                let mut recycled = self.active.rows.remove(base + from).unwrap();
                recycled.clear(blank);
                self.active.rows.insert(base + to, recycled);
            }
        }
        let delta = if down { count as i32 } else { -(count as i32) };
        self.shift_selection(top, bottom, delta, history);

        if history {
            self.block_origin += count as u64;
            self.trim_blocks();
            self.spill_history();
            self.damage_all();
        } else {
            if !self.mode.contains(Mode::ALT_SCREEN) {
                self.shift_blocks(top, bottom, delta);
            }
            self.damage_range(top, bottom);
        }
    }
    pub fn scroll_display(&mut self, scroll: Scroll) -> bool {
        let history = self.history_size();
        let offset = match scroll {
            Scroll::Top => history,
            Scroll::Bottom => 0,
            Scroll::Delta(delta) => self
                .display_offset()
                .saturating_add_signed(delta as isize)
                .min(history),
        };
        self.set_display_offset(offset)
    }

    fn set_display_offset(&mut self, offset: usize) -> bool {
        if offset != self.display_offset() {
            self.active.offset = offset;

            self.request_history_page();
            self.damage_all();
            true
        } else {
            false
        }
    }

    pub fn color(&self, color: Color) -> Rgb {
        let index = match color {
            Color::Spec(rgb) => return rgb,
            Color::Indexed(index) => usize::from(index),
            Color::Named(index) => index as usize,
        };
        self.palette_color(index)
    }

    pub(crate) fn palette_color(&self, index: usize) -> Rgb {
        if let Some(color) = self.colors[index] {
            return color;
        }
        let value = match index {
            0..=255 => return DEFAULT_PALETTE[index],
            257 => self.background,
            _ => self.foreground,
        };
        Rgb::from_packed(value)
    }

    pub fn selection_start(&mut self, point: Point, side: Side, kind: SelectionType) {
        self.selection = Some(Selection {
            start: point,
            end: point,
            start_side: side,
            end_side: side,
            kind,
        });
    }
    pub fn selection_update(&mut self, point: Point, side: Side) -> bool {
        let Some(selection) = &mut self.selection else {
            return false;
        };
        if selection.end == point && selection.end_side == side {
            return false;
        }
        selection.end = point;
        selection.end_side = side;
        true
    }
    pub fn clear_selection(&mut self) -> bool {
        self.selection.take().is_some()
    }
    pub fn selection_range(&self) -> Option<SelectionRange> {
        let selection = self.selection?;
        let mut ends = [
            (selection.start, selection.start_side),
            (selection.end, selection.end_side),
        ];
        ends.sort_by_key(|&(point, side)| (point, side == Side::Right));
        let [(mut start, start_side), (mut end, end_side)] = ends;
        let columns = self.columns();
        let source_row =
            |line: i64| Ok::<_, std::convert::Infallible>(self.absolute_row(line as i32));
        let to_point = |(line, column): (i64, usize)| Point {
            line: line as i32,
            column,
        };
        match selection.kind {
            SelectionType::Simple => {
                if start == end && start_side == end_side {
                    return None;
                }
                if start_side == Side::Right {
                    start.column += 1;
                    if start.column == columns {
                        start.line += 1;
                        start.column = 0;
                    }
                }
                if end_side == Side::Left {
                    if end.column == 0 {
                        end.line -= 1;
                        end.column = columns - 1;
                    } else {
                        end.column -= 1;
                    }
                }
                if end < start {
                    return None;
                }
            }
            SelectionType::Lines | SelectionType::Semantic => {
                let mut first = (i64::from(start.line), start.column);
                let mut last = (i64::from(end.line), end.column);
                expand_selection(selection.kind, &mut first, &mut last, source_row).unwrap();
                start = to_point(first);
                end = to_point(last);
            }
        }
        start.line = start.line.max(-(self.history_size() as i32));
        end.line = end.line.min(self.screen_lines() as i32 - 1);
        start.column = start.column.min(columns - 1);
        end.column = end.column.min(columns - 1);
        if let Some(row) = self.absolute_row(start.line) {
            start.column = row.leading_column(start.column);
        }
        if let Some(row) = self.absolute_row(end.line)
            && row.cells[end.column].flags.contains(Flags::WIDE_CHAR)
            && end.column + 1 < columns
        {
            end.column += 1;
        }
        (start <= end).then_some(SelectionRange { start, end })
    }
    /// `fresh` is mode 1049: save the cursor, enter a cleared screen, restore the cursor on exit.
    fn set_alternate(&mut self, enabled: bool, fresh: bool) {
        if enabled == self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        if enabled && fresh {
            self.active.saved = self.active.cursor;
            self.inactive.clear();
        }
        std::mem::swap(&mut self.active, &mut self.inactive);
        self.mode.set(Mode::ALT_SCREEN, enabled);
        if !enabled && fresh {
            self.active.cursor = self.active.saved;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.screen_lines();
        self.selection = None;
        self.damage_all();
        if !enabled {
            self.request_history_page();
        }
    }

    pub fn resize(&mut self, size: WindowSize) {
        assert!(size.num_cols > 0 && size.num_lines > 0);
        let old_columns = self.columns();
        let old_lines = self.screen_lines();
        let columns = usize::from(size.num_cols);
        let lines = usize::from(size.num_lines);
        if old_columns == columns && old_lines == lines {
            self.size = size;
            return;
        }
        let old_base = self.block_origin;
        let alt = self.mode.contains(Mode::ALT_SCREEN);
        let main_selection = if alt { None } else { self.selection.take() };
        let (main, alternate) = if alt {
            (&mut self.inactive, &mut self.active)
        } else {
            (&mut self.active, &mut self.inactive)
        };
        let old_hot = main.rows.len() - old_lines;
        let hot_start = old_base - old_hot as u64;
        let cold_view_top =
            (main.offset > old_hot).then(|| old_base.saturating_sub(main.offset as u64));
        if cold_view_top.is_some() {
            main.offset = 0;
        }
        let mut points = vec![main.cursor.logical_point(), main.saved.logical_point()];
        if let Some(selection) = main_selection {
            points.extend([selection.start, selection.end]);
        }
        let block_start = points.len();
        let mut block_points = Vec::new();
        for point in self
            .blocks
            .iter_mut()
            .flat_map(Block::points_mut)
            .filter(|point| point.row >= hot_start)
        {
            points.push(point.at(old_base));
            block_points.push(point);
        }
        let retained = self.history.as_ref().map_or(0, |_| HOT_ROWS);
        let mapped = resize_screen(
            main,
            old_columns,
            old_lines,
            columns,
            lines,
            &points,
            retained,
        );
        for (cursor, point) in [&mut main.cursor, &mut main.saved].into_iter().zip(&mapped) {
            cursor.point = Point {
                column: point.column.min(columns - 1),
                ..*point
            };
            cursor.wrap_pending = point.column == columns;
        }
        if let Some(mut selection) = main_selection {
            selection.start = mapped[2];
            selection.end = mapped[3];
            self.selection = Some(selection);
        }
        let new_base = hot_start + main.rows.len() as u64 - lines as u64;
        for (point, mapped) in block_points.into_iter().zip(&mapped[block_start..]) {
            *point = BlockPoint {
                row: (new_base as i64 + i64::from(mapped.line)) as u64,
                column: mapped.column,
            };
        }
        self.block_origin = new_base;
        if let Some(top) = cold_view_top {
            main.offset = new_base.saturating_sub(top) as usize;
        }
        if let Some(history) = &mut self.history {
            history.layout_epoch += 1;
            history.page.clear();
            history.requested = None;
            history.blank = Row::new(columns, Cell::default());
        }
        alternate.resize_alternate(columns, lines);
        if alt {
            self.selection = None;
        }
        self.size = size;
        self.tabs.truncate(columns);
        self.tabs
            .extend((self.tabs.len()..columns).map(|column| column != 0 && column % 8 == 0));
        self.scroll_top = 0;
        self.scroll_bottom = lines;
        // A resized frontend screen is rebuilt and needs the palette again.
        self.damage_palette();
        self.request_history_page();
    }

    #[inline]
    pub(crate) fn write_reply(&mut self, reply: std::fmt::Arguments<'_>) {
        if let Some(Event::Write(previous)) = self.events.last_mut() {
            if let Some(text) = reply.as_str() {
                previous.extend_from_slice(text.as_bytes());
            } else {
                io::Write::write_fmt(previous, reply).unwrap();
            }
        } else {
            self.events
                .push(Event::Write(std::fmt::format(reply).into_bytes()));
        }
    }
    // Mode changes are rare next to text; inlined into the parser loop they enlarge it.
    #[inline(never)]
    pub(crate) fn set_private(&mut self, mode: u16, enabled: bool) {
        match mode {
            6 => {
                self.active.cursor.origin = enabled;
                self.goto(0, 0);
            }
            47 | 1047 => {
                self.set_alternate(enabled, false);
                if mode == 1047 && !enabled {
                    self.inactive.clear();
                }
            }
            1048 if enabled => self.save_cursor_position(),
            1048 => self.restore_cursor_position(),
            1049 => self.set_alternate(enabled, true),
            3 => {
                self.scroll_top = 0;
                self.scroll_bottom = self.screen_lines();
                self.clear_screen(ClearMode::All);
                self.goto(0, 0);
            }
            _ => {
                let Some(flag) = private_mode(mode) else {
                    return;
                };
                if enabled && flag.intersects(Mode::MOUSE_MODE) {
                    self.mode.remove(Mode::MOUSE_MODE);
                }
                self.mode.set(flag, enabled);
            }
        }
    }
}

struct RowReflow {
    output: VecDeque<Row>,
    row: Row,
    column: usize,
    points: Vec<Point>,
    view_top: Option<u64>,
    old_columns: usize,
    source_view_top: u64,
    last_wrapped: bool,
}
impl RowReflow {
    fn new(old_columns: usize, columns: usize, points: usize, view_top: u64) -> Self {
        Self {
            output: VecDeque::new(),
            row: Row::new(columns, Cell::default()),
            column: 0,
            points: vec![Point::default(); points],
            view_top: None,
            old_columns,
            source_view_top: view_top,
            last_wrapped: false,
        }
    }
    fn emit(&mut self, wrapped: bool) {
        let columns = self.row.cells.len();
        let mut row = std::mem::replace(&mut self.row, Row::new(columns, Cell::default()));
        row.wrapped = wrapped;
        self.output.push_back(row);
        self.column = 0;
    }
    fn finish(&mut self) {
        if self.column != 0 || self.last_wrapped {
            self.emit(false);
        }
    }
    /// Records the output position of the anchors that sit at source column `source`.
    fn mark(&mut self, anchors: &[(u64, usize, usize)], limit: usize, source: usize) {
        for &(_, column, index) in anchors {
            if column.min(limit) == source {
                self.points[index] = Point {
                    line: self.output.len() as i32,
                    column: self.column,
                };
            }
        }
    }
    /// Reflows source row `id`; `anchors` are the tracked points in it.
    fn row(&mut self, id: u64, row: Row, anchors: &[(u64, usize, usize)]) {
        let columns = self.row.cells.len();
        if id == self.source_view_top {
            self.view_top = Some(self.output.len() as u64);
        }
        let mut used = row.used();
        if !row.wrapped {
            for (_, column, index) in anchors {
                used = used.max(if *index < 2 && *column == self.old_columns {
                    *column
                } else {
                    column + 1
                });
            }
        }
        used = used.min(self.old_columns);
        let limit = if row.wrapped { row.used } else { used };
        let mut extras = row
            .extras
            .map(Arc::unwrap_or_clone)
            .unwrap_or_default()
            .into_iter()
            .peekable();
        for source in 0..used {
            let cell = row.cells[source];
            if (cell.flags.contains(Flags::WIDE_CHAR) && self.column + 1 == columns && columns > 1)
                || self.column == columns
            {
                self.emit(true);
            }
            self.mark(anchors, limit, source);
            let extra = extras
                .next_if(|(column, _)| *column == source)
                .map(|(_, extra)| extra);
            if columns == 1 && cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            self.row.cells[self.column] = cell;
            if columns == 1 {
                self.row.cells[self.column].flags.remove(Flags::WIDE_CHAR);
            }
            if source < row.used {
                self.row.used = self.column + 1;
            }
            if let Some(extra) = extra {
                self.row.extras_mut().push((self.column, extra));
            }
            self.column += 1;
        }
        self.mark(anchors, limit, used);
        self.last_wrapped = row.wrapped;
        if !row.wrapped {
            self.emit(false);
        }
    }
}

fn resize_screen(
    screen: &mut Screen,
    old_columns: usize,
    old_lines: usize,
    columns: usize,
    lines: usize,
    points: &[Point],
    history_rows: usize,
) -> Vec<Point> {
    let old_history = screen.rows.len() - old_lines;
    let mut anchors: Vec<_> = points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            (
                (old_history as i64 + i64::from(point.line)).max(0) as u64,
                point.column,
                index,
            )
        })
        .collect();
    let old_view_top = (old_history - screen.offset) as u64;
    let rows = std::mem::take(&mut screen.rows);
    let (mut output, mut mapped, mapped_view_top) = if old_columns == columns {
        let mut output = rows;
        let removed = old_lines
            .saturating_sub(lines)
            .min(old_lines - 1 - screen.cursor.point.line as usize);
        output.truncate(output.len() - removed);
        let mapped = anchors
            .iter()
            .map(|&(row, column, _)| Point {
                line: row as i32,
                column,
            })
            .collect();
        (output, mapped, old_view_top)
    } else {
        anchors.sort_unstable();
        let mut flow = RowReflow::new(old_columns, columns, points.len(), old_view_top);
        let mut rest = anchors.as_slice();
        for (id, row) in rows.into_iter().enumerate() {
            let (here, tail) = rest.split_at(rest.partition_point(|anchor| anchor.0 <= id as u64));
            rest = tail;
            flow.row(id as u64, row, here);
        }
        flow.finish();
        (flow.output, flow.points, flow.view_top.unwrap())
    };
    let mapped_view_top = mapped_view_top as usize;
    while old_columns != columns
        && output.len() > lines
        && output.back().is_some_and(|row| row.used == 0)
        && output.len() - 1 > mapped[0].line as usize
    {
        output.pop_back();
    }
    let screen_start = output
        .len()
        .saturating_sub(lines)
        .min(mapped[0].line as usize);
    output.truncate(screen_start + lines);
    while output.len() < lines {
        output.push_back(Row::new(columns, Cell::default()));
    }
    let limit = history_rows + lines;
    let discarded = output.len().saturating_sub(limit);
    output.drain(..discarded);
    let new_history = output.len() - lines;
    for (index, point) in mapped.iter_mut().enumerate() {
        point.line -= (discarded + new_history) as i32;
        point.line = point.line.max(-(new_history as i32)).min(lines as i32 - 1);
        point.column = point
            .column
            .min(if index < 2 { columns } else { columns - 1 });
    }
    mapped[0].line = mapped[0].line.max(0);
    mapped[1].line = mapped[1].line.max(0);
    screen.offset = if screen.offset == 0 {
        0
    } else {
        new_history.saturating_sub(mapped_view_top.saturating_sub(discarded))
    };
    screen.rows = output;
    mapped
}

impl Core {
    fn finish_write(&mut self, end: usize) {
        let columns = self.columns();
        self.active.cursor.wrap_pending = end >= columns;
        self.active.cursor.point.column = end.min(columns - 1);
        self.damage_line(self.active.cursor.point.line as usize);
    }

    #[inline]
    pub(crate) fn input_text(&mut self, mut text: &str) {
        if text.len() == 1
            || self.mode.contains(Mode::INSERT)
            || !matches!(
                self.active.cursor.charsets[self.active.cursor.charset],
                StandardCharset::Ascii
            )
        {
            for c in text.chars() {
                self.input(c);
            }
            return;
        }
        let template = self.text_template();
        let underline_color = self.active.cursor.underline_color;
        while !text.is_empty() {
            if !text.as_bytes()[0].is_ascii() {
                // Avoid probing common narrow Unicode ranges a second time.
                if text.as_bytes()[0] >= 0xe3 && underline_color.is_none() {
                    let length = self.input_wide_text(text, template);
                    if length != 0 {
                        text = &text[length..];
                        continue;
                    }
                }
                let mut end = text.len();
                for (index, c) in text.char_indices() {
                    if c.is_ascii() {
                        end = index;
                        break;
                    }
                    self.input(c);
                }
                text = &text[end..];
                continue;
            }
            if self.active.cursor.wrap_pending {
                self.input(char::from(text.as_bytes()[0]));
                text = &text[1..];
                continue;
            }
            let point = self.active.cursor.point;
            let available = self.columns() - point.column;
            let bytes = &text.as_bytes()[..available.min(text.len())];
            let length = if bytes.is_ascii() {
                bytes.len()
            } else {
                bytes.iter().position(|byte| !byte.is_ascii()).unwrap()
            };
            let end = point.column + length;
            let row = self.screen_row_mut(point.line as usize);
            row.prepare_write(point.column, length, template.bg);
            for (cell, byte) in row.cells[point.column..end].iter_mut().zip(text.as_bytes()) {
                *cell = Cell {
                    c: char::from(*byte),
                    ..template
                };
            }
            if let Some(color) = underline_color {
                for column in point.column..end {
                    row.extra_mut(column).underline_color = Some(color);
                }
            }
            self.finish_write(end);
            text = &text[length..];
        }
    }

    fn input_wide_text(&mut self, text: &str, template: Cell) -> usize {
        let point = self.active.cursor.point;
        let columns = self.columns();
        if self.active.cursor.wrap_pending || columns - point.column < 2 {
            let c = text.chars().next().unwrap();
            self.input(c);
            return c.len_utf8();
        }
        let mut count = 0;
        let mut length = 0;
        for c in text.chars().take((columns - point.column) / 2) {
            if scalar_width(c) < 2 {
                break;
            }
            count += 1;
            length += c.len_utf8();
        }
        if count != 0 {
            let end = point.column + count * 2;
            let row = self.screen_row_mut(point.line as usize);
            row.prepare_write(point.column, count * 2, template.bg);
            for (cells, c) in row.cells[point.column..end]
                .chunks_exact_mut(2)
                .zip(text[..length].chars())
            {
                cells[0] = Cell {
                    c,
                    flags: template.flags | Flags::WIDE_CHAR,
                    ..template
                };
                cells[1] = template.wide_spacer();
            }
            self.finish_write(end);
            return length;
        }
        0
    }

    pub(crate) fn input(&mut self, c: char) {
        let charset = self.active.cursor.charsets[self.active.cursor.charset];
        let c = charset.map(c);
        let width = usize::from(scalar_width(c)).min(self.columns());
        let point = self.active.cursor.point;
        if width == 0 {
            let mut line = point.line as usize;
            let mut column = if self.active.cursor.wrap_pending {
                point.column
            } else if point.column > 0 {
                point.column - 1
            } else if line > 0
                && self
                    .absolute_row(point.line - 1)
                    .is_some_and(|row| row.wrapped)
            {
                line -= 1;
                self.columns() - 1
            } else {
                return;
            };
            let row = self.screen_row_mut(line);
            column = row.leading_column(column);
            row.extra_mut(column).zerowidth.push(c);
            self.damage_line(line);
            return;
        }
        if self.active.cursor.wrap_pending {
            if self.mode.contains(Mode::LINE_WRAP) {
                self.screen_row_mut(point.line as usize).wrapped = true;
                self.damage_line(point.line as usize);
                self.linefeed();
                self.carriage_return();
            } else {
                self.active.cursor.wrap_pending = false;
            }
        }
        let mut point = self.active.cursor.point;
        if width == 2 && point.column + 1 >= self.columns() {
            if self.mode.contains(Mode::LINE_WRAP) && self.columns() > 1 {
                let blank = self.blank();
                let row = self.screen_row_mut(point.line as usize);
                row.clear_range(point.column, point.column + 1, blank);
                row.used = point.column;
                row.wrapped = true;
                self.damage_line(point.line as usize);
                self.linefeed();
                self.carriage_return();
                point = self.active.cursor.point;
            } else {
                return;
            }
        }
        if self.mode.contains(Mode::INSERT) {
            self.shift_chars(width, true);
        }
        let mut cell = self.text_template();
        let underline_color = self.active.cursor.underline_color;
        cell.c = c;
        cell.flags.set(Flags::WIDE_CHAR, width == 2);
        let row = self.screen_row_mut(point.line as usize);
        row.write(point.column, cell, underline_color);
        self.finish_write(point.column + width);
    }

    pub(crate) fn goto(&mut self, line: i32, column: usize) {
        let (top, bottom) = if self.active.cursor.origin {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.screen_lines())
        };
        self.move_cursor((line.max(0) as usize + top).min(bottom - 1), column);
    }
    pub(crate) fn goto_line(&mut self, line: i32) {
        let column = self.active.cursor.point.column;
        self.goto(line, column);
    }
    pub(crate) fn goto_col(&mut self, column: usize) {
        self.move_cursor(self.active.cursor.point.line as usize, column);
    }
    pub(crate) fn move_vertical(&mut self, count: usize, down: bool) {
        let point = self.active.cursor.point;
        let line = point.line as usize;
        let line = if down {
            let bottom = if line < self.scroll_bottom {
                self.scroll_bottom
            } else {
                self.screen_lines()
            };
            line.saturating_add(count).min(bottom - 1)
        } else {
            let top = if line >= self.scroll_top {
                self.scroll_top
            } else {
                0
            };
            line.saturating_sub(count).max(top)
        };
        self.move_cursor(line, point.column);
    }
    pub(crate) fn move_forward(&mut self, count: usize) {
        let point = self.active.cursor.point;
        self.move_cursor(point.line as usize, point.column.saturating_add(count));
    }
    pub(crate) fn move_backward(&mut self, count: usize) {
        let point = self.active.cursor.point;
        self.move_cursor(point.line as usize, point.column.saturating_sub(count));
    }
    pub(crate) fn carriage_return(&mut self) {
        self.goto_col(0);
    }
    pub(crate) fn linefeed(&mut self) {
        let point = self.active.cursor.point;
        if point.line as usize + 1 == self.scroll_bottom {
            self.scroll_region(self.scroll_top, self.scroll_bottom, 1, false);
        } else if point.line as usize + 1 < self.screen_lines() {
            self.active.cursor.point.line += 1;
        }
        self.active.cursor.wrap_pending = false;
    }
    pub(crate) fn newline(&mut self) {
        self.linefeed();
        if self.mode.contains(Mode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
    }
    pub(crate) fn reverse_index(&mut self) {
        let point = self.active.cursor.point;
        if point.line as usize == self.scroll_top {
            self.scroll_region(self.scroll_top, self.scroll_bottom, 1, true);
            self.active.cursor.wrap_pending = false;
        } else {
            self.move_vertical(1, false);
        }
    }
    pub(crate) fn move_tabs(&mut self, count: u16, forward: bool) {
        let column = self.active.cursor.point.column;
        let range = if forward {
            column + 1..self.columns()
        } else {
            0..column
        };
        let mut stops = range.filter(|x| self.tabs[*x]);
        let count = usize::from(count - 1);
        self.goto_col(if forward {
            stops.nth(count).unwrap_or(self.columns() - 1)
        } else {
            stops.nth_back(count).unwrap_or(0)
        });
    }
    pub(crate) fn set_horizontal_tabstop(&mut self) {
        let column = self.active.cursor.point.column;
        self.tabs[column] = true;
    }
    pub(crate) fn clear_tabs(&mut self, all: bool) {
        if all {
            self.tabs.fill(false);
        } else {
            self.tabs[self.active.cursor.point.column] = false;
        }
    }
    pub(crate) fn set_tabs(&mut self, interval: u16) {
        let interval = usize::from(interval);
        for (column, tab) in self.tabs.iter_mut().enumerate() {
            *tab = interval != 0 && column != 0 && column % interval == 0;
        }
    }

    pub(crate) fn shift_chars(&mut self, count: usize, right: bool) {
        let point = self.active.cursor.point;
        let columns = self.columns();
        let count = count.min(columns - point.column);
        let blank = self.blank();
        let row = self.screen_row_mut(point.line as usize);
        let cells = &mut row.cells[point.column..];
        if right {
            cells.copy_within(..cells.len() - count, count);
            cells[..count].fill(blank);
            row.used = (row.used + count).min(columns);
        } else {
            cells.copy_within(count.., 0);
            let end = cells.len();
            cells[end - count..].fill(blank);
            row.used = row.used.saturating_sub(count).max(point.column);
        }
        if row
            .extras()
            .last()
            .is_some_and(|(column, _)| *column >= point.column)
        {
            let extras = row.extras_mut();
            let first = extras.partition_point(|(column, _)| *column < point.column);
            if right {
                for (column, _) in &mut extras[first..] {
                    *column += count;
                }
                let keep = extras.partition_point(|(column, _)| *column < columns);
                extras.truncate(keep);
            } else {
                let last = extras.partition_point(|(column, _)| *column < point.column + count);
                extras.drain(first..last);
                for (column, _) in &mut extras[first..] {
                    *column -= count;
                }
            }
        }
        if blank.bg != Color::Named(NamedColor::Background) {
            row.used = if right {
                row.used.max(point.column + count)
            } else {
                columns
            };
        }
        row.repair_wide(point.column.saturating_sub(1), blank);
        row.wrapped = false;
        self.active.cursor.wrap_pending = false;
        self.damage_line(point.line as usize);
    }
    pub(crate) fn erase_chars(&mut self, count: usize) {
        let point = self.active.cursor.point;
        let end = point.column.saturating_add(count).min(self.columns());
        let blank = self.blank();
        self.screen_row_mut(point.line as usize)
            .clear_range(point.column, end, blank);
        self.active.cursor.wrap_pending = false;
        self.damage_line(point.line as usize);
    }
    pub(crate) fn clear_line(&mut self, mode: LineClearMode) {
        let Point { line, column } = self.active.cursor.point;
        let blank = self.blank();
        let columns = self.columns();
        let row = self.screen_row_mut(line as usize);
        match mode {
            LineClearMode::Left if column + 1 < columns => row.clear_range(0, column + 1, blank),
            LineClearMode::Right if column > 0 => row.clear_range(column, columns, blank),
            _ => row.clear(blank),
        }
        self.active.cursor.wrap_pending = false;
        self.damage_line(line as usize);
    }
    pub(crate) fn clear_screen(&mut self, mode: ClearMode) {
        let line = self.active.cursor.point.line as usize;
        let blank = self.blank();
        let offset = self.display_offset();
        match mode {
            ClearMode::Saved => {
                self.active.rows.drain(..self.hot_history_size());

                if !self.mode.contains(Mode::ALT_SCREEN) {
                    self.clear_disk_history();
                    self.blocks.clear();
                    self.block_origin = 0;
                }
                self.active.rows.shrink_to_fit();
                self.active.offset = 0;
                if self
                    .selection
                    .is_some_and(|selection| selection.start.line < 0 || selection.end.line < 0)
                {
                    self.selection = None;
                }
            }
            ClearMode::All => {
                for y in 0..self.screen_lines() {
                    self.screen_row_mut(y).clear(blank);
                }
            }
            ClearMode::Below => {
                self.clear_line(LineClearMode::Right);
                for y in line + 1..self.screen_lines() {
                    self.screen_row_mut(y).clear(blank);
                }
                self.damage_range(line, self.screen_lines());
                return;
            }
            ClearMode::Above => {
                self.clear_line(LineClearMode::Left);
                for y in 0..line {
                    self.screen_row_mut(y).clear(blank);
                }
                self.damage_range(0, line + 1);
                return;
            }
        }
        self.active.cursor.wrap_pending = false;
        if !matches!(mode, ClearMode::Saved) || offset != 0 {
            self.damage_all();
        }
    }
    pub(crate) fn scroll(&mut self, count: usize, down: bool) {
        self.scroll_region(self.scroll_top, self.scroll_bottom, count, down);
    }
    pub(crate) fn shift_lines(&mut self, count: usize, down: bool) {
        let line = self.active.cursor.point.line as usize;
        if line >= self.scroll_top && line < self.scroll_bottom {
            self.scroll_region(line, self.scroll_bottom, count, down);
            self.carriage_return();
        }
    }
    pub(crate) fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        let top = top.saturating_sub(1);
        let bottom = bottom
            .unwrap_or(self.screen_lines())
            .min(self.screen_lines());
        if top < bottom.saturating_sub(1) {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
            self.goto(0, 0);
        }
    }
    pub(crate) fn save_cursor_position(&mut self) {
        self.active.saved = self.active.cursor;
    }
    pub(crate) fn restore_cursor_position(&mut self) {
        let mut cursor = self.active.saved;
        let (top, bottom) = if cursor.origin {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.screen_lines())
        };
        cursor.point.line = cursor.point.line.clamp(top as i32, bottom as i32 - 1);
        cursor.point.column = cursor.point.column.min(self.columns() - 1);
        self.active.cursor = cursor;
    }
    pub(crate) fn set_active_charset(&mut self, index: usize) {
        self.active.cursor.charset = index;
    }
    pub(crate) fn configure_charset(&mut self, index: usize, charset: StandardCharset) {
        self.active.cursor.charsets[index] = charset;
    }
    pub(crate) fn set_cursor_shape(&mut self, shape: CursorShape) {
        self.cursor_shape = shape;
    }
    pub(crate) fn style_mut(&mut self) -> (&mut Cell, &mut Option<Color>) {
        let cursor = &mut self.active.cursor;
        (&mut cursor.template, &mut cursor.underline_color)
    }
    pub(crate) fn set_mode(&mut self, mode: u16, enabled: bool) {
        if let Some(flag) = ansi_mode(mode) {
            self.mode.set(flag, enabled);
        }
    }
    pub(crate) fn set_keypad_application_mode(&mut self, enabled: bool) {
        self.mode.set(Mode::APP_KEYPAD, enabled);
    }
    pub(crate) fn reset_state(&mut self) {
        let mut events = std::mem::take(&mut self.events);

        let history = self.history.take();
        let revision = self.revision;
        *self = Self::new(self.size, self.foreground, self.background);
        // Frontends compare revisions, so a reset must not repeat an old one.
        self.revision = revision;
        self.damage_palette();
        self.history = history;
        self.clear_disk_history();
        events.push(Event::Title(None));
        self.events = events;
    }
    pub(crate) fn decaln(&mut self) {
        for y in 0..self.screen_lines() {
            let row = self.screen_row_mut(y);
            row.clear(Cell::default());
            for cell in &mut row.cells {
                cell.c = 'E';
            }
            row.used = row.cells.len();
        }
        self.goto(0, 0);
        self.damage_all();
    }
    pub(crate) fn device_status(&mut self, status: usize) {
        match status {
            5 => self.write_reply(format_args!("\x1b[0n")),
            6 => {
                let point = self.active.cursor.point;
                let top = if self.active.cursor.origin {
                    self.scroll_top as i32
                } else {
                    0
                };
                let line = (point.line - top).max(0) as usize;
                self.write_reply(format_args!("\x1b[{};{}R", line + 1, point.column + 1));
            }
            _ => {}
        }
    }
    pub(crate) fn report_mode(&mut self, mode: u16, private: bool) {
        let flag = if private {
            match mode {
                6 => Some(Mode::ORIGIN),
                47 | 1047 | 1049 => Some(Mode::ALT_SCREEN),
                _ => private_mode(mode),
            }
        } else {
            ansi_mode(mode)
        };
        self.write_reply(format_args!(
            "\x1b[{}{};{}$y",
            if private { "?" } else { "" },
            mode,
            flag.map_or(0, |flag| if self.mode().contains(flag) { 1 } else { 2 })
        ));
    }
    pub(crate) fn set_color(&mut self, index: usize, color: Option<Rgb>) {
        if let Some(slot) = self.colors.get_mut(index)
            && *slot != color
        {
            *slot = color;
            self.damage_palette();
        }
    }
    pub(crate) fn dynamic_color_sequence(
        &mut self,
        prefix: impl std::fmt::Display,
        index: usize,
        terminator: &str,
    ) {
        let color = self.palette_color(index);
        self.write_reply(format_args!(
            "\x1b]{prefix};rgb:{r:04x}/{g:04x}/{b:04x}{terminator}",
            r = u16::from_be_bytes([color.r; 2]),
            g = u16::from_be_bytes([color.g; 2]),
            b = u16::from_be_bytes([color.b; 2]),
        ));
    }
    pub(crate) fn clipboard_store(&mut self, primary: bool, data: &[u8]) {
        if let Some(text) = decode_utf8(data) {
            self.events.push(Event::ClipboardStore { primary, text });
        }
    }
    pub(crate) fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        self.write_reply(format_args!(
            "\x1b]52;{};{terminator}",
            char::from(clipboard)
        ));
    }
    pub(crate) fn set_title(&mut self, title: Option<Cow<'_, str>>) {
        if self.title.as_deref() == title.as_deref() {
            return;
        }
        let title = title.map(Cow::into_owned);
        self.title = title.clone();
        self.events.push(Event::Title(title));
    }
    pub(crate) fn push_title(&mut self) {
        if self.titles.len() < 4096 {
            self.titles.push(self.title.as_deref().map(Into::into));
        }
    }
    pub(crate) fn pop_title(&mut self) {
        if let Some(title) = self.titles.pop() {
            self.set_title(title.map(|title| Cow::Owned(title.into_string())));
        }
    }
}

// A view this far outside the cached cold rows loads a new page, so scrolling does not show blanks.
const PAGE_MARGIN: u64 = 256;

impl Core {
    pub fn enable_history(&mut self, budget: Arc<HistoryBudget>) {
        self.history = Some(DiskHistory {
            io: History::new(budget),
            first: 0,
            hot_start: 0,
            spool: VecDeque::new(),
            requests: VecDeque::new(),
            epoch: 0,
            layout_epoch: 0,
            page_start: 0,
            page: Vec::new(),
            requested: None,
            blank: Row::new(self.columns(), Cell::default()),
            clearing: false,
            failed: false,
        });
        self.spill_history();
    }
    pub(crate) fn history_budget(&self) -> Option<Arc<HistoryBudget>> {
        self.history
            .as_ref()
            .map(|history| history.io.budget().clone())
    }
    pub(crate) fn history_read_ready(&self) -> bool {
        self.mode.contains(Mode::ALT_SCREEN)
            || self
                .history
                .as_ref()
                .is_none_or(|history| history.spool.len() < 64)
    }
    fn spill_history(&mut self) {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        let lines = self.screen_lines();
        let Some(history) = &mut self.history else {
            let excess = self.active.rows.len().saturating_sub(lines);
            self.recycled.extend(self.active.rows.drain(..excess));
            return;
        };
        while self.active.rows.len() > lines + HOT_ROWS {
            let row = self.active.rows.pop_front().unwrap();
            history.hot_start += 1;
            if history.failed {
                history.first = history.hot_start;
                self.recycled.push_back(row);
            } else {
                history.spool.push_back(row);
            }
        }
        self.flush_history(false);
    }
    fn flush_history(&mut self, final_batch: bool) {
        let Some(history) = &mut self.history else {
            return;
        };
        if history.clearing || history.failed {
            return;
        }
        while !history.io.busy()
            && (history.spool.len() >= BATCH || final_batch && !history.spool.is_empty())
        {
            let count = history.spool.len().min(BATCH);
            let start = history.hot_start - history.spool.len() as u64;
            history
                .io
                .append(start, history.spool.drain(..count).collect());
        }
    }
    fn request_history_page(&mut self) {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        let lines = self.screen_lines();
        let hot_history = self.hot_history_size();
        let offset = self.display_offset();
        let Some(history) = &mut self.history else {
            return;
        };
        if offset <= hot_history {
            history.requested = None;
            return;
        }
        let top = history
            .hot_start
            .saturating_sub((offset - hot_history) as u64)
            .max(history.first);
        let bottom = (top + lines as u64).min(history.hot_start);
        let load = |margin: u64| {
            (
                top.saturating_sub(margin).max(history.first),
                (bottom + margin).min(history.hot_start),
            )
        };
        let (need_start, need_end) = load(PAGE_MARGIN / 4);
        let covered = |range: &Range<u64>| range.start <= need_start && need_end <= range.end;
        let cached = history.page_start..history.page_start + history.page.len() as u64;
        if covered(&cached) || history.requested.as_ref().is_some_and(covered) {
            return;
        }
        // Rows still in the spool are not on disk yet.
        if history.spool.is_empty() {
            let (start, end) = load(PAGE_MARGIN);
            history.epoch += 1;
            history
                .io
                .page(history.epoch, start, (end - start) as usize);
            history.requested = Some(start..end);
        }
    }
    pub(crate) fn poll_history(&mut self) -> bool {
        let Some(history) = &mut self.history else {
            return false;
        };
        let mut changed = false;
        let mut page_loaded = false;
        let mut failure = None;
        for reply in history.io.poll() {
            match reply {
                Reply::Stored(rows) => self.recycled.extend(rows),
                Reply::First(first) if !history.clearing && !history.failed => {
                    changed |= history.first != first;
                    history.first = first;
                }
                Reply::First(_) => (),
                Reply::Cleared => {
                    history.clearing = false;
                    changed = true;
                }
                Reply::Page { epoch, start, rows } => {
                    if history.requested.is_some() && history.epoch == epoch {
                        history.page_start = start;
                        history.page = rows;
                        history.requested = None;
                        changed = true;
                        page_loaded = true;
                    }
                }
                Reply::Error(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            self.history_failed(error);
            return true;
        }
        if changed {
            let offset = self.active.offset.min(self.history_size());
            if page_loaded || offset != self.active.offset {
                self.damage_all();
                if let Some(history) = &mut self.history {
                    for row in &mut history.page {
                        row.revision = self.revision;
                    }
                }
            }
            self.active.offset = offset;
            self.trim_blocks();
        }
        self.flush_history_requests();
        self.request_history_page();
        changed
    }
    /// A failed archive is dropped. The hot rows stay, so copying and searching still work on
    /// them, and rows that leave the hot area are discarded from now on.
    fn history_failed(&mut self, error: io::Error) {
        let history = self.history.as_mut().unwrap();
        history.io.clear();
        history.failed = true;
        history.clearing = false;
        history.spool.clear();
        history.first = history.hot_start;
        history.page.clear();
        history.requested = None;
        history.layout_epoch += 1;
        let lines = self.screen_lines();
        let main = if self.mode.contains(Mode::ALT_SCREEN) {
            &mut self.inactive
        } else {
            &mut self.active
        };
        main.offset = main.offset.min(main.rows.len() - lines);
        self.trim_blocks();
        self.damage_all();
        self.flush_history_requests();
        self.events.push(Event::HistoryError(io::Error::new(
            error.kind(),
            format!("disk history disabled; retained history is incomplete: {error}"),
        )));
    }
    fn clear_disk_history(&mut self) {
        let Some(history) = &mut self.history else {
            return;
        };
        history.first = 0;
        history.hot_start = 0;
        history.spool.clear();
        history.page.clear();
        history.requested = None;
        history.epoch += 1;
        history.layout_epoch += 1;
        if !history.failed {
            history.clearing = true;
            history.io.clear();
        }
    }
}

impl Core {
    pub fn input_generation(&self) -> u64 {
        self.input_generation
    }

    pub fn note_input(&mut self) {
        self.input_generation += 1;
        self.prompt_dirty = true;
    }

    pub fn shell_prompt(&self) -> Option<u64> {
        if self.mode.contains(Mode::ALT_SCREEN) || self.prompt_dirty {
            return None;
        }
        let block = self.blocks.back()?;
        let command = block.command?;
        (block.output.is_none() && block.end.is_none() && command == self.cursor_block_point())
            .then_some(block.id)
    }

    pub fn shell_editing(&self) -> bool {
        !self.mode.contains(Mode::ALT_SCREEN)
            && self.prompt_dirty
            && self.blocks.back().is_some_and(|block| {
                block.command.is_some() && block.output.is_none() && block.end.is_none()
            })
    }

    pub fn current_directory(&self) -> Option<&Path> {
        self.directory.as_deref()
    }
    pub(crate) fn set_directory_uri(&mut self, uri: &[u8]) {
        use std::os::unix::ffi::OsStringExt;
        let Some(uri) = uri.strip_prefix(b"file://") else {
            return;
        };
        let Some(slash) = uri.iter().position(|byte| *byte == b'/') else {
            return;
        };
        let host = &uri[..slash];
        if !matches!(host, b"" | b"localhost")
            && !std::fs::read("/proc/sys/kernel/hostname")
                .is_ok_and(|name| name.trim_ascii_end() == host)
        {
            return;
        }
        let mut decoded = Vec::with_capacity(uri.len() - slash);
        let mut bytes = uri[slash..].iter().copied();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'%' {
                let mut digit = || bytes.next().and_then(|b| (b as char).to_digit(16));
                let (Some(high), Some(low)) = (digit(), digit()) else {
                    return;
                };
                (high * 16 + low) as u8
            } else {
                byte
            };
            if byte == 0 {
                return;
            }
            decoded.push(byte);
        }
        self.directory = Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)));
    }
    pub(crate) fn shell_marker(&mut self, marker: &[u8], status: Option<&[u8]>) {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        if marker == b"P" {
            match status {
                Some(b"1" | b"2") => {
                    self.prompt_scope = Flags::PROMPT;
                    let point = self.active.cursor.point;
                    let row = self.screen_row_mut(point.line as usize);
                    // Cursor-positioned right prompts own their blank padding too.
                    if row.used < point.column {
                        for cell in &mut row.cells[row.used..point.column] {
                            cell.flags.insert(Flags::PROMPT);
                        }
                        self.damage_line(point.line as usize);
                    }
                }
                Some(b"0") => self.prompt_scope = Flags::empty(),
                _ => {}
            }
            return;
        }
        if !matches!(marker, b"A" | b"B" | b"C" | b"D") {
            return;
        }
        self.prompt_scope = if marker == b"A" {
            Flags::PROMPT
        } else {
            Flags::empty()
        };
        let point = self.cursor_block_point();
        match marker {
            b"A" => {
                // Repainting the same prompt must not grow metadata indefinitely.
                if let Some(block) = self.blocks.back_mut()
                    && block.prompt == point
                    && block.output.is_none()
                {
                    block.command = None;
                    return;
                }
                while self
                    .blocks
                    .back()
                    .is_some_and(|block| block.prompt >= point)
                {
                    self.blocks.pop_back();
                }
                if let Some(block) = self.blocks.back_mut()
                    && block.end.is_none()
                {
                    block.end = Some(point);
                    block.incomplete = true;
                }
                self.blocks.push_back(Block {
                    id: self.next_block,
                    prompt: point,
                    ..Block::default()
                });
                self.prompt_dirty = false;
                self.next_block += 1;
            }
            b"B" => {
                if let Some(block) = self.blocks.back_mut()
                    && block.output.is_none()
                {
                    block.command = Some(point);
                }
            }
            b"C" => {
                if let Some(block) = self.blocks.back_mut()
                    && block.end.is_none()
                {
                    block.started.get_or_insert_with(std::time::Instant::now);
                    block.output = Some(point);
                    self.prompt_dirty = false;
                }
            }
            b"D" => {
                if let Some(block) = self.blocks.back_mut()
                    && block.end.is_none()
                {
                    block.end = Some(point);
                    block.finished = Some(std::time::Instant::now());
                    block.status = status
                        .and_then(|s| std::str::from_utf8(s).ok())
                        .and_then(|s| s.parse().ok());
                }
            }
            _ => {}
        }
        self.trim_blocks();
    }
    fn cursor_block_point(&self) -> BlockPoint {
        let cursor = &self.main_screen().cursor;
        BlockPoint {
            row: self.block_origin + cursor.point.line as u64,
            column: cursor.logical_point().column,
        }
    }
    fn shift_blocks(&mut self, top: usize, bottom: usize, delta: i32) {
        let low = self.block_origin + top as u64;
        let high = self.block_origin + bottom as u64;
        for block in &mut self.blocks {
            let mut escaped = false;
            for point in block
                .points_mut()
                .filter(|point| (low..high).contains(&point.row))
            {
                let moved = point.row.saturating_add_signed(i64::from(delta));
                escaped |= !(low..high).contains(&moved);
                point.row = moved.clamp(low, high - 1);
            }
            block.incomplete |= escaped;
        }
    }
    /// Id of the oldest row still retained.
    fn first_row(&self) -> u64 {
        self.history.as_ref().map_or_else(
            || {
                let hot = self.main_screen().rows.len() - self.screen_lines();
                self.block_origin.saturating_sub(hot as u64)
            },
            |history| history.first,
        )
    }
    fn trim_blocks(&mut self) {
        let first = self.first_row();
        while self
            .blocks
            .front()
            .is_some_and(|block| block.end.is_some_and(|end| end.row < first))
        {
            self.blocks.pop_front();
        }
        // Repeated prompt redraws cannot grow metadata without retained terminal rows.
        let limit = (self.block_origin - first) as usize + self.screen_lines();
        self.blocks.drain(..self.blocks.len().saturating_sub(limit));
    }
    pub(crate) fn latest_command_status(&self) -> Option<Option<i32>> {
        self.blocks
            .iter()
            .rev()
            .find(|block| block.output.is_some())
            .map(|block| block.status)
    }
    pub fn recent_command_blocks(&self) -> Vec<CommandBlock> {
        let first = self.first_row();
        self.blocks
            .range(self.blocks.len().saturating_sub(4)..)
            .map(|block| block.metadata(first))
            .collect()
    }
    pub fn visible_blocks(&self) -> Vec<BlockMarker> {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return Vec::new();
        }
        let top = self
            .block_origin
            .saturating_sub(self.display_offset() as u64);
        let bottom = top + self.screen_lines() as u64;
        let first = self.first_row();
        let begin = self
            .blocks
            .partition_point(|block| block.prompt.row < top)
            .saturating_sub(1);
        self.blocks
            .range(begin..)
            .take_while(|block| block.prompt.row < bottom)
            .filter(|block| block.end.is_none_or(|end| end.row >= top))
            .map(|block| BlockMarker {
                id: block.id,
                line: line_offset(block.prompt.row.max(top), self.block_origin),
                complete: block.is_complete(first),
                status: block.status,
                output: block.output.map(|start| {
                    let end = block.end.unwrap_or_else(|| self.cursor_block_point());
                    (start.at(self.block_origin), end.at(self.block_origin))
                }),
            })
            .collect()
    }
    pub fn prompt_jump(&mut self, previous: bool) -> bool {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return false;
        }
        let top = self
            .block_origin
            .saturating_sub(self.display_offset() as u64);
        let target = if previous {
            self.blocks
                .partition_point(|block| block.prompt.row < top)
                .checked_sub(1)
                .and_then(|index| self.blocks.get(index))
                .map(|block| block.prompt.row)
        } else {
            self.blocks
                .get(self.blocks.partition_point(|block| block.prompt.row <= top))
                .map(|block| block.prompt.row)
        };
        let Some(target) = target else {
            return !previous && self.scroll_display(Scroll::Bottom);
        };
        let offset = self
            .block_origin
            .saturating_sub(target)
            .min(self.history_size() as u64) as usize;
        self.set_display_offset(offset)
    }
    fn search_base(&self) -> u64 {
        if self.mode.contains(Mode::ALT_SCREEN) {
            0
        } else {
            self.history.as_ref().map_or(0, |history| history.hot_start)
                + self.hot_history_size() as u64
        }
    }
    pub(crate) fn search_anchor(&self) -> SearchAnchor {
        let base = self.search_base();
        SearchAnchor {
            base,
            top: base.saturating_sub(self.display_offset() as u64),
            alternate: self.mode.contains(Mode::ALT_SCREEN),
            columns: self.columns(),
            selection: self.selection,
        }
    }
    pub(crate) fn restore_search(&mut self, anchor: SearchAnchor) {
        if anchor.alternate != self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        let base = self.search_base();
        let offset = base
            .saturating_sub(anchor.top)
            .min(self.history_size() as u64) as usize;
        self.set_display_offset(offset);
        self.selection = anchor.selection_at(base, self.columns());
        self.damage_all();
    }
    pub(crate) fn search(
        &mut self,
        query: &str,
        cancelled: &Arc<AtomicBool>,
    ) -> mpsc::Receiver<io::Result<crate::search::SearchResult>> {
        let query = query.to_owned();
        let cancelled = Arc::clone(cancelled);
        self.submit(move |core, sender| {
            let history = core.history.as_ref().unwrap();
            let alternate = core.mode.contains(Mode::ALT_SCREEN);
            history.io.search(
                HistorySearch {
                    query,
                    hot_start: if alternate { 0 } else { history.hot_start },
                    hot: core.active.rows.iter().cloned().collect(),
                    columns: core.columns(),
                    alternate,
                    cancelled,
                },
                sender.clone(),
            );
            Ok(())
        })
    }
    pub(crate) fn search_select(
        &mut self,
        result: &crate::search::SearchResult,
        index: usize,
    ) -> io::Result<()> {
        if result.columns != self.columns()
            || result.alternate != self.mode.contains(Mode::ALT_SCREEN)
        {
            return Err(io::Error::other(
                "pane changed size or screen; refresh the search",
            ));
        }
        let found = result.matches.get(index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "search result no longer exists")
        })?;
        let base = self.search_base();
        let first = base.saturating_sub(self.history_size() as u64);
        if found.start_row < first || found.end_row >= base + self.screen_lines() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "search result was evicted from history",
            ));
        }
        let top = found
            .start_row
            .saturating_sub((self.screen_lines() / 2) as u64);
        let offset = base.saturating_sub(top).min(self.history_size() as u64) as usize;
        self.set_display_offset(offset);
        self.selection_start(
            Point {
                line: line_offset(found.start_row, base),
                column: usize::from(found.start_column),
            },
            Side::Left,
            SelectionType::Simple,
        );
        self.selection_update(
            Point {
                line: line_offset(found.end_row, base),
                column: usize::from(found.end_column.saturating_sub(1)),
            },
            Side::Right,
        );
        self.damage_all();
        Ok(())
    }
    fn block_output(&self, id: u64) -> io::Result<(&Block, BlockPoint, BlockPoint)> {
        let block = self
            .blocks
            .iter()
            .find(|block| block.id == id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "command block no longer retained")
            })?;
        let start = block.output.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command has no output marker")
        })?;
        let end = block.end.unwrap_or_else(|| self.cursor_block_point());
        Ok((block, start, end))
    }
    pub fn copy_block(&mut self, id: u64) -> mpsc::Receiver<io::Result<HistoryCopy>> {
        self.submit(move |core, sender| {
            let (block, start, end) = core.block_output(id)?;
            let history = core.history.as_ref().unwrap();
            let (hot_start, hot) = Self::copy_rows(
                &core.main_screen().rows,
                history.hot_start,
                start.row,
                end.row,
            );
            history.io.copy(
                HistoryCopyRequest {
                    start: (start.row, start.column),
                    end: (end.row, end.column),
                    hot_start,
                    hot,
                    kind: None,
                    incomplete: block.incomplete
                        || block.prompt.row < history.first
                        || start.row < history.first,
                    in_memory: history.failed,
                },
                sender.clone(),
            );
            Ok(())
        })
    }

    pub fn copy_selection(&mut self) -> mpsc::Receiver<io::Result<HistoryCopy>> {
        self.submit(|core, sender| {
            let range = core
                .selection_range()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no selection"))?;
            let history = core.history.as_ref().unwrap();
            let hot_start = if core.mode.contains(Mode::ALT_SCREEN) {
                0
            } else {
                history.hot_start
            };
            let base = hot_start + core.hot_history_size() as u64;
            let start = (
                base.saturating_add_signed(i64::from(range.start.line)),
                range.start.column,
            );
            let end = (
                base.saturating_add_signed(i64::from(range.end.line)),
                range.end.column + 1,
            );
            let (hot_start, hot) = Self::copy_rows(&core.active.rows, hot_start, start.0, end.0);
            history.io.copy(
                HistoryCopyRequest {
                    start,
                    end,
                    hot_start,
                    hot,
                    kind: core.selection.map(|selection| selection.kind),
                    incomplete: false,
                    in_memory: history.failed,
                },
                sender.clone(),
            );
            Ok(())
        })
    }

    pub fn read_history(
        &mut self,
        block: Option<u64>,
        cursor: Option<HistoryCursor>,
        query: Option<&str>,
    ) -> (mpsc::Receiver<io::Result<HistoryPage>>, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let query = query.map(str::to_owned);
        let flag = Arc::clone(&cancelled);
        let receiver = self.submit(move |core, sender| {
            let history = core.history.as_ref().unwrap();
            if cursor.is_some_and(|cursor| cursor.epoch != history.layout_epoch) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "History cursor is stale after terminal resize or clear; read again without a cursor",
                ));
            }
            let main = core.main_screen();
            let (start, end, incomplete, command) = if let Some(id) = block {
                let (block, start, end) = core.block_output(id)?;
                let bounded_end = end.max(start);
                (
                    (start.row, start.column),
                    (bounded_end.row, bounded_end.column),
                    block.incomplete || block.prompt.row < history.first || end < start,
                    Some(block.metadata(history.first)),
                )
            } else {
                let cursor_row = core.cursor_block_point().row;
                let last_used = main
                    .rows
                    .iter()
                    .rposition(|row| row.used() != 0)
                    .map_or(cursor_row, |index| history.hot_start + index as u64);
                (
                    (0, 0),
                    (cursor_row.max(last_used), core.columns()),
                    history.first != 0,
                    None,
                )
            };
            let cursor = cursor.unwrap_or(HistoryCursor {
                epoch: history.layout_epoch,
                row: start.0,
                offset: 0,
            });
            if cursor.row < start.0 || cursor.row > end.0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "History cursor is outside the requested command or history range",
                ));
            }
            let (hot_start, hot) =
                Self::copy_rows(&main.rows, history.hot_start, cursor.row, end.0);
            history.io.read(
                HistoryRead {
                    start,
                    end,
                    hot_start,
                    hot,
                    cursor,
                    query,
                    incomplete,
                    command,
                    cancelled: flag,
                },
                sender.clone(),
            );
            Ok(())
        });
        (receiver, cancelled)
    }
}

impl Core {
    fn copy_rows(rows: &VecDeque<Row>, hot_start: u64, start: u64, end: u64) -> (u64, Vec<Row>) {
        let count = rows.len();
        let index = |id: u64| id.saturating_sub(hot_start).min(count as u64) as usize;
        // Line and word selections may widen over soft-wrapped rows.
        let mut from = index(start);
        while from > 0 && rows[from - 1].wrapped {
            from -= 1;
        }
        // A running command may move its cursor above its output marker.
        let mut to = index(end.saturating_add(1)).max(from).max(1);
        while to < count && rows[to - 1].wrapped {
            to += 1;
        }
        (
            hot_start + from as u64,
            rows.range(from..to).cloned().collect(),
        )
    }

    /// Requests wait while the rows they describe are still moving to disk, or a frame is
    /// half drawn.
    fn history_read_blocked(&mut self) -> bool {
        self.flush_history(true);
        self.history.as_ref().is_some_and(|history| {
            self.mode.contains(Mode::SYNC_UPDATE) || !history.spool.is_empty() || history.clearing
        })
    }

    fn submit<T: Send + 'static>(
        &mut self,
        run: impl FnOnce(&Core, &mpsc::Sender<io::Result<T>>) -> io::Result<()> + Send + 'static,
    ) -> mpsc::Receiver<io::Result<T>> {
        let (sender, receiver) = mpsc::channel();
        let task: Request = Box::new(move |core| {
            if let Err(error) = run(core, &sender) {
                let _ = sender.send(Err(error));
            }
        });
        if self.history_read_blocked() {
            self.history.as_mut().unwrap().requests.push_back(task);
        } else {
            task(self);
        }
        receiver
    }

    /// Submits the requests that waited for a half-drawn frame or for rows on their way to disk.
    pub(crate) fn flush_history_requests(&mut self) {
        while !self.history_read_blocked() {
            let Some(task) = self
                .history
                .as_mut()
                .and_then(|history| history.requests.pop_front())
            else {
                break;
            };
            task(self);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod disk_tests;
