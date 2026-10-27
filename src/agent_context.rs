use crate::core::{Flags, Mode};
use crate::screen::Screen;
use crate::vt::{Color, NamedColor};
use serde_json::{Value, json};
use std::fmt::Write;

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
    let mut result = metadata(screen);
    result["rows"] = json!(rows);
    result["omitted"] = json!(omitted);
    result
}

fn metadata(screen: &Screen) -> Value {
    let wrapped_rows: Vec<_> = screen
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.wrapped.then_some(index))
        .collect();
    let cursor = screen.cursor();
    json!({
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

fn html_escape(text: &str, output: &mut String) {
    for c in text.chars() {
        match c {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(c),
        }
    }
}

fn html_color(screen: &Screen, color: Color, dim: bool) -> String {
    let mut color = screen.color(color);
    if dim {
        color = color.dimmed();
    }
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

pub(crate) fn html(screen: &Screen) -> Value {
    let mut styles = Vec::<String>::new();
    let mut content = String::new();
    for (index, row) in screen.rows.iter().enumerate() {
        write!(content, "<span data-row=\"{index}\">").unwrap();
        let end = row
            .non_default_len()
            .max(row.extras().last().map_or(0, |(column, _)| column + 1));
        let mut previous = None;
        for (_, cell, extra) in row.cells_with_extras(0..end) {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = html_color(screen, cell.fg, cell.flags.contains(Flags::DIM));
            let mut bg = html_color(screen, cell.bg, false);
            let inverse = cell.flags.contains(Flags::INVERSE);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            let mut style = format!("color:{fg};background:{bg}");
            if cell.flags.contains(Flags::BOLD) {
                style.push_str(";font-weight:bold");
            }
            if cell.flags.contains(Flags::ITALIC) {
                style.push_str(";font-style:italic");
            }
            let underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
            let strike = cell.flags.contains(Flags::STRIKEOUT);
            if underline || strike {
                style.push_str(";text-decoration-line:");
                if underline {
                    style.push_str("underline");
                }
                if strike {
                    if underline {
                        style.push(' ');
                    }
                    style.push_str("line-through");
                }
                if underline {
                    let kind = if cell.flags.contains(Flags::UNDERCURL) {
                        "wavy"
                    } else if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                        "double"
                    } else if cell.flags.contains(Flags::DOTTED_UNDERLINE) {
                        "dotted"
                    } else if cell.flags.contains(Flags::DASHED_UNDERLINE) {
                        "dashed"
                    } else {
                        "solid"
                    };
                    write!(style, ";text-decoration-style:{kind}").unwrap();
                    if let Some(color) = extra.and_then(|e| e.underline_color) {
                        write!(
                            style,
                            ";text-decoration-color:{}",
                            html_color(screen, color, false)
                        )
                        .unwrap();
                    }
                }
            }
            let class = styles.iter().position(|s| *s == style).unwrap_or_else(|| {
                styles.push(style);
                styles.len() - 1
            });
            if previous != Some((class, inverse)) {
                if previous.is_some() {
                    content.push_str("</span>");
                }
                write!(content, "<span class=\"s{class}\"").unwrap();
                if inverse {
                    content.push_str(" data-inverse=\"true\"");
                }
                content.push('>');
                previous = Some((class, inverse));
            }
            if cell.flags.contains(Flags::HIDDEN) {
                content.push(' ');
                if cell.flags.contains(Flags::WIDE_CHAR) {
                    content.push(' ');
                }
            } else {
                html_escape(&cell.c.to_string(), &mut content);
                if let Some(extra) = extra {
                    for c in &extra.zerowidth {
                        html_escape(&c.to_string(), &mut content);
                    }
                }
            }
        }
        if previous.is_some() {
            content.push_str("</span>");
        }
        content.push_str("</span>\n");
    }
    let mut output = String::from("<style>");
    for (index, style) in styles.iter().enumerate() {
        write!(output, ".s{index}{{{style}}}").unwrap();
    }
    write!(
        output,
        "</style><pre data-columns=\"{}\" data-lines=\"{}\" style=\"margin:0;white-space:pre;font-family:monospace;color:{};background:{}\">{content}</pre>",
        screen.columns(),
        screen.screen_lines(),
        html_color(screen, Color::Named(NamedColor::Foreground), false),
        html_color(screen, Color::Named(NamedColor::Background), false),
    )
    .unwrap();
    let mut result = metadata(screen);
    result["html"] = Value::String(output);
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

    #[test]
    fn html_preserves_color_inverse_and_text_without_leaking_hidden_cells() {
        let source = screen(
            40,
            5,
            "\x1b[38;2;255;0;0mAB\x1b[7mC\x1b[0m<&>\"'\r\n\x1b[1;2mD\x1b[0m\x1b[8m秘密\x1b[0m中e\u{301}",
        );
        let text_view = shape(&source);
        let result = html(&source);
        assert!(result.get("rows").is_none());
        assert!(result.get("omitted").is_none());
        assert_eq!(result["cursor"], text_view["cursor"]);
        assert_eq!(result["wrapped_rows"], text_view["wrapped_rows"]);
        let output = result["html"].as_str().unwrap();
        assert!(output.contains("color:#ff0000;background:#000000"));
        assert!(output.contains("color:#000000;background:#ff0000"));
        assert!(output.contains("data-inverse=\"true\">C</span>"));
        assert!(output.contains(">AB</span>"));
        assert!(output.contains("&lt;&amp;&gt;&quot;&#39;"));
        assert!(output.contains("color:#a8a8a8;background:#000000;font-weight:bold"));
        assert!(!output.contains('秘'));
        assert!(!output.contains('密'));
        assert!(output.contains("    中e\u{301}"));
        assert_eq!(output.matches("data-row=").count(), 5);
    }

    #[test]
    fn html_keeps_background_on_blank_cells_and_original_row_numbers() {
        let result = html(&screen(
            8,
            3,
            "\x1b[48;2;0;0;238m\x1b[2K\x1b[0m\r\n────────",
        ));
        let output = result["html"].as_str().unwrap();
        assert!(output.contains("background:#0000ee"));
        assert!(output.contains(">        </span>"));
        assert!(output.contains("data-row=\"1\""));
        assert!(output.contains("────────"));
    }
}
