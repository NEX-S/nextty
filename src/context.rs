use crate::core::{Flags, Point, SelectionRange, object_selection};
use crate::screen::Screen;

const SNAPSHOT_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Object {
    pub range: SelectionRange,
    pub text: String,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub text: String,
    pub incomplete: bool,
}

pub(crate) fn object_at(screen: &Screen, mut point: Point) -> Option<Object> {
    let offset = screen.display_offset() as i64;
    let row = screen.rows.get((i64::from(point.line) + offset) as usize)?;
    if row
        .cells
        .get(point.column)?
        .flags
        .contains(Flags::WIDE_CHAR_SPACER)
    {
        point.column -= 1;
    }
    let cell = &row.cells[point.column];
    if cell.c.is_whitespace() || cell.flags.contains(Flags::HIDDEN) {
        return None;
    }
    let [start, end] = object_selection((i64::from(point.line), point.column), |line| {
        Ok::<_, std::convert::Infallible>(screen.rows.get((line + offset) as usize))
    })
    .unwrap()?;
    let wide_end = screen.rows[(end.0 + offset) as usize].cells[end.1]
        .flags
        .contains(Flags::WIDE_CHAR);
    let range = SelectionRange {
        start: Point {
            line: start.0 as i32,
            column: start.1,
        },
        end: Point {
            line: end.0 as i32,
            column: end.1 + usize::from(wide_end),
        },
    };
    let text = snapshot(screen, Some(range)).text;
    let kind = classify(&text, row)?;
    Some(Object { range, text, kind })
}

fn classify(text: &str, row: &crate::core::Row) -> Option<&'static str> {
    if ["https://", "http://", "ssh://", "file://"]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Some("URL");
    }
    if text
        .strip_prefix("0x")
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Some("Address");
    }
    let register = text.strip_prefix('$').unwrap_or(text);
    let mut lowercase = [0; 6];
    if lowercase
        .get_mut(..register.len())
        .is_some_and(|lowercase| {
            lowercase.copy_from_slice(register.as_bytes());
            lowercase.make_ascii_lowercase();
            matches!(
                &*lowercase,
                b"rax"
                    | b"rbx"
                    | b"rcx"
                    | b"rdx"
                    | b"rsi"
                    | b"rdi"
                    | b"rbp"
                    | b"rsp"
                    | b"rip"
                    | b"eax"
                    | b"ebx"
                    | b"ecx"
                    | b"edx"
                    | b"esi"
                    | b"edi"
                    | b"ebp"
                    | b"esp"
                    | b"eip"
                    | b"ax"
                    | b"bx"
                    | b"cx"
                    | b"dx"
                    | b"al"
                    | b"ah"
                    | b"bl"
                    | b"bh"
                    | b"cl"
                    | b"ch"
                    | b"dl"
                    | b"dh"
                    | b"eflags"
                    | b"rflags"
                    | b"cs"
                    | b"ds"
                    | b"es"
                    | b"fs"
                    | b"gs"
                    | b"ss"
            )
        })
        || register.strip_prefix(['r', 'R']).is_some_and(|index| {
            let index = index
                .strip_suffix(['d', 'D', 'w', 'W', 'b', 'B'])
                .unwrap_or(index);
            matches!(index, "8" | "9" | "10" | "11" | "12" | "13" | "14" | "15")
        })
    {
        return Some("Register");
    }
    if text.starts_with(['\'', '"', '`'])
        && text.len() > 1
        && text.as_bytes().first() == text.as_bytes().last()
    {
        return Some("Quoted text");
    }
    if text.contains('/')
        || text.rsplit_once(':').is_some_and(|(path, line)| {
            path.contains('.') && !line.is_empty() && line.bytes().all(|c| c.is_ascii_digit())
        })
    {
        return Some("Path");
    }
    if text.contains("@plt")
        || text.contains("::")
        || text.ends_with("()")
        || text.rsplit_once('+').is_some_and(|(name, offset)| {
            !name.is_empty()
                && !offset.is_empty()
                && (offset.bytes().all(|c| c.is_ascii_digit())
                    || offset.strip_prefix("0x").is_some_and(|hex| {
                        !hex.is_empty() && hex.bytes().all(|c| c.is_ascii_hexdigit())
                    }))
        })
    {
        return Some("Symbol");
    }
    if matches!(
        text,
        "mov"
            | "lea"
            | "call"
            | "ret"
            | "jmp"
            | "push"
            | "pop"
            | "xor"
            | "cmp"
            | "test"
            | "add"
            | "sub"
            | "leave"
            | "nop"
            | "je"
            | "jne"
            | "jz"
            | "jnz"
            | "ja"
            | "jb"
            | "jg"
            | "jl"
            | "syscall"
    ) && row.cells[..row.used()]
        .windows(2)
        .any(|pair| pair[0].c == '0' && pair[1].c == 'x')
    {
        return Some("Instruction");
    }
    None
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
    for line in range.start.line.max(first)..=range.end.line.min(last) {
        let row = &screen.rows[(line - first) as usize];
        let mut from = if line == range.start.line {
            range.start.column
        } else {
            0
        }
        .min(row.used());
        if from > 0
            && row
                .cells
                .get(from)
                .is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR_SPACER))
        {
            from -= 1;
        }
        let to = if line == range.end.line {
            range.end.column + 1
        } else {
            row.used()
        }
        .min(row.used());
        for (_, cell, extra) in row.cells_with_extras(from..to) {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                result.text.push(' ');
                if cell.flags.contains(Flags::WIDE_CHAR) {
                    result.text.push(' ');
                }
            } else {
                result.text.push(cell.c);
                if let Some(extra) = extra {
                    result.text.extend(&extra.zerowidth);
                }
            }
            if result.text.len() > SNAPSHOT_LIMIT {
                while result.text.len() > SNAPSHOT_LIMIT {
                    result.text.pop();
                }
                result.incomplete = true;
                return result;
            }
        }
        if line < range.end.line.min(last) && !row.wrapped {
            if result.text.len() == SNAPSHOT_LIMIT {
                result.incomplete = true;
                return result;
            }
            result.text.push('\n');
        }
    }
    result
}

#[cfg(any(feature = "agent", test))]
pub(crate) fn object_context(screen: &Screen, object: &Object) -> Snapshot {
    snapshot(
        screen,
        Some(SelectionRange {
            start: Point {
                line: object.range.start.line - 2,
                column: 0,
            },
            end: Point {
                line: object.range.end.line + 2,
                column: screen.columns().saturating_sub(1),
            },
        }),
    )
}

#[cfg(any(feature = "agent", test))]
pub(crate) fn region_context(screen: &Screen, object: &Object) -> Option<Snapshot> {
    let offset = screen.display_offset() as i32;
    let row = (object.range.start.line + offset) as usize;
    let separator = |row: &crate::core::Row| {
        row.cells[..row.used()].windows(4).any(|cells| {
            cells
                .iter()
                .all(|cell| matches!(cell.c, '─' | '━' | '═' | '-'))
        })
    };
    let start = (0..=row)
        .rev()
        .find(|&index| separator(&screen.rows[index]));
    let next = (row + 1..screen.rows.len()).find(|&index| separator(&screen.rows[index]));
    if start.is_none() && next.is_none() {
        return None;
    }
    let mut result = snapshot(
        screen,
        Some(SelectionRange {
            start: Point {
                line: start.unwrap_or(0) as i32 - offset,
                column: 0,
            },
            end: Point {
                line: next.unwrap_or(screen.rows.len()) as i32 - 1 - offset,
                column: screen.columns().saturating_sub(1),
            },
        }),
    );
    result.incomplete |= start.is_none() || next.is_none();
    Some(result)
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

    #[test]
    fn pwndbg_objects_use_visible_alternate_screen_and_literal_ranges() {
        let (_, screen) = screen(
            80,
            6,
            "\x1b[?1049hRAX  0x7fffffffdc40  '中\u{301}文 payload'\r\n0x5555555551aa <main+42> call puts@plt\r\nat /home/nex/vuln.c:18:7\r\nhttps://example.test/a_(b)",
        );
        for (line, column, kind, expected) in [
            (0, 1, "Register", "RAX"),
            (0, 10, "Address", "0x7fffffffdc40"),
            (0, 22, "Quoted text", "'中\u{301}文 payload'"),
            (1, 17, "Symbol", "main+42"),
            (1, 28, "Instruction", "call"),
            (2, 10, "Path", "/home/nex/vuln.c:18:7"),
            (3, 10, "URL", "https://example.test/a_(b)"),
        ] {
            let object = object_at(&screen, Point { line, column }).unwrap();
            assert_eq!((object.kind, object.text.as_str()), (kind, expected));
            assert_eq!(snapshot(&screen, Some(object.range)).text, expected);
        }
        assert!(object_at(&screen, Point { line: 2, column: 0 }).is_none());
    }

    #[test]
    fn wrapped_object_and_clipped_selection_keep_display_offset_and_unicode() {
        let (mut core, _) = screen(10, 3, "first\r\n'中\u{301}文 payload'\r\nlast\r\nend");
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
        assert_eq!(object.text, "'中\u{301}文 payload'");
        assert_eq!(object.range.start.line, -1);
        let captured = object_context(&screen, &object);
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
        assert!(selected.text.starts_with("'中\u{301}文 payload'"));
        core.clear_selection();
        assert_eq!(captured.text, object_context(&screen, &object).text);
    }

    #[test]
    fn wide_object_spacer_and_hidden_cells_preserve_visible_text() {
        let (_, screen) = screen(20, 3, "'中' \x1b[8m中a\x1b[0m end");
        let object = object_at(&screen, Point { line: 0, column: 2 }).unwrap();
        assert_eq!(object.text, "'中'");
        assert_eq!(snapshot(&screen, None).text, "'中'     end\n\n");
        assert!(object_at(&screen, Point { line: 0, column: 6 }).is_none());
        assert!(object_context(&screen, &object).incomplete);
    }

    #[test]
    fn snapshot_limit_includes_empty_line_separators() {
        let text = format!("{}\r\n", "a".repeat(1023)).repeat(64);
        let (_, screen) = screen(1024, 66, &text);
        let snapshot = snapshot(&screen, None);
        assert_eq!(snapshot.text.len(), SNAPSHOT_LIMIT);
        assert!(snapshot.incomplete);
    }

    #[test]
    fn region_with_top_outside_viewport_is_partial() {
        let (_, screen) = screen(80, 5, "RAX 0x1234\r\na-b-c-d-e\r\n──── STACK ────");
        let object = object_at(&screen, Point { line: 0, column: 5 }).unwrap();
        let region = region_context(&screen, &object).unwrap();
        assert_eq!(region.text, "RAX 0x1234\na-b-c-d-e");
        assert!(region.incomplete);
    }

    #[test]
    fn region_snapshot_stops_before_the_next_tui_section() {
        let (_, screen) = screen(
            80,
            8,
            "──── REGISTERS ────\r\nRAX 0x7fffffffdc40\r\nRDI 0x7fffffffdc40\r\n──── STACK ────\r\n0x7fffffffdc40 0x41414141",
        );
        let object = object_at(&screen, Point { line: 1, column: 5 }).unwrap();
        let region = region_context(&screen, &object).unwrap();
        assert!(region.text.contains("RAX"));
        assert!(region.text.contains("RDI"));
        assert!(!region.text.contains("STACK"));
    }
}
