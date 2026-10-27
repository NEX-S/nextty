use crate::core::{Flags, Row};
use crate::workspace::{Action, Client, PaneId};
use std::collections::VecDeque;
use std::fmt::Write;
use std::io;

const MATCH_LIMIT: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: (u64, u16),
    pub end: (u64, u16),
}

pub(crate) struct SearchResult {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub columns: usize,
    pub alternate: bool,
}

pub(crate) struct Matcher {
    pattern: Vec<char>,
    prefix: Vec<usize>,
    matched: usize,
    positions: VecDeque<(u64, u16)>,
    matches: VecDeque<SearchMatch>,
    truncated: bool,
}
impl Matcher {
    pub fn new(query: &str) -> Self {
        let pattern: Vec<char> = query.chars().collect();
        let mut prefix = vec![0; pattern.len()];
        for index in 1..pattern.len() {
            let mut matched = prefix[index - 1];
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = prefix[matched - 1];
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self {
            positions: VecDeque::with_capacity(pattern.len()),
            pattern,
            prefix,
            matched: 0,
            matches: VecDeque::new(),
            truncated: false,
        }
    }
    fn character(&mut self, character: char, start: (u64, u16), end: (u64, u16)) {
        if self.pattern.is_empty() {
            return;
        }
        while self.matched > 0 && character != self.pattern[self.matched] {
            self.matched = self.prefix[self.matched - 1];
        }
        if character == self.pattern[self.matched] {
            self.matched += 1;
            // Retain the matched suffix after fallback or an overlapping match.
            while self.positions.len() >= self.matched {
                self.positions.pop_front();
            }
            self.positions.push_back(start);
        } else {
            self.positions.clear();
        }
        if self.matched == self.pattern.len() {
            self.matches.push_back(SearchMatch {
                start: *self.positions.front().unwrap(),
                end,
            });
            if self.matches.len() > MATCH_LIMIT {
                self.matches.pop_front();
                self.truncated = true;
            }
            self.matched = self.prefix[self.matched - 1];
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
        for (column, cell, extra) in row.cells_with_extras(0..row.used()) {
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let start = (id, column as u16);
            let end = (
                id,
                (column
                    + if cell.flags.contains(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    }) as u16,
            );
            self.character(
                if cell.flags.contains(Flags::HIDDEN) {
                    ' '
                } else {
                    cell.c
                },
                start,
                end,
            );
            if !cell.flags.contains(Flags::HIDDEN)
                && let Some(extra) = extra
            {
                for &character in &extra.zerowidth {
                    self.character(character, start, end);
                }
            }
        }
        if !row.wrapped {
            self.line_break();
        }
    }
    pub fn finish(self, columns: usize, alternate: bool) -> SearchResult {
        SearchResult {
            matches: self.matches.into(),
            truncated: self.truncated,
            columns,
            alternate,
        }
    }
}

#[derive(Default)]
pub(crate) struct Search {
    pub open: bool,
    pub pane: PaneId,
    pub query: String,
    pub cursor: usize,
    pub index: usize,
    pub pending: bool,
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    controller: bool,
    request: u64,
    utf8: Vec<u8>,
}
impl Search {
    pub fn begin(&mut self, client: &mut Client, pane: PaneId) -> io::Result<()> {
        if self.open {
            self.close(client)?;
        }
        self.open = true;
        self.controller = client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller);
        self.pane = pane;
        self.query.clear();
        self.cursor = 0;
        self.submit(client)
    }
    pub fn observe(&mut self, client: &mut Client) -> io::Result<bool> {
        if self.open
            && !client.snapshot().is_some_and(|snapshot| {
                snapshot.controller == self.controller
                    && snapshot.panes.iter().any(|pane| pane.id == self.pane)
            })
        {
            self.close(client)?;
            return Ok(true);
        }
        Ok(false)
    }
    fn submit(&mut self, client: &mut Client) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        self.request += 1;
        self.index = 0;
        self.matches = Vec::new();
        self.truncated = false;
        self.pending = !self.query.is_empty();
        client.send(Action::Search {
            pane: self.pane,
            request: self.request,
            query: self.query.clone(),
        })
    }
    #[cfg(any(feature = "gui", test))]
    pub fn edit(&mut self, client: &mut Client, text: &str) -> io::Result<()> {
        let text: String = text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if text.is_empty() {
            return Ok(());
        }
        self.query.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.submit(client)
    }
    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        self.open = false;
        self.pending = false;
        self.matches = Vec::new();
        self.utf8.clear();
        client.send(Action::SearchCancel {
            pane: self.pane,
            request: self.request,
        })
    }
    pub fn next(&mut self, client: &mut Client, previous: bool) -> io::Result<()> {
        self.observe(client)?;
        if !self.open || !self.controller || self.pending || self.matches.is_empty() {
            return Ok(());
        }
        self.index = if previous {
            (self.index + self.matches.len() - 1) % self.matches.len()
        } else {
            (self.index + 1) % self.matches.len()
        };
        client.send(Action::SearchSelect {
            pane: self.pane,
            request: self.request,
            index: self.index as u32,
        })
    }
    pub fn result(
        &mut self,
        client: &mut Client,
        pane: PaneId,
        request: u64,
        matches: Vec<SearchMatch>,
        truncated: bool,
    ) -> io::Result<bool> {
        self.observe(client)?;
        if !self.open || pane != self.pane || request != self.request {
            return Ok(false);
        }
        self.pending = false;
        self.index = matches.len().saturating_sub(1);
        self.matches = matches;
        self.truncated = truncated;
        if self.controller && !self.matches.is_empty() {
            client.send(Action::SearchSelect {
                pane,
                request,
                index: self.index as u32,
            })?;
        }
        Ok(true)
    }
    pub fn key(
        &mut self,
        client: &mut Client,
        symbol: u32,
        shift: bool,
        control: bool,
    ) -> io::Result<bool> {
        if !self.open {
            return Ok(false);
        }
        match symbol {
            0xff1b => self.close(client)?,
            0xff0d | 0xff8d => self.next(client, shift)?,
            0xff52 => self.next(client, true)?,
            0xff54 => self.next(client, false)?,
            0xff51 => {
                self.cursor = self.query[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index)
            }
            0xff53 => {
                self.cursor += self.query[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
            }
            0xff50 => self.cursor = 0,
            0xff57 => self.cursor = self.query.len(),
            0xff08 => {
                let start = self.query[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index);
                self.utf8.clear();
                if start != self.cursor {
                    self.query.replace_range(start..self.cursor, "");
                    self.cursor = start;
                    self.submit(client)?;
                }
            }
            0xffff => {
                let end = self.cursor
                    + self.query[self.cursor..]
                        .chars()
                        .next()
                        .map_or(0, char::len_utf8);
                self.utf8.clear();
                if end != self.cursor {
                    self.query.replace_range(self.cursor..end, "");
                    self.submit(client)?;
                }
            }
            _ if control && matches!(char::from_u32(symbol), Some('a' | 'A' | 'u' | 'U')) => {
                self.utf8.clear();
                if !self.query.is_empty() {
                    self.query.clear();
                    self.cursor = 0;
                    self.submit(client)?;
                }
            }
            _ => (),
        }
        Ok(true)
    }
    pub fn bytes(&mut self, client: &mut Client, bytes: &[u8], pasted: bool) -> io::Result<bool> {
        if !self.open && !pasted {
            return Ok(false);
        }
        if !pasted && bytes.first() == Some(&27) {
            let (symbol, shift) = match bytes {
                b"\x1b" => (0xff1b, false),
                b"\x1b[A" | b"\x1bOA" => (0xff52, false),
                b"\x1b[B" | b"\x1bOB" => (0xff54, false),
                b"\x1b[C" | b"\x1bOC" => (0xff53, false),
                b"\x1b[D" | b"\x1bOD" => (0xff51, false),
                b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" => (0xff50, false),
                b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" => (0xff57, false),
                b"\x1b[3~" => (0xffff, false),
                b"\x1b[13;2u" | b"\x1b[27;2;13~" => (0xff0d, true),
                _ => return Ok(true),
            };
            return self.key(client, symbol, shift, false);
        }
        if pasted
            && self.utf8.is_empty()
            && !bytes.is_empty()
            && let Ok(text) = std::str::from_utf8(bytes)
            && bytes.iter().all(|&byte| byte >= 32 && byte != 127)
        {
            self.query.insert_str(self.cursor, text);
            self.cursor += text.len();
            self.submit(client)?;
            return Ok(true);
        }
        let mut changed = false;
        for &byte in bytes {
            if !pasted {
                match byte {
                    8 | 127 => {
                        let start = self.query[..self.cursor]
                            .char_indices()
                            .next_back()
                            .map_or(0, |(index, _)| index);
                        changed |= start != self.cursor;
                        self.query.replace_range(start..self.cursor, "");
                        self.cursor = start;
                        self.utf8.clear();
                        continue;
                    }
                    1 | 21 => {
                        changed |= !self.query.is_empty();
                        self.query.clear();
                        self.cursor = 0;
                        self.utf8.clear();
                        continue;
                    }
                    13 | 10 => {
                        if changed {
                            self.submit(client)?;
                            changed = false;
                        }
                        self.next(client, false)?;
                        continue;
                    }
                    _ => (),
                }
            }
            if byte < 32 || byte == 127 {
                continue;
            }
            self.utf8.push(byte);
            match std::str::from_utf8(&self.utf8) {
                Ok(text) => {
                    self.query.insert_str(self.cursor, text);
                    self.cursor += text.len();
                    self.utf8.clear();
                    changed = true;
                }
                Err(error) if error.error_len().is_some() => self.utf8.clear(),
                Err(_) => (),
            }
        }
        if changed {
            self.submit(client)?;
        }
        Ok(true)
    }
    pub fn label(&self, preedit: &str) -> String {
        let mut label = format!(
            "⌕ {}{}▏{}  ",
            &self.query[..self.cursor],
            preedit,
            &self.query[self.cursor..]
        );
        if self.pending {
            label.push_str("searching");
        } else {
            let index = if self.matches.is_empty() {
                0
            } else {
                self.index + 1
            };
            write!(label, "{index}/{}", self.matches.len()).unwrap();
        }
        if self.truncated {
            label.push_str("+ (partial)");
        }
        if !self.controller {
            label.push_str(" (read-only)");
        }
        label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, WindowSize};
    #[test]
    fn label_inserts_preedit_at_the_cursor_after_a_literal_caret_marker() {
        let search = Search {
            query: "A▏中B".into(),
            cursor: "A▏中".len(),
            controller: true,
            ..Search::default()
        };
        assert_eq!(search.label("预"), "⌕ A▏中预▏B  0/0");
        assert_eq!(search.label(""), "⌕ A▏中▏B  0/0");
    }
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
            matcher.finish(4, false).matches,
            vec![SearchMatch {
                start: (10, 2),
                end: (11, 1)
            }]
        );
        let mut matcher = Matcher::new("中");
        matcher.row(10, core.row(0));
        assert_eq!(
            matcher.finish(4, false).matches,
            vec![SearchMatch {
                start: (10, 0),
                end: (10, 2)
            }]
        );
        let mut matcher = Matcher::new("\u{301}");
        matcher.row(10, core.row(0));
        assert_eq!(
            matcher.finish(4, false).matches,
            vec![SearchMatch {
                start: (10, 2),
                end: (10, 3)
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
                .map(|found| (found.start, found.end))
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
        assert_eq!(result.matches[0].start, (2, 0));
    }
}
