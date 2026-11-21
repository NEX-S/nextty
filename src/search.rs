use crate::core::Row;
use std::collections::VecDeque;

const MATCH_LIMIT: usize = 10_000;

// Separate row IDs and columns avoid padding each coordinate pair.
#[derive(Debug, PartialEq)]
pub(crate) struct SearchMatch {
    pub start_row: u64,
    pub end_row: u64,
    pub start_column: u16,
    pub end_column: u16,
}

#[derive(Default)]
pub(crate) struct SearchResult {
    pub matches: Box<[SearchMatch]>,
    pub truncated: bool,
    pub columns: usize,
    pub alternate: bool,
}

pub(crate) struct Matcher {
    pattern: Vec<char>,
    // The last `pattern.len()` characters of the current line with their start positions.
    window: VecDeque<(char, (u64, u16))>,
    matches: VecDeque<SearchMatch>,
    truncated: bool,
}
impl Matcher {
    pub fn new(query: &str) -> Self {
        Self {
            pattern: query.chars().collect(),
            window: VecDeque::new(),
            matches: VecDeque::new(),
            truncated: false,
        }
    }
    fn character(&mut self, character: char, start: (u64, u16), end: (u64, u16)) {
        if self.window.len() == self.pattern.len() {
            self.window.pop_front();
        }
        self.window.push_back((character, start));
        if self.window.len() == self.pattern.len()
            && self
                .window
                .iter()
                .map(|(c, _)| *c)
                .eq(self.pattern.iter().copied())
        {
            self.matches.push_back(SearchMatch {
                start_row: self.window[0].1.0,
                end_row: end.0,
                start_column: self.window[0].1.1,
                end_column: end.1,
            });
            if self.matches.len() > MATCH_LIMIT {
                self.matches.pop_front();
                self.truncated = true;
            }
        }
    }
    pub fn line_break(&mut self) {
        self.window.clear();
    }
    pub fn gap(&mut self) {
        self.line_break();
        self.truncated = true;
    }
    pub fn row(&mut self, id: u64, row: &Row) {
        if self.pattern.is_empty() {
            return;
        }
        for (column, character) in row.characters(0..row.used()) {
            let start = (id, column as u16);
            let end = (id, (column + row.cells[column].width()) as u16);
            self.character(character, start, end);
        }
        if !row.wrapped {
            self.line_break();
        }
    }
    /// A row of unstyled text, one character per column.
    pub fn plain(&mut self, id: u64, text: &str, wrapped: bool) {
        if self.pattern.is_empty() {
            return;
        }
        for (column, character) in text.chars().enumerate() {
            let column = column as u16;
            self.character(character, (id, column), (id, column + 1));
        }
        if !wrapped {
            self.line_break();
        }
    }
    pub fn finish(self, columns: usize, alternate: bool) -> SearchResult {
        SearchResult {
            matches: Vec::from(self.matches).into_boxed_slice(),
            truncated: self.truncated,
            columns,
            alternate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, WindowSize};

    fn core(columns: u16, text: &[u8]) -> Core {
        let size = WindowSize {
            num_cols: columns,
            num_lines: 3,
            cell_width: 0,
            cell_height: 0,
        };
        let mut core = Core::new(size, 0xffffff, 0);
        crate::vt::Parser::new().advance(&mut core, text);
        core
    }
    /// Start and end (row, column) of every match of `query` over the first rows, numbered from 10.
    fn found(core: &Core, query: &str, rows: usize) -> Vec<((u64, u16), (u64, u16))> {
        let mut matcher = Matcher::new(query);
        for row in 0..rows {
            matcher.row(10 + row as u64, core.row(row));
        }
        let matches = matcher.finish(0, false).matches;
        let ends = |m: &SearchMatch| ((m.start_row, m.start_column), (m.end_row, m.end_column));
        matches.iter().map(ends).collect()
    }

    #[test]
    fn literal_matches_preserve_wide_combining_and_soft_wrapped_cell_ranges() {
        let wrapped = core(4, "中a\u{301}bc".as_bytes());
        assert_eq!(found(&wrapped, "a\u{301}bc", 2), [((10, 2), (11, 1))]);
        assert_eq!(found(&wrapped, "中", 1), [((10, 0), (10, 2))]);
        assert_eq!(found(&wrapped, "\u{301}", 1), [((10, 2), (10, 3))]);
        let overlapping = core(4, b"ababacaba");
        assert_eq!(
            found(&overlapping, "aba", 3),
            [((10, 0), (10, 3)), ((10, 2), (11, 1)), ((11, 2), (12, 1))]
        );
    }

    #[test]
    fn results_are_bounded_and_do_not_join_hard_lines_or_reveal_hidden_text() {
        let core = core(20, b"ab\r\ncd\x1b[8msecret");
        assert!(found(&core, "bc", 2).is_empty());
        assert!(found(&core, "secret", 2).is_empty());
        let mut matcher = Matcher::new("a");
        for row in 0..MATCH_LIMIT as u64 + 2 {
            matcher.row(row, core.row(0));
        }
        let result = matcher.finish(20, false);
        assert_eq!(result.matches.len(), MATCH_LIMIT);
        assert!(result.truncated);
        assert_eq!(result.matches[0].start_row, 2);
    }
}
