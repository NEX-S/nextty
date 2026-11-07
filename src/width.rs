//! Unicode 18.0.0 ordinary terminal widths; maintenance notes: tools/unicode/README.md.

#[path = "width_tables.rs"]
mod tables;
use std::borrow::Cow;
use tables::*;

/// Width of one scalar, with C0/C1 controls mapped to zero. U+17D8 has width 3.
/// The terminal grid applies its own two-column clamp at the call site.
pub fn scalar_width(ch: char) -> u8 {
    if ch.is_ascii() {
        return u8::from(!ch.is_ascii_control());
    }
    let cp = ch as usize;
    let page = usize::from(WIDTH_PAGES[cp >> 8]);
    let packed = WIDTH_DATA[page][(cp >> 2) & 63];
    (packed >> ((cp & 3) * 2)) & 3
}

pub fn text_width(text: &str) -> usize {
    let prefix = printable_ascii_prefix(text.as_bytes());
    prefix
        + text[prefix..]
            .chars()
            .map(|c| usize::from(scalar_width(c).min(2)))
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
    let columns = usize::from(columns);
    // With widths capped at two columns, UTF-8 byte length is an upper bound.
    if text.len() <= columns
        && !text[printable_ascii_prefix(text.as_bytes())..]
            .chars()
            .any(char::is_control)
    {
        return Cow::Borrowed(text.trim_end());
    }
    let end = text.len().min(columns);
    // A non-ASCII scalar at the boundary may extend the label without using a column.
    if end < text.len()
        && text.as_bytes()[end].is_ascii()
        && printable_ascii_prefix(&text.as_bytes()[..end]) == end
    {
        return Cow::Borrowed(text[..end].trim_end());
    }
    let mut used = 0usize;
    let mut end = 0;
    let mut controls = false;
    for ch in text.chars() {
        let control = ch.is_control();
        used += if control {
            1
        } else {
            usize::from(scalar_width(ch).min(2))
        };
        if used > columns {
            break;
        }
        controls |= control;
        end += ch.len_utf8();
    }
    let text = &text[..end];
    if !controls {
        return Cow::Borrowed(text.trim_end());
    }
    let mut label = String::with_capacity(text.len());
    label.extend(
        text.chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch }),
    );
    label.truncate(label.trim_end().len());
    Cow::Owned(label)
}

pub fn fit_middle(text: &str, columns: u16) -> Cow<'_, str> {
    let prefix = printable_ascii_prefix(text.as_bytes());
    let clean = if text[prefix..].chars().any(char::is_control) {
        Cow::Owned(
            text.chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect::<String>(),
        )
    } else {
        Cow::Borrowed(text)
    };
    if clean.len() <= usize::from(columns)
        || prefix <= usize::from(columns) && text_width(&clean) <= usize::from(columns)
    {
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
        let width = usize::from(scalar_width(ch).min(2));
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
    fn scalar_policy_includes_unicode_17_and_width_three() {
        for (ch, expected) in [
            ('a', 1),
            ('中', 2),
            ('\u{301}', 0),
            ('\u{115f}', 2),
            ('\u{1160}', 0),
            ('\u{17a4}', 2),
            ('\u{17d8}', 3),
            ('\u{2d7f}', 1),
            ('\u{1faea}', 2),
            ('\u{1f1e6}', 1),
            ('\0', 0),
            ('\u{85}', 0),
        ] {
            assert_eq!(scalar_width(ch), expected, "{ch:?}");
        }
    }

    #[test]
    fn unicode_18_additions_use_the_current_scalar_properties() {
        assert_eq!(scalar_width('\u{5c8}'), 0);
        assert_eq!(scalar_width('\u{18e00}'), 2);
        assert_eq!(scalar_width('\u{1facc}'), 2);
    }
}
