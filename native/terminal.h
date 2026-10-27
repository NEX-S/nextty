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
    NT_FOCUS, NT_RESIZE, NT_CLOSE, NT_PASTE, NT_FRAME, NT_EXPOSE, NT_PRESENTED,
    NT_WINDOW_STATE, NT_FRAME_RATE, NT_POINTER_LEAVE
};

typedef struct {
    int kind;
    uint32_t code;
    uint32_t modifiers;
    double x, y;
    const char *text;
    size_t length;
    int start, end;
} NtEvent;

/* Text is valid during the callback. Preedit ranges are UTF-8 byte offsets.
 * Button code: 1 left, 2 middle, 3 right; start: pressed. Scroll y: lines up.
 * Motion/button x,y are surface-local logical pixels. Focus code: active.
 * Pointer leave clears hover without reporting a terminal mouse movement.
 * Key code is an XKB keysym; printable text arrives only through NT_TEXT.
 * Paste code: primary; text is the completed, unmodified UTF-8 selection.
 * Trace-only frame code/x/y: frame id, monotonic receipt, raw server time (ms).
 * Trace-only window state code: activated/suspended/fullscreen/maximized bits;
 * x: monotonic receipt. Trace-only frame rate x: submission interval in nanoseconds. */
typedef void (*NtEventFn)(void *, const NtEvent *);

NtWindow *nt_window_new(int width, int height);
void nt_window_free(NtWindow *);
const char *nt_window_error(NtWindow *);
int nt_window_wait(NtWindow *, struct pollfd *sources, size_t count, int timeout_ms, NtEventFn, void *);
void nt_window_size(NtWindow *, int *width, int *height, double *scale);
void nt_window_title(NtWindow *, const char *title);
void nt_window_minimize(NtWindow *);
void nt_window_maximize(NtWindow *, int maximized);
void nt_window_resize_cursor(NtWindow *, int enabled);
void nt_window_cursor_rect(NtWindow *, int x, int y, int width, int height);
void nt_window_set_selection(NtWindow *, int primary, const char *, size_t);
/* Takes ownership on all paths. Normal file validation, reads and closes run on the sender. */
void nt_window_set_selection_file(NtWindow *, int primary, int fd);
void nt_window_paste(NtWindow *, int primary);
/* Dirty frame packets are limited to the current output refresh rate. The first
 * frame and updates after idle are ready immediately. Call only for dirty content.
 * Unknown refresh leaves scheduling to the graphics driver. A not-ready check
 * requests a one-shot timer to wake the event loop; it never marks content dirty. Wayland suspended state prevents drawing until resumed.
 * Frame callbacks and presentation feedback are diagnostics, never draw gates. */
int nt_window_frame_ready(NtWindow *);
void nt_window_prepare_frame(NtWindow *);
/* Native Wayland display and surface handles. */
void *nt_window_display(NtWindow *);
void *nt_window_surface(NtWindow *);

typedef struct {
    uint32_t start, end;
    uint32_t color; /* RRGGBBAA */
    uint32_t flags; /* bit 0 bold, bit 1 italic */
} NtRun;

NtRenderer *nt_renderer_new(NtWindow *, const char *family, float font_size);
void nt_renderer_free(NtRenderer *);
const char *nt_renderer_error(NtRenderer *);
void nt_renderer_metrics(NtRenderer *, float *cell_width, float *cell_height);
/* The Rust frontend supplies UTF-8 text and runs at character boundaries.
 * columns maps each UTF-8 byte to its logical terminal column; NULL is ASCII.
 * Copies input for worker shaping at the current scale. Drop cached lines after
 * scale changes, and free all lines before their renderer. */
NtLine *nt_line_new(NtRenderer *, const char *, size_t, const NtRun *, size_t,
                    const uint16_t *columns);
void nt_line_free(NtLine *);
int nt_renderer_begin(NtRenderer *, uint32_t background);
/* Replace the current clip background, including its alpha. */
int nt_renderer_clear(NtRenderer *, uint32_t background);
int nt_renderer_clip(NtRenderer *, float x, float y, float width, float height);
void nt_renderer_rect(NtRenderer *, float x, float y, float width, float height,
                      uint32_t color, int wave);
void nt_renderer_stroke(NtRenderer *, float x1, float y1, float x2, float y2,
                        float width, uint32_t color);
void nt_renderer_round_rect(NtRenderer *, float x, float y, float width, float height,
                            float radius, uint32_t color);
void nt_renderer_tab(NtRenderer *, float x, float y, float width, float height,
                     float inset, uint32_t color);
void nt_renderer_line(NtRenderer *, const NtLine *, float x, float y, float factor);
/* Present transfers the frame packet to one GPU worker. While pending, only
 * ready/finish/fd/metrics/free and nt_line_free may be called. Ready returns -1 on error, 0 while
 * pending, and 1 when the next frame may be built. */
int nt_renderer_present(NtRenderer *);
int nt_renderer_ready(NtRenderer *);
int nt_renderer_finish(NtRenderer *);
int nt_renderer_fd(NtRenderer *);
double nt_renderer_swap_time(NtRenderer *);

#endif
