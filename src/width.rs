//! Unicode 18.0.0 ordinary terminal widths; maintenance notes: tools/unicode/README.md.

#[path = "width_tables.rs"]
mod tables;
use std::borrow::Cow;
use tables::*;

/// Terminal columns of one scalar, with C0/C1 controls mapped to zero. The table gives U+17D8
/// width 3; a cell holds at most two columns.
pub fn scalar_width(ch: char) -> u8 {
    if ch.is_ascii() {
        return u8::from(!ch.is_ascii_control());
    }
    let cp = ch as usize;
    let page = usize::from(WIDTH_PAGES[cp >> 8]);
    let packed = WIDTH_DATA[page][(cp >> 2) & 63];
    ((packed >> ((cp & 3) * 2)) & 3).min(2)
}

pub fn text_width(text: &str) -> usize {
    let prefix = printable_ascii_prefix(text.as_bytes());
    prefix
        + text[prefix..]
            .chars()
            .map(|c| usize::from(scalar_width(c)))
            .sum::<usize>()
}

pub(crate) fn printable_ascii_prefix(bytes: &[u8]) -> usize {
    // Whole blocks can vectorize; keep early exit for the remaining bytes.
    let prefix = bytes
        .chunks_exact(32)
        .take_while(|chunk| {
            chunk.iter().fold(true, |printable, byte| {
                printable & (b' '..=b'~').contains(byte)
            })
        })
        .count()
        * 32;
    prefix
        + bytes[prefix..]
            .iter()
            .position(|byte| !(b' '..=b'~').contains(byte))
            .unwrap_or(bytes.len() - prefix)
}

/// One clipping policy keeps the GUI and TUI header labels on the same columns.
pub fn fit_label(text: &str, columns: u16) -> Cow<'_, str> {
    let mut used = 0;
    let mut end = 0;
    for ch in text.chars() {
        used += if ch.is_control() {
            1
        } else {
            usize::from(scalar_width(ch))
        };
        if used > usize::from(columns) {
            break;
        }
        end += ch.len_utf8();
    }
    let text = &text[..end];
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text.trim_end());
    }
    let mut label: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    label.truncate(label.trim_end().len());
    Cow::Owned(label)
}

pub fn fit_middle(text: &str, columns: u16) -> Cow<'_, str> {
    let clean = if text.chars().any(char::is_control) {
        Cow::Owned(
            text.chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect::<String>(),
        )
    } else {
        Cow::Borrowed(text)
    };
    if text_width(&clean) <= usize::from(columns) {
        return clean;
    }
    if columns == 0 {
        return Cow::Borrowed("");
    }
    let left = (columns - 1) / 2;
    let right = usize::from(columns - 1 - left);
    let prefix = fit_label(&clean, left);
    let mut used = 0;
    let mut start = clean.len();
    for (index, ch) in clean.char_indices().rev() {
        let width = usize::from(scalar_width(ch));
        if used + width > right {
            break;
        }
        used += width;
        start = index;
    }
    Cow::Owned([prefix.as_ref(), "…", &clean[start..]].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn middle_clipping_preserves_path_identity_and_unicode_column_budget() {
        let path = "src/很长的目录/important.rs";
        let clipped = fit_middle(path, 18);
        assert!(clipped.starts_with("src/"));
        assert!(clipped.ends_with(".rs"));
        assert!(clipped.contains('…'));
        assert!(text_width(&clipped) <= 18);
        assert_eq!(fit_middle("src/main.rs", 20), "src/main.rs");
        assert!(!fit_middle("src/line\nname.rs", 12).contains('\n'));
    }

    #[test]
    fn labels_clip_by_columns_and_replace_controls() {
        for (text, columns, expected) in [
            ("中A", 1, ""),
            ("中A", 2, "中"),
            ("e\u{301}x", 1, "e\u{301}"),
            ("a\tb", 2, "a"),
            ("a\0b", 3, "a b"),
            ("中 \n尾", 4, "中"),
        ] {
            assert_eq!(fit_label(text, columns), expected);
        }
    }

    #[test]
    fn scalar_policy_includes_unicode_17_and_clamps_width_three() {
        for (ch, expected) in [
            ('a', 1),
            ('中', 2),
            ('\u{301}', 0),
            ('\u{115f}', 2),
            ('\u{1160}', 0),
            ('\u{17a4}', 2),
            ('\u{17d8}', 2),
            ('\u{2d7f}', 1),
            ('\u{1faea}', 2),
            ('\u{1f1e6}', 1),
            ('\0', 0),
            ('\u{85}', 0),
            ('\u{5c8}', 0),
            ('\u{18e00}', 2),
            ('\u{1facc}', 2),
        ] {
            assert_eq!(scalar_width(ch), expected, "{ch:?}");
        }
    }
}
