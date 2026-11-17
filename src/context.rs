use crate::core::{Flags, Point, SelectionRange, object_selection};
use crate::screen::Screen;

const SNAPSHOT_LIMIT: usize = 64 * 1024;

#[derive(Debug, PartialEq)]
pub(crate) struct Object {
    pub range: SelectionRange,
    pub text: String,
    pub kind: &'static str,
}

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Snapshot {
    pub text: String,
    pub incomplete: bool,
}

pub(crate) fn object_at(screen: &Screen, mut point: Point) -> Option<Object> {
    let offset = screen.display_offset() as i64;
    let row = screen.rows.get((i64::from(point.line) + offset) as usize)?;
    row.cells.get(point.column)?;
    point.column = row.leading_column(point.column);
    let cell = &row.cells[point.column];
    if cell.flags.contains(Flags::HIDDEN) {
        return None;
    }
    let [start, end] = object_selection((i64::from(point.line), point.column), |line| {
        Ok::<_, std::convert::Infallible>(screen.rows.get((line + offset) as usize))
    })
    .unwrap()?;
    let wide_end = screen.rows[(end.0 + offset) as usize].cells[end.1]
        .flags
        .contains(Flags::WIDE_CHAR);
    let mut range = SelectionRange {
        start: Point {
            line: start.0 as i32,
            column: start.1,
        },
        end: Point {
            line: end.0 as i32,
            column: end.1 + usize::from(wide_end),
        },
    };
    let mut text = snapshot(screen, Some(range)).text;
    // A quoted link/path/IP highlights and copies the inner value, not the quotes.
    if let Some(inner) = unquote(&text)
        && classify(inner).is_some()
    {
        range.start.column += 1;
        range.end.column -= 1;
        text = inner.to_string();
    }
    let kind = classify(&text)?;
    Some(Object { range, text, kind })
}

fn unquote(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    (bytes.len() >= 2
        && matches!(bytes[0], b'\'' | b'"' | b'`')
        && bytes[0] == bytes[bytes.len() - 1])
        .then(|| &text[1..text.len() - 1])
}

fn classify(text: &str) -> Option<&'static str> {
    if ["https://", "http://", "ssh://", "file://"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Some("URL");
    }
    if is_ip(text) {
        return Some("IP address");
    }
    if is_path(text) {
        return Some("Path");
    }
    None
}

fn is_ip(text: &str) -> bool {
    let (host, port) = match text.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (text, None),
    };
    if port.is_some_and(|port| port.is_empty() || !port.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.len() <= 3
                && octet.bytes().all(|c| c.is_ascii_digit())
                && octet.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

fn is_path(text: &str) -> bool {
    text.len() > 1 && text.contains('/') && !matches!(text, "/*" | "*/")
        || text.rsplit_once(':').is_some_and(|(path, line)| {
            path.contains('.') && !line.is_empty() && line.bytes().all(|c| c.is_ascii_digit())
        })
}

pub(crate) fn snapshot(screen: &Screen, range: Option<SelectionRange>) -> Snapshot {
    let mut result = Snapshot::default();
    if screen.rows.is_empty() {
        return result;
    }
    let first = -(screen.display_offset() as i32);
    let last = first + screen.screen_lines() as i32 - 1;
    let range = range.unwrap_or(SelectionRange {
        start: Point {
            line: first,
            column: 0,
        },
        end: Point {
            line: last,
            column: screen.columns().saturating_sub(1),
        },
    });
    result.incomplete = range.start.line < first || range.end.line > last;
    let last_line = range.end.line.min(last);
    for line in range.start.line.max(first)..=last_line {
        let row = &screen.rows[(line - first) as usize];
        let mut from = if line == range.start.line {
            range.start.column
        } else {
            0
        }
        .min(row.used());
        if from < row.cells.len() {
            from = row.leading_column(from);
        }
        let to = if line == range.end.line {
            range.end.column + 1
        } else {
            row.used()
        }
        .min(row.used());
        for ch in row
            .characters(from..to)
            .map(|(_, ch)| ch)
            .chain((line < last_line && !row.wrapped).then_some('\n'))
        {
            if result.text.len() + ch.len_utf8() > SNAPSHOT_LIMIT {
                result.incomplete = true;
                return result;
            }
            result.text.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, Scroll, WindowSize};
    use crate::screen::Update;

    fn screen(columns: u16, lines: u16, text: &str) -> (Core, Screen) {
        let mut core = Core::new(
            WindowSize {
                num_cols: columns,
                num_lines: lines,
                cell_width: 0,
                cell_height: 0,
            },
            0xffffff,
            0,
        );
        crate::vt::Parser::new().advance(&mut core, text.as_bytes());
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true)).unwrap();
        (core, screen)
    }

    fn assert_hover_range(
        screen: &Screen,
        text: &str,
        expected: &str,
        kind: &str,
    ) -> std::ops::Range<usize> {
        let start = text[..text.find(expected).unwrap()].chars().count();
        let end = start + expected.chars().count();
        for column in start..end {
            let object = object_at(screen, Point { line: 0, column })
                .unwrap_or_else(|| panic!("{text}, column {column}"));
            assert_eq!((object.text.as_str(), object.kind), (expected, kind));
            assert_eq!((object.range.start.line, object.range.end.line), (0, 0));
            assert_eq!(
                (object.range.start.column, object.range.end.column),
                (start, end - 1)
            );
        }
        start..end
    }

    #[test]
    fn hover_highlights_only_links_paths_and_ip() {
        for (text, kind) in [
            ("https://example.test/a_(b)", "URL"),
            ("http://host/path", "URL"),
            ("ssh://user@host", "URL"),
            ("file:///tmp/x", "URL"),
            ("src/main.rs", "Path"),
            ("/home/nex/vuln.c:18:7", "Path"),
            ("192.168.1.1", "IP address"),
            ("10.0.0.1:8080", "IP address"),
            ("255.255.255.255", "IP address"),
        ] {
            let (_, screen) = screen(120, 3, text);
            assert_hover_range(&screen, text, text, kind);
        }
        for text in [
            "RAX",
            "0x7fffffffdc40",
            "%rsp",
            "$0x20",
            "main+42",
            "puts@plt",
            "movq",
            "call",
            "这是一段中文",
            "256.1.1.1",
            "1.2.3",
            "hello",
        ] {
            let (_, screen) = screen(120, 3, text);
            for column in 0..screen.rows[0].used() {
                assert!(
                    object_at(&screen, Point { line: 0, column }).is_none(),
                    "{text}, column {column}"
                );
            }
        }
    }

    #[test]
    fn hover_excludes_assignments_and_surrounding_punctuation() {
        for (text, expected, kind) in [
            ("file=src/main.rs", "src/main.rs", "Path"),
            ("../xxx/references/*.md", "../xxx/references/*.md", "Path"),
            ("at (main.rs:12:3).", "main.rs:12:3", "Path"),
            (
                "url=https://host/a_(b)?x=1&y=2;",
                "https://host/a_(b)?x=1&y=2",
                "URL",
            ),
            ("see https://host/path。", "https://host/path", "URL"),
            ("┌─/tmp/hover.c────────┐", "/tmp/hover.c", "Path"),
            ("docs/tutorial.md（现在只写了", "docs/tutorial.md", "Path"),
            (
                "docs/captcha-training-survey.md。它说明字符数字验证码",
                "docs/captcha-training-survey.md",
                "Path",
            ),
        ] {
            let (_, screen) = screen(80, 3, text);
            let range = assert_hover_range(&screen, text, expected, kind);
            for column in (0..screen.rows[0].used()).filter(|column| !range.contains(column)) {
                let object = object_at(&screen, Point { line: 0, column });
                assert!(object.is_none(), "{text}, column {column}: {object:?}");
            }
        }
        let (_, screen) = screen(80, 3, "21K / 256K");
        assert!(object_at(&screen, Point { line: 0, column: 4 }).is_none());
    }

    #[test]
    fn quoted_objects_unwrap_and_prose_is_not_hoverable() {
        let (_, s) = screen(80, 3, "'src/中文/main.rs'");
        let object = object_at(&s, Point { line: 0, column: 1 }).unwrap();
        assert_eq!(
            (object.text.as_str(), object.kind),
            ("src/中文/main.rs", "Path")
        );
        assert_eq!(
            (object.range.start.column, object.range.end.column),
            (1, 16)
        );

        let (_, s) = screen(80, 3, "\"192.168.1.1\"");
        let object = object_at(&s, Point { line: 0, column: 1 }).unwrap();
        assert_eq!(
            (object.text.as_str(), object.kind),
            ("192.168.1.1", "IP address")
        );

        let (_, s) = screen(80, 3, "\"https://host/path\"");
        let object = object_at(&s, Point { line: 0, column: 1 }).unwrap();
        assert_eq!(
            (object.text.as_str(), object.kind),
            ("https://host/path", "URL")
        );

        for text in ["'这是一段中文'", "\"中文 payload\"", "'中文.rs'"] {
            let (_, s) = screen(80, 3, text);
            for column in 0..s.rows[0].used() {
                assert!(
                    object_at(&s, Point { line: 0, column }).is_none(),
                    "{text}, column {column}"
                );
            }
        }
    }

    #[test]
    fn wrapped_object_and_clipped_selection_keep_display_offset_and_unicode() {
        let (mut core, _) = screen(10, 3, "first\r\n'中\u{301}文/payload'\r\nlast\r\nend");
        core.scroll_display(Scroll::Delta(1));
        let mut screen = Screen::default();
        screen.apply(Update::capture(&core, true)).unwrap();
        let object = object_at(
            &screen,
            Point {
                line: -1,
                column: 3,
            },
        )
        .unwrap();
        assert_eq!(object.text, "中\u{301}文/payload");
        assert_eq!(object.kind, "Path");
        assert_eq!(object.range.start.line, -1);
        let selected = snapshot(
            &screen,
            Some(SelectionRange {
                start: Point {
                    line: -8,
                    column: 0,
                },
                end: Point { line: 1, column: 9 },
            }),
        );
        assert!(selected.incomplete);
        assert!(selected.text.starts_with("'中\u{301}文/payload'"));
    }

    #[test]
    fn wide_object_spacer_and_hidden_cells_preserve_visible_text() {
        let (_, screen) = screen(20, 3, "'中/a' \x1b[8m中a\x1b[0m end");
        let object = object_at(&screen, Point { line: 0, column: 2 }).unwrap();
        assert_eq!(object.text, "中/a");
        assert_eq!(object.kind, "Path");
        assert_eq!(snapshot(&screen, None).text, "'中/a'     end\n\n");
        assert!(object_at(&screen, Point { line: 0, column: 8 }).is_none());
    }

    #[test]
    fn snapshot_limit_includes_empty_line_separators() {
        let text = format!("{}\r\n", "a".repeat(1023)).repeat(64);
        let (_, screen) = screen(1024, 66, &text);
        let snapshot = snapshot(&screen, None);
        assert_eq!(snapshot.text.len(), SNAPSHOT_LIMIT);
        assert!(snapshot.incomplete);
    }
}
