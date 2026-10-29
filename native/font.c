#include "font.h"
#include "font_data.h"
#include <cairo-ft.h>
#include <fontconfig/fontconfig.h>
#include <fribidi.h>
#include <hb-ft.h>
#include <hb-ot.h>
#include <limits.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <utf8proc.h>

typedef struct {
    cairo_scaled_font_t *cairo;
    hb_font_t *hb;
    FcCharSet *charset; /* Borrowed from the style's font set; NULL excludes a candidate. */
    int color;
} Font;

typedef struct {
    FcPattern *query;
    FcFontSet *set;
    Font *fonts;
} Style;

struct NtFonts {
    FcConfig *config;
    char *family;
    char *error;
    size_t error_size;
    double size, scale;
    float width, height, baseline;
    cairo_font_options_t *options;
    Style styles[8];
    hb_buffer_t *buffer;
    struct { uint32_t key; Font *font; } choices[2048]; /* Borrowed from styles. */
};

typedef struct {
    uint32_t byte, cluster;
    hb_script_t script;
    unsigned flags;
    uint32_t color;
    int ignorable;
    Font *font;
} Character;

static int fail(NtFonts *f, const char *format, ...) {
    va_list args;
    va_start(args, format);
    vsnprintf(f->error, f->error_size, format, args);
    va_end(args);
    return 0;
}

static int installed_family(NtFonts *f, const char *family) {
    if (!strcmp(family, "monospace")) return 1;
    FcPattern *query = FcPatternCreate();
    FcObjectSet *objects = FcObjectSetBuild(FC_FAMILY, NULL);
    if (!query || !objects) {
        if (query) FcPatternDestroy(query);
        if (objects) FcObjectSetDestroy(objects);
        return fail(f, "Cannot allocate Fontconfig font query");
    }
    FcPatternAddString(query, FC_FAMILY, (const FcChar8 *)family);
    FcFontSet *set = FcFontList(f->config, query, objects);
    int found = 0;
    if (set) {
        for (int i = 0; i < set->nfont && !found; ++i) {
            FcChar8 *name;
            for (int j = 0; FcPatternGetString(set->fonts[i], FC_FAMILY, j, &name) == FcResultMatch; ++j)
                if (!FcStrCmpIgnoreCase(name, (const FcChar8 *)family)) found = 1;
        }
        FcFontSetDestroy(set);
    }
    FcObjectSetDestroy(objects);
    FcPatternDestroy(query);
    return found || fail(f, "font_family \"%s\" is not installed; install it or use monospace", family);
}

static void clear_styles(NtFonts *f) {
    memset(f->choices, 0, sizeof f->choices);
    for (size_t i = 0; i < sizeof f->styles / sizeof *f->styles; ++i) {
        Style *s = &f->styles[i];
        if (s->set) {
            for (int j = 0; j < s->set->nfont; ++j) {
                if (s->fonts && s->fonts[j].hb) hb_font_destroy(s->fonts[j].hb);
                if (s->fonts && s->fonts[j].cairo) cairo_scaled_font_destroy(s->fonts[j].cairo);
            }
            FcFontSetDestroy(s->set);
        }
        if (s->query) FcPatternDestroy(s->query);
        free(s->fonts);
        *s = (Style){0};
    }
}

void nt_fonts_free(NtFonts *f) {
    if (!f) return;
    clear_styles(f);
    if (f->buffer) hb_buffer_destroy(f->buffer);
    if (f->options) cairo_font_options_destroy(f->options);
    if (f->config) FcConfigDestroy(f->config);
    free(f->family);
    free(f);
}

static Style *style(NtFonts *f, unsigned flags) {
    Style *s = &f->styles[flags];
    if (s->set) return s;
    s->query = FcPatternCreate();
    if (!s->query) { fail(f, "Cannot allocate font pattern"); return NULL; }
    FcPatternAddString(s->query, FC_FAMILY, (const FcChar8 *)(flags & 4 ? "emoji" : f->family));
    FcPatternAddDouble(s->query, FC_PIXEL_SIZE, f->size * f->scale);
    FcPatternAddInteger(s->query, FC_WEIGHT, flags & 1 ? FC_WEIGHT_BOLD : FC_WEIGHT_REGULAR);
    FcPatternAddInteger(s->query, FC_SLANT, flags & 2 ? FC_SLANT_ITALIC : FC_SLANT_ROMAN);
    if (flags & 4) FcPatternAddBool(s->query, FC_COLOR, FcTrue);
    FcConfigSubstitute(f->config, s->query, FcMatchPattern);
    cairo_ft_font_options_substitute(f->options, s->query);
    FcDefaultSubstitute(s->query);
    FcResult result;
    s->set = FcFontSort(f->config, s->query, FcFalse, NULL, &result);
    if (!s->set || !s->set->nfont) {
        fail(f, "Fontconfig could not load font_family \"%s\"", f->family);
        return NULL;
    }
    s->fonts = calloc((size_t)s->set->nfont, sizeof *s->fonts);
    if (!s->fonts) { fail(f, "Cannot allocate fallback fonts"); return NULL; }
    for (int i = 0; i < s->set->nfont; ++i) {
        if (FcPatternGetCharSet(s->set->fonts[i], FC_CHARSET, 0, &s->fonts[i].charset) != FcResultMatch)
            s->fonts[i].charset = NULL;
        if (flags & 4) {
            FcBool color = FcFalse;
            FcPatternGetBool(s->set->fonts[i], FC_COLOR, 0, &color);
            if (!color) s->fonts[i].charset = NULL;
        }
    }
    return s;
}

static Font *load_font(NtFonts *f, Style *s, int index) {
    Font *font = &s->fonts[index];
    if (font->cairo) return font;
    FcPattern *pattern = FcFontRenderPrepare(f->config, s->query, s->set->fonts[index]);
    if (!pattern) { fail(f, "Cannot prepare fallback font"); return NULL; }
    FcMatrix transform, *part;
    FcMatrixInit(&transform);
    for (int i = 0; FcPatternGetMatrix(pattern, FC_MATRIX, i, &part) == FcResultMatch; ++i)
        FcMatrixMultiply(&transform, &transform, part);
    cairo_font_face_t *face = cairo_ft_font_face_create_for_pattern(pattern);
    FcPatternDestroy(pattern);
    cairo_matrix_t matrix, ctm;
    cairo_matrix_init(&matrix, transform.xx, -transform.yx, -transform.xy, transform.yy, 0, 0);
    cairo_matrix_scale(&matrix, f->size * f->scale, f->size * f->scale);
    cairo_matrix_init_identity(&ctm);
    font->cairo = cairo_scaled_font_create(face, &matrix, &ctm, f->options);
    cairo_font_face_destroy(face);
    cairo_status_t status = cairo_scaled_font_status(font->cairo);
    if (status != CAIRO_STATUS_SUCCESS) {
        fail(f, "Cannot create Cairo font: %s", cairo_status_to_string(status));
        cairo_scaled_font_destroy(font->cairo);
        font->cairo = NULL;
        return NULL;
    }
    FT_Face ft = cairo_ft_scaled_font_lock_face(font->cairo);
    if (!ft) {
        fail(f, "Cannot access Cairo FreeType face");
        cairo_scaled_font_destroy(font->cairo);
        font->cairo = NULL;
        return NULL;
    }
    font->hb = hb_ft_font_create_referenced(ft);
    /* OT metrics avoid FreeType's hinted advances; Cairo still owns rasterization. */
    hb_ot_font_set_funcs(font->hb);
    hb_face_t *hb_face = hb_font_get_face(font->hb);
    font->color = hb_ot_color_has_paint(hb_face) || hb_ot_color_has_layers(hb_face) ||
                  hb_ot_color_has_png(hb_face) || hb_ot_color_has_svg(hb_face);
    double xscale = hypot(transform.xx, transform.yx);
    double yscale = fabs(transform.xx * transform.yy - transform.xy * transform.yx) / xscale;
    double slant = (transform.xy * transform.xx + transform.yy * transform.yx) /
                   (transform.xy * transform.xy + transform.yy * transform.yy);
    hb_font_set_scale(font->hb, (int)llround(f->size * f->scale * xscale * 1024),
                      (int)llround(f->size * f->scale * yscale * 1024));
    hb_font_set_synthetic_slant(font->hb, (float)slant);
    cairo_ft_scaled_font_unlock_face(font->cairo);
    return font;
}

int nt_fonts_scale(NtFonts *f, double scale) {
    if (!isfinite(scale) || scale <= 0 || f->size * scale > INT_MAX / 1024.0)
        return fail(f, "Invalid window/font scale %.6g", scale);
    if (scale != f->scale) {
        clear_styles(f);
        f->scale = scale;
    }
    return 1;
}

void nt_fonts_metrics(const NtFonts *f, float *width, float *height) {
    if (width) *width = f->width;
    if (height) *height = f->height;
}

NtFonts *nt_fonts_new(const char *family, float size, char *error, size_t error_size) {
    NtFonts *f = calloc(1, sizeof *f);
    if (!f) { snprintf(error, error_size, "Cannot allocate font owner"); return NULL; }
    f->error = error;
    f->error_size = error_size;
    f->size = size;
    f->scale = 1;
    if (!nt_fonts_scale(f, 1)) goto failed;
    f->buffer = hb_buffer_create();
    hb_buffer_set_flags(f->buffer, HB_BUFFER_FLAG_REMOVE_DEFAULT_IGNORABLES);
    f->family = malloc(strlen(family) + 1);
    if (!f->family) { fail(f, "Cannot allocate font family"); goto failed; }
    strcpy(f->family, family);
    f->config = FcConfigReference(FcConfigGetCurrent());
    if (!f->config) { fail(f, "Cannot initialize Fontconfig"); goto failed; }
    if (!installed_family(f, family)) goto failed;
    f->options = cairo_font_options_create();
    cairo_font_options_set_antialias(f->options, CAIRO_ANTIALIAS_GRAY);
    cairo_font_options_set_hint_style(f->options, CAIRO_HINT_STYLE_SLIGHT);
    cairo_font_options_set_hint_metrics(f->options, CAIRO_HINT_METRICS_OFF);
    Style *s = style(f, 0);
    if (!s) goto failed;
    Font *font = NULL;
    for (int i = 0; i < s->set->nfont; ++i) {
        FcCharSet *charset = s->fonts[i].charset;
        if (charset && FcCharSetHasChar(charset, 'm')) {
            font = load_font(f, s, i);
            break;
        }
    }
    if (!font) { fail(f, "Cannot load terminal metric glyph"); goto failed; }
    hb_codepoint_t glyph;
    hb_font_get_nominal_glyph(font->hb, 'm', &glyph);
    f->width = ceilf((float)hb_font_get_glyph_h_advance(font->hb, glyph) / 1024);
    f->height = ceilf(size * 1.35f);
    hb_font_extents_t extents;
    if (!hb_font_get_h_extents(font->hb, &extents)) { fail(f, "Cannot load font metrics"); goto failed; }
    float ascent = (float)extents.ascender / 1024, descent = (float)-extents.descender / 1024;
    f->baseline = (f->height - ascent - descent) * 0.5f + ascent;
    if (f->width < 1 || !isfinite(f->width) || !isfinite(f->height)) {
        fail(f, "Font produced invalid terminal cell metrics");
        goto failed;
    }
    return f;
failed:
    nt_fonts_free(f);
    return NULL;
}

static int emoji_presentation(uint32_t code) {
    if (code < nt_emoji_presentation[0][0]) return 0;
    size_t first = 0, end = sizeof nt_emoji_presentation / sizeof *nt_emoji_presentation;
    while (first < end) {
        size_t middle = first + (end - first) / 2;
        if (code < nt_emoji_presentation[middle][0]) end = middle;
        else if (code > nt_emoji_presentation[middle][1]) first = middle + 1;
        else return 1;
    }
    return 0;
}

static Font *find_cluster_font(NtFonts *f, const FriBidiChar *codes, Character *chars, size_t start, size_t end, unsigned flags) {
    /* Cached font choices do not inspect coverage properties. */
    for (size_t j = start; j < end; ++j)
        chars[j].ignorable = codes[j] >= 0x80 && utf8proc_get_property((int32_t)codes[j])->ignorable;
    int emoji = 0;
    for (size_t j = start; j < end; ++j) {
        if (emoji_presentation(codes[j])) emoji = 1;
        if (codes[j] == 0xfe0e) { emoji = 0; break; }
        if (codes[j] == 0xfe0f) emoji = 1;
    }
    for (int pass = 0; pass <= emoji; ++pass) {
        Style *s = style(f, flags | (emoji && !pass ? 4 : 0));
        if (!s) return NULL;
        for (int i = 0; i < s->set->nfont; ++i) {
            FcCharSet *charset = s->fonts[i].charset;
            if (!charset) continue;
            size_t j = start;
            for (; j < end; ++j)
                if (!chars[j].ignorable && !FcCharSetHasChar(charset, codes[j])) break;
            if (j == end) return load_font(f, s, i);
        }
    }
    /* Keep the base visible when no font covers all combining characters. */
    Style *s = style(f, flags);
    if (!s) return NULL;
    for (int i = 0; i < s->set->nfont; ++i) {
        FcCharSet *charset = s->fonts[i].charset;
        if (charset && FcCharSetHasChar(charset, codes[start])) return load_font(f, s, i);
    }
    return load_font(f, s, 0);
}

static inline Font *cluster_font(NtFonts *f, const FriBidiChar *codes, Character *chars, size_t start, size_t end, unsigned flags) {
    if (end != start + 1)
        return find_cluster_font(f, codes, chars, start, end, flags);
    uint32_t key = (codes[start] << 2) | flags;
    size_t slot = (key * 2654435761u) >> 21;
    if (f->choices[slot].font && f->choices[slot].key == key)
        return f->choices[slot].font;
    Font *font = find_cluster_font(f, codes, chars, start, end, flags);
    f->choices[slot].key = key;
    f->choices[slot].font = font;
    return font;
}

static unsigned color_glyph(Font *font, uint32_t glyph) {
    if (!font->color) return 0;
    hb_face_t *face = hb_font_get_face(font->hb);
    if (hb_ot_color_glyph_has_paint(face, glyph) ||
        hb_ot_color_glyph_get_layers(face, glyph, 0, NULL, NULL)) return 1;
    if (hb_ot_color_has_png(face)) {
        hb_blob_t *blob = hb_ot_color_glyph_reference_png(font->hb, glyph);
        unsigned length = hb_blob_get_length(blob);
        hb_blob_destroy(blob);
        if (length) return 1;
    }
    if (hb_ot_color_has_svg(face)) {
        hb_blob_t *blob = hb_ot_color_glyph_reference_svg(face, glyph);
        unsigned length = hb_blob_get_length(blob);
        hb_blob_destroy(blob);
        if (length) return 1;
    }
    return 0;
}

void nt_font_glyphs_free(NtFontGlyph *glyphs, size_t count) {
    for (size_t i = 0; i < count; ++i)
        if (!i || glyphs[i].font != glyphs[i - 1].font) cairo_scaled_font_destroy(glyphs[i].font);
    free(glyphs);
}

int nt_fonts_shape(NtFonts *f, const char *text, size_t length, const NtRun *runs,
                   size_t run_count, const uint16_t *columns, NtFontGlyph **glyphs, size_t *count) {
    *glyphs = NULL;
    *count = 0;
    if (!length) return 1;
    if (length > INT_MAX) return fail(f, "Terminal line is too long to shape");
    size_t ascii_end = 0;
    while (ascii_end < length && (unsigned char)text[ascii_end] >= 0x20 &&
           (unsigned char)text[ascii_end] <= 0x7e) ++ascii_end;
    int printable_ascii = ascii_end == length;
    size_t characters = length;
    if (!printable_ascii) {
        characters = 0;
        for (size_t i = 0; i < length; ++i)
            characters += ((unsigned char)text[i] & 0xc0) != 0x80;
    }
    size_t item_size = sizeof(Character) + sizeof(FriBidiChar) + sizeof(FriBidiLevel) +
        (printable_ascii ? 0 : sizeof(FriBidiCharType) + sizeof(FriBidiBracketType));
    if (characters > SIZE_MAX / item_size) return fail(f, "Line itemization is too large");
    /* Keep the character, code and bidi arrays in one scratch allocation. */
    Character *chars = malloc(characters * item_size);
    if (!chars) return fail(f, "Cannot allocate line itemization");
    FriBidiChar *codes = (FriBidiChar *)(chars + characters);
    FriBidiLevel *levels = (FriBidiLevel *)(codes + characters);
    NtFontGlyph *out = NULL;
    size_t used = 0, capacity = 0;
    hb_buffer_t *buffer = f->buffer;
    int success = 0;
    size_t byte = 0, run = 0, cluster = 0, n = 0;
    int32_t grapheme_state = 0;
    hb_unicode_funcs_t *unicode = hb_unicode_funcs_get_default();
    hb_script_t previous = HB_SCRIPT_UNKNOWN;
    do {
        int32_t decoded = (unsigned char)text[byte];
        utf8proc_ssize_t bytes = 1;
        hb_script_t script = HB_SCRIPT_LATIN;
        int boundary = 1;
        if (!printable_ascii) {
            bytes = utf8proc_iterate((const uint8_t *)text + byte, (ptrdiff_t)(length - byte), &decoded);
            if (bytes < 0) { fail(f, "Cannot decode terminal text: %s", utf8proc_errmsg(bytes)); goto done; }
            script = hb_unicode_script(unicode, (uint32_t)decoded);
            if (script != HB_SCRIPT_COMMON && script != HB_SCRIPT_INHERITED && script != HB_SCRIPT_UNKNOWN) previous = script;
            else script = previous;
            boundary = !n || utf8proc_grapheme_break_stateful((int32_t)codes[n - 1], decoded, &grapheme_state);
        }
        uint32_t code = (uint32_t)decoded;
        codes[n] = code;
        while (run < run_count && runs[run].end <= byte) ++run;
        int in_run = run < run_count && runs[run].start <= byte;
        /* The terminal's newer Unicode tables also identify combining marks. */
        if (boundary && (!n || !columns || columns[byte] != columns[chars[n - 1].byte])) cluster = n;
        chars[n] = (Character){ .byte = byte, .cluster = cluster,
            .script = script,
            .flags = in_run ? runs[run].flags & 3 : 0,
            .color = in_run ? runs[run].color : 0xffffffff };
        byte += (size_t)bytes;
        ++n;
    } while (byte < length);
    /* With an automatic base, printable ASCII resolves entirely to level zero. */
    if (printable_ascii) {
        memset(levels, 0, n * sizeof *levels);
    } else {
        FriBidiCharType *types = (FriBidiCharType *)(codes + characters);
        FriBidiBracketType *brackets = (FriBidiBracketType *)(types + characters);
        levels = (FriBidiLevel *)(brackets + characters);
        fribidi_get_bidi_types(codes, (FriBidiStrIndex)n, types);
        fribidi_get_bracket_types(codes, (FriBidiStrIndex)n, types, brackets);
        FriBidiParType base = FRIBIDI_PAR_ON;
        if (!fribidi_get_par_embedding_levels_ex(types, brackets, (FriBidiStrIndex)n, &base, levels)) {
            fail(f, "FriBidi could not itemize terminal line"); goto done;
        }
    }
    if (!printable_ascii) {
        hb_script_t next = HB_SCRIPT_LATIN;
        for (size_t i = n; i--;) {
            if (chars[i].script == HB_SCRIPT_UNKNOWN) chars[i].script = next;
            else next = chars[i].script;
        }
    }
    for (size_t i = 0; i < n;) {
        size_t end = i + 1;
        while (end < n && chars[end].cluster == i) ++end;
        Font *font = cluster_font(f, codes, chars, i, end, chars[i].flags);
        if (!font) goto done;
        for (size_t j = i; j < end; ++j) {
            if (chars[j].flags == chars[i].flags) chars[j].font = font;
            else {
                chars[j].font = cluster_font(f, codes, chars, i, end, chars[j].flags);
                if (!chars[j].font) goto done;
            }
        }
        i = end;
    }
    hb_feature_t features[] = {{ HB_TAG('l','i','g','a'), 0, 0, UINT_MAX }, { HB_TAG('c','a','l','t'), 0, 0, UINT_MAX }};
    hb_language_t language = hb_language_get_default();
    double pen = 0, anchor = 0;
    size_t previous_column = SIZE_MAX;
    for (size_t start = 0; start < n;) {
        size_t end = start + 1;
        while (end < n && chars[end].font == chars[start].font &&
               chars[end].color == chars[start].color &&
               chars[end].script == chars[start].script && levels[end] == levels[start]) ++end;
        size_t end_byte = end < n ? chars[end].byte : length;
        Font *font = chars[start].font;
        hb_buffer_clear_contents(buffer);
        hb_buffer_set_direction(buffer, levels[start] & 1 ? HB_DIRECTION_RTL : HB_DIRECTION_LTR);
        hb_buffer_set_script(buffer, chars[start].script);
        hb_buffer_set_language(buffer, language);
        hb_buffer_add_codepoints(buffer, codes, (int)n, (unsigned)start, (int)(end - start));
        hb_shape(font->hb, buffer, features, sizeof features / sizeof *features);
        unsigned shaped;
        hb_glyph_info_t *info = hb_buffer_get_glyph_infos(buffer, &shaped);
        hb_glyph_position_t *positions = hb_buffer_get_glyph_positions(buffer, NULL);
        if (!hb_buffer_allocation_successful(buffer)) { fail(f, "HarfBuzz could not allocate shaped line"); goto done; }
        /* The terminal column map uses UTF-8 byte offsets. */
        if (!printable_ascii)
            for (unsigned j = 0; j < shaped; ++j) info[j].cluster = chars[info[j].cluster].byte;
        hb_codepoint_t blank_glyph = HB_CODEPOINT_INVALID;
        for (unsigned j = 0; j < shaped; ++j) {
            size_t offset = info[j].cluster;
            size_t column = columns ? columns[offset] : offset;
            if (column != previous_column) { anchor = pen; previous_column = column; }
            unsigned straight = 0;
            size_t cluster_end = end_byte;
            if (levels[start] & 1) {
                if (j) cluster_end = info[j - 1].cluster;
            } else if (j + 1 < shaped) cluster_end = info[j + 1].cluster;
            if ((unsigned char)text[offset] == 0xe2 && offset + 3 <= length &&
                (unsigned char)text[offset + 1] == 0x94 &&
                ((unsigned char)text[offset + 2] == 0x80 || (unsigned char)text[offset + 2] == 0x82) &&
                cluster_end == offset + 3 &&
                (!j || info[j - 1].cluster != offset) && (j + 1 == shaped || info[j + 1].cluster != offset) &&
                (!columns || offset + 3 == length || columns[offset + 3] != column))
                straight = (unsigned char)text[offset + 2] == 0x80 ? 0x2500 : 0x2502;
            hb_glyph_extents_t extents;
            int blank = text[offset] == ' ' && (info[j].codepoint == blank_glyph ||
                        (hb_font_get_glyph_extents(font->hb, info[j].codepoint, &extents) &&
                         (!extents.width || !extents.height)));
            if (blank) blank_glyph = info[j].codepoint;
            if (!blank) {
                if (used == capacity) {
                    size_t needed = used + shaped - j;
                    capacity = capacity * 2 > needed ? capacity * 2 : needed;
                    NtFontGlyph *grown = realloc(out, capacity * sizeof *out);
                    if (!grown) { fail(f, "Cannot allocate line glyphs"); goto done; }
                    out = grown;
                }
                if (!used || out[used - 1].font != font->cairo) cairo_scaled_font_reference(font->cairo);
                out[used++] = (NtFontGlyph){ .font = font->cairo, .glyph = info[j].codepoint,
                    .x = (float)(column * f->width + (straight ? 0 :
                         (pen - anchor + positions[j].x_offset / 1024.0) / f->scale)),
                    .y = f->baseline - (float)(positions[j].y_offset / (1024.0 * f->scale)),
                    .color = chars[start].color, .color_glyph = color_glyph(font, info[j].codepoint), .straight = straight };
            }
            pen += positions[j].x_advance / 1024.0;
        }
        start = end;
    }
    *glyphs = out;
    *count = used;
    success = 1;
done:
    if (!success) nt_font_glyphs_free(out, used);
    free(chars);
    return success;
}
