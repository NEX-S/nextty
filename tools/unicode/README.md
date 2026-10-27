# Unicode width maintenance

The normal build reads `src/width_tables.rs` and `native/font_data.h`; it runs no
generator and needs no network access. The font table supplies Emoji_Presentation
ranges for color-font selection. `src/width.rs` provides `scalar_width(char) -> u8` for Unicode
18.0.0, with East Asian Ambiguous characters narrow and C0/C1 controls zero.
U+17D8 retains its original width of three; the terminal grid applies its own
two-column clamp.

The terminal uses scalar grid positions for Core, preedit placement and the
input-method anchor. It has no separate emoji/script sequence-width algorithm.
A sequence such as `👩‍💻a` must allocate enough scalar cells to place `a` after
both emoji; measuring the emoji sequence as a single two-column glyph disagrees
with that grid and can make the native shaper reject the preedit cells.

`generate.py` is a project-specific Python standard-library generator. It derives
deduplicated width pages directly from six official Unicode files pinned by URL
and SHA-256 in `SOURCES.json`: EastAsianWidth, DerivedCoreProperties,
HangulSyllableType, GraphemeBreakProperty, PropList and emoji-data. The generated
data is covered by `LICENSE-UNICODE` (Unicode License V3). Updating is an explicit
maintenance action:

```sh
python3 tools/unicode/generate.py --download
python3 tools/unicode/generate.py --check
```

A cached input directory can be selected with `--data PATH`; without `--download`
the generator is entirely offline. To change Unicode versions, review the input
manifest and scalar policy together, then repeat all-scalar differential checks
against the chosen baseline. Do not silently accept changed input hashes.

The scalar behavior follows the public ordinary-width rules documented by
[unicode-width 0.2.2](https://docs.rs/unicode-width/0.2.2/unicode_width/), which was
used as a temporary differential oracle for the Unicode 17 baseline during the
dependency removal. Unicode 18 applies the same scalar policy to the pinned
official version 18 data. The generator and deduplicated width pages were written
for this application; no crate source, generated tables or generation script is
vendored. Only the
ordinary scalar policy is implemented. Unused sequence-width and CJK-width
algorithms and their dedicated data inputs are omitted.

Primary data documentation:
[UCD 18.0.0](https://www.unicode.org/Public/18.0.0/ucd/ReadMe.txt),
[UAX #44](https://www.unicode.org/reports/tr44/),
[Emoji 18.0 data](https://www.unicode.org/Public/18.0.0/ucd/emoji/emoji-data.txt).
