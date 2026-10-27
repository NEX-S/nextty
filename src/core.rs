#[cfg(feature = "tui")]
use crate::history::{History, HistoryConfig, HistoryCopy, Reflow, Reply};
use crate::vt::{
    Attr, ClearMode, Color, CursorShape, LineClearMode, NamedColor, Rgb, StandardCharset,
};
use crate::{base64::decode_utf8, width::scalar_width};
use std::borrow::Cow;
use std::collections::VecDeque;
#[cfg(feature = "tui")]
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMarker {
    pub id: u64,
    pub line: i32,
    pub complete: bool,
    pub status: Option<i32>,
    pub has_output: bool,
    pub output: Option<(Point, Point)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    pub id: u64,
    pub complete: bool,
    pub status: Option<i32>,
    pub has_output: bool,
}
#[cfg(feature = "tui")]
#[derive(Clone, Debug)]
struct Block {
    id: u64,
    prompt: BlockPoint,
    command: Option<BlockPoint>,
    output: Option<BlockPoint>,
    end: Option<BlockPoint>,
    status: Option<i32>,
    incomplete: bool,
}
#[cfg(feature = "tui")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BlockPoint {
    row: u64,
    column: usize,
}
#[cfg(feature = "tui")]
struct DiskHistory {
    io: History,
    first: u64,
    hot_start: u64,
    spool_start: u64,
    stored_end: u64,
    spool: VecDeque<Row>,
    epoch: u64,
    page_start: u64,
    page: Vec<Row>,
    requested: Option<(u64, u64)>,
    blank: Row,
    desired_size: Option<WindowSize>,
    resizing: Option<PendingResize>,
    clear_pending: bool,
    clear_requested: bool,
}
#[cfg(feature = "tui")]
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
            let value = ANSI[index];
            Rgb {
                r: (value >> 16) as u8,
                g: (value >> 8) as u8,
                b: value as u8,
            }
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Flags(u16);

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
    pub const fn bits(self) -> u16 {
        self.0
    }
    pub const fn from_bits(bits: u16) -> Option<Self> {
        if bits & !0x7fff == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
    pub const fn empty() -> Self {
        Self(0)
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

impl std::ops::BitOr for Flags {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Mode(u32);

impl Mode {
    pub const fn bits(self) -> u32 {
        self.0
    }
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !0x1ffff == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
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

impl std::ops::BitOr for Mode {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
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
    pub zerowidth: Vec<char>,
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
        extras.sort_unstable_by_key(|(column, _)| *column);
        if extras.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return None;
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
    pub(crate) fn non_default_len(&self) -> usize {
        self.cells
            .iter()
            .rposition(|cell| *cell != Cell::default())
            .map_or(0, |column| column + 1)
    }
    pub fn extras(&self) -> &[(usize, Extra)] {
        self.extras
            .as_deref()
            .map_or(&[], |extras| extras.as_slice())
    }

    pub(crate) fn extras_mut(&mut self) -> &mut Vec<(usize, Extra)> {
        Arc::make_mut(self.extras.get_or_insert_with(Default::default))
    }

    fn clear_all_extras(&mut self) {
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
        let first = extras.partition_point(|(column, _)| *column < range.start);
        let mut extras = &extras[first..];
        let cells = &self.cells[range.start..];
        range.zip(cells).map(move |(column, cell)| {
            let extra = if extras.first().is_some_and(|(index, _)| *index == column) {
                let extra = &extras[0].1;
                extras = &extras[1..];
                Some(extra)
            } else {
                None
            };
            (column, cell, extra)
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

    fn extra_mut(&mut self, column: usize) -> &mut Extra {
        let extras = self.extras_mut();
        let index = match extras.last() {
            Some((last, _)) if *last == column => extras.len() - 1,
            Some((last, _)) if *last > column => {
                match extras.binary_search_by_key(&column, |(column, _)| *column) {
                    Ok(index) => index,
                    Err(index) => {
                        extras.insert(index, (column, Extra::default()));
                        index
                    }
                }
            }
            _ => {
                extras.push((column, Extra::default()));
                extras.len() - 1
            }
        };
        &mut extras[index].1
    }

    fn clear_extras(&mut self, start: usize, end: usize) {
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
        } else {
            self.cells.fill(blank);
        }
        self.wrapped = false;
        self.used = if blank.bg == Color::Named(NamedColor::Background) {
            0
        } else {
            self.cells.len()
        };
        self.clear_all_extras();
    }

    fn clear_range(&mut self, start: usize, end: usize, blank: Cell) {
        let end = end.min(self.cells.len());
        if start >= end {
            return;
        }
        let mut start = start;
        let mut end = end;
        if self.cells[start].flags.contains(Flags::WIDE_CHAR_SPACER) && start > 0 {
            start -= 1;
        }
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
        let mut start = column;
        let mut end = column + width;
        if self.cells[column].flags.contains(Flags::WIDE_CHAR_SPACER) && column > 0 {
            start -= 1;
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
    fn write(&mut self, column: usize, cell: Cell, width: usize, underline_color: Option<Color>) {
        self.prepare_write(column, width, cell.bg);
        self.cells[column] = cell;
        if width == 2 {
            self.cells[column + 1] = Cell {
                c: ' ',
                flags: (cell.flags - Flags::WIDE_CHAR) | Flags::WIDE_CHAR_SPACER,
                ..cell
            };
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
pub enum Side {
    Left,
    Right,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionType {
    Simple,
    Semantic,
    Lines,
    Object,
}

pub(crate) fn object_selection<R: std::borrow::Borrow<Row>, E>(
    point: (i64, usize),
    mut row: impl FnMut(i64) -> std::result::Result<Option<R>, E>,
) -> std::result::Result<Option<[(i64, usize); 2]>, E> {
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
    let mut right_limited = false;
    loop {
        let Some(current) = row(line)? else {
            break;
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
                break;
            }
        }
        right_limited = anchor.is_some_and(|anchor| cells.len() - anchor > LIMIT);
        if !current.wrapped || right_limited {
            break;
        }
        line += 1;
    }
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
                        || "=([{,:".contains(cells[index - 1].0)))
        {
            quote = Some((ch, index));
        }
    }
    let boundary = |ch: char| ch.is_whitespace() || ",│|\"'`<>".contains(ch);
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
    while start < anchor && "([{=".contains(cells[start].0) {
        start += 1;
    }
    for (open, close) in [('(', ')'), ('[', ']'), ('{', '}')] {
        let mut balance = cells[start..end].iter().fold(0i32, |balance, &(ch, _)| {
            balance + i32::from(ch == open) - i32::from(ch == close)
        });
        while end > anchor + 1 && balance < 0 && cells[end - 1].0 == close {
            end -= 1;
            balance += 1;
        }
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
    HistoryCleared,
    ClipboardStore {
        primary: bool,
        text: String,
    },
    #[cfg(feature = "tui")]
    HistoryError(std::io::Error),
}

#[derive(Clone, Debug)]
struct Cursor {
    point: Point,
    template: Cell,
    underline_color: Option<Color>,
    charsets: [StandardCharset; 4],
    charset: usize,
    wrap_pending: bool,
    origin: bool,
}
impl Default for Cursor {
    fn default() -> Self {
        Self {
            point: Point::default(),
            template: Cell::default(),
            underline_color: None,
            charsets: [StandardCharset::Ascii; 4],
            charset: 0,
            wrap_pending: false,
            origin: false,
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
            rows: (0..lines)
                .map(|_| Row::new(columns, Cell::default()))
                .collect(),
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

#[cfg(feature = "tui")]
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
    colors: [Option<Rgb>; 269],
    events: Vec<Event>,
    title: Option<String>,
    titles: Vec<Option<String>>,
    #[cfg(feature = "tui")]
    history: Option<DiskHistory>,
    #[cfg(feature = "tui")]
    blocks: VecDeque<Block>,
    #[cfg(feature = "tui")]
    next_block: u64,
    #[cfg(feature = "tui")]
    prompt_scope: Flags,
    #[cfg(feature = "tui")]
    block_origin: u64,
    #[cfg(feature = "tui")]
    directory: Option<PathBuf>,
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
            colors: [None; 269],
            events: Vec::new(),
            title: None,
            titles: Vec::new(),
            #[cfg(feature = "tui")]
            history: None,
            #[cfg(feature = "tui")]
            blocks: VecDeque::new(),
            #[cfg(feature = "tui")]
            next_block: 1,
            #[cfg(feature = "tui")]
            prompt_scope: Flags::empty(),
            #[cfg(feature = "tui")]
            block_origin: 0,
            #[cfg(feature = "tui")]
            directory: None,
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
        #[cfg(feature = "tui")]
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
        #[cfg(feature = "tui")]
        if let Some(history) = &self.history {
            return &history.blank;
        }
        unreachable!("visible row outside the screen")
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
        #[cfg(feature = "tui")]
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
        #[cfg(feature = "tui")]
        if !self.mode.contains(Mode::ALT_SCREEN) {
            cell.flags.insert(self.prompt_scope);
        }
        #[cfg(not(feature = "tui"))]
        cell.flags.remove(Flags::PROMPT | Flags::RIGHT_PROMPT);
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
    fn scroll_region_up(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom - top);
        let blank = self.blank();
        let history = top == 0 && !self.mode.contains(Mode::ALT_SCREEN);
        for _ in 0..count {
            if history {
                #[cfg(feature = "tui")]
                let disk = self.history.is_some();
                #[cfg(not(feature = "tui"))]
                let disk = false;
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
                let mut recycled = self.active.rows.remove(base + top).unwrap();
                recycled.clear(blank);
                self.active.rows.insert(base + bottom - 1, recycled);
            }
        }
        self.shift_selection(top, bottom, -(count as i32), history);
        #[cfg(feature = "tui")]
        if !self.mode.contains(Mode::ALT_SCREEN) {
            if history {
                self.block_origin += count as u64;
                self.trim_blocks();
            } else {
                self.shift_blocks(top, bottom, -(count as i32));
            }
        }
        if history {
            #[cfg(feature = "tui")]
            self.spill_history();
            self.damage_all();
        } else {
            self.damage_range(top, bottom);
        }
    }
    fn scroll_region_down(&mut self, top: usize, bottom: usize, count: usize) {
        let count = count.min(bottom - top);
        let blank = self.blank();
        let base = self.hot_history_size();
        for _ in 0..count {
            let mut recycled = self.active.rows.remove(base + bottom - 1).unwrap();
            recycled.clear(blank);
            self.active.rows.insert(base + top, recycled);
        }
        self.shift_selection(top, bottom, count as i32, false);
        #[cfg(feature = "tui")]
        if !self.mode.contains(Mode::ALT_SCREEN) {
            self.shift_blocks(top, bottom, count as i32);
        }
        self.damage_range(top, bottom);
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
        if offset != self.display_offset() {
            self.active.offset = offset;
            #[cfg(feature = "tui")]
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
        if let Some(color) = self.colors[index] {
            return color;
        }
        let value = match index {
            0..256 => return DEFAULT_PALETTE[index],
            256 | 258 | 267 => self.foreground,
            257 => self.background,
            259..267 => {
                let c = self.color(Color::Indexed((index - 259) as u8));
                return Rgb {
                    r: c.r / 2,
                    g: c.g / 2,
                    b: c.b / 2,
                };
            }
            268 => {
                let c = self.color(Color::Named(NamedColor::Foreground));
                return Rgb {
                    r: c.r / 2,
                    g: c.g / 2,
                    b: c.b / 2,
                };
            }
            _ => self.foreground,
        };
        Rgb {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
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
        #[cfg(feature = "tui")]
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
        match selection.kind {
            SelectionType::Object => {
                let object = |point: Point| {
                    object_selection((i64::from(point.line), point.column), |line| {
                        Ok::<_, std::convert::Infallible>(self.absolute_row(line as i32))
                    })
                    .unwrap()
                };
                let first_object = object(start);
                let last_object = if start == end {
                    first_object
                } else {
                    object(end)
                };
                if let Some([first, _]) = first_object {
                    start = Point {
                        line: first.0 as i32,
                        column: first.1,
                    };
                }
                if let Some([_, last]) = last_object {
                    end = Point {
                        line: last.0 as i32,
                        column: last.1,
                    };
                }
            }
            SelectionType::Simple => {
                if selection.start == selection.end && selection.start_side == selection.end_side {
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
            SelectionType::Lines => {
                while self
                    .absolute_row(start.line - 1)
                    .is_some_and(|row| row.wrapped)
                {
                    start.line -= 1;
                }
                while self.absolute_row(end.line).is_some_and(|row| row.wrapped)
                    && end.line + 1 < self.screen_lines() as i32
                {
                    end.line += 1;
                }
                start.column = 0;
                end.column = columns - 1;
            }
            SelectionType::Semantic => {
                let group = |c: char| {
                    if c.is_whitespace() {
                        0
                    } else if ",│`|:\"' ()[]{}<>\t".contains(c) {
                        1
                    } else {
                        2
                    }
                };
                if let Some(row) = self.absolute_row(start.line) {
                    if row.cells[start.column]
                        .flags
                        .contains(Flags::WIDE_CHAR_SPACER)
                        && start.column > 0
                    {
                        start.column -= 1;
                    }
                    let class = group(row.cells[start.column].c);
                    loop {
                        let mut previous = start;
                        if previous.column > 0 {
                            previous.column -= 1;
                        } else if let Some(row) = self
                            .absolute_row(previous.line - 1)
                            .filter(|row| row.wrapped)
                        {
                            previous.line -= 1;
                            previous.column = row.used.saturating_sub(1);
                        } else {
                            break;
                        }
                        let row = self.absolute_row(previous.line).unwrap();
                        if row.cells[previous.column]
                            .flags
                            .contains(Flags::WIDE_CHAR_SPACER)
                            && previous.column > 0
                        {
                            previous.column -= 1;
                        }
                        if group(row.cells[previous.column].c) != class {
                            break;
                        }
                        start = previous;
                    }
                }
                if let Some(row) = self.absolute_row(end.line) {
                    if row.cells[end.column]
                        .flags
                        .contains(Flags::WIDE_CHAR_SPACER)
                        && end.column > 0
                    {
                        end.column -= 1;
                    }
                    let class = group(row.cells[end.column].c);
                    loop {
                        let row = self.absolute_row(end.line).unwrap();
                        let mut next = end;
                        next.column += if row.cells[end.column].flags.contains(Flags::WIDE_CHAR) {
                            2
                        } else {
                            1
                        };
                        if row.wrapped
                            && next.column >= row.used
                            && end.line + 1 < self.screen_lines() as i32
                        {
                            next.line += 1;
                            next.column = 0;
                        } else if next.column >= columns {
                            break;
                        }
                        let Some(row) = self.absolute_row(next.line) else {
                            break;
                        };
                        if group(row.cells[next.column].c) != class {
                            break;
                        }
                        end = next;
                    }
                }
            }
        }
        start.line = start.line.max(-(self.history_size() as i32));
        end.line = end.line.min(self.screen_lines() as i32 - 1);
        start.column = start.column.min(columns - 1);
        end.column = end.column.min(columns - 1);
        if let Some(row) = self.absolute_row(start.line)
            && row.cells[start.column]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            && start.column > 0
        {
            start.column -= 1;
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
            for (_, cell, extra) in row.cells_with_extras(start..end) {
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::PROMPT)
                {
                    continue;
                }
                text.push(cell.c);
                if let Some(extra) = extra {
                    text.extend(&extra.zerowidth);
                }
            }
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
            self.active.saved = self.active.cursor.clone();
        }
        if enabled && clear {
            self.inactive.clear();
        }
        std::mem::swap(&mut self.active, &mut self.inactive);
        self.mode.set(Mode::ALT_SCREEN, enabled);
        if !enabled && save {
            self.active.cursor = self.active.saved.clone();
        }
        self.scroll_top = 0;
        self.scroll_bottom = self.screen_lines();
        self.selection = None;
        self.damage_all();
    }

    pub fn resize(&mut self, size: WindowSize) {
        #[cfg(feature = "tui")]
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
        let mut cursor = main.cursor.point;
        cursor.column += usize::from(main.cursor.wrap_pending);
        let mut saved = main.saved.point;
        saved.column += usize::from(main.saved.wrap_pending);
        let mut points = vec![cursor, saved];
        if let Some(selection) = main_selection {
            points.extend([selection.start, selection.end]);
        }
        let mapped = resize_screen(main, old_columns, old_lines, columns, lines, &points);
        main.cursor.point = Point {
            column: mapped[0].column.min(columns - 1),
            ..mapped[0]
        };
        main.saved.point = Point {
            column: mapped[1].column.min(columns - 1),
            ..mapped[1]
        };
        main.cursor.wrap_pending = mapped[0].column == columns;
        main.saved.wrap_pending = mapped[1].column == columns;
        if let Some(mut selection) = main_selection {
            selection.start = mapped[2];
            selection.end = mapped[3];
            self.selection = Some(selection);
        }
        alternate.resize_alternate(columns, lines);
        if self.mode.contains(Mode::ALT_SCREEN) {
            self.selection = None;
        }
        self.size = size;
        self.tabs.resize_with(columns, || false);
        for column in old_columns..columns {
            self.tabs[column] = column != 0 && column % 8 == 0;
        }
        self.scroll_top = 0;
        self.scroll_bottom = lines;
        self.dirty = vec![true; lines];
    }

    fn write_reply(&mut self, reply: impl Into<Cow<'static, str>>) {
        let reply = reply.into();
        if let Some(Event::Write(previous)) = self.events.last_mut() {
            previous.extend_from_slice(reply.as_bytes());
        } else {
            self.events
                .push(Event::Write(reply.into_owned().into_bytes()));
        }
    }
    fn set_private(&mut self, mode: u16, enabled: bool) {
        let raw = mode;
        let flag = match raw {
            1 => Mode::APP_CURSOR,
            6 => {
                self.active.cursor.origin = enabled;
                self.goto(0, 0);
                return;
            }
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
            47 => {
                self.set_alternate(enabled, false, false);
                return;
            }
            1047 => {
                self.set_alternate(enabled, false, false);
                if !enabled {
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
            _ => return,
        };
        if enabled && flag.intersects(Mode::MOUSE_MODE) {
            self.mode.remove(Mode::MOUSE_MODE);
        }
        self.mode.set(flag, enabled);
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
    let absolute: Vec<_> = points
        .iter()
        .map(|point| {
            (
                (old_history as i64 + i64::from(point.line)).max(0) as usize,
                point.column,
            )
        })
        .collect();
    let old_view_top = screen.rows.len() - old_lines - screen.offset;
    let mut mapped = vec![Point::default(); points.len()];
    let mut output = VecDeque::new();
    let mut source_row = 0;
    let mut mapped_view_top = 0;
    if old_columns == columns {
        output = std::mem::take(&mut screen.rows);
        let removed = old_lines
            .saturating_sub(lines)
            .min(old_lines - 1 - screen.cursor.point.line as usize);
        output.truncate(output.len() - removed);
        mapped_view_top = old_view_top;
        for (point, (line, column)) in mapped.iter_mut().zip(&absolute) {
            *point = Point {
                line: *line as i32,
                column: *column,
            };
        }
    } else {
        let mut cells = Vec::new();
        let mut extras = Vec::new();
        let mut point_offsets = Vec::new();
        while !screen.rows.is_empty() {
            let group_start = source_row;
            cells.clear();
            point_offsets.clear();
            let mut text_used = 0;
            loop {
                let row = screen.rows.pop_front().unwrap();
                let base = cells.len();
                text_used = text_used.max(base + row.used);
                let mut used = row.used;
                for (index, (y, x)) in absolute.iter().enumerate() {
                    if *y == source_row {
                        // A wide wrap gap is outside the logical text, even if a saved
                        // cursor or selection endpoint lands there after a resize.
                        let column = if row.wrapped { (*x).min(row.used) } else { *x };
                        if !row.wrapped {
                            used = used.max(if index < 2 && column == old_columns {
                                column
                            } else {
                                column + 1
                            });
                        }
                        point_offsets.push((index, base + column));
                    }
                }
                used = used.min(old_columns);
                cells.extend_from_slice(&row.cells[..used]);
                extras.extend(
                    row.extras
                        .map(Arc::unwrap_or_clone)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(x, _)| *x < used)
                        .map(|(x, extra)| (base + x, extra)),
                );
                source_row += 1;
                if !row.wrapped || screen.rows.is_empty() {
                    break;
                }
            }
            let output_start = output.len();
            let mut row = Row::new(columns, Cell::default());
            let mut column = 0;
            let mut input = 0;
            let mut extras = extras.drain(..).peekable();
            loop {
                if input == cells.len() {
                    break;
                }
                if cells[input].flags.contains(Flags::WIDE_CHAR)
                    && column + 1 == columns
                    && columns > 1
                {
                    row.wrapped = true;
                    row.used = column;
                    output.push_back(row);
                    row = Row::new(columns, Cell::default());
                    column = 0;
                }
                if column == columns {
                    row.wrapped = true;
                    output.push_back(row);
                    row = Row::new(columns, Cell::default());
                    column = 0;
                }
                for (index, offset) in &point_offsets {
                    if *offset == input {
                        mapped[*index] = Point {
                            line: output.len() as i32,
                            column,
                        };
                    }
                }
                let cell = cells[input];
                if columns == 1 && cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    input += 1;
                    continue;
                }
                row.cells[column] = cell;
                if input < text_used {
                    row.used = column + 1;
                }
                if columns == 1 {
                    row.cells[column].flags.remove(Flags::WIDE_CHAR);
                }
                if extras.peek().is_some_and(|(x, _)| *x == input) {
                    let (_, extra) = extras.next().unwrap();
                    row.extras_mut().push((column, extra));
                }
                column += 1;
                input += 1;
            }
            for (index, offset) in &point_offsets {
                if *offset == input {
                    mapped[*index] = Point {
                        line: output.len() as i32,
                        column,
                    };
                }
            }
            output.push_back(row);
            if old_view_top >= group_start && old_view_top < source_row {
                mapped_view_top =
                    output_start + (old_view_top - group_start) * old_columns / columns;
            }
        }
    }
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
            let length = bytes
                .iter()
                .position(|byte| !byte.is_ascii())
                .unwrap_or(bytes.len());
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
            self.active.cursor.wrap_pending = length == available;
            self.active.cursor.point.column = end.min(self.columns() - 1);
            self.damage_line(point.line as usize);
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
                cells[1] = Cell {
                    c: ' ',
                    flags: (template.flags - Flags::WIDE_CHAR) | Flags::WIDE_CHAR_SPACER,
                    ..template
                };
            }
            self.active.cursor.wrap_pending = end == columns;
            self.active.cursor.point.column = end.min(columns - 1);
            self.damage_line(point.line as usize);
            return length;
        }
        0
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
            if row.cells[column].flags.contains(Flags::WIDE_CHAR_SPACER) && column > 0 {
                column -= 1;
            }
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
            self.insert_blank(width);
        }
        let mut cell = self.text_template();
        let underline_color = self.active.cursor.underline_color;
        cell.c = c;
        cell.flags.set(Flags::WIDE_CHAR, width == 2);
        let row = self.screen_row_mut(point.line as usize);
        row.write(point.column, cell, width, underline_color);
        let columns = self.columns();
        let cursor = &mut self.active.cursor;
        if point.column + width >= columns {
            cursor.point.column = columns - 1;
            cursor.wrap_pending = true;
        } else {
            cursor.point.column += width;
        }
        self.damage_line(point.line as usize);
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
    pub(crate) fn move_up(&mut self, count: usize) {
        let point = self.active.cursor.point;
        let top = if point.line as usize >= self.scroll_top {
            self.scroll_top
        } else {
            0
        };
        self.move_cursor(
            (point.line as usize).saturating_sub(count).max(top),
            point.column,
        );
    }
    pub(crate) fn move_down(&mut self, count: usize) {
        let point = self.active.cursor.point;
        let bottom = if (point.line as usize) < self.scroll_bottom {
            self.scroll_bottom
        } else {
            self.screen_lines()
        };
        self.move_cursor(
            (point.line as usize).saturating_add(count).min(bottom - 1),
            point.column,
        );
    }
    pub(crate) fn move_forward(&mut self, count: usize) {
        let point = self.active.cursor.point;
        self.move_cursor(point.line as usize, point.column.saturating_add(count));
    }
    pub(crate) fn move_backward(&mut self, count: usize) {
        let point = self.active.cursor.point;
        self.move_cursor(point.line as usize, point.column.saturating_sub(count));
    }
    pub(crate) fn move_down_and_cr(&mut self, count: usize) {
        self.move_down(count);
        self.carriage_return();
    }
    pub(crate) fn move_up_and_cr(&mut self, count: usize) {
        self.move_up(count);
        self.carriage_return();
    }
    pub(crate) fn backspace(&mut self) {
        self.move_backward(1);
    }
    pub(crate) fn carriage_return(&mut self) {
        self.goto_col(0);
    }
    pub(crate) fn linefeed(&mut self) {
        let point = self.active.cursor.point;
        if point.line as usize + 1 == self.scroll_bottom {
            self.scroll_region_up(self.scroll_top, self.scroll_bottom, 1);
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
            self.scroll_region_down(self.scroll_top, self.scroll_bottom, 1);
            self.active.cursor.wrap_pending = false;
        } else {
            self.move_up(1);
        }
    }
    pub(crate) fn put_tab(&mut self, count: u16) {
        self.move_forward_tabs(count);
    }
    pub(crate) fn move_forward_tabs(&mut self, count: u16) {
        let mut column = self.active.cursor.point.column;
        if count != 0 {
            column = (column + 1..self.columns())
                .filter(|x| self.tabs[*x])
                .nth(usize::from(count - 1))
                .unwrap_or(self.columns() - 1);
        }
        self.goto_col(column);
    }
    pub(crate) fn move_backward_tabs(&mut self, count: u16) {
        let mut column = self.active.cursor.point.column;
        if count != 0 {
            column = (0..column)
                .rev()
                .filter(|x| self.tabs[*x])
                .nth(usize::from(count - 1))
                .unwrap_or(0);
        }
        self.goto_col(column);
    }
    pub(crate) fn set_horizontal_tabstop(&mut self) {
        let column = self.active.cursor.point.column;
        self.tabs[column] = true;
    }
    pub(crate) fn clear_tabs(&mut self, all: bool) {
        match all {
            false => {
                let column = self.active.cursor.point.column;
                self.tabs[column] = false;
            }
            true => self.tabs.fill(false),
        }
    }
    pub(crate) fn set_tabs(&mut self, interval: u16) {
        for (column, tab) in self.tabs.iter_mut().enumerate() {
            *tab = interval != 0 && column != 0 && column % usize::from(interval) == 0;
        }
    }

    pub(crate) fn insert_blank(&mut self, count: usize) {
        let point = self.active.cursor.point;
        let columns = self.columns();
        let count = count.min(columns - point.column);
        let blank = self.blank();
        let row = self.screen_row_mut(point.line as usize);
        let cells = &mut row.cells[point.column..];
        cells.copy_within(..cells.len() - count, count);
        cells[..count].fill(blank);
        if row
            .extras()
            .last()
            .is_some_and(|(column, _)| *column >= point.column)
        {
            let extras = row.extras_mut();
            let first = extras.partition_point(|(column, _)| *column < point.column);
            for (column, _) in &mut extras[first..] {
                *column += count;
            }
            let keep = extras.partition_point(|(column, _)| *column < columns);
            extras.truncate(keep);
        }
        row.used = (row.used + count).min(columns);
        if blank.bg != Color::Named(NamedColor::Background) {
            row.used = row.used.max(point.column + count);
        }
        row.repair_wide(point.column.saturating_sub(1), blank);
        row.wrapped = false;
        self.active.cursor.wrap_pending = false;
        self.damage_line(point.line as usize);
    }
    pub(crate) fn delete_chars(&mut self, count: usize) {
        let point = self.active.cursor.point;
        let columns = self.columns();
        let count = count.min(columns - point.column);
        let blank = self.blank();
        let row = self.screen_row_mut(point.line as usize);
        row.cells.copy_within(point.column + count.., point.column);
        row.cells[columns - count..].fill(blank);
        if row
            .extras()
            .last()
            .is_some_and(|(column, _)| *column >= point.column)
        {
            let extras = row.extras_mut();
            let first = extras.partition_point(|(column, _)| *column < point.column);
            let last = extras.partition_point(|(column, _)| *column < point.column + count);
            extras.drain(first..last);
            for (column, _) in &mut extras[first..] {
                *column -= count;
            }
        }
        row.used = row.used.saturating_sub(count).max(point.column);
        if blank.bg != Color::Named(NamedColor::Background) {
            row.used = columns;
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
                let history = self.history_size();
                self.active.rows.drain(..self.hot_history_size());
                #[cfg(feature = "tui")]
                if !self.mode.contains(Mode::ALT_SCREEN) {
                    self.clear_disk_history();
                }
                #[cfg(feature = "tui")]
                if !self.mode.contains(Mode::ALT_SCREEN) {
                    self.blocks.clear();
                    self.block_origin = 0;
                }
                self.active.rows.shrink_to_fit();
                self.active.offset = 0;
                if history != 0 && !self.mode.contains(Mode::ALT_SCREEN) {
                    self.events.push(Event::HistoryCleared);
                }
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
    pub(crate) fn scroll_up(&mut self, count: usize) {
        self.scroll_region_up(self.scroll_top, self.scroll_bottom, count);
    }
    pub(crate) fn scroll_down(&mut self, count: usize) {
        self.scroll_region_down(self.scroll_top, self.scroll_bottom, count);
    }
    pub(crate) fn insert_blank_lines(&mut self, count: usize) {
        let line = self.active.cursor.point.line as usize;
        if line >= self.scroll_top && line < self.scroll_bottom {
            self.scroll_region_down(line, self.scroll_bottom, count);
            self.carriage_return();
        }
    }
    pub(crate) fn delete_lines(&mut self, count: usize) {
        let line = self.active.cursor.point.line as usize;
        if line >= self.scroll_top && line < self.scroll_bottom {
            self.scroll_region_up(line, self.scroll_bottom, count);
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
        self.active.saved = self.active.cursor.clone();
    }
    pub(crate) fn restore_cursor_position(&mut self) {
        let mut cursor = self.active.saved.clone();
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
            Attr::Bold => flags.insert(Flags::BOLD),
            Attr::Dim => flags.insert(Flags::DIM),
            Attr::Italic => flags.insert(Flags::ITALIC),
            Attr::Reverse => flags.insert(Flags::INVERSE),
            Attr::Hidden => flags.insert(Flags::HIDDEN),
            Attr::Strike => flags.insert(Flags::STRIKEOUT),
            Attr::Underline
            | Attr::DoubleUnderline
            | Attr::Undercurl
            | Attr::DottedUnderline
            | Attr::DashedUnderline => {
                flags.remove(Flags::ALL_UNDERLINES);
                flags.insert(match attr {
                    Attr::DoubleUnderline => Flags::DOUBLE_UNDERLINE,
                    Attr::Undercurl => Flags::UNDERCURL,
                    Attr::DottedUnderline => Flags::DOTTED_UNDERLINE,
                    Attr::DashedUnderline => Flags::DASHED_UNDERLINE,
                    _ => Flags::UNDERLINE,
                });
            }
            Attr::CancelBold => flags.remove(Flags::BOLD),
            Attr::CancelBoldDim => flags.remove(Flags::BOLD | Flags::DIM),
            Attr::CancelItalic => flags.remove(Flags::ITALIC),
            Attr::CancelUnderline => flags.remove(Flags::ALL_UNDERLINES),
            Attr::CancelReverse => flags.remove(Flags::INVERSE),
            Attr::CancelHidden => flags.remove(Flags::HIDDEN),
            Attr::CancelStrike => flags.remove(Flags::STRIKEOUT),
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::UnderlineColor(color) => cursor.underline_color = color,
        }
    }
    pub(crate) fn set_mode(&mut self, mode: u16) {
        match mode {
            4 => self.mode.insert(Mode::INSERT),
            20 => self.mode.insert(Mode::LINE_FEED_NEW_LINE),
            _ => {}
        }
    }
    pub(crate) fn unset_mode(&mut self, mode: u16) {
        match mode {
            4 => self.mode.remove(Mode::INSERT),
            20 => self.mode.remove(Mode::LINE_FEED_NEW_LINE),
            _ => {}
        }
    }
    pub(crate) fn set_private_mode(&mut self, mode: u16) {
        self.set_private(mode, true);
    }
    pub(crate) fn unset_private_mode(&mut self, mode: u16) {
        self.set_private(mode, false);
    }
    pub(crate) fn set_keypad_application_mode(&mut self) {
        self.mode.insert(Mode::APP_KEYPAD);
    }
    pub(crate) fn unset_keypad_application_mode(&mut self) {
        self.mode.remove(Mode::APP_KEYPAD);
    }
    pub(crate) fn reset_state(&mut self) {
        let size = self.size;
        let foreground = self.foreground;
        let background = self.background;
        let mut events = std::mem::take(&mut self.events);
        #[cfg(feature = "tui")]
        let history = self.history.take();
        *self = Self::new(size, foreground, background);
        #[cfg(feature = "tui")]
        {
            self.history = history;
            self.clear_disk_history();
        }
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
            None => self.write_reply("\x1b[?6c"),
            Some('>') => self.write_reply("\x1b[>0;1;0c"),
            Some('=') => self.write_reply("\x1bP!|00000000\x1b\\"),
            _ => {}
        }
    }
    pub(crate) fn device_status(&mut self, status: usize) {
        match status {
            5 => self.write_reply("\x1b[0n"),
            6 => {
                let point = self.active.cursor.point;
                let top = if self.active.cursor.origin {
                    self.scroll_top as i32
                } else {
                    0
                };
                let line = (point.line - top).max(0) as usize;
                self.write_reply(format!("\x1b[{};{}R", line + 1, point.column + 1));
            }
            _ => {}
        }
    }
    pub(crate) fn report_mode(&mut self, mode: u16) {
        let enabled = match mode {
            4 => Some(self.mode.contains(Mode::INSERT)),
            20 => Some(self.mode.contains(Mode::LINE_FEED_NEW_LINE)),
            _ => None,
        };
        self.write_reply(format!(
            "\x1b[{};{}$y",
            mode,
            enabled.map_or(0, |enabled| if enabled { 1 } else { 2 })
        ));
    }
    pub(crate) fn report_private_mode(&mut self, mode: u16) {
        let flag = match mode {
            1 => Some(Mode::APP_CURSOR),
            6 => Some(Mode::ORIGIN),
            7 => Some(Mode::LINE_WRAP),
            25 => Some(Mode::SHOW_CURSOR),
            47 | 1047 | 1049 => Some(Mode::ALT_SCREEN),
            1000 => Some(Mode::MOUSE_CLICK),
            1002 => Some(Mode::MOUSE_DRAG),
            1003 => Some(Mode::MOUSE_MOTION),
            1004 => Some(Mode::FOCUS_IN_OUT),
            1005 => Some(Mode::UTF8_MOUSE),
            1006 => Some(Mode::SGR_MOUSE),
            1007 => Some(Mode::ALTERNATE_SCROLL),
            2004 => Some(Mode::BRACKETED_PASTE),
            2026 => Some(Mode::SYNC_UPDATE),
            _ => None,
        };
        self.write_reply(format!(
            "\x1b[?{};{}$y",
            mode,
            flag.map_or(0, |flag| if self.mode().contains(flag) { 1 } else { 2 })
        ));
    }
    pub(crate) fn set_color(&mut self, index: usize, color: Rgb) {
        if let Some(slot) = self.colors.get_mut(index)
            && *slot != Some(color)
        {
            *slot = Some(color);
            self.damage_all();
        }
    }
    pub(crate) fn reset_color(&mut self, index: usize) {
        if let Some(slot) = self.colors.get_mut(index)
            && slot.take().is_some()
        {
            self.damage_all();
        }
    }
    pub(crate) fn dynamic_color_sequence(
        &mut self,
        prefix: String,
        index: usize,
        terminator: &str,
    ) {
        let color = if index < 256 {
            Color::Indexed(index as u8)
        } else {
            Color::Named(match index {
                257 => NamedColor::Background,
                258 => NamedColor::Cursor,
                _ => NamedColor::Foreground,
            })
        };
        let color = self.color(color);
        self.write_reply(format!(
            "\x1b]{prefix};rgb:{:04x}/{:04x}/{:04x}{terminator}",
            u16::from(color.r) * 257,
            u16::from(color.g) * 257,
            u16::from(color.b) * 257
        ));
    }
    pub(crate) fn clipboard_store(&mut self, clipboard: u8, data: &[u8]) {
        if !matches!(clipboard, b'c' | b'p' | b's') {
            return;
        }
        if let Some(text) = decode_utf8(data) {
            self.events.push(Event::ClipboardStore {
                primary: clipboard == b'p',
                text,
            });
        }
    }
    pub(crate) fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        if matches!(clipboard, b'c' | b'p' | b's') {
            self.write_reply(format!("\x1b]52;{};{terminator}", char::from(clipboard)));
        }
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
            self.titles.push(self.title.clone());
        }
    }
    pub(crate) fn pop_title(&mut self) {
        if let Some(title) = self.titles.pop() {
            self.set_title(title.map(Cow::Owned));
        }
    }
    pub(crate) fn text_area_size_pixels(&mut self) {
        self.write_reply(format!(
            "\x1b[4;{};{}t",
            u32::from(self.size.num_lines) * u32::from(self.size.cell_height),
            u32::from(self.size.num_cols) * u32::from(self.size.cell_width)
        ));
    }
    pub(crate) fn text_area_size_chars(&mut self) {
        self.write_reply(format!(
            "\x1b[8;{};{}t",
            self.size.num_lines, self.size.num_cols
        ));
    }
    pub(crate) fn report_keyboard_mode(&mut self) {
        self.write_reply("\x1b[?0u");
    }
    pub(crate) fn report_modify_other_keys(&mut self) {
        self.write_reply("\x1b[>4;0m");
    }
}

#[cfg(feature = "tui")]
impl Core {
    pub fn enable_history(&mut self, config: HistoryConfig) -> std::io::Result<()> {
        self.history = Some(DiskHistory {
            io: History::new(config)?,
            first: 0,
            hot_start: 0,
            spool_start: 0,
            stored_end: 0,
            spool: VecDeque::new(),
            epoch: 0,
            page_start: 0,
            page: Vec::new(),
            requested: None,
            blank: Row::new(self.columns(), Cell::default()),
            desired_size: None,
            resizing: None,
            clear_pending: false,
            clear_requested: false,
        });
        self.spill_history();
        Ok(())
    }
    pub fn history_event_fd(&self) -> Option<std::os::fd::RawFd> {
        self.history.as_ref().map(|history| history.io.event_fd())
    }
    pub fn history_loading(&self) -> bool {
        self.history.as_ref().is_some_and(|h| {
            h.requested.is_some()
                || h.resizing.is_some()
                || h.desired_size.is_some()
                || h.clear_pending
        })
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
        if history.clear_pending {
            if !history.clear_requested {
                match history.io.clear() {
                    Ok(()) => history.clear_requested = true,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => (),
                    Err(error) => self.events.push(Event::HistoryError(error)),
                }
            }
            return;
        }
        while history.spool.len() >= 32 || (final_batch && !history.spool.is_empty()) {
            let count = history.spool.len().min(32);
            let rows = history.spool.drain(..count).collect();
            match history.io.append(history.spool_start, rows) {
                Ok(()) => history.spool_start += count as u64,
                Err(rows) => {
                    for row in rows.into_iter().rev() {
                        history.spool.push_front(row);
                    }
                    break;
                }
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
        if history.page_start == start
            && history.page.len() >= lines.min((history.hot_start - start) as usize)
        {
            return;
        }
        if history
            .requested
            .is_some_and(|(_, old_start)| old_start == start)
        {
            return;
        }
        history.epoch += 1;
        if history.spool.is_empty()
            && history.io.page(
                history.epoch,
                start,
                lines.min((history.hot_start - start) as usize),
            )
        {
            history.requested = Some((history.epoch, start));
        }
    }
    pub(crate) fn poll_history(&mut self, read_notification: bool) -> std::io::Result<bool> {
        let mut changed = false;
        let mut page_loaded = false;
        let mut notified = false;
        let mut reflow = None;
        if let Some(history) = &mut self.history {
            notified = read_notification && history.io.drain()?;
            history.io.check_budget();
            for reply in history.io.poll() {
                match reply {
                    Reply::Stored { first, end } => {
                        if history.clear_pending {
                            continue;
                        }
                        history.first = first;
                        history.stored_end = end;
                        changed = true;
                    }
                    Reply::Cleared => {
                        history.clear_pending = false;
                        history.clear_requested = false;
                        changed = true;
                    }
                    Reply::Page {
                        epoch,
                        start,
                        first,
                        rows,
                    } => {
                        if history
                            .requested
                            .is_some_and(|(requested, _)| requested == epoch)
                        {
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
                    Reply::Error(error) => return Err(error),
                }
            }
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
            #[cfg(feature = "tui")]
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
        if history.resizing.is_some() || history.clear_pending || !history.spool.is_empty() {
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
        let main = if self.mode.contains(Mode::ALT_SCREEN) {
            &self.inactive
        } else {
            &self.active
        };
        let base = history.hot_start + main.rows.len() as u64 - self.screen_lines() as u64;
        let absolute = |point: Point| {
            (
                base.saturating_add_signed(i64::from(point.line)),
                point.column,
            )
        };
        let mut cursor = main.cursor.point;
        cursor.column += usize::from(main.cursor.wrap_pending);
        let mut saved = main.saved.point;
        saved.column += usize::from(main.saved.wrap_pending);
        let mut points = vec![absolute(cursor), absolute(saved)];
        let selection = if self.mode.contains(Mode::ALT_SCREEN) {
            None
        } else {
            self.selection
        };
        if let Some(selection) = selection {
            points.extend([absolute(selection.start), absolute(selection.end)]);
        }
        #[cfg(feature = "tui")]
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
        let history = self.history.as_mut().unwrap();
        if history.io.reflow(request).is_ok() {
            history.resizing = Some(PendingResize {
                size,
                selection,
                #[cfg(feature = "tui")]
                block_points,
                at_bottom: main.offset == 0,
            });
            history.desired_size = None;
            history.requested = None;
            history.epoch += 1;
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
        main.cursor.point = point(points[0]).unwrap_or_default();
        main.saved.point = point(points[1]).unwrap_or_default();
        for cursor in [&mut main.cursor, &mut main.saved] {
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
        #[cfg(feature = "tui")]
        for (id, indices) in pending.block_points {
            if let Some(block) = self.blocks.iter_mut().find(|block| block.id == id) {
                let convert = |index: Option<usize>| {
                    index
                        .and_then(|index| points[index])
                        .map(|(row, column)| BlockPoint { row, column })
                };
                let prompt = convert(indices[0]);
                if let Some(prompt) = prompt {
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
        #[cfg(feature = "tui")]
        {
            self.block_origin = base;
        }
        self.size = pending.size;
        self.scroll_top = 0;
        self.scroll_bottom = lines;
        self.tabs = (0..columns).map(|x| x != 0 && x % 8 == 0).collect();
        self.dirty = vec![true; lines];
        history.first = first;
        history.hot_start = hot_start;
        history.spool_start = hot_start;
        history.stored_end = hot_start;
        history.page.clear();
        history.requested = None;
        history.blank = Row::new(columns, Cell::default());
    }
    fn clear_disk_history(&mut self) {
        if let Some(history) = &mut self.history {
            let desired_size = history
                .desired_size
                .or_else(|| history.resizing.as_ref().map(|pending| pending.size));
            history.clear_pending = true;
            history.clear_requested = false;
            history.first = 0;
            history.hot_start = 0;
            history.spool_start = 0;
            history.stored_end = 0;
            history.spool.clear();
            history.page.clear();
            history.requested = None;
            history.epoch += 1;
            history.resizing = None;
            history.desired_size = desired_size;
        }
        self.flush_history(true);
    }
}

#[cfg(feature = "tui")]
impl Core {
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
        let mut decoded = Vec::new();
        let mut bytes = uri[slash..].iter().copied();
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                let Some(high) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                    return;
                };
                let Some(low) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                    return;
                };
                let byte = (high * 16 + low) as u8;
                if byte == 0 {
                    return;
                }
                decoded.push(byte);
            } else {
                if byte == 0 {
                    return;
                }
                decoded.push(byte);
            }
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
            column: self.active.cursor.point.column + usize::from(self.active.cursor.wrap_pending),
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
                });
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
                    block.output = Some(point);
                }
            }
            b"D" => {
                if let Some(block) = self.blocks.back_mut()
                    && block.end.is_none()
                {
                    block.end = Some(point);
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
            for point in [&mut block.command, &mut block.output, &mut block.end]
                .into_iter()
                .flatten()
            {
                if point.row >= low && point.row < high {
                    let moved = point.row.saturating_add_signed(i64::from(delta));
                    if moved < low || moved >= high {
                        block.incomplete = true;
                    }
                    point.row = moved.clamp(low, high - 1);
                }
            }
            if block.prompt.row >= low && block.prompt.row < high {
                let moved = block.prompt.row.saturating_add_signed(i64::from(delta));
                if moved < low || moved >= high {
                    block.incomplete = true;
                }
                block.prompt.row = moved.clamp(low, high - 1);
            }
        }
    }
    fn trim_blocks(&mut self) {
        let first = self.block_origin.saturating_sub(self.history_size() as u64);
        while self
            .blocks
            .front()
            .is_some_and(|block| block.end.is_some_and(|end| end.row < first))
        {
            self.blocks.pop_front();
        }
        // Repeated prompt redraws cannot grow metadata without retained terminal rows.
        let limit = self
            .history_size()
            .saturating_add(self.screen_lines())
            .max(1);
        while self.blocks.len() > limit {
            self.blocks.pop_front();
        }
    }
    pub fn recent_command_blocks(&self) -> Vec<CommandBlock> {
        let main = if self.mode.contains(Mode::ALT_SCREEN) {
            &self.inactive
        } else {
            &self.active
        };
        let first = self.history.as_ref().map_or_else(
            || {
                self.block_origin
                    .saturating_sub((main.rows.len() - self.screen_lines()) as u64)
            },
            |history| history.first,
        );
        self.blocks
            .iter()
            .skip(self.blocks.len().saturating_sub(4))
            .map(|block| CommandBlock {
                id: block.id,
                complete: block.end.is_some()
                    && !block.incomplete
                    && block.prompt.row >= first
                    && block.output.is_none_or(|point| point.row >= first),
                status: block.status,
                has_output: block.output.is_some(),
            })
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
            .map(|block| {
                let incomplete = block.incomplete
                    || block.prompt.row < first
                    || block.output.is_some_and(|point| point.row < first);
                BlockMarker {
                    id: block.id,
                    line: (block.prompt.row.max(top) as i64 - self.block_origin as i64) as i32,
                    complete: block.end.is_some() && !incomplete,
                    status: block.status,
                    has_output: block.output.is_some(),
                    output: block.output.map(|start| {
                        let end = block.end.unwrap_or(BlockPoint {
                            row: self.block_origin + self.active.cursor.point.line as u64,
                            column: self.active.cursor.point.column
                                + usize::from(self.active.cursor.wrap_pending),
                        });
                        let point = |point: BlockPoint| Point {
                            line: (i128::from(point.row) - i128::from(self.block_origin))
                                .clamp(i128::from(i32::MIN), i128::from(i32::MAX))
                                as i32,
                            column: point.column,
                        };
                        (point(start), point(end))
                    }),
                }
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
        self.scroll_display(Scroll::Delta(offset as i32 - self.display_offset() as i32))
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
        self.scroll_display(Scroll::Delta(offset as i32 - self.display_offset() as i32));
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
        query: String,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
        wake: std::os::fd::OwnedFd,
    ) -> std::io::Result<std::sync::mpsc::Receiver<std::io::Result<crate::search::SearchResult>>>
    {
        let alternate = self.mode.contains(Mode::ALT_SCREEN);
        if !alternate {
            self.flush_history(true);
            self.require_history_ready_for_copy()?;
        }
        let history = self.history.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "pane search requires workspace history",
            )
        })?;
        let hot_start = if alternate { 0 } else { history.hot_start };
        history.io.search(
            query,
            (hot_start, self.active.rows.iter().cloned().collect()),
            (self.columns(), alternate),
            cancelled,
            wake,
        )
    }
    pub(crate) fn search_select(
        &mut self,
        result: &crate::search::SearchResult,
        index: usize,
    ) -> std::io::Result<()> {
        if result.columns != self.columns()
            || result.alternate != self.mode.contains(Mode::ALT_SCREEN)
        {
            return Err(std::io::Error::other(
                "pane changed size or screen; refresh the search",
            ));
        }
        let found = result.matches.get(index).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "search result no longer exists",
            )
        })?;
        let base = self.search_base();
        let first = base.saturating_sub(self.history_size() as u64);
        if found.start.0 < first || found.end.0 >= base + self.screen_lines() as u64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "search result was evicted from history",
            ));
        }
        let top = found
            .start
            .0
            .saturating_sub((self.screen_lines() / 2) as u64);
        let offset = base.saturating_sub(top).min(self.history_size() as u64) as usize;
        self.scroll_display(Scroll::Delta(offset as i32 - self.display_offset() as i32));
        self.selection_start(
            Point {
                line: (i128::from(found.start.0) - i128::from(base)) as i32,
                column: usize::from(found.start.1),
            },
            Side::Left,
            SelectionType::Simple,
        );
        self.selection_update(
            Point {
                line: (i128::from(found.end.0) - i128::from(base)) as i32,
                column: usize::from(found.end.1.saturating_sub(1)),
            },
            Side::Right,
        );
        self.damage_all();
        Ok(())
    }
    pub fn copy_block(
        &mut self,
        id: u64,
    ) -> std::io::Result<std::sync::mpsc::Receiver<std::io::Result<HistoryCopy>>> {
        self.flush_history(true);
        self.require_history_ready_for_copy()?;
        let block = self
            .blocks
            .iter()
            .find(|block| block.id == id)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "command block no longer retained",
                )
            })?;
        let start = block.output.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "command has no output marker",
            )
        })?;
        let end = block.end.unwrap_or(BlockPoint {
            row: self.block_origin + self.active.cursor.point.line as u64,
            column: self.active.cursor.point.column + usize::from(self.active.cursor.wrap_pending),
        });
        let history = self.history.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "disk history is not enabled",
            )
        })?;
        let first = self.block_origin.saturating_sub(self.history_size() as u64);
        let incomplete = block.incomplete || block.prompt.row < first || start.row < first;
        let (hot_start, hot) = self.copy_hot_rows(history.hot_start, start.row, end.row);
        history.io.copy(
            (start.row, start.column),
            (end.row, end.column),
            hot_start,
            hot,
            None,
            incomplete,
        )
    }
}

#[cfg(feature = "tui")]
impl Core {
    fn copy_hot_rows(&self, hot_start: u64, start: u64, end: u64) -> (u64, Vec<Row>) {
        let count = self.active.rows.len() as u64;
        let from = start.saturating_sub(hot_start).min(count) as usize;
        let to = end.saturating_add(1).saturating_sub(hot_start).min(count) as usize;
        // A running command may move its cursor above its output marker.
        (
            hot_start + from as u64,
            self.active
                .rows
                .range(from..to.max(from))
                .cloned()
                .collect(),
        )
    }

    fn require_history_ready_for_copy(&self) -> std::io::Result<()> {
        if self.mode.contains(Mode::SYNC_UPDATE)
            || self.history.as_ref().is_some_and(|history| {
                !history.spool.is_empty()
                    || history.clear_pending
                    || history.resizing.is_some()
                    || history.desired_size.is_some()
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "history is still being committed or resized",
            ));
        }
        Ok(())
    }
    pub fn copy_selection(
        &mut self,
    ) -> std::io::Result<std::sync::mpsc::Receiver<std::io::Result<HistoryCopy>>> {
        self.flush_history(true);
        self.require_history_ready_for_copy()?;
        let range = self
            .selection_range()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no selection"))?;
        let history = self.history.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "disk history is not enabled",
            )
        })?;
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
            self.copy_hot_rows(hot_start, start.0, end.0)
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
mod tests;

#[cfg(all(test, feature = "tui"))]
mod disk_tests;

#[cfg(all(test, feature = "tui"))]
mod history_boundary_tests;
