use crate::core::{Flags, Mode};
use crate::screen::Screen;
use crate::vt::CursorShape;
use serde_json::{Value, json};

fn horizontal(character: char) -> bool {
    matches!(
        character,
        '-' | '=' | '─' | '━' | '┄' | '┅' | '┈' | '┉' | '╌' | '╍' | '═'
    )
}

pub(crate) fn shape(screen: &Screen, query: Option<&str>) -> Value {
    let mut text = String::new();
    let mut row_ranges = Vec::with_capacity(screen.rows.len());
    let cursor = screen.cursor();
    let cursor_row = i64::from(cursor.0.line) + screen.display_offset() as i64;
    let cursor_visible =
        cursor.1 != CursorShape::Hidden && (0..screen.screen_lines() as i64).contains(&cursor_row);
    for (index, row) in screen.rows.iter().enumerate() {
        let start = text.len();
        for (_, cell, extra) in row.cells_with_extras(0..row.used()) {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                text.push(' ');
                if cell.flags.contains(Flags::WIDE_CHAR) {
                    text.push(' ');
                }
            } else {
                text.push(cell.c);
                if let Some(extra) = extra {
                    text.extend(&extra.zerowidth);
                }
            }
        }
        row_ranges.push(start..text.len());
        if !row.wrapped && index + 1 < screen.rows.len() {
            text.push('\n');
        }
    }
    let mut matches = vec![false; row_ranges.len()];
    if let Some(query) = query {
        let mut offset = 0;
        while let Some(found) = text[offset..].find(query) {
            let start = offset + found;
            let end = start + query.len();
            let first = row_ranges.partition_point(|row| row.start <= start) - 1;
            let last = row_ranges
                .partition_point(|row| {
                    row.start < end || (query.ends_with('\n') && row.start == end)
                })
                .saturating_sub(1)
                .max(first);
            matches[first..=last].fill(true);
            let Some(next) = text[start..].chars().next() else {
                break;
            };
            offset = start + next.len_utf8();
        }
    }
    let mut rows = Vec::new();
    let mut omitted = Vec::<Value>::new();
    for (index, range) in row_ranges.iter().enumerate() {
        let text = &text[range.clone()];
        if query.is_some() {
            if (cursor_visible && cursor_row == index as i64)
                || matches[index.saturating_sub(1)..(index + 2).min(matches.len())].contains(&true)
            {
                rows.push(json!([index, text]));
            }
            continue;
        }
        let text = text.trim_end_matches(' ');
        let reason = if cursor_visible && cursor_row == index as i64 {
            None
        } else if text.is_empty() {
            Some("blank")
        } else if text.chars().all(|c| c == ' ' || horizontal(c))
            && text.chars().filter(|c| horizontal(*c)).nth(2).is_some()
        {
            Some("separator")
        } else {
            None
        };
        if let Some(reason) = reason {
            if let Some(last) = omitted.last_mut()
                && last["reason"] == reason
                && last["end_row"].as_u64() == Some(index as u64 - 1)
            {
                last["end_row"] = json!(index);
            } else {
                omitted.push(json!({"start_row": index, "end_row": index, "reason": reason}));
            }
        } else {
            rows.push(json!([index, text]));
        }
    }
    let wrapped_rows: Vec<_> = screen
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.wrapped.then_some(index))
        .collect();
    let mut result = json!({
        "columns": screen.columns(),
        "lines": screen.screen_lines(),
        "alternate_screen": screen.mode().contains(Mode::ALT_SCREEN),
        "display_offset": screen.display_offset(),
        "row_coordinates": "zero-based viewport",
        "cursor": {
            "row": cursor_row,
            "column": cursor.0.column,
            "visible": cursor_visible,
        },
        "wrapped_rows": wrapped_rows,
    });
    result["rows"] = Value::Array(rows);
    result["omitted"] = Value::Array(omitted);
    if query.is_some() {
        result["matches"] = json!(
            matches
                .iter()
                .enumerate()
                .filter_map(|(index, matched)| matched.then_some(index))
                .collect::<Vec<_>>()
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, WindowSize};
    use crate::screen::Update;

    fn screen(columns: u16, lines: u16, text: &str) -> Screen {
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
        screen
    }

    #[test]
    fn preserves_debugger_facts_titles_and_columns() {
        let screen = screen(
            120,
            10,
            "\x1b[?1049h────[ REGISTERS / show-flags off / show-compact-regs off ]────\r\n  *RSP  0x7fffffffdc40   │ RIP  0x555555551aa\r\n\r\n────────────────\r\n ► 0x555555551aa <main+42>:  mov rax, [rbp - 0x18]  \r\n  中\u{301}文  e\u{301} ◂— 0x20 —▸ 0x30\r\n=== amd64 / 64-bit / little endian ===\r\n├── └─│\r\n──[ ptr —▸ target - 8 ]──",
        );
        let result = shape(&screen, None);
        assert_eq!(result["alternate_screen"], true);
        assert_eq!(
            result["rows"],
            json!([
                [
                    0,
                    "────[ REGISTERS / show-flags off / show-compact-regs off ]────"
                ],
                [1, "  *RSP  0x7fffffffdc40   │ RIP  0x555555551aa"],
                [4, " ► 0x555555551aa <main+42>:  mov rax, [rbp - 0x18]"],
                [5, "  中\u{301}文  e\u{301} ◂— 0x20 —▸ 0x30"],
                [6, "=== amd64 / 64-bit / little endian ==="],
                [7, "├── └─│"],
                [8, "──[ ptr —▸ target - 8 ]──"],
            ])
        );
        assert_eq!(
            result["omitted"],
            json!([
                {"start_row": 2, "end_row": 2, "reason": "blank"},
                {"start_row": 3, "end_row": 3, "reason": "separator"},
                {"start_row": 9, "end_row": 9, "reason": "blank"},
            ])
        );
        assert_eq!(result["cursor"]["row"], 8);
        assert_eq!(result["cursor"]["visible"], true);
    }

    #[test]
    fn keeps_physical_wrapping_and_masks_hidden_cells() {
        let screen = screen(6, 5, "12345678\r\nA\x1b[8m秘密\x1b[0mB\r\n-\r\n==\x1b[?25l");
        let result = shape(&screen, None);
        assert_eq!(
            result["rows"],
            json!([[0, "123456"], [1, "78"], [2, "A    B"], [3, "-"], [4, "=="]])
        );
        assert_eq!(result["wrapped_rows"], json!([0]));
        assert_eq!(result["omitted"], json!([]));
        assert_eq!(result["cursor"]["visible"], false);
        assert_eq!(shape(&screen, Some("秘密"))["matches"], json!([]));
    }

    #[test]
    fn query_keeps_literal_match_context_and_only_a_visible_input_row() {
        let mut screen = screen(
            80,
            8,
            "unrelated\r\nbefore\r\n ► 中\u{301} <main+4>: mov rax, [rbp - 8]\r\nafter\r\nother\r\n(gdb) \x1b[8;1H",
        );
        let result = shape(&screen, Some("[rbp - 8]"));
        assert_eq!(result["matches"], json!([2]));
        assert_eq!(
            result["rows"],
            json!([
                [1, "before"],
                [2, " ► 中\u{301} <main+4>: mov rax, [rbp - 8]"],
                [3, "after"],
                [7, ""],
            ])
        );
        assert_eq!(
            result["cursor"],
            json!({"row": 7, "column": 0, "visible": true})
        );
        let result = shape(&screen, Some("absent"));
        assert_eq!(result["matches"], json!([]));
        assert_eq!(result["rows"], json!([[7, ""]]));
        screen.cursor.1 = CursorShape::Hidden;
        let result = shape(&screen, Some("absent"));
        assert_eq!(result["matches"], json!([]));
        assert_eq!(result["rows"], json!([]));
    }

    #[test]
    fn query_matches_raw_rows_across_soft_wraps_and_explicit_hard_newlines() {
        let screen = screen(
            6,
            8,
            "before\r\nab中e\u{301}FG\r\nafter\r\ntail\r\nlast\x1b[8;1H(gdb) ",
        );
        let result = shape(&screen, Some("中e\u{301}FG"));
        assert_eq!(result["matches"], json!([1, 2]));
        assert_eq!(
            result["rows"],
            json!([
                [0, "before"],
                [1, "ab中e\u{301}F"],
                [2, "G"],
                [3, "after"],
                [7, "(gdb) "],
            ])
        );
        assert_eq!(result["wrapped_rows"], json!([1]));
        let result = shape(&screen, Some("FG\nafter"));
        assert_eq!(result["matches"], json!([1, 2, 3]));
        assert_eq!(
            result["rows"],
            json!([
                [0, "before"],
                [1, "ab中e\u{301}F"],
                [2, "G"],
                [3, "after"],
                [4, "tail"],
                [7, "(gdb) "],
            ])
        );
        assert_eq!(shape(&screen, Some("Gafter"))["matches"], json!([]));
    }

    #[test]
    fn query_returns_omitted_rows_and_their_context_without_cleanup() {
        let screen = screen(20, 5, "before\r\n   \r\n────\r\nafter\x1b[?25l");
        let full = shape(&screen, None);
        assert_eq!(full["rows"], json!([[0, "before"], [3, "after"]]));
        let result = shape(&screen, Some("────"));
        assert_eq!(result["matches"], json!([2]));
        assert_eq!(
            result["rows"],
            json!([[1, "   "], [2, "────"], [3, "after"]])
        );
        assert_eq!(result["omitted"], json!([]));
        let result = shape(&screen, Some("   "));
        assert_eq!(result["matches"], json!([1]));
        assert_eq!(
            result["rows"],
            json!([[0, "before"], [1, "   "], [2, "────"]])
        );
    }
}
