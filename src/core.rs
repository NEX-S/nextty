use crate::history::{
    BATCH, History, HistoryConfig, HistoryCopy, HistoryCursor, HistoryPage, HistoryRead, Reflow,
    Reply,
};
use crate::vt::{
    Attr, ClearMode, Color, CursorShape, LineClearMode, NamedColor, PALETTE_SIZE, Rgb,
    StandardCharset,
};
use crate::{base64::decode_utf8, width::scalar_width};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockMarker {
    pub id: u64,
    pub line: i32,
    pub complete: bool,
    pub status: Option<i32>,
    pub output: Option<(Point, Point)>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    pub id: u64,
    pub complete: bool,
    pub status: Option<i32>,
    pub has_output: bool,
    pub elapsed_ms: Option<u64>,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BlockPoint {
    row: u64,
    column: usize,
}

struct DiskHistory {
    io: History,
    first: u64,
    hot_start: u64,
    spool: VecDeque<Row>,
    epoch: u64,
    layout_epoch: u64,
    page_start: u64,
    page: Vec<Row>,
    requested: Option<u64>,
    blank: Row,
    desired_size: Option<WindowSize>,
    resizing: Option<PendingResize>,
    clear: HistoryClear,
}

#[derive(PartialEq, Eq)]
enum HistoryClear {
    Idle,
    Queued,
    Requested,
}

struct PendingResize {
    size: WindowSize,
    selection: Option<Selection>,
    block_points: Vec<(u64, [Option<usize>; 4])>,
    at_bottom: bool,
}

const HISTORY_LIMIT: usize = 10_000;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowSize {
    pub num_cols: u16,
    pub num_lines: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub line: i32,
    pub column: usize,
}

macro_rules! bitset {
    ($name:ident, $bits:ty, $mask:expr) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        #[repr(transparent)]
        pub struct $name($bits);

        impl $name {
            pub const fn bits(self) -> $bits {
                self.0
            }
            pub const fn from_bits(bits: $bits) -> Option<Self> {
                if bits & !$mask == 0 {
                    Some(Self(bits))
                } else {
                    None
                }
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

bitset!(Flags, u16, 0x7fff);
bitset!(Mode, u32, 0x1ffff);

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
    pub const RIGHT_PROMPT: Self = Self(1 << 14);
    pub const ALL_UNDERLINES: Self = Self(
        Self::UNDERLINE.0
            | Self::DOUBLE_UNDERLINE.0
            | Self::UNDERCURL.0
            | Self::DOTTED_UNDERLINE.0
            | Self::DASHED_UNDERLINE.0,
    );
    #[cfg(any(feature = "gui", feature = "tui"))]
    pub(crate) const TEXT_STYLE: Self = Self(
        Self::BOLD.0
            | Self::ITALIC.0
            | Self::DIM.0
            | Self::INVERSE.0
            | Self::ALL_UNDERLINES.0
            | Self::STRIKEOUT.0,
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
    pub const UTF8_MOUSE: Self = Self(1 << 7);
    pub const ALT_SCREEN: Self = Self(1 << 8);
    pub const ALTERNATE_SCROLL: Self = Self(1 << 9);
    pub const INSERT: Self = Self(1 << 10);
    pub const ORIGIN: Self = Self(1 << 11);
    pub const LINE_WRAP: Self = Self(1 << 12);
    pub const SHOW_CURSOR: Self = Self(1 << 13);
    pub const LINE_FEED_NEW_LINE: Self = Self(1 << 14);
    pub const APP_KEYPAD: Self = Self(1 << 15);
    pub const SYNC_UPDATE: Self = Self(1 << 16);
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
        1005 => Mode::UTF8_MOUSE,
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
    // Column order avoids scanning unrelated extras for cell access and erasure.
    pub(crate) extras: Option<Arc<Vec<(usize, Extra)>>>,
}

impl Row {
    pub fn from_parts(
        cells: Vec<Cell>,
        wrapped: bool,
        used: usize,
        mut extras: Vec<(usize, Extra)>,
    ) -> Option<Self> {
        if cells.is_empty()
            || used > cells.len()
            || extras.iter().any(|(column, _)| *column >= cells.len())
        {
            return None;
        }
        if !extras.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            extras.sort_unstable_by_key(|(column, _)| *column);
            if extras.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                return None;
            }
        }
        Some(Self {
            cells,
            wrapped,
            used,
            extras: (!extras.is_empty()).then(|| Arc::new(extras)),
        })
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
        if let Some(extras) = self.extras.as_mut().and_then(Arc::get_mut) {
            extras.clear();
        } else {
            self.extras = None;
        }
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

    pub(crate) fn new(columns: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; columns],
            wrapped: false,
            used: 0,
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
        let index = match extras.last() {
            Some((last, _)) if *last == column => extras.len() - 1,
            Some((last, _)) if *last > column => {
                match extras.binary_search_by_key(&column, |(column, _)| *column) {
                    Ok(index) => index,
                    Err(index) => {
                        return &mut extras.insert_mut(index, (column, Extra::default())).1;
                    }
                }
            }
            _ => return &mut extras.push_mut((column, Extra::default())).1,
        };
        &mut extras[index].1
    }

    pub(crate) fn clear_extras(&mut self, start: usize, end: usize) {
        let extras = self.extras();
        if extras.last().is_none_or(|(column, _)| *column < start) {
            return;
        }
        let first = extras.partition_point(|(column, _)| *column < start);
        let last = extras.partition_point(|(column, _)| *column < end);
        if first != last {
            if first == 0 && last == extras.len() {
                self.clear_all_extras();
            } else {
                self.extras_mut().drain(first..last);
            }
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

#[derive(Clone, Copy, Debug)]
pub enum Scroll {
    Delta(i32),
    Top,
    Bottom,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Left = 0,
    Right = 1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SelectionType {
    Simple = 0,
    Semantic = 1,
    Lines = 2,
    Object = 3,
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

pub(crate) fn object_selection<R: std::borrow::Borrow<Row>, E>(
    point: (i64, usize),
    mut row: impl FnMut(i64) -> Result<Option<R>, E>,
) -> Result<Option<[(i64, usize); 2]>, E> {
    // Bound work on one user gesture, even for output containing enormous soft-wrapped lines.
    const LIMIT: usize = 4096;
    let Some(anchor_row) = row(point.0)? else {
        return Ok(None);
    };
    if point.1 >= anchor_row.borrow().used() {
        return Ok(None);
    }
    let mut first = point.0;
    let mut before = point.1;
    while before < LIMIT {
        let Some(previous) = row(first - 1)? else {
            break;
        };
        let previous = previous.borrow();
        if !previous.wrapped {
            break;
        }
        before += previous.used();
        first -= 1;
    }
    // Allow for wide glyphs without shifting Vec's power-of-two growth steps.
    let capacity = anchor_row
        .borrow()
        .used()
        .min(LIMIT * 2)
        .div_ceil(2)
        .next_power_of_two();
    let mut cells = Vec::with_capacity(capacity);
    let mut anchor = None;
    let mut line = first;
    let right_limited = 'rows: loop {
        let Some(current) = row(line)? else {
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
    let Some(anchor) = anchor else {
        return Ok(None);
    };
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
                    return Ok(Some([cells[start].1, cells[index].1]));
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
        return Ok(Some([cells[anchor].1; 2]));
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
        return Ok(None);
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
            return Ok(Some([cells[register_start].1, cells[register_end - 1].1]));
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
        return Ok(Some([cells[anchor].1; 2]));
    }
    Ok(Some([cells[start].1, cells[end - 1].1]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

pub struct Core {
    size: WindowSize,
    active: Screen,
    inactive: Screen,
    mode: Mode,
    scroll_top: usize,
    scroll_bottom: usize,
    tabs: Vec<bool>,
    dirty: Vec<bool>,
    selection: Option<Selection>,
    cursor_shape: CursorShape,
    foreground: u32,
    background: u32,
    colors: [Option<Rgb>; PALETTE_SIZE],
    // Cleared with row damage; scrolled frames damage every row without a new palette.
    palette_dirty: bool,
    events: Vec<Event>,
    title: Option<String>,
    titles: Vec<Option<Box<str>>>,

    history: Option<DiskHistory>,

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
            dirty: vec![true; lines],
            selection: None,
            cursor_shape: CursorShape::Block,
            foreground,
            background,
            colors: [None; PALETTE_SIZE],
            palette_dirty: true,
            events: Vec::new(),
            title: None,
            titles: Vec::new(),

            history: None,

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
        self.active.rows.len() - self.screen_lines()
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
        if let Some(row) = self.absolute_row(line) {
            return row;
        }

        if let Some(history) = &self.history {
            return &history.blank;
        }
        unreachable!("visible row outside the screen")
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
    pub fn dirty_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.dirty
            .iter()
            .enumerate()
            .filter_map(|(y, dirty)| dirty.then_some(y))
    }
    pub fn reset_damage(&mut self) {
        self.dirty.fill(false);
        self.palette_dirty = false;
    }
    pub(crate) fn palette_dirty(&self) -> bool {
        self.palette_dirty
    }
    fn damage_all(&mut self) {
        self.dirty.fill(true);
    }
    fn damage_range(&mut self, start: usize, end: usize) {
        let offset = self.display_offset();
        let start = (start + offset).min(self.dirty.len());
        let end = (end + offset).min(self.dirty.len());
        self.dirty[start..end].fill(true);
    }
    fn damage_line(&mut self, line: usize) {
        let viewport = line + self.display_offset();
        if let Some(dirty) = self.dirty.get_mut(viewport) {
            *dirty = true;
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
                let disk = self.history.is_some();

                let mut recycled = if !disk && self.hot_history_size() == HISTORY_LIMIT {
                    self.active.rows.pop_front().unwrap()
                } else {
                    Row::new(self.columns(), blank)
                };
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
            259..=266 | 268 => {
                let c = self.palette_color(if index == 268 {
                    NamedColor::Foreground as usize
                } else {
                    index - 259
                });
                return Rgb {
                    r: c.r / 2,
                    g: c.g / 2,
                    b: c.b / 2,
                };
            }
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
        // Pending reflow must not restore a selection cleared by newer input.

        if let Some(pending) = self
            .history
            .as_mut()
            .and_then(|history| history.resizing.as_mut())
        {
            pending.selection = None;
        }
        self.selection.take().is_some()
    }
    pub fn selection_range(&self) -> Option<SelectionRange> {
        let selection = self.selection?;
        let ((mut start, start_side), (mut end, end_side)) = if selection.start < selection.end
            || selection.start == selection.end && selection.start_side == Side::Left
        {
            (
                (selection.start, selection.start_side),
                (selection.end, selection.end_side),
            )
        } else {
            (
                (selection.end, selection.end_side),
                (selection.start, selection.start_side),
            )
        };
        let columns = self.columns();
        let source_row =
            |line: i64| Ok::<_, std::convert::Infallible>(self.absolute_row(line as i32));
        let to_point = |(line, column): (i64, usize)| Point {
            line: line as i32,
            column,
        };
        match selection.kind {
            SelectionType::Object => {
                let object = |point: Point| {
                    object_selection((i64::from(point.line), point.column), source_row).unwrap()
                };
                let first_object = object(start);
                let last_object = if start == end {
                    first_object
                } else {
                    object(end)
                };
                if let Some([first, _]) = first_object {
                    start = to_point(first);
                }
                if let Some([_, last]) = last_object {
                    end = to_point(last);
                }
            }
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
    pub fn selection_to_string(&self) -> Option<String> {
        let range = self.selection_range()?;
        let mut text = String::new();
        for line in range.start.line..=range.end.line {
            let row = self.absolute_row(line)?;
            let start = if line == range.start.line {
                range.start.column
            } else {
                0
            };
            let end = if line == range.end.line {
                range.end.column + 1
            } else {
                self.columns()
            }
            .min(row.used);
            text.extend(
                row.characters(start..end)
                    .filter(|(column, _)| !row.cells[*column].flags.contains(Flags::PROMPT))
                    .map(|(_, ch)| ch),
            );
            if line != range.end.line && !row.wrapped {
                text.push('\n');
            }
        }
        Some(text)
    }

    fn set_alternate(&mut self, enabled: bool, clear: bool, save: bool) {
        if enabled == self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        if enabled && save {
            self.active.saved = self.active.cursor;
        }
        if enabled && clear {
            self.inactive.clear();
        }
        std::mem::swap(&mut self.active, &mut self.inactive);
        self.mode.set(Mode::ALT_SCREEN, enabled);
        if !enabled && save {
            self.active.cursor = self.active.saved;
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.screen_lines();
        self.selection = None;
        self.damage_all();
    }

    pub fn resize(&mut self, size: WindowSize) {
        if let Some(history) = &mut self.history {
            history.desired_size = Some(size);
            self.start_history_resize();
            return;
        }
        assert!(size.num_cols > 0 && size.num_lines > 0);
        let old_columns = self.columns();
        let old_lines = self.screen_lines();
        let columns = usize::from(size.num_cols);
        let lines = usize::from(size.num_lines);
        if old_columns == columns && old_lines == lines {
            self.size = size;
            return;
        }
        let main_selection = if !self.mode.contains(Mode::ALT_SCREEN) {
            self.selection.take()
        } else {
            None
        };
        let (main, alternate) = if self.mode.contains(Mode::ALT_SCREEN) {
            (&mut self.inactive, &mut self.active)
        } else {
            (&mut self.active, &mut self.inactive)
        };
        let mut points = vec![main.cursor.logical_point(), main.saved.logical_point()];
        if let Some(selection) = main_selection {
            points.extend([selection.start, selection.end]);
        }
        let mapped = resize_screen(main, old_columns, old_lines, columns, lines, &points);
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
        alternate.resize_alternate(columns, lines);
        if self.mode.contains(Mode::ALT_SCREEN) {
            self.selection = None;
        }
        self.finish_resize(size);
    }

    fn finish_resize(&mut self, size: WindowSize) {
        let columns = usize::from(size.num_cols);
        let lines = usize::from(size.num_lines);
        self.size = size;
        self.tabs.truncate(columns);
        self.tabs
            .extend((self.tabs.len()..columns).map(|column| column != 0 && column % 8 == 0));
        self.scroll_top = 0;
        self.scroll_bottom = lines;
        self.dirty.clear();
        self.dirty.resize(lines, true);
        // A resized frontend screen is rebuilt and needs the palette again.
        self.palette_dirty = true;
    }

    #[inline]
    fn write_reply(&mut self, reply: std::fmt::Arguments<'_>) {
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
                return;
            }
            47 | 1047 => {
                self.set_alternate(enabled, false, false);
                if mode == 1047 && !enabled {
                    self.inactive.clear();
                }
                return;
            }
            1048 => {
                if enabled {
                    self.save_cursor_position();
                } else {
                    self.restore_cursor_position();
                }
                return;
            }
            1049 => {
                self.set_alternate(enabled, enabled, true);
                return;
            }
            3 => {
                self.scroll_top = 0;
                self.scroll_bottom = self.screen_lines();
                self.clear_screen(ClearMode::All);
                self.goto(0, 0);
                return;
            }
            _ => {}
        }
        let Some(flag) = private_mode(mode) else {
            return;
        };
        if enabled && flag.intersects(Mode::MOUSE_MODE) {
            self.mode.remove(Mode::MOUSE_MODE);
        }
        self.mode.set(flag, enabled);
    }
}

pub(crate) struct RowReflow {
    row: Row,
    column: usize,
    pub(crate) count: u64,
    pub(crate) points: Vec<Option<(u64, usize)>>,
    pub(crate) view_top: Option<u64>,
    old_columns: usize,
    source_view_top: u64,
    last_wrapped: bool,
}
impl RowReflow {
    pub(crate) fn new(old_columns: usize, columns: usize, points: usize, view_top: u64) -> Self {
        Self {
            row: Row::new(columns, Cell::default()),
            column: 0,
            count: 0,
            points: vec![None; points],
            view_top: None,
            old_columns,
            source_view_top: view_top,
            last_wrapped: false,
        }
    }
    fn emit(&mut self, wrapped: bool) -> Row {
        let columns = self.row.cells.len();
        let mut row = std::mem::replace(&mut self.row, Row::new(columns, Cell::default()));
        row.wrapped = wrapped;
        self.column = 0;
        self.count += 1;
        row
    }
    pub(crate) fn finish(&mut self) -> Option<Row> {
        (self.column != 0 || self.last_wrapped).then(|| self.emit(false))
    }
    pub(crate) fn row<E>(
        &mut self,
        id: u64,
        row: Row,
        anchors: &[(u64, usize, usize)],
        anchor: &mut usize,
        emit: &mut impl FnMut(Row) -> Result<(), E>,
    ) -> Result<Vec<Cell>, E> {
        let columns = self.row.cells.len();
        if id == self.source_view_top {
            self.view_top = Some(self.count);
        }
        while anchors.get(*anchor).is_some_and(|(line, _, _)| *line < id) {
            *anchor += 1;
        }
        let begin = *anchor;
        while anchors.get(*anchor).is_some_and(|(line, _, _)| *line == id) {
            *anchor += 1;
        }
        let anchors = &anchors[begin..*anchor];
        if self.old_columns == columns {
            for (_, column, index) in anchors {
                self.points[*index] = Some((self.count, *column));
            }
            self.count += 1;
            emit(row)?;
            return Ok(Vec::new());
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
                emit(self.emit(true))?;
            }
            for (_, column, index) in anchors {
                if (*column).min(if row.wrapped { row.used } else { used }) == source {
                    self.points[*index] = Some((self.count, self.column));
                }
            }
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
        for (_, column, index) in anchors {
            if (*column).min(if row.wrapped { row.used } else { used }) == used {
                self.points[*index] = Some((self.count, self.column));
            }
        }
        self.last_wrapped = row.wrapped;
        if !row.wrapped {
            emit(self.emit(false))?;
        }
        Ok(row.cells)
    }
}

fn resize_screen(
    screen: &mut Screen,
    old_columns: usize,
    old_lines: usize,
    columns: usize,
    lines: usize,
    points: &[Point],
) -> Vec<Point> {
    let old_history = screen.rows.len() - old_lines;
    let anchors = points.iter().enumerate().map(|(index, point)| {
        (
            (old_history as i64 + i64::from(point.line)).max(0) as u64,
            point.column,
            index,
        )
    });
    let old_view_top = screen.rows.len() - old_lines - screen.offset;
    let mut flow = RowReflow::new(old_columns, columns, points.len(), old_view_top as u64);
    let rows = std::mem::take(&mut screen.rows);
    let mut output;
    if old_columns == columns {
        output = rows;
        let removed = old_lines
            .saturating_sub(lines)
            .min(old_lines - 1 - screen.cursor.point.line as usize);
        output.truncate(output.len() - removed);
        flow.view_top = Some(old_view_top as u64);
        for (row, column, index) in anchors {
            flow.points[index] = Some((row, column));
        }
    } else {
        let mut anchors: Vec<_> = anchors.collect();
        anchors.sort_unstable();
        output = VecDeque::new();
        let mut anchor = 0;
        for (id, row) in rows.into_iter().enumerate() {
            flow.row(id as u64, row, &anchors, &mut anchor, &mut |row| {
                output.push_back(row);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        }
        if let Some(row) = flow.finish() {
            output.push_back(row);
        }
    }
    let mut mapped: Vec<_> = flow
        .points
        .into_iter()
        .map(|point| {
            let (line, column) = point.unwrap();
            Point {
                line: line as i32,
                column,
            }
        })
        .collect();
    let mapped_view_top = flow.view_top.unwrap() as usize;
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
    let limit = HISTORY_LIMIT + lines;
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
                let extras = row.extras_mut();
                let insert = extras.partition_point(|(column, _)| *column < point.column);
                extras.splice(
                    insert..insert,
                    (point.column..end).map(|column| {
                        (
                            column,
                            Extra {
                                underline_color: Some(color),
                                ..Extra::default()
                            },
                        )
                    }),
                );
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

    // Keep the REP buffer out of the shared CSI dispatch path.
    #[inline(never)]
    pub(crate) fn repeat_ascii(&mut self, byte: u8, count: usize) {
        let bytes = [byte; 64];
        let text = std::str::from_utf8(&bytes).unwrap();
        for start in (0..count).step_by(text.len()) {
            self.input_text(&text[..(count - start).min(text.len())]);
        }
    }

    pub(crate) fn input(&mut self, c: char) {
        let charset = self.active.cursor.charsets[self.active.cursor.charset];
        let c = charset.map(c);
        let width = usize::from(scalar_width(c)).min(2).min(self.columns());
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
        let mut column = self.active.cursor.point.column;
        if count != 0 {
            let range = if forward {
                column + 1..self.columns()
            } else {
                0..column
            };
            let mut stops = range.filter(|x| self.tabs[*x]);
            let count = usize::from(count - 1);
            column = if forward {
                stops.nth(count).unwrap_or(self.columns() - 1)
            } else {
                stops.nth_back(count).unwrap_or(0)
            };
        }
        self.goto_col(column);
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
    pub(crate) fn terminal_attribute(&mut self, attr: Attr) {
        let cursor = &mut self.active.cursor;
        let flags = &mut cursor.template.flags;
        match attr {
            Attr::Reset => {
                cursor.template = Cell::default();
                cursor.underline_color = None;
            }
            Attr::Flags(value, enabled) => flags.set(value, enabled),
            Attr::Underline(value) => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(value);
            }
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::UnderlineColor(color) => cursor.underline_color = color,
        }
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
        *self = Self::new(self.size, self.foreground, self.background);

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
    pub(crate) fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => self.write_reply(format_args!("\x1b[?6c")),
            Some('>') => self.write_reply(format_args!("\x1b[>0;1;0c")),
            Some('=') => self.write_reply(format_args!("\x1bP!|00000000\x1b\\")),
            _ => {}
        }
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
            if !self.palette_dirty {
                self.palette_dirty = true;
                self.damage_all();
            }
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
    pub(crate) fn text_area_size_pixels(&mut self) {
        self.write_reply(format_args!(
            "\x1b[4;{};{}t",
            u32::from(self.size.num_lines) * u32::from(self.size.cell_height),
            u32::from(self.size.num_cols) * u32::from(self.size.cell_width)
        ));
    }
    pub(crate) fn text_area_size_chars(&mut self) {
        let size = self.size;
        self.write_reply(format_args!("\x1b[8;{};{}t", size.num_lines, size.num_cols));
    }
    pub(crate) fn report_keyboard_mode(&mut self) {
        self.write_reply(format_args!("\x1b[?0u"));
    }
    pub(crate) fn report_modify_other_keys(&mut self) {
        self.write_reply(format_args!("\x1b[>4;0m"));
    }
}

impl Core {
    pub fn enable_history(&mut self, config: HistoryConfig) -> io::Result<()> {
        self.history = Some(DiskHistory {
            io: History::new(config)?,
            first: 0,
            hot_start: 0,
            spool: VecDeque::new(),
            epoch: 0,
            layout_epoch: 0,
            page_start: 0,
            page: Vec::new(),
            requested: None,
            blank: Row::new(self.columns(), Cell::default()),
            desired_size: None,
            resizing: None,
            clear: HistoryClear::Idle,
        });
        self.spill_history();
        Ok(())
    }
    pub fn history_event_fd(&self) -> Option<std::os::fd::RawFd> {
        self.history.as_ref().map(|history| history.io.event_fd())
    }
    pub(crate) fn resize_pending(&self) -> bool {
        self.history
            .as_ref()
            .is_some_and(|history| history.desired_size.is_some() || history.resizing.is_some())
    }
    pub(crate) fn history_read_ready(&self) -> bool {
        self.history.as_ref().is_none_or(|history| {
            history.spool.len() < 64 && history.resizing.is_none() && history.desired_size.is_none()
        })
    }
    fn spill_history(&mut self) {
        if self.mode.contains(Mode::ALT_SCREEN) {
            return;
        }
        let lines = self.screen_lines();
        if let Some(history) = &mut self.history {
            while self.active.rows.len() > lines + history.io.hot_rows {
                history
                    .spool
                    .push_back(self.active.rows.pop_front().unwrap());
                history.hot_start += 1;
            }
        }
        self.flush_history(false);
    }
    fn flush_history(&mut self, final_batch: bool) {
        let Some(history) = &mut self.history else {
            return;
        };
        if history.clear != HistoryClear::Idle {
            if history.clear == HistoryClear::Queued {
                match history.io.clear() {
                    Ok(()) => history.clear = HistoryClear::Requested,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => (),
                    Err(error) => self.events.push(Event::HistoryError(error)),
                }
            }
            return;
        }
        while history.spool.len() >= BATCH || (final_batch && !history.spool.is_empty()) {
            let count = history.spool.len().min(BATCH);
            let start = history.hot_start - history.spool.len() as u64;
            let rows = history.spool.drain(..count).collect();
            if let Err(rows) = history.io.append(start, rows) {
                for row in rows.into_iter().rev() {
                    history.spool.push_front(row);
                }
                break;
            }
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
        if history.resizing.is_some() {
            return;
        }
        if offset <= hot_history {
            history.requested = None;
            return;
        }
        let start = history
            .hot_start
            .saturating_sub((offset - hot_history) as u64)
            .max(history.first);
        let count = lines.min((history.hot_start - start) as usize);
        if history.page_start == start && history.page.len() >= count {
            return;
        }
        if history.requested == Some(start) {
            return;
        }
        if history.spool.is_empty() && history.io.page(history.epoch + 1, start, count) {
            history.epoch += 1;
            history.requested = Some(start);
        }
    }
    pub(crate) fn poll_history(&mut self, read_notification: bool) -> io::Result<bool> {
        let mut changed = false;
        let mut page_loaded = false;
        let mut notified = false;
        let mut reflow = None;
        let mut failure = None;
        if let Some(history) = &mut self.history {
            notified = read_notification && history.io.drain()?;
            history.io.check_budget();
            for reply in history.io.poll() {
                match reply {
                    Reply::Stored { first } => {
                        if history.clear != HistoryClear::Idle {
                            continue;
                        }
                        changed |= history.first != first;
                        history.first = first;
                    }
                    Reply::Cleared => {
                        history.clear = HistoryClear::Idle;
                        changed = true;
                    }
                    Reply::Page {
                        epoch,
                        start,
                        first,
                        rows,
                    } => {
                        if history.requested.is_some() && history.epoch == epoch {
                            history.first = history.first.max(first);
                            history.page_start = start;
                            history.page = rows;
                            history.requested = None;
                            changed = true;
                            page_loaded = true;
                        }
                    }
                    Reply::Reflow {
                        rows,
                        first,
                        hot_start,
                        points,
                        view_top,
                    } => reflow = Some((rows, first, hot_start, points, view_top)),
                    Reply::Error(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        if let Some(error) = failure {
            let history = self.history.take().unwrap();
            let size = history
                .desired_size
                .or(history.resizing.map(|pending| pending.size));
            let lines = self.screen_lines();
            let main = if self.mode.contains(Mode::ALT_SCREEN) {
                &mut self.inactive
            } else {
                &mut self.active
            };
            for row in history.spool.into_iter().rev() {
                main.rows.push_front(row);
            }
            main.rows
                .drain(..main.rows.len().saturating_sub(HISTORY_LIMIT + lines));
            main.offset = main.offset.min(main.rows.len() - lines);
            self.selection = None;
            history.io.shutdown();
            self.trim_blocks();
            if let Some(size) = size {
                self.resize(size);
            }
            self.damage_all();
            self.events.push(Event::HistoryError(io::Error::new(
                error.kind(),
                format!("disk history disabled; retained history is incomplete: {error}"),
            )));
            return Ok(true);
        }
        if let Some((rows, first, hot_start, points, view_top)) = reflow {
            self.finish_history_resize(rows, first, hot_start, points, view_top);
            changed = true;
        }
        if changed {
            let offset = self.active.offset.min(self.history_size());
            if page_loaded || offset != self.active.offset {
                self.damage_all();
            }
            self.active.offset = offset;
            if self
                .history
                .as_ref()
                .is_none_or(|history| history.resizing.is_none())
            {
                self.trim_blocks();
            }
        }
        self.flush_history(true);
        self.start_history_resize();
        self.request_history_page();
        Ok(changed || notified)
    }
    fn start_history_resize(&mut self) {
        let Some(history) = &self.history else { return };
        if history.resizing.is_some()
            || history.clear != HistoryClear::Idle
            || !history.spool.is_empty()
        {
            return;
        }
        let Some(size) = history.desired_size else {
            return;
        };
        if size.num_cols == self.size.num_cols && size.num_lines == self.size.num_lines {
            self.size = size;
            self.history.as_mut().unwrap().desired_size = None;
            return;
        }
        let main = self.main_screen();
        let base = history.hot_start + main.rows.len() as u64 - self.screen_lines() as u64;
        let absolute = |point: Point| {
            (
                base.saturating_add_signed(i64::from(point.line)),
                point.column,
            )
        };
        let mut points = vec![
            absolute(main.cursor.logical_point()),
            absolute(main.saved.logical_point()),
        ];
        let selection = if self.mode.contains(Mode::ALT_SCREEN) {
            None
        } else {
            self.selection
        };
        if let Some(selection) = selection {
            points.extend([absolute(selection.start), absolute(selection.end)]);
        }
        let block_points = self
            .blocks
            .iter()
            .map(|block| {
                let mut indices = [None; 4];
                for (index, point) in [Some(block.prompt), block.command, block.output, block.end]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(point) = point {
                        indices[index] = Some(points.len());
                        points.push((point.row, point.column));
                    }
                }
                (block.id, indices)
            })
            .collect();
        let request = Reflow {
            hot_start: history.hot_start,
            hot: main.rows.iter().cloned().collect(),
            points,
            old_columns: self.columns(),
            old_lines: self.screen_lines(),
            columns: usize::from(size.num_cols),
            lines: usize::from(size.num_lines),
            view_top: base.saturating_sub(main.offset as u64),
            keep_rows: history.io.hot_rows + usize::from(size.num_lines),
        };
        let at_bottom = main.offset == 0;
        let history = self.history.as_mut().unwrap();
        if history.io.reflow(request).is_ok() {
            history.resizing = Some(PendingResize {
                size,
                selection,
                block_points,
                at_bottom,
            });
            history.desired_size = None;
            history.requested = None;
            history.epoch += 1;
            history.layout_epoch += 1;
        }
    }
    fn finish_history_resize(
        &mut self,
        rows: VecDeque<Row>,
        first: u64,
        hot_start: u64,
        points: Vec<Option<(u64, usize)>>,
        view_top: u64,
    ) {
        let history = self.history.as_mut().unwrap();
        let Some(pending) = history.resizing.take() else {
            return;
        };
        let columns = usize::from(pending.size.num_cols);
        let lines = usize::from(pending.size.num_lines);
        let base = hot_start + rows.len() as u64 - lines as u64;
        let point = |value: Option<(u64, usize)>| {
            value.map(|(line, column)| Point {
                line: (line as i64 - base as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                column,
            })
        };
        let (main, alternate) = if self.mode.contains(Mode::ALT_SCREEN) {
            (&mut self.inactive, &mut self.active)
        } else {
            (&mut self.active, &mut self.inactive)
        };
        main.rows = rows;
        for (cursor, mapped) in [&mut main.cursor, &mut main.saved].into_iter().zip(&points) {
            cursor.point = point(*mapped).unwrap_or_default();
            cursor.point.line = cursor.point.line.clamp(0, lines as i32 - 1);
            cursor.wrap_pending = cursor.point.column == columns;
            cursor.point.column = cursor.point.column.min(columns - 1);
        }
        main.offset = if pending.at_bottom {
            0
        } else {
            base.saturating_sub(view_top.max(first)) as usize
        };
        self.selection = pending.selection.and_then(|mut selection| {
            selection.start = point(points[2])?;
            selection.end = point(points[3])?;
            Some(selection)
        });
        for (id, indices) in pending.block_points {
            if let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) {
                let convert = |index: Option<usize>| {
                    index
                        .and_then(|index| points[index])
                        .map(|(row, column)| BlockPoint { row, column })
                };
                if let Some(prompt) = convert(indices[0]) {
                    block.prompt = prompt;
                } else {
                    block.incomplete = true;
                    block.prompt = BlockPoint {
                        row: first,
                        column: 0,
                    };
                }
                for (target, index, output) in [
                    (&mut block.command, indices[1], false),
                    (&mut block.output, indices[2], true),
                    (&mut block.end, indices[3], false),
                ] {
                    if index.is_some() {
                        let mapped = convert(index);
                        if mapped.is_none() {
                            block.incomplete = true;
                        }
                        *target = mapped.or_else(|| {
                            output.then_some(BlockPoint {
                                row: first,
                                column: 0,
                            })
                        });
                    }
                }
            }
        }
        alternate.resize_alternate(columns, lines);
        if self.mode.contains(Mode::ALT_SCREEN) {
            self.selection = None;
        }
        self.block_origin = base;
        history.first = first;
        history.hot_start = hot_start;
        history.page.clear();
        history.requested = None;
        history.blank = Row::new(columns, Cell::default());
        self.finish_resize(pending.size);
    }
    fn clear_disk_history(&mut self) {
        if let Some(history) = &mut self.history {
            let desired_size = history
                .desired_size
                .or_else(|| history.resizing.as_ref().map(|pending| pending.size));
            history.clear = HistoryClear::Queued;
            history.first = 0;
            history.hot_start = 0;
            history.spool.clear();
            history.page.clear();
            history.requested = None;
            history.epoch += 1;
            history.layout_epoch += 1;
            history.resizing = None;
            history.desired_size = desired_size;
        }
        self.flush_history(true);
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
        let cursor = BlockPoint {
            row: self.block_origin + self.active.cursor.point.line as u64,
            column: self.active.cursor.logical_point().column,
        };
        (block.output.is_none() && block.end.is_none() && command == cursor).then_some(block.id)
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
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let Some(uri) = uri.strip_prefix(b"file://") else {
            return;
        };
        let Some(slash) = uri.iter().position(|byte| *byte == b'/') else {
            return;
        };
        let host = &uri[..slash];
        if !host.is_empty() && host != b"localhost" {
            let mut name = [0u8; 256];
            if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
                return;
            }
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len());
            if host != &name[..end] {
                return;
            }
        }
        let path = &uri[slash..];
        let escapes = memchr::memchr_iter(b'%', path).count();
        if escapes == 0 {
            if !path.contains(&0)
                && self
                    .directory
                    .as_ref()
                    .is_none_or(|directory| directory.as_os_str().as_bytes() != path)
            {
                self.directory = Some(PathBuf::from(std::ffi::OsString::from_vec(path.to_vec())));
            }
            return;
        }
        let mut decoded = Vec::with_capacity(path.len().saturating_sub(escapes * 2));
        let mut bytes = path.iter().copied();
        while let Some(byte) = bytes.next() {
            let byte = if byte == b'%' {
                let Some(high) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                    return;
                };
                let Some(low) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
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
                    if status == Some(b"2") {
                        self.prompt_scope.insert(Flags::RIGHT_PROMPT);
                    }
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
        match marker {
            b"A" => self.prompt_scope = Flags::PROMPT,
            b"B" | b"C" | b"D" => self.prompt_scope = Flags::empty(),
            _ => return,
        }
        let point = BlockPoint {
            row: self.block_origin + self.active.cursor.point.line as u64,
            column: self.active.cursor.logical_point().column,
        };
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
                    command: None,
                    output: None,
                    end: None,
                    status: None,
                    incomplete: false,
                    started: None,
                    finished: None,
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
            _ => return,
        }
        self.trim_blocks();
    }
    fn shift_blocks(&mut self, top: usize, bottom: usize, delta: i32) {
        let low = self.block_origin + top as u64;
        let high = self.block_origin + bottom as u64;
        for block in &mut self.blocks {
            for point in std::iter::once(&mut block.prompt).chain(
                [&mut block.command, &mut block.output, &mut block.end]
                    .into_iter()
                    .flatten(),
            ) {
                if point.row >= low && point.row < high {
                    let moved = point.row.saturating_add_signed(i64::from(delta));
                    if moved < low || moved >= high {
                        block.incomplete = true;
                    }
                    point.row = moved.clamp(low, high - 1);
                }
            }
        }
    }
    fn trim_blocks(&mut self) {
        let history = self.history_size();
        let first = self.block_origin.saturating_sub(history as u64);
        while self
            .blocks
            .front()
            .is_some_and(|block| block.end.is_some_and(|end| end.row < first))
        {
            self.blocks.pop_front();
        }
        // Repeated prompt redraws cannot grow metadata without retained terminal rows.
        let limit = history.saturating_add(self.screen_lines()).max(1);
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
        let main = self.main_screen();
        let first = self.history.as_ref().map_or_else(
            || {
                self.block_origin
                    .saturating_sub((main.rows.len() - self.screen_lines()) as u64)
            },
            |history| history.first,
        );
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
        let first = self.block_origin.saturating_sub(self.history_size() as u64);
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
                line: (block.prompt.row.max(top) as i64 - self.block_origin as i64) as i32,
                complete: block.is_complete(first),
                status: block.status,
                output: block.output.map(|start| {
                    let end = block.end.unwrap_or(BlockPoint {
                        row: self.block_origin + self.active.cursor.point.line as u64,
                        column: self.active.cursor.logical_point().column,
                    });
                    let point = |point: BlockPoint| Point {
                        line: (i128::from(point.row) - i128::from(self.block_origin))
                            .clamp(i128::from(i32::MIN), i128::from(i32::MAX))
                            as i32,
                        column: point.column,
                    };
                    (point(start), point(end))
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
        self.selection = if anchor.columns == self.columns() {
            anchor.selection.map(|mut selection| {
                let delta = (i128::from(base) - i128::from(anchor.base))
                    .clamp(i32::MIN as i128, i32::MAX as i128) as i32;
                selection.start.line = selection.start.line.saturating_sub(delta);
                selection.end.line = selection.end.line.saturating_sub(delta);
                selection
            })
        } else {
            None
        };
        self.damage_all();
    }
    pub(crate) fn search(
        &mut self,
        query: &str,
        cancelled: &Arc<AtomicBool>,
        wake: BorrowedFd<'_>,
    ) -> io::Result<mpsc::Receiver<io::Result<crate::search::SearchResult>>> {
        let alternate = self.mode.contains(Mode::ALT_SCREEN);
        if !alternate {
            self.prepare_history_read()?;
        }
        let history = self.history.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "pane search requires workspace history",
            )
        })?;
        let hot_start = if alternate { 0 } else { history.hot_start };
        history.io.search(
            query.to_owned(),
            (hot_start, self.active.rows.iter().cloned().collect()),
            (self.columns(), alternate),
            Arc::clone(cancelled),
            wake.try_clone_to_owned()?,
        )
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
                line: (i128::from(found.start_row) - i128::from(base)) as i32,
                column: usize::from(found.start_column),
            },
            Side::Left,
            SelectionType::Simple,
        );
        self.selection_update(
            Point {
                line: (i128::from(found.end_row) - i128::from(base)) as i32,
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
        let main = self.main_screen();
        let end = block.end.unwrap_or(BlockPoint {
            row: self.block_origin + main.cursor.point.line as u64,
            column: main.cursor.logical_point().column,
        });
        Ok((block, start, end))
    }
    pub fn copy_block(&mut self, id: u64) -> io::Result<mpsc::Receiver<io::Result<HistoryCopy>>> {
        self.prepare_history_read()?;
        let (block, start, end) = self.block_output(id)?;
        let main = self.main_screen();
        let history = self.disk_history()?;
        let first = history.first;
        let incomplete = block.incomplete || block.prompt.row < first || start.row < first;
        let (hot_start, hot) = Self::copy_rows(&main.rows, history.hot_start, start.row, end.row);
        history.io.copy(
            (start.row, start.column),
            (end.row, end.column),
            hot_start,
            hot,
            None,
            incomplete,
        )
    }

    pub fn read_history(
        &mut self,
        block: Option<u64>,
        cursor: Option<HistoryCursor>,
        query: Option<&str>,
    ) -> io::Result<(mpsc::Receiver<io::Result<HistoryPage>>, Arc<AtomicBool>)> {
        self.prepare_history_read()?;
        let history = self.disk_history()?;
        if cursor.is_some_and(|cursor| cursor.epoch != history.layout_epoch) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "History cursor is stale after terminal resize or clear; read again without a cursor",
            ));
        }
        let main = self.main_screen();
        let (start, end, incomplete, command) = if let Some(id) = block {
            let (block, start, end) = self.block_output(id)?;
            let bounded_end = end.max(start);
            (
                (start.row, start.column),
                (bounded_end.row, bounded_end.column),
                block.incomplete || block.prompt.row < history.first || end < start,
                Some(block.metadata(history.first)),
            )
        } else {
            let cursor_row = self.block_origin + main.cursor.point.line as u64;
            let last_used = main
                .rows
                .iter()
                .rposition(|row| row.used() != 0)
                .map_or(cursor_row, |index| history.hot_start + index as u64);
            (
                (0, 0),
                (cursor_row.max(last_used), self.columns()),
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
        let (hot_start, hot) = Self::copy_rows(&main.rows, history.hot_start, cursor.row, end.0);
        let cancelled = Arc::new(AtomicBool::new(false));
        let receiver = history.io.read(HistoryRead {
            start,
            end,
            hot_start,
            hot,
            cursor,
            query: query.map(str::to_owned),
            incomplete,
            command,
            cancelled: Arc::clone(&cancelled),
        })?;
        Ok((receiver, cancelled))
    }
}

impl Core {
    fn copy_rows(rows: &VecDeque<Row>, hot_start: u64, start: u64, end: u64) -> (u64, Vec<Row>) {
        let count = rows.len() as u64;
        let from = start.saturating_sub(hot_start).min(count) as usize;
        let to = end.saturating_add(1).saturating_sub(hot_start).min(count) as usize;
        // A running command may move its cursor above its output marker.
        (
            hot_start + from as u64,
            rows.range(from..to.max(from)).cloned().collect(),
        )
    }

    fn disk_history(&self) -> io::Result<&DiskHistory> {
        self.history.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Unsupported, "disk history is not enabled")
        })
    }

    fn prepare_history_read(&mut self) -> io::Result<()> {
        self.flush_history(true);
        if self.mode.contains(Mode::SYNC_UPDATE)
            || self.history.as_ref().is_some_and(|history| {
                !history.spool.is_empty()
                    || history.clear != HistoryClear::Idle
                    || history.resizing.is_some()
                    || history.desired_size.is_some()
            })
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
    pub fn copy_selection(&mut self) -> io::Result<mpsc::Receiver<io::Result<HistoryCopy>>> {
        self.prepare_history_read()?;
        let range = self
            .selection_range()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no selection"))?;
        let history = self.disk_history()?;
        let hot_start = if self.mode.contains(Mode::ALT_SCREEN) {
            0
        } else {
            history.hot_start
        };
        let base = hot_start + self.hot_history_size() as u64;
        let start = (
            base.saturating_add_signed(i64::from(range.start.line)),
            range.start.column,
        );
        let end = (
            base.saturating_add_signed(i64::from(range.end.line)),
            range.end.column + 1,
        );
        let kind = self.selection.map(|selection| selection.kind);
        // Word/line expansion may cross an uncached history-page boundary. Keep
        // its hot context, including the archive boundary at zero in alternate mode.
        let (hot_start, hot) = if self.mode.contains(Mode::ALT_SCREEN) {
            (hot_start, self.active.rows.iter().cloned().collect())
        } else if kind == Some(SelectionType::Simple) {
            Self::copy_rows(&self.active.rows, hot_start, start.0, end.0)
        } else {
            let count = self.active.rows.len();
            let mut from = start.0.saturating_sub(hot_start).min(count as u64) as usize;
            let mut to = end
                .0
                .saturating_add(1)
                .saturating_sub(hot_start)
                .min(count as u64) as usize;
            while from > 0 && self.active.rows[from - 1].wrapped {
                from -= 1;
            }
            // A cold selection may expand into the first retained logical line.
            to = to.max(1).min(count);
            while to < count && self.active.rows[to - 1].wrapped {
                to += 1;
            }
            (
                hot_start + from as u64,
                self.active
                    .rows
                    .range(from..to.max(from))
                    .cloned()
                    .collect(),
            )
        };
        history.io.copy(start, end, hot_start, hot, kind, false)
    }
}

#[cfg(test)]
fn settle_history(core: &mut Core) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        core.poll_history(true).unwrap();
        let main = core.main_screen();
        let cursor = HistoryCursor {
            epoch: core.history.as_ref().unwrap().layout_epoch,
            row: core.block_origin + main.cursor.point.line as u64,
            offset: 0,
        };
        match core.read_history(None, Some(cursor), None) {
            Ok((receiver, _)) => {
                receiver
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    .unwrap()
                    .unwrap();
                core.poll_history(true).unwrap();
                return;
            }
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "history worker barrier stalled"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod disk_tests;

#[cfg(test)]
mod history_boundary_tests;
