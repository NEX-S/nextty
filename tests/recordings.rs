use nextty::core::{Core, Flags, WindowSize};
use nextty::vt::{Color, Parser};

struct Snapshot {
    columns: u16,
    lines: u16,
    rows: &'static [Row],
    /// Combining characters by row and column.
    extras: &'static [(usize, usize, &'static str)],
}

struct Row {
    text: &'static str,
    /// Columns `start..end` with foreground, background and flags; other cells have the defaults.
    styles: &'static [(usize, usize, &'static str, &'static str, &'static str)],
}

fn color(value: Color) -> String {
    match value {
        Color::Named(name) => format!("named:{name:?}"),
        Color::Indexed(index) => format!("indexed:{index}"),
        Color::Spec(rgb) => format!("rgb:{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b),
    }
}

fn flags(value: Flags) -> String {
    [
        ("BOLD", Flags::BOLD),
        ("ITALIC", Flags::ITALIC),
        ("DIM", Flags::DIM),
        ("INVERSE", Flags::INVERSE),
        ("UNDERLINE", Flags::UNDERLINE),
        ("DOUBLE_UNDERLINE", Flags::DOUBLE_UNDERLINE),
        ("UNDERCURL", Flags::UNDERCURL),
        ("DOTTED_UNDERLINE", Flags::DOTTED_UNDERLINE),
        ("DASHED_UNDERLINE", Flags::DASHED_UNDERLINE),
        ("STRIKEOUT", Flags::STRIKEOUT),
        ("HIDDEN", Flags::HIDDEN),
        ("WIDE_CHAR", Flags::WIDE_CHAR),
        ("WIDE_CHAR_SPACER", Flags::WIDE_CHAR_SPACER),
    ]
    .into_iter()
    .filter_map(|(name, flag)| value.contains(flag).then_some(name))
    .collect::<Vec<_>>()
    .join("|")
}

fn replay(recording: &[u8], expected: &Snapshot) {
    for chunk_size in [recording.len(), 7] {
        let mut core = Core::new(
            WindowSize {
                num_cols: expected.columns,
                num_lines: expected.lines,
                cell_width: 10,
                cell_height: 22,
            },
            0xd8dee9,
            0x14161b,
        );
        let mut parser = Parser::new();
        for chunk in recording.chunks(chunk_size) {
            parser.advance(&mut core, chunk);
        }
        core.finish_sync();
        for (y, expected) in expected.rows.iter().enumerate() {
            let row = core.row(y);
            let text: String = row
                .cells
                .iter()
                .map(|cell| match cell.c {
                    '\t' => ' ',
                    ch => ch,
                })
                .collect();
            assert_eq!(text, expected.text, "row {y}, chunk size {chunk_size}");
            for (x, cell) in row.cells.iter().enumerate() {
                let (fg, bg, style) = expected
                    .styles
                    .iter()
                    .find(|(start, end, ..)| (*start..*end).contains(&x))
                    .map_or(("named:Foreground", "named:Background", ""), |style| {
                        (style.2, style.3, style.4)
                    });
                assert_eq!(color(cell.fg), fg, "foreground at {y}:{x}");
                assert_eq!(color(cell.bg), bg, "background at {y}:{x}");
                assert_eq!(flags(cell.flags), style, "style at {y}:{x}");
            }
        }
        let extras = (0..expected.rows.len()).flat_map(|y| {
            let row = core.row(y);
            let extras = row.extras().iter();
            extras
                .filter(|(_, extra)| !extra.zerowidth.is_empty())
                .map(move |(x, extra)| (y, *x, extra.zerowidth.iter().collect::<String>()))
        });
        let expected_extras = expected
            .extras
            .iter()
            .map(|(y, x, text)| (*y, *x, text.to_string()));
        assert!(
            extras.eq(expected_extras),
            "combining characters, chunk size {chunk_size}"
        );
    }
}

macro_rules! recordings {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                replay(
                    include_bytes!(concat!("recordings/", stringify!($name), ".bin")),
                    &include!(concat!("recordings/", stringify!($name), ".rs")),
                );
            }
        )*
    };
}

recordings!(
    vim_simple_edit,
    tmux_git_log,
    tmux_htop,
    zerowidth,
    wrapline_alt_toggle,
    scroll_in_region_up_preserves_history,
);
