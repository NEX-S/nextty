#ifndef NEXTTY_H
#define NEXTTY_H

#include <stddef.h>
#include <stdint.h>
#include <poll.h>

typedef struct NtWindow NtWindow;
typedef struct NtRenderer NtRenderer;
typedef struct NtLine NtLine;

enum { NT_SHIFT = 1, NT_CONTROL = 2, NT_ALT = 4, NT_SUPER = 8 };
enum {
    NT_KEY = 1, NT_TEXT, NT_PREEDIT, NT_MOTION, NT_BUTTON, NT_SCROLL,
    NT_FOCUS, NT_RESIZE, NT_CLOSE, NT_PASTE, NT_EXPOSE, NT_PRESENTED,
    NT_WINDOW_STATE, NT_POINTER_LEAVE
};

typedef struct {
    int kind;
    uint32_t code;
    uint32_t modifiers;
    double x, y;
    const char *text;
    size_t length;
    int start;
} NtEvent;

/* Text is valid during the callback. Button code: 1 left, 2 middle, 3 right; start: pressed.
 * Scroll y: vertical lines up.
 * Motion/button x,y are surface-local logical pixels. Focus code: active.
 * Pointer leave clears hover without reporting a terminal mouse movement.
 * Key code is an XKB keysym and text is its direct printable input.
 * NT_TEXT carries independent compose/IME commits.
 * Paste: text is the completed, unmodified UTF-8 selection.
 * Window state code: maximized.
 * Presented (tracing only): code is the frame number, x its CLOCK_MONOTONIC time. */
typedef void (*NtEventFn)(void *, const NtEvent *);

/* The window and renderer live until the process exits. Creation always returns an object;
 * a non-empty error string means it failed. Events reach the callback while nt_window_new
 * and nt_window_wait run. */
NtWindow *nt_window_new(int width, int height, NtEventFn, void *);
const char *nt_window_error(NtWindow *);
int nt_window_wait(NtWindow *, struct pollfd *sources, size_t count, int timeout_ms);
void nt_window_size(NtWindow *, int *width, int *height, double *scale);
void nt_window_title(NtWindow *, const char *title);
void nt_window_minimize(NtWindow *);
void nt_window_maximize(NtWindow *, int maximized);
void nt_window_resize_cursor(NtWindow *, int enabled);
void nt_window_cursor_rect(NtWindow *, int x, int y, int width, int height);
/* Serves the selection from a regular file and takes ownership of it. */
void nt_window_set_selection(NtWindow *, int primary, int file);
/* Answers with one NT_PASTE event; request the next paste after it arrives. */
void nt_window_paste(NtWindow *, int primary);
/* Dirty frame packets are limited to the current output refresh rate. The first
 * frame and updates after idle are ready immediately. Call only for dirty content.
 * Unknown refresh leaves scheduling to the graphics driver. A not-ready check
 * requests a one-shot timer to wake the event loop; it never marks content dirty.
 * Wayland suspended state prevents drawing until resumed. Presentation feedback is
 * a diagnostic, never a draw gate. */
int nt_window_frame_ready(NtWindow *);
void nt_window_prepare_frame(NtWindow *);
void *nt_window_display(NtWindow *);
void *nt_window_surface(NtWindow *);

typedef struct {
    uint32_t start, end;
    uint32_t color; /* RRGGBBAA */
    uint32_t flags; /* bit 0 bold, bit 1 italic */
} NtRun;

/* Quad shapes the fragment shader draws. A tab's param is the inset of its slanted sides; a
 * stroke's is its radius, and its quad is the segment's bounding box padded by radius + 1. */
enum {
    NT_RECT, NT_MASK_GLYPH, NT_COLOR_GLYPH, NT_UNDERCURL, NT_TAB, NT_STROKE, NT_STROKE_RISING
};

NtRenderer *nt_renderer_new(NtWindow *, const char *family, float font_size);
const char *nt_renderer_error(NtRenderer *);
void nt_renderer_metrics(NtRenderer *, float *cell_width, float *cell_height);
/* UTF-8 text with runs at character boundaries. columns maps each byte to its logical
 * terminal column; NULL means one column per byte. The line keeps a copy and is shaped
 * on the render thread when first drawn, at the scale of that frame. */
NtLine *nt_line_new(const char *, size_t, const NtRun *, size_t, const uint16_t *columns);
void nt_line_free(NtLine *);
void nt_renderer_begin(NtRenderer *, uint32_t background);
/* Replace the current clip background, including its alpha. */
void nt_renderer_clear(NtRenderer *, uint32_t background);
void nt_renderer_clip(NtRenderer *, float x, float y, float width, float height);
void nt_renderer_quad(NtRenderer *, float x, float y, float width, float height,
                      uint32_t color, int mode, float param);
void nt_renderer_line(NtRenderer *, NtLine *, float x, float y);
/* Present hands the frame packet to the render thread. While it is pending, only
 * ready/finish/fd/metrics and nt_line_free may be called. Ready returns -1 on error, 0
 * while pending, and 1 when the next frame may be built. */
void nt_renderer_present(NtRenderer *);
int nt_renderer_ready(NtRenderer *);
int nt_renderer_finish(NtRenderer *);
int nt_renderer_fd(NtRenderer *);

#endif
