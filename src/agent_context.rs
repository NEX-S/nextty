use crate::core::{Flags, Mode};
use crate::screen::Screen;
use serde_json::{Value, json};

pub(crate) fn shape(screen: &Screen) -> Value {
    let mut rows = Vec::new();
    let mut omitted = Vec::<Value>::new();
    for (index, row) in screen.rows.iter().enumerate() {
        let mut text = String::new();
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
        let text = text.trim_end_matches(' ');
        let reason = if text.is_empty() {
            Some("blank")
        } else if text.chars().filter(|c| *c != ' ').count() >= 3
            && text
                .chars()
                .all(|c| matches!(c, ' ' | '-' | '=' | '\u{2500}'..='\u{257f}'))
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
    let cursor = screen.cursor();
    json!({
        "rows": rows,
        "omitted": omitted,
        "columns": screen.columns(),
        "lines": screen.screen_lines(),
        "alternate_screen": screen.mode().contains(Mode::ALT_SCREEN),
        "display_offset": screen.display_offset(),
        "row_coordinates": "zero-based viewport",
        "cursor": {
            "row": i64::from(cursor.0.line) + screen.display_offset() as i64,
            "column": cursor.0.column,
        },
        "wrapped_rows": wrapped_rows,
    })
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
    fn retains_debugger_facts_columns_and_titles_with_explicit_omissions() {
        let screen = screen(
            100,
            8,
            "\x1b[?1049h──[ REGISTERS ]──\r\n  RSP  0x7fffffffdc40   │ RIP  0x555555551aa\r\n\r\n────────────────\r\n0x555555551aa <main+42>:  mov rax, [rbp - 0x18]  \r\n  中\u{301}文  e\u{301}\r\n=== title ===",
        );
        let result = shape(&screen);
        assert_eq!(result["alternate_screen"], true);
        assert_eq!(
            result["rows"],
            json!([
                [0, "──[ REGISTERS ]──"],
                [1, "  RSP  0x7fffffffdc40   │ RIP  0x555555551aa"],
                [4, "0x555555551aa <main+42>:  mov rax, [rbp - 0x18]"],
                [5, "  中\u{301}文  e\u{301}"],
                [6, "=== title ==="],
            ])
        );
        assert_eq!(
            result["omitted"],
            json!([
                {"start_row": 2, "end_row": 2, "reason": "blank"},
                {"start_row": 3, "end_row": 3, "reason": "separator"},
                {"start_row": 7, "end_row": 7, "reason": "blank"},
            ])
        );
        assert_eq!(result["cursor"], json!({"row": 6, "column": 13}));
    }

    #[test]
    fn keeps_physical_wrapping_and_masks_hidden_cells() {
        let result = shape(&screen(6, 5, "12345678\r\nA\x1b[8m秘密\x1b[0mB\r\n-\r\n=="));
        assert_eq!(
            result["rows"],
            json!([[0, "123456"], [1, "78"], [2, "A    B"], [3, "-"], [4, "=="]])
        );
        assert_eq!(result["wrapped_rows"], json!([0]));
        assert_eq!(result["omitted"], json!([]));
    }
}
