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
    unsigned color_glyph, straight;
} NtFontGlyph;

/* All calls run on the renderer worker. Error storage outlives the font owner. */
NtFonts *nt_fonts_new(const char *family, float size, char *error, size_t error_size);
void nt_fonts_free(NtFonts *);
void nt_fonts_metrics(const NtFonts *, float *width, float *height);
int nt_fonts_scale(NtFonts *, double scale);
/* Each contiguous font group owns one scaled-font reference; the caller owns
 * the array and releases these references before freeing it. Positions are
 * logical pixels anchored to the supplied UTF-8 byte-to-terminal-column map. */
int nt_fonts_shape(NtFonts *, const char *, size_t, const NtRun *, size_t,
                   const uint16_t *, NtFontGlyph **glyphs, size_t *count);

#endif
