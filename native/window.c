#define _GNU_SOURCE
#include "protocols/cursor-shape-client.h"
#include "protocols/fractional-scale-client.h"
#include "protocols/presentation-time-client.h"
#include "protocols/primary-selection-client.h"
#include "protocols/text-input-client.h"
#include "protocols/viewporter-client.h"
#include "protocols/xdg-shell-client.h"
#include "terminal.h"
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <locale.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon-compose.h>
#include <xkbcommon/xkbcommon.h>

enum { TRANSFER_TIMEOUT_MS = 10000, FILE_CHUNK_SIZE = 16384 };

typedef struct Queued Queued;
struct Queued {
    NtEvent event;
    Queued *next;
    char text[];
};
typedef struct Transfer Transfer;
struct Transfer {
    Transfer *next;
    int fd, writing;
    char *bytes;
    size_t length, capacity, offset;
    int64_t deadline;
};
typedef struct FileTransfer FileTransfer;
struct FileTransfer {
    FileTransfer *next;
    int fd, file_fd;
    off_t offset;
    size_t length;
    int64_t deadline;
};
typedef struct Source Source;
struct Source {
    Source *next;
    NtWindow *window;
    int primary, file_fd;
    void *proxy;
    size_t length;
    char bytes[];
};
typedef struct Offer Offer;
struct Offer {
    NtWindow *window;
    void *proxy;
    int primary;
    const char *mime;
    Offer *next;
};
typedef struct Output Output;
struct Output {
    NtWindow *window;
    struct wl_output *proxy;
    uint32_t id;
    int entered, refresh;
    Output *next;
};
typedef struct {
    NtWindow *window;
    uint32_t id;
} Feedback;
struct NtWindow {
    int width, height;
    int frame_fd;
    int64_t frame_interval_ns, last_frame_ns, frame_deadline_ns;
    double scale, mouse_x, mouse_y;
    char error[256];
    NtEventFn callback;
    void *callback_data;
    Queued *queued, **queued_tail;
    unsigned delivered;
    uint32_t modifiers;
    int cursor_x, cursor_y, cursor_w, cursor_h;
    uint32_t pointer_serial;
    int pointer_inside, resize_cursor;
    Source *pending_source[2];
    int paste_pending, paste_primary;
    Transfer *transfers;
    FileTransfer *file_transfers;
    struct xkb_context *xkb_context;
    struct xkb_compose_state *compose;
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_surface *surface;
    struct xdg_wm_base *shell;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    struct wp_viewporter *viewporter;
    struct wp_viewport *viewport;
    struct wp_fractional_scale_manager_v1 *scale_manager;
    struct wp_fractional_scale_v1 *fractional_scale;
    struct wp_presentation *presentation;
    uint32_t frame_id;
    int suspended;
    struct wl_seat *seat;
    struct wl_keyboard *keyboard;
    struct wl_pointer *pointer;
    struct xkb_state *key_state;
    struct wp_cursor_shape_manager_v1 *cursor_manager;
    struct wp_cursor_shape_device_v1 *cursor_device;
    struct wl_data_device_manager *data_manager;
    struct wl_data_device *data_device;
    struct zwp_primary_selection_device_manager_v1 *primary_manager;
    struct zwp_primary_selection_device_v1 *primary_device;
    struct zwp_text_input_manager_v3 *text_manager;
    struct zwp_text_input_v3 *text_input;
    Output *outputs;
    Source *sources;
    Offer *offers, *selection[2], *dnd_offer;
    Source *current_source[2];
    uint32_t serial;
    int configured, pending_width, pending_height;
    int text_enabled;
    char *pending_preedit, *pending_commit;
    double axis_y;
    int axis_value_y;
};

/* Listener entries libwayland requires but this client has no use for; it ignores their arguments. */
static void noop() {}

static _Thread_local char startup_error[256];
static int64_t now_ns(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return (int64_t)time.tv_sec * 1000000000 + time.tv_nsec;
}
static int64_t now_ms(void) {
    return now_ns() / 1000000;
}
static void fail(NtWindow *w, const char *format, ...) {
    if (w->error[0])
        return;
    va_list ap;
    va_start(ap, format);
    vsnprintf(w->error, sizeof w->error, format, ap);
    va_end(ap);
}
static void emit(NtWindow *w, NtEvent event);
static void schedule_frame(NtWindow *w, int64_t deadline) {
    if (w->frame_deadline_ns == deadline)
        return;
    struct itimerspec timer = {0};
    if (deadline) {
        timer.it_value.tv_sec = deadline / 1000000000;
        timer.it_value.tv_nsec = deadline % 1000000000;
    }
    if (timerfd_settime(w->frame_fd, TFD_TIMER_ABSTIME, &timer, NULL) < 0)
        fail(w, "scheduling frame: %s", strerror(errno));
    else
        w->frame_deadline_ns = deadline;
}
static void set_frame_period(NtWindow *w, int64_t frame_interval_ns) {
    if (w->frame_interval_ns == frame_interval_ns)
        return;
    w->frame_interval_ns = frame_interval_ns;
    schedule_frame(w, 0);
    /* The cancelled timer may have been the only wake-up for a frame Rust is waiting on. */
    w->delivered++;
}
static void wayland_error(NtWindow *w, const char *operation) {
    int error = wl_display_get_error(w->display);
    if (error == EPROTO) {
        const struct wl_interface *interface = NULL;
        uint32_t id = 0;
        uint32_t code = wl_display_get_protocol_error(w->display, &interface, &id);
        fail(w, "%s: protocol %s#%u error %u", operation, interface ? interface->name : "unknown",
             id, code);
    } else
        fail(w, "%s: %s", operation, strerror(error ? error : errno));
}
static void queue_event(NtWindow *w, const NtEvent *event) {
    Queued *q = malloc(sizeof *q + (event->text ? event->length : 0));
    q->event = *event;
    q->next = NULL;
    if (event->text) {
        memcpy(q->text, event->text, event->length);
        q->event.text = q->text;
    }
    *w->queued_tail = q;
    w->queued_tail = &q->next;
}
static void emit(NtWindow *w, NtEvent event) {
    if (w->callback) {
        w->callback(w->callback_data, &event);
        w->delivered++;
    } else
        queue_event(w, &event);
}
static void drain_events(NtWindow *w) {
    while (w->queued) {
        Queued *q = w->queued;
        w->queued = q->next;
        w->callback(w->callback_data, &q->event);
        w->delivered++;
        free(q);
    }
    w->queued_tail = &w->queued;
}
static void append_bytes(char **bytes, size_t *length, size_t *capacity, const void *data,
                         size_t n) {
    if (*length + n > *capacity) {
        *capacity = (*length + n) * 2 < 4096 ? 4096 : (*length + n) * 2;
        *bytes = realloc(*bytes, *capacity);
    }
    memcpy(*bytes + *length, data, n);
    *length += n;
}
static void file_transfer_free(FileTransfer *t) {
    close(t->fd);
    close(t->file_fd);
    free(t);
}
static void queue_file(NtWindow *w, int fd, int file_fd) {
    FileTransfer *t = calloc(1, sizeof *t);
    struct stat st = {0};
    fstat(file_fd, &st);
    t->fd = fd;
    t->file_fd = file_fd;
    t->length = (size_t)st.st_size;
    t->deadline = now_ms() + TRANSFER_TIMEOUT_MS;
    t->next = w->file_transfers;
    w->file_transfers = t;
}
static void process_file_transfers(NtWindow *w, const struct pollfd *fds) {
    FileTransfer **link = &w->file_transfers;
    while (*link) {
        FileTransfer *t = *link;
        int ready = (fds++)->revents;
        int done = now_ms() >= t->deadline || (size_t)t->offset == t->length;
        if (!done && ready) {
            size_t count = t->length - (size_t)t->offset;
            if (count > FILE_CHUNK_SIZE)
                count = FILE_CHUNK_SIZE;
            ssize_t n;
            do n = splice(t->file_fd, &t->offset, t->fd, NULL, count, SPLICE_F_NONBLOCK);
            while (n < 0 && errno == EINTR);
            if (n > 0)
                t->deadline = now_ms() + TRANSFER_TIMEOUT_MS;
            else if (!n || errno != EAGAIN)
                done = 1;
            if ((size_t)t->offset == t->length)
                done = 1;
        }
        if (!done) {
            link = &t->next;
            continue;
        }
        *link = t->next;
        file_transfer_free(t);
    }
}
static void finish_paste(NtWindow *w, const char *bytes, size_t length) {
    if (!w->paste_pending)
        return;
    w->paste_pending = 0;
    if (!bytes) {
        bytes = "";
        length = 0;
    }
    emit(w, (NtEvent){.kind = NT_PASTE, .text = bytes, .length = length});
}
static void add_transfer(NtWindow *w, int fd, int writing, const char *bytes, size_t length,
                         int file_fd) {
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK);
    if (writing && file_fd >= 0) {
        queue_file(w, fd, fcntl(file_fd, F_DUPFD_CLOEXEC, 0));
        return;
    }
    Transfer *t = calloc(1, sizeof *t);
    t->fd = fd;
    t->writing = writing;
    t->deadline = now_ms() + TRANSFER_TIMEOUT_MS;
    if (writing && length) {
        t->bytes = malloc(length);
        memcpy(t->bytes, bytes, length);
        t->length = length;
    }
    t->next = w->transfers;
    w->transfers = t;
}
static void process_transfers(NtWindow *w, const struct pollfd *fds) {
    Transfer **link = &w->transfers;
    while (*link) {
        Transfer *t = *link;
        int ready = (fds++)->revents;
        int done = 0, good = 1;
        if (now_ms() >= t->deadline) {
            done = 1;
            good = 0;
        }
        if (!done && t->writing && t->offset == t->length)
            done = 1;
        if (!done && ready) {
            /* A continuously ready clipboard pipe must leave time for input. */
            char buffer[32768];
            size_t count = sizeof buffer;
            if (t->writing && t->length - t->offset < count)
                count = t->length - t->offset;
            ssize_t n;
            do {
                n = t->writing ? write(t->fd, t->bytes + t->offset, count)
                               : read(t->fd, buffer, count);
            } while (n < 0 && errno == EINTR);
            if (n > 0) {
                t->deadline = now_ms() + TRANSFER_TIMEOUT_MS;
                if (t->writing) {
                    t->offset += (size_t)n;
                    done = t->offset == t->length;
                } else {
                    append_bytes(&t->bytes, &t->length, &t->capacity, buffer, (size_t)n);
                }
            } else if (!n) {
                done = 1;
            } else if (errno != EAGAIN) {
                done = 1;
                good = 0;
            }
        }
        if (!done) {
            link = &t->next;
            continue;
        }
        *link = t->next;
        close(t->fd);
        if (!t->writing)
            finish_paste(w, good ? t->bytes : NULL, t->length);
        free(t->bytes);
        free(t);
    }
}
static uint32_t xkb_modifiers(struct xkb_state *state) {
    uint32_t m = 0;
    if (xkb_state_mod_name_is_active(state, XKB_MOD_NAME_SHIFT, XKB_STATE_MODS_EFFECTIVE) > 0)
        m |= NT_SHIFT;
    if (xkb_state_mod_name_is_active(state, XKB_MOD_NAME_CTRL, XKB_STATE_MODS_EFFECTIVE) > 0)
        m |= NT_CONTROL;
    if (xkb_state_mod_name_is_active(state, XKB_MOD_NAME_ALT, XKB_STATE_MODS_EFFECTIVE) > 0)
        m |= NT_ALT;
    if (xkb_state_mod_name_is_active(state, XKB_MOD_NAME_LOGO, XKB_STATE_MODS_EFFECTIVE) > 0)
        m |= NT_SUPER;
    return m;
}
static void keyboard_text(NtWindow *w, uint32_t key, int composing) {
    xkb_keysym_t sym = xkb_state_key_get_one_sym(w->key_state, key + 8);
    char text[128];
    int n = xkb_state_key_get_utf8(w->key_state, key + 8, text, sizeof text);
    if (n >= (int)sizeof text)
        n = 0;
    int printable = n > 0 && (unsigned char)text[0] >= 32 && (unsigned char)text[0] != 127;
    int modified = w->modifiers & (NT_CONTROL | NT_ALT | NT_SUPER);
    if (!modified && composing && w->compose) {
        xkb_compose_state_feed(w->compose, sym);
        switch (xkb_compose_state_get_status(w->compose)) {
        case XKB_COMPOSE_COMPOSING:
            emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
            return;
        case XKB_COMPOSE_COMPOSED: {
            int size = xkb_compose_state_get_utf8(w->compose, NULL, 0);
            char *s = malloc((size_t)size + 1);
            xkb_compose_state_get_utf8(w->compose, s, (size_t)size + 1);
            emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
            emit(w, (NtEvent){.kind = NT_TEXT, .text = s, .length = (size_t)size});
            free(s);
            xkb_compose_state_reset(w->compose);
            return;
        }
        case XKB_COMPOSE_CANCELLED:
            xkb_compose_state_reset(w->compose);
            emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
            if (printable) return;
            break;
        default:
            break;
        }
    }
    int key_text = modified ? n > 0 && (w->modifiers & NT_ALT) : printable;
    emit(w, (NtEvent){.kind = NT_KEY,
                      .code = sym,
                      .modifiers = w->modifiers,
                      .text = key_text ? text : NULL,
                      .length = key_text ? (size_t)n : 0});
}
static void size_changed(NtWindow *w);
static void wayland_selection(NtWindow *w, int primary);

/* Wayland listeners use the display's default queue; the driver may enqueue onto it. */
static void shell_ping(void *data, struct xdg_wm_base *shell, uint32_t serial) {
    xdg_wm_base_pong(shell, serial);
}
static const struct xdg_wm_base_listener shell_listener = {.ping = shell_ping};
static void toplevel_configure(void *data, struct xdg_toplevel *top, int32_t width, int32_t height,
                               struct wl_array *states) {
    NtWindow *w = data;
    w->pending_width = width;
    w->pending_height = height;
    int suspended = 0, maximized = 0;
    uint32_t *state;
    wl_array_for_each(state, states) {
        suspended |= *state == XDG_TOPLEVEL_STATE_SUSPENDED;
        maximized |= *state == XDG_TOPLEVEL_STATE_MAXIMIZED;
    }
    if (suspended != w->suspended) {
        w->suspended = suspended;
        schedule_frame(w, 0);
        if (!suspended)
            emit(w, (NtEvent){.kind = NT_EXPOSE});
    }
    emit(w, (NtEvent){.kind = NT_WINDOW_STATE, .code = (uint32_t)maximized});
}
static void toplevel_close(void *data, struct xdg_toplevel *top) {
    emit(data, (NtEvent){.kind = NT_CLOSE});
}
static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure, .close = toplevel_close, .configure_bounds = noop,
    .wm_capabilities = noop};
static void surface_configure(void *data, struct xdg_surface *surface, uint32_t serial) {
    NtWindow *w = data;
    xdg_surface_ack_configure(surface, serial);
    if (w->pending_width > 0)
        w->width = w->pending_width;
    if (w->pending_height > 0)
        w->height = w->pending_height;
    w->configured = 1;
    size_changed(w);
}
static const struct xdg_surface_listener xdg_surface_listener = {.configure = surface_configure};
static void update_refresh(NtWindow *w) {
    int refresh = 0;
    for (Output *output = w->outputs; output; output = output->next)
        if (output->entered && output->refresh > refresh)
            refresh = output->refresh;
    set_frame_period(w, refresh ? (1000000000000LL + refresh - 1) / refresh : 0);
}
static void surface_enter(void *data, struct wl_surface *surface, struct wl_output *output) {
    NtWindow *w = data;
    Output *o = wl_output_get_user_data(output);
    o->entered = 1;
    update_refresh(w);
}
static void surface_leave(void *data, struct wl_surface *surface, struct wl_output *output) {
    NtWindow *w = data;
    Output *o = wl_output_get_user_data(output);
    o->entered = 0;
    update_refresh(w);
}
static const struct wl_surface_listener surface_listener = {
    .enter = surface_enter, .leave = surface_leave, .preferred_buffer_scale = noop,
    .preferred_buffer_transform = noop};
static void fractional_scale(void *data, struct wp_fractional_scale_v1 *obj, uint32_t scale) {
    NtWindow *w = data;
    double value = scale / 120.0;
    if (value != w->scale) {
        w->scale = value;
        size_changed(w);
    }
}
static const struct wp_fractional_scale_v1_listener fractional_listener = {.preferred_scale =
                                                                               fractional_scale};
static void output_mode(void *data, struct wl_output *o, uint32_t flags, int32_t width,
                        int32_t height, int32_t refresh) {
    Output *output = data;
    if (flags & WL_OUTPUT_MODE_CURRENT)
        output->refresh = refresh > 0 ? refresh : 0;
}
static void output_done(void *data, struct wl_output *o) {
    Output *out = data;
    update_refresh(out->window);
}
static const struct wl_output_listener output_listener = {
    .geometry = noop, .mode = output_mode, .done = output_done, .scale = noop};

static void keyboard_keymap(void *data, struct wl_keyboard *keyboard, uint32_t format, int fd,
                            uint32_t size) {
    NtWindow *w = data;
    char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    struct xkb_keymap *keymap = xkb_keymap_new_from_buffer(
        w->xkb_context, map, size - 1, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(map, size);
    xkb_state_unref(w->key_state);
    w->key_state = xkb_state_new(keymap);
    xkb_keymap_unref(keymap);
}
static void keyboard_enter(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface, struct wl_array *keys) {
    NtWindow *w = data;
    w->serial = serial;
    emit(w, (NtEvent){.kind = NT_FOCUS, .code = 1});
}
static void keyboard_leave(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                           struct wl_surface *surface) {
    NtWindow *w = data;
    w->modifiers = 0;
    if (w->compose)
        xkb_compose_state_reset(w->compose);
    emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
    emit(w, (NtEvent){.kind = NT_FOCUS});
}
static void keyboard_key(void *data, struct wl_keyboard *keyboard, uint32_t serial, uint32_t time,
                         uint32_t key, uint32_t state) {
    NtWindow *w = data;
    w->serial = serial;
    for (int i = 0; i < 2; i++)
        wayland_selection(w, i);
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED || state == WL_KEYBOARD_KEY_STATE_REPEATED)
        keyboard_text(w, key, state == WL_KEYBOARD_KEY_STATE_PRESSED);
}
static void keyboard_modifiers(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                               uint32_t depressed, uint32_t latched, uint32_t locked,
                               uint32_t group) {
    NtWindow *w = data;
    w->serial = serial;
    xkb_state_update_mask(w->key_state, depressed, latched, locked, 0, 0, group);
    w->modifiers = xkb_modifiers(w->key_state);
}
static const struct wl_keyboard_listener keyboard_listener = {.keymap = keyboard_keymap,
                                                              .enter = keyboard_enter,
                                                              .leave = keyboard_leave,
                                                              .key = keyboard_key,
                                                              .modifiers = keyboard_modifiers,
                                                              .repeat_info = noop};
static void set_cursor_shape(NtWindow *w) {
    wp_cursor_shape_device_v1_set_shape(w->cursor_device, w->pointer_serial,
        w->resize_cursor == 2 ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NS_RESIZE :
        w->resize_cursor ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_EW_RESIZE :
                           WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT);
}
static void pointer_enter(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y) {
    NtWindow *w = data;
    w->pointer_serial = serial;
    w->pointer_inside = 1;
    set_cursor_shape(w);
    w->mouse_x = wl_fixed_to_double(x);
    w->mouse_y = wl_fixed_to_double(y);
    emit(w,
         (NtEvent){.kind = NT_MOTION, .x = w->mouse_x, .y = w->mouse_y, .modifiers = w->modifiers});
}
static void pointer_leave(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface) {
    NtWindow *w = data;
    w->pointer_inside = 0;
    emit(w, (NtEvent){.kind = NT_POINTER_LEAVE});
}
static void pointer_motion(void *data, struct wl_pointer *pointer, uint32_t time, wl_fixed_t x,
                           wl_fixed_t y) {
    NtWindow *w = data;
    w->mouse_x = wl_fixed_to_double(x);
    w->mouse_y = wl_fixed_to_double(y);
    emit(w,
         (NtEvent){.kind = NT_MOTION, .x = w->mouse_x, .y = w->mouse_y, .modifiers = w->modifiers});
}
static void pointer_button(void *data, struct wl_pointer *pointer, uint32_t serial, uint32_t time,
                           uint32_t button, uint32_t state) {
    NtWindow *w = data;
    w->serial = serial;
    for (int i = 0; i < 2; i++)
        wayland_selection(w, i);
    uint32_t code = button == 0x110 ? 1 : button == 0x112 ? 2 : button == 0x111 ? 3 : button;
    emit(w, (NtEvent){.kind = NT_BUTTON,
                      .code = code,
                      .modifiers = w->modifiers,
                      .x = w->mouse_x,
                      .y = w->mouse_y,
                      .start = state == WL_POINTER_BUTTON_STATE_PRESSED});
}
static void pointer_axis(void *data, struct wl_pointer *pointer, uint32_t time, uint32_t axis,
                         wl_fixed_t value) {
    NtWindow *w = data;
    if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL)
        w->axis_y += wl_fixed_to_double(value) / 10.0;
}
static void pointer_frame(void *data, struct wl_pointer *pointer) {
    NtWindow *w = data;
    double y = w->axis_value_y ? w->axis_value_y / 120.0 : w->axis_y;
    if (y)
        emit(w, (NtEvent){.kind = NT_SCROLL, .y = -y, .modifiers = w->modifiers});
    w->axis_y = 0;
    w->axis_value_y = 0;
}
static void pointer_axis_value(void *data, struct wl_pointer *pointer, uint32_t axis,
                               int32_t value) {
    NtWindow *w = data;
    if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL)
        w->axis_value_y = value;
}
static const struct wl_pointer_listener pointer_listener = {.enter = pointer_enter,
                                                            .leave = pointer_leave,
                                                            .motion = pointer_motion,
                                                            .button = pointer_button,
                                                            .axis = pointer_axis,
                                                            .frame = pointer_frame,
                                                            .axis_source = noop,
                                                            .axis_stop = noop,
                                                            .axis_value120 = pointer_axis_value,
                                                            .axis_relative_direction = noop};
static void clear_pending_text(NtWindow *w) {
    free(w->pending_commit);
    w->pending_commit = NULL;
    free(w->pending_preedit);
    w->pending_preedit = NULL;
}
static void text_enter(void *data, struct zwp_text_input_v3 *input, struct wl_surface *surface) {
    NtWindow *w = data;
    w->text_enabled = 1;
    zwp_text_input_v3_enable(input);
    zwp_text_input_v3_set_content_type(input, ZWP_TEXT_INPUT_V3_CONTENT_HINT_NONE,
                                       ZWP_TEXT_INPUT_V3_CONTENT_PURPOSE_TERMINAL);
    zwp_text_input_v3_set_cursor_rectangle(input, w->cursor_x, w->cursor_y, w->cursor_w,
                                           w->cursor_h);
    zwp_text_input_v3_commit(w->text_input);
}
static void text_leave(void *data, struct zwp_text_input_v3 *input, struct wl_surface *surface) {
    NtWindow *w = data;
    if (w->text_enabled) {
        zwp_text_input_v3_disable(input);
        zwp_text_input_v3_commit(w->text_input);
    }
    w->text_enabled = 0;
    clear_pending_text(w);
    emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
}
static void text_preedit(void *data, struct zwp_text_input_v3 *input, const char *text,
                         int32_t begin, int32_t end) {
    NtWindow *w = data;
    (void)begin;
    (void)end;
    free(w->pending_preedit);
    w->pending_preedit = strdup(text ? text : "");
}
static void text_commit_string(void *data, struct zwp_text_input_v3 *input, const char *text) {
    NtWindow *w = data;
    free(w->pending_commit);
    w->pending_commit = strdup(text ? text : "");
}
static void text_done(void *data, struct zwp_text_input_v3 *input, uint32_t serial) {
    NtWindow *w = data;
    /* Stale serials still carry valid edits. No state requests are issued from
       done: application cursor changes have already committed their own state. */
    if (w->pending_commit) {
        emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
        if (*w->pending_commit)
            emit(w, (NtEvent){.kind = NT_TEXT,
                              .text = w->pending_commit,
                              .length = strlen(w->pending_commit)});
    }
    const char *s = w->pending_preedit ? w->pending_preedit : "";
    emit(w, (NtEvent){.kind = NT_PREEDIT, .text = s, .length = strlen(s)});
    clear_pending_text(w);
}
static const struct zwp_text_input_v3_listener text_listener = {.enter = text_enter,
                                                                .leave = text_leave,
                                                                .preedit_string = text_preedit,
                                                                .commit_string = text_commit_string,
                                                                .delete_surrounding_text = noop,
                                                                .done = text_done};
static void source_remove(Source *source) {
    NtWindow *w = source->window;
    Source **link = &w->sources;
    while (*link != source)
        link = &(*link)->next;
    *link = source->next;
    if (w->current_source[source->primary] == source)
        w->current_source[source->primary] = NULL;
    if (w->pending_source[source->primary] == source)
        w->pending_source[source->primary] = NULL;
    if (source->proxy) {
        if (source->primary)
            zwp_primary_selection_source_v1_destroy(source->proxy);
        else
            wl_data_source_destroy(source->proxy);
    }
    if (source->file_fd >= 0)
        close(source->file_fd);
    free(source);
}
static void source_send(void *data, struct wl_data_source *proxy, const char *mime, int32_t fd) {
    Source *s = data;
    add_transfer(s->window, fd, 1, s->bytes, s->length, s->file_fd);
}
static void source_cancel(void *data, struct wl_data_source *proxy) {
    source_remove(data);
}
static const struct wl_data_source_listener source_listener = {
    .target = noop, .send = source_send, .cancelled = source_cancel, .dnd_drop_performed = noop,
    .dnd_finished = noop, .action = noop};
static void primary_send(void *data, struct zwp_primary_selection_source_v1 *source,
                         const char *mime, int32_t fd) {
    source_send(data, NULL, mime, fd);
}
static void primary_cancel(void *data, struct zwp_primary_selection_source_v1 *source) {
    source_remove(data);
}
static const struct zwp_primary_selection_source_v1_listener primary_source_listener = {
    .send = primary_send, .cancelled = primary_cancel};
static void wayland_selection(NtWindow *w, int primary) {
    Source *s = w->pending_source[primary];
    if (!w->serial || !s)
        return;
    if (primary) {
        s->proxy = zwp_primary_selection_device_manager_v1_create_source(w->primary_manager);
        zwp_primary_selection_source_v1_add_listener(s->proxy, &primary_source_listener, s);
        zwp_primary_selection_source_v1_offer(s->proxy, "text/plain;charset=utf-8");
        zwp_primary_selection_source_v1_offer(s->proxy, "UTF8_STRING");
        zwp_primary_selection_source_v1_offer(s->proxy, "text/plain");
        zwp_primary_selection_device_v1_set_selection(w->primary_device, s->proxy, w->serial);
    } else {
        s->proxy = wl_data_device_manager_create_data_source(w->data_manager);
        wl_data_source_add_listener(s->proxy, &source_listener, s);
        wl_data_source_offer(s->proxy, "text/plain;charset=utf-8");
        wl_data_source_offer(s->proxy, "UTF8_STRING");
        wl_data_source_offer(s->proxy, "text/plain");
        wl_data_device_set_selection(w->data_device, s->proxy, w->serial);
    }
    w->pending_source[primary] = NULL;
    w->current_source[primary] = s;
}
static void offer_mime(Offer *o, const char *mime) {
    if (!strcmp(mime, "text/plain;charset=utf-8"))
        o->mime = "text/plain;charset=utf-8";
    else if (!strcmp(mime, "UTF8_STRING") &&
             (!o->mime || strcmp(o->mime, "text/plain;charset=utf-8")))
        o->mime = "UTF8_STRING";
    else if (!strcmp(mime, "text/plain") && !o->mime)
        o->mime = "text/plain";
}
static void data_offer_mime(void *data, struct wl_data_offer *offer, const char *mime) {
    offer_mime(data, mime);
}
static const struct wl_data_offer_listener offer_listener = {
    .offer = data_offer_mime, .source_actions = noop, .action = noop};
static void primary_offer_mime(void *data, struct zwp_primary_selection_offer_v1 *offer,
                               const char *mime) {
    offer_mime(data, mime);
}
static const struct zwp_primary_selection_offer_v1_listener primary_offer_listener = {
    .offer = primary_offer_mime};
static void offer_destroy(Offer *o) {
    if (!o)
        return;
    NtWindow *w = o->window;
    Offer **link = &w->offers;
    while (*link != o)
        link = &(*link)->next;
    *link = o->next;
    if (w->dnd_offer == o)
        w->dnd_offer = NULL;
    if (o->primary)
        zwp_primary_selection_offer_v1_destroy(o->proxy);
    else
        wl_data_offer_destroy(o->proxy);
    free(o);
}
static void data_offer(void *data, struct wl_data_device *device, struct wl_data_offer *offer) {
    NtWindow *w = data;
    Offer *o = calloc(1, sizeof *o);
    o->window = w;
    o->proxy = offer;
    o->next = w->offers;
    w->offers = o;
    wl_data_offer_add_listener(offer, &offer_listener, o);
}
static void data_leave(void *data, struct wl_data_device *device) {
    NtWindow *w = data;
    Offer *offer = w->dnd_offer;
    w->dnd_offer = NULL;
    if (offer && offer != w->selection[0] && offer != w->selection[1])
        offer_destroy(offer);
}
static void data_enter(void *data, struct wl_data_device *device, uint32_t serial,
                       struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y,
                       struct wl_data_offer *offer) {
    NtWindow *w = data;
    data_leave(data, device);
    if (offer) {
        w->dnd_offer = wl_data_offer_get_user_data(offer);
        wl_data_offer_accept(offer, serial, NULL);
    }
}
static void data_selection(void *data, struct wl_data_device *device, struct wl_data_offer *offer) {
    NtWindow *w = data;
    Offer *old = w->selection[0];
    w->selection[0] = offer ? wl_data_offer_get_user_data(offer) : NULL;
    if (w->dnd_offer == w->selection[0])
        w->dnd_offer = NULL;
    if (old != w->selection[0])
        offer_destroy(old);
}
static const struct wl_data_device_listener data_listener = {.data_offer = data_offer,
                                                             .enter = data_enter,
                                                             .leave = data_leave,
                                                             .motion = noop,
                                                             .drop = data_leave,
                                                             .selection = data_selection};
static void primary_offer(void *data, struct zwp_primary_selection_device_v1 *device,
                          struct zwp_primary_selection_offer_v1 *offer) {
    NtWindow *w = data;
    Offer *o = calloc(1, sizeof *o);
    o->window = w;
    o->primary = 1;
    o->proxy = offer;
    o->next = w->offers;
    w->offers = o;
    zwp_primary_selection_offer_v1_add_listener(offer, &primary_offer_listener, o);
}
static void primary_selection(void *data, struct zwp_primary_selection_device_v1 *device,
                              struct zwp_primary_selection_offer_v1 *offer) {
    NtWindow *w = data;
    Offer *old = w->selection[1];
    w->selection[1] = offer ? zwp_primary_selection_offer_v1_get_user_data(offer) : NULL;
    if (old != w->selection[1])
        offer_destroy(old);
}
static const struct zwp_primary_selection_device_v1_listener primary_device_listener = {
    .data_offer = primary_offer, .selection = primary_selection};
static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t caps) {
    NtWindow *w = data;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !w->keyboard) {
        w->keyboard = wl_seat_get_keyboard(seat);
        wl_keyboard_add_listener(w->keyboard, &keyboard_listener, w);
    }
    if (!(caps & WL_SEAT_CAPABILITY_KEYBOARD) && w->keyboard) {
        wl_keyboard_release(w->keyboard);
        w->keyboard = NULL;
    }
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !w->pointer) {
        w->pointer = wl_seat_get_pointer(seat);
        w->cursor_device = wp_cursor_shape_manager_v1_get_pointer(w->cursor_manager, w->pointer);
        wl_pointer_add_listener(w->pointer, &pointer_listener, w);
    }
    if (!(caps & WL_SEAT_CAPABILITY_POINTER) && w->pointer) {
        w->pointer_inside = 0;
        wp_cursor_shape_device_v1_destroy(w->cursor_device);
        w->cursor_device = NULL;
        wl_pointer_release(w->pointer);
        w->pointer = NULL;
    }
}
static const struct wl_seat_listener seat_listener = {.capabilities = seat_capabilities,
                                                      .name = noop};
static void presentation_clock(void *data, struct wp_presentation *presentation,
                               uint32_t clock_id) {
    if (clock_id != CLOCK_MONOTONIC)
        fail(data, "presentation feedback must use CLOCK_MONOTONIC");
}
static const struct wp_presentation_listener presentation_listener = {.clock_id =
                                                                          presentation_clock};
static void feedback_presented(void *data, struct wp_presentation_feedback *proxy,
                               uint32_t seconds_hi, uint32_t seconds_lo, uint32_t nanoseconds,
                               uint32_t refresh, uint32_t sequence_hi, uint32_t sequence_lo,
                               uint32_t flags) {
    Feedback *feedback = data;
    double seconds =
        (double)(((uint64_t)seconds_hi << 32) | seconds_lo) + (double)nanoseconds / 1e9;
    emit(feedback->window, (NtEvent){.kind = NT_PRESENTED, .code = feedback->id, .x = seconds});
    wp_presentation_feedback_destroy(proxy);
    free(feedback);
}
static void feedback_discarded(void *data, struct wp_presentation_feedback *proxy) {
    wp_presentation_feedback_destroy(proxy);
    free(data);
}
static const struct wp_presentation_feedback_listener feedback_listener = {
    .sync_output = noop,
    .presented = feedback_presented,
    .discarded = feedback_discarded};
static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    NtWindow *w = data;
#define BIND(iface, field, required) \
    if (!strcmp(interface, #iface)) \
        w->field = wl_registry_bind(registry, name, &iface##_interface, required);
    BIND(wl_compositor, compositor, 6)
    else BIND(xdg_wm_base, shell, 6)
    else BIND(wl_seat, seat, 10)
    else BIND(wl_data_device_manager, data_manager, 3)
    else BIND(zwp_primary_selection_device_manager_v1, primary_manager, 1)
    else BIND(zwp_text_input_manager_v3, text_manager, 1)
    else BIND(wp_cursor_shape_manager_v1, cursor_manager, 1)
    else BIND(wp_viewporter, viewporter, 1)
    else BIND(wp_fractional_scale_manager_v1, scale_manager, 1)
    else if (getenv("NEXTTY_TRACE") && !strcmp(interface, "wp_presentation")) {
        w->presentation = wl_registry_bind(registry, name, &wp_presentation_interface, 1);
        wp_presentation_add_listener(w->presentation, &presentation_listener, w);
    } else if (!strcmp(interface, "wl_output")) {
        Output *o = calloc(1, sizeof *o);
        o->window = w;
        o->id = name;
        o->proxy = wl_registry_bind(registry, name, &wl_output_interface, 2);
        o->next = w->outputs;
        w->outputs = o;
        wl_output_add_listener(o->proxy, &output_listener, o);
    }
#undef BIND
}
static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    NtWindow *w = data;
    Output **link = &w->outputs;
    while (*link) {
        Output *o = *link;
        if (o->id == name) {
            *link = o->next;
            wl_output_destroy(o->proxy);
            free(o);
            update_refresh(w);
            return;
        }
        link = &o->next;
    }
}
static const struct wl_registry_listener registry_listener = {.global = registry_global,
                                                              .global_remove = registry_remove};
static void size_changed(NtWindow *w) {
    wp_viewport_set_destination(w->viewport, w->width, w->height);

    xdg_surface_set_window_geometry(w->xdg_surface, 0, 0, w->width, w->height);
    emit(w, (NtEvent){.kind = NT_RESIZE});
}
static int wayland_new(NtWindow *w) {
    w->display = wl_display_connect(NULL);
    if (!w->display) {
        fail(w, "connecting to Wayland: %s", strerror(errno));
        return 0;
    }
    w->registry = wl_display_get_registry(w->display);
    wl_registry_add_listener(w->registry, &registry_listener, w);
    if (wl_display_roundtrip(w->display) < 0) {
        wayland_error(w, "reading Wayland globals");
        return 0;
    }
    if (w->error[0])
        return 0;
    if (!w->compositor || !w->shell || !w->seat || !w->cursor_manager ||
        !w->data_manager || !w->primary_manager || !w->text_manager ||
        !w->viewporter || !w->scale_manager) {
        fail(w, "Wayland compositor lacks required GNOME window/input/selection/scale protocols");
        return 0;
    }
    xdg_wm_base_add_listener(w->shell, &shell_listener, w);
    wl_seat_add_listener(w->seat, &seat_listener, w);
    w->data_device = wl_data_device_manager_get_data_device(w->data_manager, w->seat);
    wl_data_device_add_listener(w->data_device, &data_listener, w);
    w->primary_device = zwp_primary_selection_device_manager_v1_get_device(
        w->primary_manager, w->seat);
    zwp_primary_selection_device_v1_add_listener(w->primary_device, &primary_device_listener, w);
    w->text_input = zwp_text_input_manager_v3_get_text_input(w->text_manager, w->seat);
    zwp_text_input_v3_add_listener(w->text_input, &text_listener, w);
    w->surface = wl_compositor_create_surface(w->compositor);
    wl_surface_add_listener(w->surface, &surface_listener, w);
    w->xdg_surface = xdg_wm_base_get_xdg_surface(w->shell, w->surface);
    xdg_surface_add_listener(w->xdg_surface, &xdg_surface_listener, w);
    w->toplevel = xdg_surface_get_toplevel(w->xdg_surface);
    xdg_toplevel_add_listener(w->toplevel, &toplevel_listener, w);
    xdg_toplevel_set_title(w->toplevel, "nextty");
    xdg_toplevel_set_app_id(w->toplevel, "nextty");
    w->viewport = wp_viewporter_get_viewport(w->viewporter, w->surface);
    w->fractional_scale =
        wp_fractional_scale_manager_v1_get_fractional_scale(w->scale_manager, w->surface);
    wp_fractional_scale_v1_add_listener(w->fractional_scale, &fractional_listener, w);
    wl_surface_commit(w->surface);
    while (!w->configured && !w->error[0])
        if (wl_display_dispatch(w->display) < 0) {
            wayland_error(w, "waiting for Wayland configure");
            return 0;
        }
    return !w->error[0];
}
static void start_paste(NtWindow *w) {
    if (!w->paste_pending)
        return;
    int primary = w->paste_primary;
    Source *own = w->current_source[primary];
    if (own) {
        if (own->file_fd >= 0) {
            int pipefd[2];
            if (pipe2(pipefd, O_CLOEXEC | O_NONBLOCK) < 0) {
                fail(w, "creating local clipboard pipe: %s", strerror(errno));
                finish_paste(w, NULL, 0);
                return;
            }
            add_transfer(w, pipefd[1], 1, NULL, own->length, own->file_fd);
            add_transfer(w, pipefd[0], 0, NULL, 0, -1);
            return;
        }
        finish_paste(w, own->bytes, own->length);
        return;
    }
    Offer *o = w->selection[primary];
    if (!o || !o->mime) {
        finish_paste(w, NULL, 0);
        return;
    }
    int pipefd[2];
    if (pipe2(pipefd, O_CLOEXEC) < 0) {
        fail(w, "creating clipboard pipe: %s", strerror(errno));
        finish_paste(w, NULL, 0);
        return;
    }
    if (primary)
        zwp_primary_selection_offer_v1_receive(o->proxy, o->mime, pipefd[1]);
    else
        wl_data_offer_receive(o->proxy, o->mime, pipefd[1]);
    close(pipefd[1]);
    add_transfer(w, pipefd[0], 0, NULL, 0, -1);
    if (wl_display_flush(w->display) < 0 && errno != EAGAIN)
        wayland_error(w, "flushing clipboard request");
}
static int poll_timeout(NtWindow *w, int timeout) {
    int64_t deadline = INT64_MAX;
    for (Transfer *t = w->transfers; t; t = t->next)
        if (t->deadline < deadline)
            deadline = t->deadline;
    for (FileTransfer *t = w->file_transfers; t; t = t->next)
        if (t->deadline < deadline)
            deadline = t->deadline;
    if (deadline != INT64_MAX) {
        int64_t delay = deadline - now_ms();
        if (delay < 0)
            delay = 0;
        if (delay > INT_MAX)
            delay = INT_MAX;
        if (timeout < 0 || delay < timeout)
            timeout = (int)delay;
    }
    return timeout;
}
/* Polls Wayland, the frame timer, the caller's sources and the clipboard transfers, then
   dispatches. A read on the display is already prepared. */
static void poll_and_dispatch(NtWindow *w, struct pollfd *sources, size_t source_count,
                              int timeout_ms) {
    size_t count = 2 + source_count;
    for (Transfer *t = w->transfers; t; t = t->next)
        count++;
    for (FileTransfer *t = w->file_transfers; t; t = t->next)
        count++;
    struct pollfd fds[count];
    fds[0] = (struct pollfd){.fd = wl_display_get_fd(w->display), .events = POLLIN};
    if (wl_display_flush(w->display) < 0) {
        if (errno != EAGAIN) {
            wl_display_cancel_read(w->display);
            wayland_error(w, "flushing Wayland events");
            return;
        }
        fds[0].events |= POLLOUT;
    }
    fds[1] = (struct pollfd){.fd = w->frame_fd, .events = POLLIN};
    memcpy(fds + 2, sources, source_count * sizeof *sources);
    struct pollfd *at = fds + 2 + source_count, *transfers = at;
    for (Transfer *t = w->transfers; t; t = t->next)
        *at++ = (struct pollfd){.fd = t->fd, .events = t->writing ? POLLOUT : POLLIN};
    struct pollfd *files = at;
    for (FileTransfer *t = w->file_transfers; t; t = t->next)
        *at++ = (struct pollfd){.fd = t->fd, .events = POLLOUT};
    int status = poll(fds, count, poll_timeout(w, timeout_ms));
    if (status < 0 && errno != EINTR)
        fail(w, "polling terminal events: %s", strerror(errno));
    /* Wayland callbacks may prepend transfers; process the polled list first. */
    process_transfers(w, transfers);
    process_file_transfers(w, files);
    if (status > 0 && (fds[0].revents & POLLIN)) {
        if (wl_display_read_events(w->display) < 0)
            wayland_error(w, "reading Wayland events");
    } else
        wl_display_cancel_read(w->display);
    if (status > 0 && (fds[0].revents & POLLOUT) && wl_display_flush(w->display) < 0 &&
        errno != EAGAIN)
        wayland_error(w, "flushing Wayland display");
    if (!w->error[0] && wl_display_dispatch_pending(w->display) < 0)
        wayland_error(w, "dispatching Wayland display");
    uint64_t expirations;
    if ((fds[1].revents & POLLIN) &&
        read(w->frame_fd, &expirations, sizeof expirations) == sizeof expirations)
        w->frame_deadline_ns = 0;
    if (fds[0].revents & (POLLHUP | POLLERR | POLLNVAL))
        fail(w, "display connection closed");
    for (size_t i = 0; i < source_count; ++i)
        sources[i].revents = fds[i + 2].revents;
}
int nt_window_wait(NtWindow *w, struct pollfd *sources, size_t source_count, int timeout_ms, NtEventFn callback, void *data) {
    w->callback = callback;
    w->callback_data = data;
    w->delivered = 0;
    drain_events(w);
    while (!w->error[0] && wl_display_prepare_read(w->display) < 0)
        if (wl_display_dispatch_pending(w->display) < 0)
            wayland_error(w, "dispatching queued Wayland events");
    if (!w->error[0])
        poll_and_dispatch(w, sources, source_count, w->delivered ? 0 : timeout_ms);
    w->callback = NULL;
    w->callback_data = NULL;
    return w->error[0] ? -1 : (int)w->delivered;
}

NtWindow *nt_window_new(int width, int height) {
    NtWindow *w = calloc(1, sizeof *w);
    w->width = width;
    w->height = height;
    w->scale = 1;
    w->cursor_w = w->cursor_h = 1;
    w->queued_tail = &w->queued;
    w->frame_fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    const char *locale = setlocale(LC_CTYPE, "");
    signal(SIGPIPE, SIG_IGN);
    w->xkb_context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_compose_table *table = xkb_compose_table_new_from_locale(
        w->xkb_context, locale ? locale : "C", XKB_COMPOSE_COMPILE_NO_FLAGS);
    if (table) {
        w->compose = xkb_compose_state_new(table, XKB_COMPOSE_STATE_NO_FLAGS);
        xkb_compose_table_unref(table);
    }
    if (wayland_new(w))
        return w;
    snprintf(startup_error, sizeof startup_error, "%s", w->error);
    nt_window_free(w);
    return NULL;
}
const char *nt_window_error(NtWindow *w) {
    return w ? w->error : startup_error;
}
void nt_window_size(NtWindow *w, int *width, int *height, double *scale) {
    if (width)
        *width = w->width;
    if (height)
        *height = w->height;
    if (scale)
        *scale = w->scale;
}
void nt_window_title(NtWindow *w, const char *title) {
    xdg_toplevel_set_title(w->toplevel, title);
}
void nt_window_minimize(NtWindow *w) {
    xdg_toplevel_set_minimized(w->toplevel);
}
void nt_window_maximize(NtWindow *w, int maximized) {
    if (maximized)
        xdg_toplevel_set_maximized(w->toplevel);
    else
        xdg_toplevel_unset_maximized(w->toplevel);
}
void nt_window_resize_cursor(NtWindow *w, int enabled) {
    if (w->resize_cursor == enabled)
        return;
    w->resize_cursor = enabled;
    if (w->pointer_inside)
        set_cursor_shape(w);
}
void nt_window_cursor_rect(NtWindow *w, int x, int y, int width, int height) {
    if (w->cursor_x == x && w->cursor_y == y && w->cursor_w == width && w->cursor_h == height)
        return;
    w->cursor_x = x;
    w->cursor_y = y;
    w->cursor_w = width;
    w->cursor_h = height;
    if (w->text_enabled) {
        zwp_text_input_v3_set_cursor_rectangle(w->text_input, x, y, width, height);
        zwp_text_input_v3_commit(w->text_input);
    }
}
static void set_selection(NtWindow *w, int primary, const char *bytes, size_t length, int fd) {
    primary = !!primary;
    Source *source = malloc(sizeof *source + length);
    if (w->pending_source[primary])
        source_remove(w->pending_source[primary]);
    *source = (Source){.next = w->sources,
                       .window = w,
                       .primary = primary,
                       .file_fd = fd,
                       .length = length};
    if (length)
        memcpy(source->bytes, bytes, length);
    w->sources = source;
    w->pending_source[primary] = source;
    wayland_selection(w, primary);
}
void nt_window_set_selection(NtWindow *w, int primary, const char *bytes, size_t length) {
    set_selection(w, primary, bytes, length, -1);
}
void nt_window_set_selection_file(NtWindow *w, int primary, int fd) {
    fcntl(fd, F_SETFD, FD_CLOEXEC);
    set_selection(w, primary, NULL, 0, fd);
}
void nt_window_paste(NtWindow *w, int primary) {
    if (w->paste_pending) {
        fail(w, "paste request while another paste is pending");
        return;
    }
    w->paste_pending = 1;
    w->paste_primary = !!primary;
    start_paste(w);
}

int nt_window_frame_ready(NtWindow *w) {
    if (w->suspended)
        return 0;
    int64_t deadline = w->last_frame_ns + w->frame_interval_ns;
    if (!w->last_frame_ns || !w->frame_interval_ns || now_ns() >= deadline)
        return 1;
    /* Only a caller with dirty content requests a deadline. An absolute timer
       stays fixed across repeated readiness checks, including driver dispatch. */
    schedule_frame(w, deadline);
    return 0;
}
void nt_window_prepare_frame(NtWindow *w) {
    int64_t now = now_ns(), elapsed = now - w->last_frame_ns;
    /* Preparation time must not accumulate into the refresh period. */
    w->last_frame_ns = w->last_frame_ns && w->frame_interval_ns &&
        elapsed >= w->frame_interval_ns && elapsed < 2 * w->frame_interval_ns
        ? w->last_frame_ns + w->frame_interval_ns : now;
    schedule_frame(w, 0);
    if (w->presentation) {
        Feedback *feedback = malloc(sizeof *feedback);
        *feedback = (Feedback){w, ++w->frame_id};
        wp_presentation_feedback_add_listener(
            wp_presentation_feedback(w->presentation, w->surface), &feedback_listener, feedback);
    }
}
void *nt_window_display(NtWindow *w) {
    return w->display;
}
void *nt_window_surface(NtWindow *w) {
    return w->surface;
}
/* Only the display needs an orderly goodbye: the compositor drops every other object with the
   connection and the process ends right after, so nothing else is released. */
void nt_window_free(NtWindow *w) {
    if (w->display) {
        wl_display_flush(w->display);
        wl_display_disconnect(w->display);
    }
    close(w->frame_fd);
    free(w);
}
