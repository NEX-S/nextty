//! Unicode 18.0.0 ordinary terminal widths; maintenance notes: tools/unicode/README.md.

#[path = "width_tables.rs"]
mod tables;
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
    if text.is_ascii() {
        return text.bytes().filter(|byte| !byte.is_ascii_control()).count();
    }
    text.chars()
        .map(|c| usize::from(scalar_width(c).min(2)))
        .sum()
}

/// One clipping policy keeps the GUI and TUI header labels on the same columns.
pub fn fit_label(text: &str, columns: u16) -> std::borrow::Cow<'_, str> {
    // With widths capped at two columns, UTF-8 byte length is an upper bound.
    if text.len() <= usize::from(columns) && !text.chars().any(char::is_control) {
        return std::borrow::Cow::Borrowed(text.trim_end());
    }
    let end = text.len().min(usize::from(columns));
    // A non-ASCII scalar at the boundary may extend the label without using a column.
    if end < text.len()
        && text.as_bytes()[end].is_ascii()
        && text.as_bytes()[..end]
            .iter()
            .all(|byte| (b' '..=b'~').contains(byte))
    {
        return std::borrow::Cow::Borrowed(text[..end].trim_end());
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
        if used > usize::from(columns) {
            break;
        }
        controls |= control;
        end += ch.len_utf8();
    }
    let text = &text[..end];
    if !controls {
        return std::borrow::Cow::Borrowed(text.trim_end());
    }
    let mut label = String::with_capacity(text.len());
    label.extend(
        text.chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch }),
    );
    label.truncate(label.trim_end().len());
    std::borrow::Cow::Owned(label)
}

#[cfg(test)]
mod tests {
    use super::*;

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
