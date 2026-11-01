use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use std::fmt::Write;
use std::ops::Range;

pub(crate) const BOLD: u8 = 1;
pub(crate) const ITALIC: u8 = 2;
pub(crate) const CODE: u8 = 4;
pub(crate) const LINK: u8 = 8;
pub(crate) const QUOTE: u8 = 16;

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
    spans: Vec<(Range<usize>, u8)>,
    prefix: Option<(usize, u64, usize, usize)>,
}

pub(crate) fn fingerprint(source: &str) -> u64 {
    use std::hash::Hasher;
    let mut hash = std::hash::DefaultHasher::new();
    hash.write(source.as_bytes());
    hash.finish()
}

impl Markdown {
    fn push(&mut self, text: &str, style: u8) {
        let start = self.text.len();
        if text.chars().any(|ch| ch.is_control() && ch != '\n') {
            self.text.extend(text.chars().map(|ch| {
                if ch.is_control() && ch != '\n' {
                    ' '
                } else {
                    ch
                }
            }));
        } else {
            self.text.push_str(text);
        }
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

    pub fn render(source: &str, previous: Option<&Self>) -> Self {
        // References and HTML can change earlier blocks when more source arrives.
        let prefix = previous
            .and_then(|document| document.prefix)
            .filter(|&(end, hash, _, _)| {
                source
                    .get(..end)
                    .is_some_and(|text| fingerprint(text) == hash)
            });
        let tail = &source.as_bytes()[prefix.map_or(0, |(end, _, _, _)| end)..];
        let reusable = memchr::memchr2(b'[', b'<', tail).is_none();
        let prefix = prefix.filter(|_| reusable);
        let (mut output, offset) = if let Some((source_end, _, text_end, spans_end)) = prefix {
            let previous = previous.unwrap();
            let mut output = Self {
                text: String::with_capacity(text_end + source.len() - source_end),
                spans: previous.spans[..spans_end].to_vec(),
                prefix: None,
            };
            output.text.push_str(&previous.text[..text_end]);
            if let Some((range, _)) = output.spans.last_mut() {
                range.end = text_end;
            }
            (output, source_end)
        } else {
            (Self::default(), 0)
        };
        let mut styles = Vec::with_capacity(8);
        styles.push(0);
        let mut lists = Vec::new();
        let mut number_text = String::new();
        let mut links = Vec::new();
        let mut quote_depth = 0;
        for (event, range) in Parser::new(&source[offset..]).into_offset_iter() {
            if reusable && styles.len() == 1 && !matches!(&event, Event::End(_)) {
                let start = offset + range.start;
                let start = source[..start].rfind('\n').map_or(0, |index| index + 1);
                output.prefix = Some((start, 0, output.text.len(), output.spans.len()));
            }
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
        output.prefix = output
            .prefix
            .filter(|&(source_end, _, text_end, _)| source_end != 0 && text_end <= end)
            .map(|(source_end, _, text_end, spans_end)| {
                (
                    source_end,
                    fingerprint(&source[..source_end]),
                    text_end,
                    spans_end,
                )
            });
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_prefix_reuse_matches_full_rendering() {
        let sources = [
            "first **bold**\n\nsecond *italic*\n\nlast `code` and 中\n",
            "- first\n\n- second\n  continued\n\noutside\n\nlast\n",
            "> quote\n>\n> next\n\nplain\n\nlast\n",
            "intro\n\n```rust\nlet x = 1;\n\n```\n\nafter\n\nlast\n",
            "intro\n\n~~~\n\n~~~\n\nlast\n",
            "intro\n\n    indented\n\n    more\n\nlast\n",
            "intro\n\nsetext\n---\n\nlast\n",
            "intro\n\n---\n\nlast\n",
            "[old][ref]\n\nplain\n\n[ref]: https://example.com\n",
            "intro\n\n[old][ref]\n\n[ref]: https://example.com\n",
            "intro\n\n<div>\nhtml\n</div>\n\nlast\n",
        ];
        for source in sources {
            let mut previous = None;
            for end in source
                .char_indices()
                .map(|(index, _)| index)
                .chain(std::iter::once(source.len()))
            {
                let text = &source[..end];
                let incremental = Markdown::render(text, previous.as_ref());
                let full = Markdown::render(text, None);
                assert_eq!(incremental.text, full.text, "{text:?}");
                assert_eq!(incremental.spans, full.spans, "{text:?}");
                previous = Some(incremental);
            }
        }
        let old = Markdown::render("old paragraph\n\nlast", None);
        let changed = "new paragraph\n\nlast";
        let reused = Markdown::render(changed, Some(&old));
        let full = Markdown::render(changed, None);
        assert_eq!(reused.text, full.text);
        assert_eq!(reused.spans, full.spans);
    }

    #[test]
    fn renders_nested_styles_and_preserves_code_and_links() {
        let document = Markdown::render(
            "## 标题\n\n**bold _italic_** and `a*b`\n\n> quote\n\n1. first\n2. second\n\n```rust\n  x < y\n\n  z\n```\n\n[docs](https://example.com)",
            None,
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
    fn code_controls_preserve_unicode_text_and_style_offsets() {
        let document = Markdown::render("```text\n中\tA\u{85}B\ne\u{301}\x1bC\n```", None);
        assert_eq!(document.text, "中 A B\ne\u{301} C");
        assert_eq!(
            document.spans(0..document.text.len()).collect::<Vec<_>>(),
            vec![(0..document.text.len(), CODE)]
        );
    }

    #[test]
    fn streaming_unclosed_code_keeps_the_partial_line() {
        let document = Markdown::render("answer\n\n```gdb\nx/4gx $sp\n0x123", None);
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
