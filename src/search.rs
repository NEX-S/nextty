use crate::core::Row;
use crate::input::keysym;
use crate::workspace::{Action, Client, PaneId};
use std::collections::VecDeque;
use std::io;

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
    pattern: Vec<char>,
    // Queries fit in a wire frame, so prefix lengths fit in u32.
    prefix: Vec<u32>,
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
            let mut matched = prefix[index - 1] as usize;
            while matched > 0 && pattern[index] != pattern[matched] {
                matched = prefix[matched - 1] as usize;
            }
            if pattern[index] == pattern[matched] {
                matched += 1;
            }
            prefix[index] = matched as u32;
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
            self.matched = self.prefix[self.matched - 1] as usize;
        }
        if character == self.pattern[self.matched] {
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
            self.matched = self.prefix[self.matched - 1] as usize;
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

#[derive(Default)]
pub(crate) struct Search {
    pub open: bool,
    pub pane: PaneId,
    pub query: String,
    pub cursor: usize,
    pub index: usize,
    pub pending: bool,
    pub count: usize,
    pub truncated: bool,
    controller: bool,
    request: u64,
}
impl Search {
    pub fn begin(&mut self, client: &mut Client, pane: PaneId) -> io::Result<()> {
        self.close(client)?;
        self.open = true;
        self.controller = client
            .snapshot()
            .is_some_and(|snapshot| snapshot.controller);
        self.pane = pane;
        self.query.clear();
        self.cursor = 0;
        self.submit(client)
    }
    pub fn observe(&mut self, client: &mut Client) -> io::Result<()> {
        if self.open
            && !client.snapshot().is_some_and(|snapshot| {
                snapshot.controller == self.controller && snapshot.pane(self.pane).is_some()
            })
        {
            self.close(client)?;
        }
        Ok(())
    }
    fn submit(&mut self, client: &mut Client) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        self.request += 1;
        self.index = 0;
        self.count = 0;
        self.truncated = false;
        self.pending = !self.query.is_empty();
        client.send(Action::Search {
            pane: self.pane,
            request: self.request,
            query: self.query.clone(),
        })
    }
    pub fn edit(&mut self, client: &mut Client, text: &str) -> io::Result<()> {
        let text = crate::input::without_controls(text);
        if text.is_empty() {
            return Ok(());
        }
        self.query.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.submit(client)
    }
    pub fn close(&mut self, client: &mut Client) -> io::Result<()> {
        if !std::mem::take(&mut self.open) {
            return Ok(());
        }
        self.pending = false;
        self.count = 0;
        client.send(Action::SearchCancel {
            pane: self.pane,
            request: self.request,
        })
    }
    pub fn next(&mut self, client: &mut Client, previous: bool) -> io::Result<()> {
        self.observe(client)?;
        if !self.open || !self.controller || self.pending || self.count == 0 {
            return Ok(());
        }
        self.index = if previous {
            (self.index + self.count - 1) % self.count
        } else {
            (self.index + 1) % self.count
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
        count: usize,
        truncated: bool,
    ) -> io::Result<bool> {
        self.observe(client)?;
        if !self.open || pane != self.pane || request != self.request {
            return Ok(false);
        }
        self.pending = false;
        self.index = count.saturating_sub(1);
        self.count = count;
        self.truncated = truncated;
        if self.controller && self.count != 0 {
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
    ) -> io::Result<()> {
        if !self.open {
            return Ok(());
        }
        match symbol {
            keysym::ESCAPE => self.close(client)?,
            keysym::RETURN | keysym::KP_ENTER => self.next(client, shift)?,
            keysym::UP => self.next(client, true)?,
            keysym::DOWN => self.next(client, false)?,
            keysym::LEFT => self.cursor = self.previous(),
            keysym::RIGHT => self.cursor = self.following(),
            keysym::HOME => self.cursor = 0,
            keysym::END => self.cursor = self.query.len(),
            keysym::BACKSPACE | keysym::DELETE => {
                let range = if symbol == keysym::BACKSPACE {
                    self.previous()..self.cursor
                } else {
                    self.cursor..self.following()
                };
                if self.delete(range) {
                    self.submit(client)?;
                }
            }
            _ if control
                && matches!(char::from_u32(symbol), Some('a' | 'A' | 'u' | 'U'))
                && self.delete(0..self.query.len()) =>
            {
                self.submit(client)?;
            }
            _ => (),
        }
        Ok(())
    }
    fn previous(&self) -> usize {
        self.query
            .floor_char_boundary(self.cursor.saturating_sub(1))
    }
    fn following(&self) -> usize {
        self.query.ceil_char_boundary(self.cursor + 1)
    }
    fn delete(&mut self, range: std::ops::Range<usize>) -> bool {
        self.cursor = range.start;
        let changed = !range.is_empty();
        self.query.replace_range(range, "");
        changed
    }
    pub fn display(&self, preedit: &str, columns: usize) -> (String, usize) {
        use crate::width::{fit_label, scalar_width, text_width};
        let mut status = if self.pending {
            "searching".into()
        } else {
            format!(
                "{}/{}",
                if self.count == 0 { 0 } else { self.index + 1 },
                self.count
            )
        };
        if self.truncated {
            status.push('+');
        }
        if !self.controller {
            status.push_str(if columns >= 40 { " read-only" } else { " RO" });
        }
        let status = fit_label(&status, (columns.saturating_sub(5).min(columns / 2)) as u16);
        let status_width = text_width(&status);
        let budget = columns.saturating_sub(status_width + 4);
        let before = format!("{}{preedit}", &self.query[..self.cursor]);
        let mut width = 0;
        let start = before
            .char_indices()
            .rev()
            .take_while(|(_, ch)| {
                let next = width + usize::from(scalar_width(*ch));
                if next > budget.saturating_sub(1) {
                    false
                } else {
                    width = next;
                    true
                }
            })
            .last()
            .map_or(before.len(), |(index, _)| index);
        let before = &before[start..];
        let after = fit_label(
            &self.query[self.cursor..],
            budget.saturating_sub(width + 1) as u16,
        );
        let caret = 2 + text_width(before);
        let mut label = format!("⌕ {before}▏{after}");
        let padding = columns.saturating_sub(text_width(&label) + status_width);
        label.extend(std::iter::repeat_n(' ', padding));
        label.push_str(&status);
        (
            fit_label(&label, columns as u16).into_owned(),
            caret.min(columns.saturating_sub(1)),
        )
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
        let (label, caret) = search.display("预", 40);
        assert!(label.starts_with("⌕ A▏中预▏B"));
        assert!(label.ends_with("0/0"));
        assert_eq!(caret, 8);
        let (label, caret) = search.display("", 12);
        assert!(label.ends_with("0/0"));
        assert!(crate::width::text_width(&label) <= 12);
        assert!(caret < 12);
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
