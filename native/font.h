#ifndef NEXTTY_FONT_H
#define NEXTTY_FONT_H

#include "terminal.h"
#include <cairo.h>

typedef struct NtFonts NtFonts;

typedef struct {
    cairo_scaled_font_t *font;
    uint32_t glyph;
    float x, y;
    uint32_t color;
    unsigned color_glyph, cell_shape;
} NtFontGlyph;

/* Created before the render thread starts, which then makes every other call. Error storage
 * outlives the fonts. */
NtFonts *nt_fonts_new(const char *family, float size, char *error, size_t error_size);
void nt_fonts_free(NtFonts *);
void nt_fonts_metrics(const NtFonts *, float *width, float *height);
void nt_fonts_scale(NtFonts *, double scale);
/* Positions are logical pixels anchored to the supplied UTF-8 byte-to-terminal-column map.
 * Release the returned array with nt_font_glyphs_free. */
int nt_fonts_shape(NtFonts *, const char *, size_t, const NtRun *, size_t,
                   const uint16_t *, NtFontGlyph **glyphs, size_t *count);

void nt_font_glyphs_free(NtFontGlyph *glyphs, size_t count);

#endif
