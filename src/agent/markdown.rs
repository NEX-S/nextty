use crate::width::text_width;
use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

pub(crate) const BOLD: u8 = 1;
pub(crate) const ITALIC: u8 = 2;
pub(crate) const CODE: u8 = 4;
const LINK: u8 = 8;
const QUOTE: u8 = 16;

pub(crate) fn color(style: u8, foreground: u32) -> u32 {
    let dark_text = crate::panel::dark_text(foreground);
    if style & CODE != 0 {
        if dark_text { 0x685520 } else { 0xc6b890 }
    } else if style & (BOLD | LINK) != 0 {
        crate::panel::diff_colors(foreground).0
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
    pub fn render(source: &str) -> Self {
        let mut output = Self::default();
        output.blocks(&mut Parser::new_ext(source, Options::ENABLE_TABLES));
        output
            .text
            .truncate(output.text.trim_end_matches('\n').len());
        output
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

    fn newline(&mut self, count: usize) {
        if !self.text.is_empty() {
            let existing = self.text.bytes().rev().take_while(|b| *b == b'\n').count();
            self.push(&"\n".repeat(count.saturating_sub(existing)), 0);
        }
    }

    fn content(&mut self, text: &str, style: u8, quote_depth: usize) {
        for part in text.split_inclusive('\n') {
            if self.text.is_empty() || self.text.ends_with('\n') {
                self.push(&"│ ".repeat(quote_depth), QUOTE);
            }
            self.push(part, style);
        }
    }

    /// Renders events up to the end of the enclosing table cell, or of the document.
    fn blocks<'a>(&mut self, events: &mut impl Iterator<Item = Event<'a>>) {
        let mut styles: smallvec::SmallVec<[u8; 8]> = smallvec::smallvec![0];
        let mut lists = Vec::new();
        let mut links = Vec::new();
        let mut quote_depth = 0;
        while let Some(event) = events.next() {
            let style = *styles.last().unwrap();
            match event {
                Event::Start(Tag::Table(alignments)) => {
                    self.table(&alignments, events, quote_depth)
                }
                Event::End(TagEnd::TableCell) => return,
                Event::Start(tag) => {
                    let next = match tag {
                        Tag::Heading { .. } => {
                            self.newline(2);
                            style | BOLD
                        }
                        Tag::Strong => style | BOLD,
                        Tag::Emphasis => style | ITALIC,
                        Tag::CodeBlock(_) => {
                            self.newline(2);
                            style | CODE
                        }
                        Tag::BlockQuote(_) => {
                            self.newline(1);
                            quote_depth += 1;
                            style | QUOTE
                        }
                        Tag::List(number) => {
                            self.newline(1);
                            lists.push(number);
                            style
                        }
                        Tag::Item => {
                            self.newline(1);
                            self.push(&"  ".repeat(lists.len() - 1), 0);
                            if let Some(Some(number)) = lists.last_mut() {
                                self.push(&format!("{number}. "), 0);
                                *number += 1;
                            } else {
                                self.push("• ", 0);
                            }
                            style
                        }
                        Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                            links.push((self.text.len(), dest_url));
                            style | LINK
                        }
                        _ => style,
                    };
                    styles.push(next);
                }
                Event::End(tag) => {
                    styles.pop();
                    match tag {
                        TagEnd::Heading(_) | TagEnd::CodeBlock => self.newline(2),
                        TagEnd::Paragraph => self.newline(if lists.is_empty() { 2 } else { 1 }),
                        TagEnd::List(_) => {
                            lists.pop();
                            self.newline(if lists.is_empty() { 2 } else { 1 });
                        }
                        TagEnd::Item => self.newline(1),
                        TagEnd::BlockQuote(_) => {
                            quote_depth -= 1;
                            self.newline(2);
                        }
                        TagEnd::Link | TagEnd::Image => {
                            let (start, destination) = links.pop().unwrap();
                            if self.text[start..] != *destination {
                                self.push(&format!(" ({destination})"), LINK);
                            }
                        }
                        _ => (),
                    }
                }
                Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                    self.content(&text, style, quote_depth);
                }
                Event::Code(text) => self.content(&text, style | CODE, quote_depth),
                Event::SoftBreak | Event::HardBreak => self.push("\n", style),
                Event::Rule => {
                    self.newline(2);
                    self.push("───", QUOTE);
                    self.newline(2);
                }
                _ => (),
            }
        }
    }

    fn table<'a>(
        &mut self,
        alignments: &[Alignment],
        events: &mut impl Iterator<Item = Event<'a>>,
        quote_depth: usize,
    ) {
        let mut rows: Vec<Vec<Self>> = Vec::new();
        while let Some(event) = events.next() {
            match event {
                Event::End(TagEnd::Table) => break,
                Event::Start(Tag::TableHead | Tag::TableRow) => rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => {
                    let mut cell = Self::default();
                    cell.blocks(events);
                    rows.last_mut().unwrap().push(cell);
                }
                _ => (),
            }
        }
        let widths: Vec<usize> = (0..alignments.len())
            .map(|column| {
                let widest = rows.iter().map(|row| text_width(&row[column].text)).max();
                widest.unwrap_or(1).clamp(1, 48)
            })
            .collect();
        let quote = "│ ".repeat(quote_depth);
        self.newline(2);
        for (index, row) in rows.into_iter().enumerate() {
            self.push(&quote, QUOTE);
            self.push("│", QUOTE);
            for ((cell, width), alignment) in row.into_iter().zip(&widths).zip(alignments) {
                let padding = width.saturating_sub(text_width(&cell.text));
                let left = match alignment {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                self.push(&" ".repeat(left + 1), 0);
                for (range, style) in cell.spans(0..cell.text.len()) {
                    let header = if index == 0 { BOLD } else { 0 };
                    self.push(&cell.text[range], style | header);
                }
                self.push(&" ".repeat(padding - left + 1), 0);
                self.push("│", QUOTE);
            }
            self.push("\n", 0);
            if index == 0 {
                let borders: Vec<_> = widths.iter().map(|width| "─".repeat(width + 2)).collect();
                self.push(&format!("{quote}├{}┤", borders.join("┼")), QUOTE);
                self.push("\n", 0);
            }
        }
        self.newline(2);
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
        let style = |text: &str| {
            let start = document.text.find(text).unwrap();
            document.spans(start..start + text.len()).next().unwrap().1
        };
        assert_eq!(
            (style("中文"), style("12"), style("ok")),
            (BOLD, CODE, ITALIC)
        );
        let long = "x".repeat(4096);
        let document = Markdown::render(&format!(
            "| A | B |\n| - | - |\n| {long} | value |\n| short | value |"
        ));
        assert!(document.text.contains(&long));
        assert!(text_width(document.text.lines().last().unwrap()) < 64);
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
    fn code_controls_and_unclosed_streaming_code_keep_text_and_style_offsets() {
        let document = Markdown::render("```text\n中\tA\u{85}B\ne\u{301}\x1bC\n```");
        assert_eq!(document.text, "中 A B\ne\u{301} C");
        assert_eq!(
            document.spans(0..document.text.len()).collect::<Vec<_>>(),
            vec![(0..document.text.len(), CODE)]
        );
        let document = Markdown::render("answer\n\n```gdb\nx/4gx $sp\n0x123");
        assert_eq!(document.text, "answer\n\nx/4gx $sp\n0x123");
        let end = document.text.len();
        assert_eq!(document.spans(end - 5..end).next().unwrap().1, CODE);
    }
}
