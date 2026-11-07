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

pub(crate) struct SearchResult {
    pub matches: Box<[SearchMatch]>,
    pub truncated: bool,
    pub columns: usize,
    pub alternate: bool,
}

pub(crate) struct Matcher {
    // Queries fit in a wire frame, so fallback positions fit in u32.
    pattern: Box<[(char, u32)]>,
    matched: usize,
    positions: VecDeque<(u64, u16)>,
    matches: VecDeque<SearchMatch>,
    truncated: bool,
}
impl Matcher {
    pub fn new(query: &str) -> Self {
        let mut pattern = Vec::with_capacity(query.chars().count());
        pattern.extend(query.chars().map(|character| (character, 0)));
        for index in 1..pattern.len() {
            let mut matched = pattern[index - 1].1 as usize;
            while matched > 0 && pattern[index].0 != pattern[matched].0 {
                matched = pattern[matched - 1].1 as usize;
            }
            if pattern[index].0 == pattern[matched].0 {
                matched += 1;
            }
            pattern[index].1 = matched as u32;
        }
        Self {
            positions: VecDeque::new(),
            pattern: pattern.into_boxed_slice(),
            matched: 0,
            matches: VecDeque::new(),
            truncated: false,
        }
    }
    fn character(&mut self, character: char, start: (u64, u16), end: (u64, u16)) {
        while self.matched > 0 && character != self.pattern[self.matched].0 {
            self.matched = self.pattern[self.matched - 1].1 as usize;
        }
        if character == self.pattern[self.matched].0 {
            self.matched += 1;
            // Retain the matched suffix after fallback or an overlapping match.
            if self.positions.len() >= self.matched {
                self.positions
                    .drain(..self.positions.len() - self.matched + 1);
            }
            self.positions.push_back(start);
        } else {
            self.positions.clear();
        }
        if self.matched == self.pattern.len() {
            let &(start_row, start_column) = self.positions.front().unwrap();
            self.matches.push_back(SearchMatch {
                start_row,
                end_row: end.0,
                start_column,
                end_column: end.1,
            });
            if self.matches.len() > MATCH_LIMIT {
                self.matches.pop_front();
                self.truncated = true;
            }
            self.matched = self.pattern[self.matched - 1].1 as usize;
        }
    }
    pub fn line_break(&mut self) {
        self.matched = 0;
        self.positions.clear();
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

    #[test]
    fn literal_matches_preserve_wide_combining_and_soft_wrapped_cell_ranges() {
        let mut core = Core::new(
            WindowSize {
                num_cols: 4,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(&mut core, "中a\u{301}bc".as_bytes());
        let mut matcher = Matcher::new("a\u{301}bc");
        matcher.row(10, core.row(0));
        matcher.row(11, core.row(1));
        assert_eq!(
            matcher.finish(4, false).matches.as_ref(),
            vec![SearchMatch {
                start_row: 10,
                end_row: 11,
                start_column: 2,
                end_column: 1,
            }]
        );
        let mut matcher = Matcher::new("中");
        matcher.row(10, core.row(0));
        assert_eq!(
            matcher.finish(4, false).matches.as_ref(),
            vec![SearchMatch {
                start_row: 10,
                end_row: 10,
                start_column: 0,
                end_column: 2,
            }]
        );
        let mut matcher = Matcher::new("\u{301}");
        matcher.row(10, core.row(0));
        assert_eq!(
            matcher.finish(4, false).matches.as_ref(),
            vec![SearchMatch {
                start_row: 10,
                end_row: 10,
                start_column: 2,
                end_column: 3,
            }]
        );
        crate::vt::Parser::new().advance(&mut core, b"\x1b[2J\x1b[Hababacaba");
        let mut matcher = Matcher::new("aba");
        for row in 0..3 {
            matcher.row(10 + row as u64, core.row(row));
        }
        assert_eq!(
            matcher
                .finish(4, false)
                .matches
                .iter()
                .map(|found| (
                    (found.start_row, found.start_column),
                    (found.end_row, found.end_column)
                ))
                .collect::<Vec<_>>(),
            [((10, 0), (10, 3)), ((10, 2), (11, 1)), ((11, 2), (12, 1))]
        );
    }
    #[test]
    fn results_are_bounded_and_do_not_join_hard_lines_or_reveal_hidden_text() {
        let mut core = Core::new(
            WindowSize {
                num_cols: 20,
                num_lines: 3,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(&mut core, b"ab\r\ncd\x1b[8msecret");
        for query in ["bc", "secret"] {
            let mut matcher = Matcher::new(query);
            matcher.row(0, core.row(0));
            matcher.row(1, core.row(1));
            assert!(matcher.finish(20, false).matches.is_empty());
        }
        let mut matcher = Matcher::new("a");
        for row in 0..MATCH_LIMIT as u64 + 2 {
            matcher.row(row, core.row(0));
        }
        let result = matcher.finish(20, false);
        assert_eq!(result.matches.len(), MATCH_LIMIT);
        assert!(result.truncated);
        assert_eq!(
            (result.matches[0].start_row, result.matches[0].start_column),
            (2, 0)
        );
    }
}
