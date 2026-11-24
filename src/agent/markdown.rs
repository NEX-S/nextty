use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::fmt::Write;
use std::ops::Range;

pub(crate) const BOLD: u8 = 1;
pub(crate) const ITALIC: u8 = 2;
pub(crate) const CODE: u8 = 4;
const LINK: u8 = 8;
const QUOTE: u8 = 16;

pub(crate) fn color(style: u8, foreground: u32) -> u32 {
    let dark_text = (foreground >> 16 & 255) + (foreground >> 8 & 255) + (foreground & 255) < 384;
    if style & CODE != 0 {
        if dark_text { 0x685520 } else { 0xc6b890 }
    } else if style & (BOLD | LINK) != 0 {
        if dark_text { 0x35552a } else { 0xb5d2a2 }
    } else if style & QUOTE != 0 {
        if dark_text { 0x56615f } else { 0x889391 }
    } else {
        foreground
    }
}

#[derive(Default)]
pub(crate) struct Markdown {
    pub text: String,
    // Each span starts at the previous end.
    spans: Vec<(usize, u8)>,
}

impl Markdown {
    fn push(&mut self, text: &str, style: u8) {
        self.text.extend(text.chars().map(|ch| {
            if ch.is_control() && ch != '\n' {
                ' '
            } else {
                ch
            }
        }));
        if let Some((end, previous)) = self.spans.last_mut()
            && *previous == style
        {
            *end = self.text.len();
        } else if !text.is_empty() {
            self.spans.push((self.text.len(), style));
        }
    }

    fn finish_link(&mut self, start: usize, destination: &str) {
        if &self.text[start..] != destination {
            self.push(" (", LINK);
            self.push(destination, LINK);
            self.push(")", LINK);
        }
    }

    fn newline(&mut self, count: usize) {
        if !self.text.is_empty() {
            let existing = self
                .text
                .bytes()
                .rev()
                .take_while(|b| *b == b'\n')
                .take(count)
                .count();
            for _ in existing..count {
                self.push("\n", 0);
            }
        }
    }

    pub fn spans(&self, range: Range<usize>) -> impl Iterator<Item = (Range<usize>, u8)> + '_ {
        let first = self.spans.partition_point(|(end, _)| *end <= range.start);
        self.spans[first..]
            .iter()
            .scan(range.start, move |start, &(end, style)| {
                let from = *start;
                *start = end;
                let to = range.end.min(end);
                (from < to).then_some((from..to, style))
            })
    }

    fn content(&mut self, text: &str, style: u8, quote_depth: usize) {
        if quote_depth == 0 {
            self.push(text, style);
            return;
        }
        for part in text.split_inclusive('\n') {
            if self.text.ends_with('\n') || self.text.is_empty() {
                for _ in 0..quote_depth {
                    self.push("│ ", QUOTE);
                }
            }
            self.push(part, style);
        }
    }

    fn table<'a>(
        &mut self,
        alignments: &[Alignment],
        events: &mut impl Iterator<Item = Event<'a>>,
        quote_depth: usize,
    ) {
        let mut rows: Vec<Vec<Self>> = Vec::new();
        let mut styles: smallvec::SmallVec<[u8; 8]> = smallvec::smallvec![0];
        let mut links = Vec::new();
        for event in events {
            match event {
                Event::End(TagEnd::Table) => break,
                Event::Start(Tag::TableHead | Tag::TableRow) => rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => {
                    rows.last_mut().unwrap().push(Self::default());
                    styles.truncate(1);
                }
                Event::End(TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell) => (),
                event => {
                    let cell = rows.last_mut().unwrap().last_mut().unwrap();
                    let style = *styles.last().unwrap();
                    match event {
                        Event::Start(tag) => {
                            let next = match tag {
                                Tag::Strong => style | BOLD,
                                Tag::Emphasis => style | ITALIC,
                                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                                    links.push((cell.text.len(), dest_url));
                                    style | LINK
                                }
                                _ => style,
                            };
                            styles.push(next);
                        }
                        Event::End(tag) => {
                            styles.pop();
                            if matches!(tag, TagEnd::Link | TagEnd::Image) {
                                let (start, destination) = links.pop().unwrap();
                                cell.finish_link(start, &destination);
                            }
                        }
                        Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                            cell.push(&text, style);
                        }
                        Event::Code(text) => cell.push(&text, style | CODE),
                        Event::SoftBreak | Event::HardBreak => cell.push(" ", style),
                        _ => (),
                    }
                }
            }
        }
        let widths: Vec<_> = (0..alignments.len())
            .map(|column| {
                let mut width = 1;
                for row in &rows {
                    width = width.max(crate::width::text_width(&row[column].text));
                    if width >= 48 {
                        return 48;
                    }
                }
                width
            })
            .collect();
        let spaces = " ".repeat(widths.iter().max().unwrap() + 1);
        self.newline(2);
        for (row_index, row) in rows.into_iter().enumerate() {
            for _ in 0..quote_depth {
                self.push("│ ", QUOTE);
            }
            self.push("│", QUOTE);
            for (column, cell) in row.into_iter().enumerate() {
                let padding = widths[column].saturating_sub(crate::width::text_width(&cell.text));
                let left = match alignments[column] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                self.push(&spaces[..left + 1], 0);
                for (range, style) in cell.spans(0..cell.text.len()) {
                    self.push(
                        &cell.text[range],
                        style | if row_index == 0 { BOLD } else { 0 },
                    );
                }
                self.push(&spaces[..padding - left + 1], 0);
                self.push("│", QUOTE);
            }
            self.push("\n", 0);
            if row_index == 0 {
                for _ in 0..quote_depth {
                    self.push("│ ", QUOTE);
                }
                self.push("├", QUOTE);
                let border = "─".repeat(spaces.len() + 1);
                for (column, width) in widths.iter().enumerate() {
                    self.push(&border[..(width + 2) * "─".len()], QUOTE);
                    self.push(
                        if column + 1 == widths.len() {
                            "┤"
                        } else {
                            "┼"
                        },
                        QUOTE,
                    );
                }
                self.push("\n", 0);
            }
        }
        self.newline(2);
    }

    pub fn render(source: &str) -> Self {
        let mut output = Self::default();
        let mut styles: smallvec::SmallVec<[u8; 8]> = smallvec::smallvec![0];
        let mut lists = Vec::new();
        let mut number_text = String::new();
        let mut links = Vec::new();
        let mut quote_depth = 0;
        let mut events = Parser::new_ext(source, Options::ENABLE_TABLES);
        while let Some(event) = events.next() {
            let style = *styles.last().unwrap();
            match event {
                Event::Start(tag) => {
                    let mut next = style;
                    match tag {
                        Tag::Table(alignments) => {
                            output.table(&alignments, &mut events, quote_depth);
                            continue;
                        }
                        Tag::Heading { .. } => {
                            output.newline(2);
                            next |= BOLD;
                        }
                        Tag::Strong => next |= BOLD,
                        Tag::Emphasis => next |= ITALIC,
                        Tag::CodeBlock(_) => {
                            output.newline(2);
                            next |= CODE;
                        }
                        Tag::BlockQuote(_) => {
                            output.newline(1);
                            quote_depth += 1;
                            next |= QUOTE;
                        }
                        Tag::List(number) => {
                            output.newline(1);
                            lists.push(number);
                        }
                        Tag::Item => {
                            output.newline(1);
                            for _ in 1..lists.len() {
                                output.push("  ", 0);
                            }
                            if let Some(Some(number)) = lists.last_mut() {
                                number_text.clear();
                                write!(&mut number_text, "{number}. ").unwrap();
                                output.push(&number_text, 0);
                                *number += 1;
                            } else {
                                output.push("• ", 0);
                            }
                        }
                        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                            links.push((output.text.len(), dest_url));
                            next |= LINK;
                        }
                        _ => (),
                    }
                    styles.push(next);
                }
                Event::End(tag) => {
                    styles.pop();
                    match tag {
                        TagEnd::Heading(_) | TagEnd::CodeBlock => output.newline(2),
                        TagEnd::Paragraph => output.newline(if lists.is_empty() { 2 } else { 1 }),
                        TagEnd::List(_) => {
                            lists.pop();
                            output.newline(if lists.is_empty() { 2 } else { 1 });
                        }
                        TagEnd::Item => output.newline(1),
                        TagEnd::BlockQuote(_) => {
                            quote_depth -= 1;
                            output.newline(2);
                        }
                        TagEnd::Link | TagEnd::Image => {
                            let (start, destination) = links.pop().unwrap();
                            output.finish_link(start, &destination);
                        }
                        _ => (),
                    }
                }
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    output.content(&text, style, quote_depth);
                }
                Event::Code(text) => output.content(&text, style | CODE, quote_depth),
                Event::SoftBreak | Event::HardBreak => output.push("\n", style),
                Event::Rule => {
                    output.newline(2);
                    output.push("───", QUOTE);
                    output.newline(2);
                }
                _ => (),
            }
        }
        let end = output.text.trim_end_matches('\n').len();
        output.text.truncate(end);
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_nested_styles_and_preserves_code_and_links() {
        let document = Markdown::render(
            "## 标题\n\n**bold _italic_** and `a*b`\n\n> quote\n\n1. first\n2. second\n\n```rust\n  x < y\n\n  z\n```\n\n[docs](https://example.com)",
        );
        assert_eq!(
            document.text,
            "标题\n\nbold italic and a*b\n\n│ quote\n\n1. first\n2. second\n\n  x < y\n\n  z\n\ndocs (https://example.com)"
        );
        let italic = document.text.find("italic").unwrap();
        assert_eq!(
            document.spans(italic..italic + 6).collect::<Vec<_>>(),
            vec![(italic..italic + 6, BOLD | ITALIC)]
        );
        let code = document.text.find("  x").unwrap();
        assert_eq!(document.spans(code..code + 7).next().unwrap().1, CODE);
    }

    #[test]
    fn tables_align_unicode_columns_and_preserve_inline_styles() {
        let document = Markdown::render(
            "| 名称 | 值 | 说明 |\n| :--- | ---: | :---: |\n| **中文** | `12` | *ok* |\n| x | 3 | wide |",
        );
        assert_eq!(
            document.text,
            "│ 名称 │ 值 │ 说明 │\n├──────┼────┼──────┤\n│ 中文 │ 12 │  ok  │\n│ x    │  3 │ wide │"
        );
        let rows: Vec<_> = document.text.lines().collect();
        assert!(rows.iter().all(|row| crate::width::text_width(row) == 20));
        let start = document.text.find("中文").unwrap();
        assert_eq!(
            document
                .spans(start..start + "中文".len())
                .next()
                .unwrap()
                .1,
            BOLD
        );
        let start = document.text.find("12").unwrap();
        assert_eq!(document.spans(start..start + 2).next().unwrap().1, CODE);
        let start = document.text.find("ok").unwrap();
        assert_eq!(document.spans(start..start + 2).next().unwrap().1, ITALIC);
    }

    #[test]
    fn long_table_cells_keep_text_without_padding_every_row_to_their_width() {
        let long = "x".repeat(4096);
        let document = Markdown::render(&format!(
            "| A | B |\n| - | - |\n| {long} | value |\n| short | value |"
        ));
        assert!(document.text.contains(&long));
        assert!(crate::width::text_width(document.text.lines().last().unwrap()) < 64);
    }

    #[test]
    fn nested_lists_use_dots_and_keep_ordered_start_numbers() {
        let document =
            Markdown::render("- first\n  - child\n  - **second**\n- last\n\n3. third\n4. fourth");
        assert_eq!(
            document.text,
            "• first\n  • child\n  • second\n• last\n\n3. third\n4. fourth"
        );
    }

    #[test]
    fn code_controls_preserve_unicode_text_and_style_offsets() {
        for (source, expected) in [
            (
                "```text\n中\tA\u{85}B\ne\u{301}\x1bC\n```",
                "中 A B\ne\u{301} C",
            ),
            ("```text\n中e\u{301}\u{85}A\tB\n```", "中e\u{301} A B"),
        ] {
            let document = Markdown::render(source);
            assert_eq!(document.text, expected);
            assert_eq!(
                document.spans(0..document.text.len()).collect::<Vec<_>>(),
                vec![(0..document.text.len(), CODE)]
            );
        }
    }

    #[test]
    fn streaming_unclosed_code_keeps_the_partial_line() {
        let document = Markdown::render("answer\n\n```gdb\nx/4gx $sp\n0x123");
        assert_eq!(document.text, "answer\n\nx/4gx $sp\n0x123");
        assert_eq!(
            document
                .spans(document.text.len() - 5..document.text.len())
                .next()
                .unwrap()
                .1,
            CODE
        );
    }
}
