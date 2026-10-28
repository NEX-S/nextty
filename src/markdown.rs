use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::ops::Range;

pub(crate) const BOLD: u8 = 1;
pub(crate) const ITALIC: u8 = 2;
pub(crate) const CODE: u8 = 4;
pub(crate) const LINK: u8 = 8;
pub(crate) const QUOTE: u8 = 16;

pub(crate) fn color(style: u8, foreground: u32) -> u32 {
    if style & CODE != 0 {
        0xc6b890
    } else if style & (BOLD | LINK) != 0 {
        0xb5d2a2
    } else if style & QUOTE != 0 {
        0x889391
    } else {
        foreground
    }
}

#[derive(Default)]
pub(crate) struct Markdown {
    pub text: String,
    spans: Vec<(Range<usize>, u8)>,
}

impl Markdown {
    fn push(&mut self, text: &str, style: u8) {
        let start = self.text.len();
        self.text.extend(text.chars().map(|ch| {
            if ch.is_control() && ch != '\n' {
                ' '
            } else {
                ch
            }
        }));
        if let Some((range, previous)) = self.spans.last_mut()
            && *previous == style
        {
            range.end = self.text.len();
        } else if !text.is_empty() {
            self.spans.push((start..self.text.len(), style));
        }
    }

    fn newline(&mut self, count: usize) {
        if !self.text.is_empty() {
            let existing = self.text.bytes().rev().take_while(|b| *b == b'\n').count();
            for _ in existing..count {
                self.push("\n", 0);
            }
        }
    }

    pub fn spans(&self, range: Range<usize>) -> impl Iterator<Item = (Range<usize>, u8)> + '_ {
        let first = self
            .spans
            .partition_point(|(span, _)| span.end <= range.start);
        self.spans[first..]
            .iter()
            .take_while(move |(span, _)| span.start < range.end)
            .filter_map(move |(span, style)| {
                let start = range.start.max(span.start);
                let end = range.end.min(span.end);
                (start < end).then_some((start..end, *style))
            })
    }

    fn content(&mut self, text: &str, style: u8, quote_depth: usize) {
        for part in text.split_inclusive('\n') {
            if quote_depth > 0 && (self.text.ends_with('\n') || self.text.is_empty()) {
                for _ in 0..quote_depth {
                    self.push("│ ", QUOTE);
                }
            }
            self.push(part, style);
        }
    }

    pub fn render(source: &str) -> Self {
        let mut output = Self::default();
        let mut styles = vec![0];
        let mut lists = Vec::new();
        let mut links = Vec::new();
        let mut quote_depth = 0;
        for event in Parser::new(source) {
            let style = *styles.last().unwrap();
            match event {
                Event::Start(tag) => {
                    let mut next = style;
                    match tag {
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
                        Tag::Paragraph => (),
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
                                output.push(&format!("{number}. "), 0);
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
                            if output.text[start..] != *destination {
                                output.push(" (", LINK);
                                output.push(&destination, LINK);
                                output.push(")", LINK);
                            }
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
