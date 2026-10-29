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
#include <math.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon-compose.h>
#include <xkbcommon/xkbcommon.h>

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
    int fd, file_fd, metadata_ready, write_ready;
    off_t offset;
    size_t length;
    int64_t deadline;
    size_t buffered, sent;
    char buffer[];
};
typedef struct {
    pthread_mutex_t mutex;
    FileTransfer *pending;
    int wakeup_fd, closed, error;
    _Atomic unsigned references;
} FileSender;
typedef struct Source Source;
struct Source {
    Source *next;
    NtWindow *window;
    int primary, file_fd;
    void *proxy;
    char *bytes;
    size_t length;
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
typedef struct Feedback Feedback;
struct Feedback {
    Feedback *next;
    NtWindow *window;
    struct wp_presentation_feedback *proxy;
    uint32_t id;
};
struct NtWindow {
    int width, height, tracing;
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
    char *pending_selection[2];
    int pending_selection_file[2];
    size_t pending_selection_len[2];
    int paste_pending, paste_primary;
    Transfer *transfers;
    FileSender *file_sender;
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
    Feedback *feedbacks;
    uint32_t frame_id, frame_callback_id;
    int suspended;
    int presentation_clock;
    struct wl_seat *seat;
    struct wl_keyboard *keyboard;
    struct wl_pointer *pointer;
    struct xkb_state *key_state;
    struct wl_callback *frame;
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
    int preedit_begin, preedit_end;
    double axis_x, axis_y;
    int axis_value_x, axis_value_y;
};

static _Thread_local char startup_error[256];
static int64_t now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
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
static int64_t now_ns(void) {
    struct timespec time;
    clock_gettime(CLOCK_MONOTONIC, &time);
    return (int64_t)time.tv_sec * 1000000000 + time.tv_nsec;
}
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
    /* A new deadline may already have elapsed while Rust was waiting. */
    emit(w, (NtEvent){.kind = NT_FRAME});
    if (w->tracing)
        emit(w, (NtEvent){.kind = NT_FRAME_RATE, .x = frame_interval_ns});
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
    Queued *q = malloc(sizeof *q + (event->text ? event->length + 1 : 0));
    if (!q) {
        fail(w, "allocating pending window event: %s", strerror(errno));
        return;
    }
    q->event = *event;
    q->next = NULL;
    if (event->text) {
        memcpy(q->text, event->text, event->length);
        q->text[event->length] = 0;
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
static void clear_events(Queued *q) {
    while (q) {
        Queued *next = q->next;
        free(q);
        q = next;
    }
}
static int append_bytes(NtWindow *w, char **bytes, size_t *length, size_t *capacity,
                        const void *data, size_t n) {
    if (n > SIZE_MAX - *length - 1) {
        fail(w, "clipboard size overflow");
        return 0;
    }
    if (*length + n + 1 > *capacity) {
        size_t cap = *length + n + 1;
        if (cap < 4096)
            cap = 4096;
        else if (cap <= SIZE_MAX / 2)
            cap *= 2;
        char *p = realloc(*bytes, cap);
        if (!p) {
            fail(w, "allocating clipboard: %s", strerror(errno));
            return 0;
        }
        *bytes = p;
        *capacity = cap;
    }
    if (n)
        memcpy(*bytes + *length, data, n);
    *length += n;
    (*bytes)[*length] = 0;
    return 1;
}
static void start_paste(NtWindow *w);
static void file_transfer_free(FileTransfer *t) {
    if (t->fd >= 0)
        close(t->fd);
    close(t->file_fd);
    free(t);
}
static void file_sender_release(FileSender *sender) {
    if (atomic_fetch_sub(&sender->references, 1) == 1) {
        close(sender->wakeup_fd);
        pthread_mutex_destroy(&sender->mutex);
        free(sender);
    }
}
static int file_sender_notify(FileSender *sender) {
    uint64_t value = 1;
    ssize_t n;
    do n = write(sender->wakeup_fd, &value, sizeof value); while (n < 0 && errno == EINTR);
    return n == (ssize_t)sizeof value || (n < 0 && errno == EAGAIN) ? 0 : errno;
}
static int file_sender_closed(FileSender *sender) {
    pthread_mutex_lock(&sender->mutex);
    int closed = sender->closed;
    pthread_mutex_unlock(&sender->mutex);
    return closed;
}
static void *file_sender_run(void *data) {
    FileSender *sender = data;
    FileTransfer *active = NULL;
    int error = 0;
    for (;;) {
        ssize_t n;
        pthread_mutex_lock(&sender->mutex);
        while (sender->pending) {
            FileTransfer *t = sender->pending;
            sender->pending = t->next;
            t->next = active;
            active = t;
        }
        int closed = sender->closed;
        pthread_mutex_unlock(&sender->mutex);
        if (closed)
            break;
        FileTransfer **link = &active;
        while (*link) {
            if (file_sender_closed(sender))
                goto done;
            FileTransfer *t = *link;
            if (t->fd < 0) {
                *link = t->next;
                file_transfer_free(t);
                continue;
            }
            int done = now_ms() >= t->deadline;
            if (!done && !t->metadata_ready) {
                struct stat st;
                int status;
                do status = fstat(t->file_fd, &st);
                while (status < 0 && errno == EINTR && !file_sender_closed(sender));
                if (file_sender_closed(sender))
                    goto done;
                if (status < 0) {
                    fprintf(stderr, "clipboard file metadata: %s\n", strerror(errno));
                    done = 1;
                } else if (!S_ISREG(st.st_mode) || st.st_size < 0) {
                    fprintf(stderr, "invalid clipboard file\n");
                    done = 1;
                } else {
                    t->length = (size_t)st.st_size;
                    t->metadata_ready = 1;
                }
            }
            if ((size_t)t->offset == t->length && t->sent == t->buffered)
                done = 1;
            if (!done && t->write_ready && t->sent == t->buffered) {
                size_t count = t->length - (size_t)t->offset;
                if (count > 16384)
                    count = 16384;
                do n = pread(t->file_fd, t->buffer, count, t->offset);
                while (n < 0 && errno == EINTR && !file_sender_closed(sender));
                if (file_sender_closed(sender))
                    goto done;
                if (n > 0) {
                    t->offset += n;
                    t->buffered = (size_t)n;
                    t->sent = 0;
                } else
                    done = !n || (errno != EAGAIN && errno != EWOULDBLOCK);
            }
            if (!done && t->write_ready && t->sent < t->buffered) {
                if (file_sender_closed(sender))
                    goto done;
                do n = write(t->fd, t->buffer + t->sent, t->buffered - t->sent);
                while (n < 0 && errno == EINTR && !file_sender_closed(sender));
                if (file_sender_closed(sender))
                    goto done;
                if (n > 0) {
                    t->sent += (size_t)n;
                    t->deadline = now_ms() + 10000;
                }
                done = ((size_t)t->offset == t->length && t->sent == t->buffered) || !n ||
                       (n < 0 && errno != EAGAIN && errno != EWOULDBLOCK);
            }
            if (done) {
                *link = t->next;
                file_transfer_free(t);
            } else
                link = &t->next;
        }
        size_t count = 1;
        int timeout = -1;
        for (FileTransfer *t = active; t; t = t->next) {
            count++;
            int64_t remaining = t->deadline - now_ms();
            int delay = remaining < 0 ? 0 : (int)remaining;
            if (timeout < 0 || delay < timeout)
                timeout = delay;
        }
        struct pollfd local[16], *fds = local;
        if (count > sizeof local / sizeof *local) {
            fds = malloc(count * sizeof *fds);
            if (!fds) {
                error = errno;
                break;
            }
        }
        fds[0] = (struct pollfd){.fd = sender->wakeup_fd, .events = POLLIN};
        size_t at = 1;
        for (FileTransfer *t = active; t; t = t->next)
            fds[at++] = (struct pollfd){.fd = t->fd, .events = POLLOUT};
        int status = poll(fds, (nfds_t)count, timeout);
        if (status < 0 && errno != EINTR)
            error = errno;
        if (fds[0].revents & POLLIN) {
            uint64_t value;
            do n = read(sender->wakeup_fd, &value, sizeof value); while (n < 0 && errno == EINTR);
            if (n < 0 && errno != EAGAIN)
                error = errno;
        }
        if (fds[0].revents & (POLLERR | POLLHUP | POLLNVAL))
            error = EIO;
        at = 1;
        for (FileTransfer *t = active; t; t = t->next)
            t->write_ready = fds[at++].revents != 0;
        if (fds != local)
            free(fds);
        if (error)
            break;
    }
done:
    pthread_mutex_lock(&sender->mutex);
    sender->closed = 1;
    sender->error = error;
    while (sender->pending) {
        FileTransfer *t = sender->pending;
        sender->pending = t->next;
        t->next = active;
        active = t;
    }
    pthread_mutex_unlock(&sender->mutex);
    if (error)
        fprintf(stderr, "clipboard file sender: %s\n", strerror(error));
    while (active) {
        FileTransfer *t = active;
        active = t->next;
        file_transfer_free(t);
    }
    file_sender_release(sender);
    return NULL;
}
static int file_sender_start(NtWindow *w) {
    if (w->file_sender)
        return 1;
    FileSender *sender = calloc(1, sizeof *sender);
    int error = sender ? pthread_mutex_init(&sender->mutex, NULL) : errno;
    if (error) {
        free(sender);
        fail(w, "creating clipboard file sender: %s", strerror(error));
        return 0;
    }
    sender->wakeup_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (sender->wakeup_fd < 0) {
        int saved = errno;
        pthread_mutex_destroy(&sender->mutex);
        free(sender);
        fail(w, "creating clipboard wakeup: %s", strerror(saved));
        return 0;
    }
    atomic_init(&sender->references, 2);
    pthread_attr_t attributes;
    error = pthread_attr_init(&attributes);
    if (!error) {
        error = pthread_attr_setdetachstate(&attributes, PTHREAD_CREATE_DETACHED);
        pthread_t thread;
        if (!error)
            error = pthread_create(&thread, &attributes, file_sender_run, sender);
        pthread_attr_destroy(&attributes);
    }
    if (error) {
        close(sender->wakeup_fd);
        pthread_mutex_destroy(&sender->mutex);
        free(sender);
        fail(w, "starting clipboard file sender: %s", strerror(error));
        return 0;
    }
    w->file_sender = sender;
    return 1;
}
static void queue_file(NtWindow *w, int fd, int file_fd) {
    FileTransfer *t = calloc(1, sizeof *t + (fd < 0 ? 0 : 16384));
    if (!t) {
        int error = errno;
        if (fd >= 0)
            close(fd);
        close(file_fd);
        fail(w, "allocating clipboard file transfer: %s", strerror(error));
        return;
    }
    t->fd = fd;
    t->file_fd = file_fd;
    t->write_ready = 1;
    t->deadline = now_ms() + 10000;
    FileSender *sender = w->file_sender;
    pthread_mutex_lock(&sender->mutex);
    int error = sender->error;
    if (!sender->closed) {
        t->next = sender->pending;
        sender->pending = t;
        error = file_sender_notify(sender);
        t = NULL;
    }
    pthread_mutex_unlock(&sender->mutex);
    if (t)
        file_transfer_free(t);
    if (error)
        fail(w, "sending clipboard file: %s", strerror(error));
}
static void finish_paste(NtWindow *w, const char *bytes, size_t length) {
    if (!w->paste_pending)
        return;
    int primary = w->paste_primary;
    w->paste_pending = 0;
    if (!bytes) {
        bytes = "";
        length = 0;
    }
    emit(w, (NtEvent){.kind = NT_PASTE, .code = primary, .text = bytes, .length = length});
}
static void add_transfer(NtWindow *w, int fd, int writing, const char *bytes, size_t length,
                         int file_fd) {
    int flags = fcntl(fd, F_GETFL);
    if (fcntl(fd, F_SETFD, FD_CLOEXEC) < 0 || flags < 0 ||
        fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
        int error = errno;
        close(fd);
        fail(w, "configuring clipboard descriptor: %s", strerror(error));
        return;
    }
    if (writing && file_fd >= 0) {
        int copy = fcntl(file_fd, F_DUPFD_CLOEXEC, 0);
        if (copy < 0) {
            close(fd);
            fail(w, "duplicating clipboard file: %s", strerror(errno));
            return;
        }
        queue_file(w, fd, copy);
        return;
    }
    Transfer *t = calloc(1, sizeof *t);
    if (!t) {
        close(fd);
        fail(w, "allocating clipboard transfer: %s", strerror(errno));
        return;
    }
    t->fd = fd;
    t->writing = writing;
    t->deadline = now_ms() + 10000;
    if (writing) {
        t->bytes = malloc(length ? length : 1);
        if (!t->bytes) {
            close(fd);
            free(t);
            fail(w, "allocating clipboard transfer: %s", strerror(errno));
            return;
        }
        if (length)
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
                t->deadline = now_ms() + 10000;
                if (t->writing) {
                    t->offset += (size_t)n;
                    done = t->offset == t->length;
                } else if (!append_bytes(w, &t->bytes, &t->length, &t->capacity, buffer, (size_t)n)) {
                    done = 1;
                    good = 0;
                }
            } else if (!n) {
                done = 1;
            } else if (errno != EAGAIN && errno != EWOULDBLOCK) {
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
            finish_paste(w, good ? (t->bytes ? t->bytes : "") : NULL, t->length);
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
static int printable_utf8(const char *text, int n) {
    return n > 0 && (unsigned char)text[0] >= 32 && (unsigned char)text[0] != 127;
}
static void keyboard_text(NtWindow *w, xkb_keysym_t sym, const char *text, int n, int compose) {
    int key_text = n > 0 && (w->modifiers & NT_ALT);
    emit(w, (NtEvent){.kind = NT_KEY,
                      .code = sym,
                      .modifiers = w->modifiers,
                      .text = key_text ? text : NULL,
                      .length = key_text ? (size_t)n : 0});
    if (w->modifiers & (NT_CONTROL | NT_ALT | NT_SUPER))
        return;
    if (compose && w->compose) {
        xkb_compose_state_feed(w->compose, sym);
        switch (xkb_compose_state_get_status(w->compose)) {
        case XKB_COMPOSE_COMPOSING:
            emit(w, (NtEvent){.kind = NT_PREEDIT, .text = "", .start = 0, .end = 0});
            return;
        case XKB_COMPOSE_COMPOSED: {
            int size = xkb_compose_state_get_utf8(w->compose, NULL, 0);
            char *s = malloc((size_t)size + 1);
            if (!s) {
                fail(w, "allocating composed text: %s", strerror(errno));
                return;
            }
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
            return;
        default:
            break;
        }
    }
    if (printable_utf8(text, n))
        emit(w, (NtEvent){.kind = NT_TEXT, .text = text, .length = (size_t)n});
}
static void size_changed(NtWindow *w);
static void wayland_selection(NtWindow *w, int primary);

/* Wayland listeners use the display's default queue; the driver may enqueue onto it. */
static void shell_ping(void *data, struct xdg_wm_base *shell, uint32_t serial) {
    xdg_wm_base_pong(shell, serial);
}
static const struct xdg_wm_base_listener shell_listener = {.ping = shell_ping};
static void frame_done(void *data, struct wl_callback *cb, uint32_t time) {
    NtWindow *w = data;
    NtEvent event = {.kind = NT_FRAME};
    struct timespec received;
    clock_gettime(CLOCK_MONOTONIC, &received);
    event.code = w->frame_callback_id;
    event.x = received.tv_sec + received.tv_nsec / 1e9;
    event.y = time;
    wl_callback_destroy(cb);
    w->frame = NULL;
    emit(w, event);
}
static const struct wl_callback_listener frame_listener = {.done = frame_done};
static void toplevel_configure(void *data, struct xdg_toplevel *top, int32_t width, int32_t height,
                               struct wl_array *states) {
    NtWindow *w = data;
    w->pending_width = width;
    w->pending_height = height;
    uint32_t flags = 0;
    uint32_t *state;
    wl_array_for_each(state, states) {
        if (*state == XDG_TOPLEVEL_STATE_ACTIVATED)
            flags |= 1;
        else if (*state == XDG_TOPLEVEL_STATE_SUSPENDED)
            flags |= 2;
        else if (*state == XDG_TOPLEVEL_STATE_FULLSCREEN)
            flags |= 4;
        else if (*state == XDG_TOPLEVEL_STATE_MAXIMIZED)
            flags |= 8;
    }
    int suspended = !!(flags & 2);
    if (suspended != w->suspended) {
        w->suspended = suspended;
        schedule_frame(w, 0);
        if (!suspended)
            emit(w, (NtEvent){.kind = NT_EXPOSE});
    }
    double received = 0;
    if (w->tracing) {
        struct timespec timestamp;
        clock_gettime(CLOCK_MONOTONIC, &timestamp);
        received = timestamp.tv_sec + timestamp.tv_nsec / 1e9;
    }
    emit(w, (NtEvent){.kind = NT_WINDOW_STATE, .code = flags, .x = received});
}
static void toplevel_close(void *data, struct xdg_toplevel *top) {
    emit(data, (NtEvent){.kind = NT_CLOSE});
}
static void toplevel_bounds(void *data, struct xdg_toplevel *top, int32_t width, int32_t height) {
}
static void toplevel_capabilities(void *data, struct xdg_toplevel *top, struct wl_array *caps) {
}
static const struct xdg_toplevel_listener toplevel_listener = {.configure = toplevel_configure,
                                                               .close = toplevel_close,
                                                               .configure_bounds = toplevel_bounds,
                                                               .wm_capabilities =
                                                                   toplevel_capabilities};
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
static void surface_scale(void *data, struct wl_surface *surface, int32_t scale) {
}
static void surface_transform(void *data, struct wl_surface *surface, uint32_t transform) {
}
static const struct wl_surface_listener surface_listener = {.enter = surface_enter,
                                                            .leave = surface_leave,
                                                            .preferred_buffer_scale = surface_scale,
                                                            .preferred_buffer_transform =
                                                                surface_transform};
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
static void output_geometry(void *data, struct wl_output *o, int32_t x, int32_t y, int32_t pw,
                            int32_t ph, int32_t sub, const char *make, const char *model,
                            int32_t transform) {
}
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
static void output_scale(void *data, struct wl_output *o, int32_t scale) {
}
static void output_name(void *data, struct wl_output *o, const char *name) {
}
static const struct wl_output_listener output_listener = {.geometry = output_geometry,
                                                          .mode = output_mode,
                                                          .done = output_done,
                                                          .scale = output_scale,
                                                          .name = output_name,
                                                          .description = output_name};

static void keyboard_keymap(void *data, struct wl_keyboard *keyboard, uint32_t format, int fd,
                            uint32_t size) {
    NtWindow *w = data;
    if (format != WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1 || !size) {
        close(fd);
        fail(w, "unsupported Wayland keyboard keymap format %u", format);
        return;
    }
    char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (map == MAP_FAILED) {
        fail(w, "mapping keyboard keymap: %s", strerror(errno));
        return;
    }
    struct xkb_keymap *keymap = xkb_keymap_new_from_buffer(
        w->xkb_context, map, size - 1, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
    munmap(map, size);
    if (!keymap) {
        fail(w, "compiling Wayland keyboard keymap");
        return;
    }
    struct xkb_state *state = xkb_state_new(keymap);
    xkb_keymap_unref(keymap);
    if (!state) {
        fail(w, "creating Wayland keyboard state");
        return;
    }
    xkb_state_unref(w->key_state);
    w->key_state = state;
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
static void wayland_key(NtWindow *w, uint32_t key, int composing) {
    if (!w->key_state)
        return;
    xkb_keysym_t sym = xkb_state_key_get_one_sym(w->key_state, key + 8);
    char text[128];
    int n = xkb_state_key_get_utf8(w->key_state, key + 8, text, sizeof text);
    if (n >= (int)sizeof text)
        n = 0;
    keyboard_text(w, sym, text, n, composing);
}
static void keyboard_key(void *data, struct wl_keyboard *keyboard, uint32_t serial, uint32_t time,
                         uint32_t key, uint32_t state) {
    NtWindow *w = data;
    w->serial = serial;
    for (int i = 0; i < 2; i++)
        if (w->pending_selection[i] || w->pending_selection_file[i] >= 0)
            wayland_selection(w, i);
    if (state == WL_KEYBOARD_KEY_STATE_PRESSED || state == WL_KEYBOARD_KEY_STATE_REPEATED)
        wayland_key(w, key, state == WL_KEYBOARD_KEY_STATE_PRESSED);
}
static void keyboard_modifiers(void *data, struct wl_keyboard *keyboard, uint32_t serial,
                               uint32_t depressed, uint32_t latched, uint32_t locked,
                               uint32_t group) {
    NtWindow *w = data;
    w->serial = serial;
    if (w->key_state) {
        xkb_state_update_mask(w->key_state, depressed, latched, locked, 0, 0, group);
        w->modifiers = xkb_modifiers(w->key_state);
    }
}
static void keyboard_repeat(void *data, struct wl_keyboard *keyboard, int32_t rate, int32_t delay) {
    /* Mutter owns repetition for wl_keyboard v10 and sends repeat_info(0, 0). */
}
static const struct wl_keyboard_listener keyboard_listener = {.keymap = keyboard_keymap,
                                                              .enter = keyboard_enter,
                                                              .leave = keyboard_leave,
                                                              .key = keyboard_key,
                                                              .modifiers = keyboard_modifiers,
                                                              .repeat_info = keyboard_repeat};
static void pointer_enter(void *data, struct wl_pointer *pointer, uint32_t serial,
                          struct wl_surface *surface, wl_fixed_t x, wl_fixed_t y) {
    NtWindow *w = data;
    w->pointer_serial = serial;
    w->pointer_inside = 1;
    wp_cursor_shape_device_v1_set_shape(w->cursor_device, serial,
        w->resize_cursor == 2 ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NS_RESIZE :
        w->resize_cursor ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_EW_RESIZE : WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT);
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
        if (w->pending_selection[i] || w->pending_selection_file[i] >= 0)
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
    double v = wl_fixed_to_double(value);
    if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL)
        w->axis_y += v / 10.0;
    else
        w->axis_x += v / 10.0;
}
static void pointer_frame(void *data, struct wl_pointer *pointer) {
    NtWindow *w = data;
    double x = w->axis_value_x ? w->axis_value_x / 120.0 : w->axis_x;
    double y = w->axis_value_y ? w->axis_value_y / 120.0 : w->axis_y;
    if (x || y)
        emit(w, (NtEvent){.kind = NT_SCROLL, .x = -x, .y = -y, .modifiers = w->modifiers});
    w->axis_x = w->axis_y = 0;
    w->axis_value_x = w->axis_value_y = 0;
}
static void pointer_axis_source(void *data, struct wl_pointer *pointer, uint32_t source) {
}
static void pointer_axis_stop(void *data, struct wl_pointer *pointer, uint32_t time,
                              uint32_t axis) {
}
static void pointer_axis_value(void *data, struct wl_pointer *pointer, uint32_t axis,
                               int32_t value) {
    NtWindow *w = data;
    if (axis == WL_POINTER_AXIS_VERTICAL_SCROLL)
        w->axis_value_y = value;
    else
        w->axis_value_x = value;
}
static void pointer_axis_direction(void *data, struct wl_pointer *pointer, uint32_t axis,
                                   uint32_t direction) {
}
static const struct wl_pointer_listener pointer_listener = {.enter = pointer_enter,
                                                            .leave = pointer_leave,
                                                            .motion = pointer_motion,
                                                            .button = pointer_button,
                                                            .axis = pointer_axis,
                                                            .frame = pointer_frame,
                                                            .axis_source = pointer_axis_source,
                                                            .axis_stop = pointer_axis_stop,
                                                            .axis_value120 = pointer_axis_value,
                                                            .axis_relative_direction =
                                                                pointer_axis_direction};
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
    free(w->pending_commit);
    w->pending_commit = NULL;
    free(w->pending_preedit);
    w->pending_preedit = NULL;
    emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
}
static void text_preedit(void *data, struct zwp_text_input_v3 *input, const char *text,
                         int32_t begin, int32_t end) {
    NtWindow *w = data;
    free(w->pending_preedit);
    w->pending_preedit = strdup(text ? text : "");
    if (!w->pending_preedit) {
        fail(w, "copying IME preedit: %s", strerror(errno));
        return;
    }
    w->preedit_begin = begin;
    w->preedit_end = end;
}
static void text_commit_string(void *data, struct zwp_text_input_v3 *input, const char *text) {
    NtWindow *w = data;
    free(w->pending_commit);
    w->pending_commit = strdup(text ? text : "");
    if (!w->pending_commit) {
        fail(w, "copying IME commit: %s", strerror(errno));
        return;
    }
}
static void text_delete(void *data, struct zwp_text_input_v3 *input, uint32_t before,
                        uint32_t after) {
    /* No surrounding text is offered by this terminal; the protocol forbids
       nonzero deletion beyond the offered range. */
    NtWindow *w = data;
    if (before || after)
        fail(w, "input method requested deletion without surrounding text");
}
static void text_done(void *data, struct zwp_text_input_v3 *input, uint32_t serial) {
    NtWindow *w = data;
    /* Stale serials still carry valid edits. No state requests are issued from
       done: application cursor changes have already committed their own state. */
    if (w->pending_commit)
        emit(w, (NtEvent){.kind = NT_PREEDIT, .text = ""});
    if (w->pending_commit && *w->pending_commit)
        emit(w, (NtEvent){.kind = NT_TEXT,
                          .text = w->pending_commit,
                          .length = strlen(w->pending_commit)});
    const char *s = w->pending_preedit ? w->pending_preedit : "";
    emit(w, (NtEvent){.kind = NT_PREEDIT,
                      .text = s,
                      .length = strlen(s),
                      .start = w->pending_preedit ? w->preedit_begin : 0,
                      .end = w->pending_preedit ? w->preedit_end : 0});
    free(w->pending_commit);
    w->pending_commit = NULL;
    free(w->pending_preedit);
    w->pending_preedit = NULL;
}
static const struct zwp_text_input_v3_listener text_listener = {.enter = text_enter,
                                                                .leave = text_leave,
                                                                .preedit_string = text_preedit,
                                                                .commit_string = text_commit_string,
                                                                .delete_surrounding_text =
                                                                    text_delete,
                                                                .done = text_done};
static void source_remove(Source *source) {
    NtWindow *w = source->window;
    Source **link = &w->sources;
    while (*link && *link != source)
        link = &(*link)->next;
    if (*link)
        *link = source->next;
    if (w->current_source[source->primary] == source)
        w->current_source[source->primary] = NULL;
    if (source->primary)
        zwp_primary_selection_source_v1_destroy(source->proxy);
    else
        wl_data_source_destroy(source->proxy);
    free(source->bytes);
    if (source->file_fd >= 0)
        queue_file(w, -1, source->file_fd);
    free(source);
}
static void source_target(void *data, struct wl_data_source *source, const char *mime) {
}
static void source_send(void *data, struct wl_data_source *proxy, const char *mime, int32_t fd) {
    Source *s = data;
    add_transfer(s->window, fd, 1, s->bytes, s->length, s->file_fd);
}
static void source_cancel(void *data, struct wl_data_source *proxy) {
    source_remove(data);
}
static void source_drop(void *data, struct wl_data_source *proxy) {
}
static void source_action(void *data, struct wl_data_source *proxy, uint32_t action) {
}
static const struct wl_data_source_listener source_listener = {.target = source_target,
                                                               .send = source_send,
                                                               .cancelled = source_cancel,
                                                               .dnd_drop_performed = source_drop,
                                                               .dnd_finished = source_drop,
                                                               .action = source_action};
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
    if (!w->serial)
        return;
    Source *s = calloc(1, sizeof *s);
    if (!s) {
        fail(w, "allocating clipboard source: %s", strerror(errno));
        return;
    }
    s->window = w;
    s->primary = primary;
    s->length = w->pending_selection_len[primary];
    s->bytes = w->pending_selection[primary];
    s->file_fd = w->pending_selection_file[primary];
    w->pending_selection_file[primary] = -1;
    w->pending_selection[primary] = NULL;
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
    s->next = w->sources;
    w->sources = s;
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
static void data_offer_action(void *data, struct wl_data_offer *offer, uint32_t action) {
}
static const struct wl_data_offer_listener offer_listener = {
    .offer = data_offer_mime, .source_actions = data_offer_action, .action = data_offer_action};
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
    while (*link && *link != o)
        link = &(*link)->next;
    if (*link)
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
    if (!o) {
        fail(w, "allocating clipboard offer: %s", strerror(errno));
        return;
    }
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
static void data_motion(void *data, struct wl_data_device *device, uint32_t time, wl_fixed_t x,
                        wl_fixed_t y) {
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
                                                             .motion = data_motion,
                                                             .drop = data_leave,
                                                             .selection = data_selection};
static void primary_offer(void *data, struct zwp_primary_selection_device_v1 *device,
                          struct zwp_primary_selection_offer_v1 *offer) {
    NtWindow *w = data;
    Offer *o = calloc(1, sizeof *o);
    if (!o) {
        fail(w, "allocating primary offer: %s", strerror(errno));
        return;
    }
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
static void seat_name(void *data, struct wl_seat *seat, const char *name) {
}
static const struct wl_seat_listener seat_listener = {.capabilities = seat_capabilities,
                                                      .name = seat_name};
static void presentation_clock(void *data, struct wp_presentation *presentation,
                               uint32_t clock_id) {
    NtWindow *w = data;
    w->presentation_clock = (int)clock_id;
}
static const struct wp_presentation_listener presentation_listener = {.clock_id =
                                                                          presentation_clock};
static void feedback_remove(Feedback *feedback) {
    Feedback **link = &feedback->window->feedbacks;
    while (*link != feedback)
        link = &(*link)->next;
    *link = feedback->next;
    wp_presentation_feedback_destroy(feedback->proxy);
    free(feedback);
}
static void feedback_output(void *data, struct wp_presentation_feedback *feedback,
                            struct wl_output *output) {
}
static void feedback_presented(void *data, struct wp_presentation_feedback *proxy,
                               uint32_t seconds_hi, uint32_t seconds_lo, uint32_t nanoseconds,
                               uint32_t refresh, uint32_t sequence_hi, uint32_t sequence_lo,
                               uint32_t flags) {
    Feedback *feedback = data;
    NtWindow *w = feedback->window;
    double seconds =
        (double)(((uint64_t)seconds_hi << 32) | seconds_lo) + (double)nanoseconds / 1e9;
    emit(w, (NtEvent){.kind = NT_PRESENTED,
                      .code = feedback->id,
                      .modifiers = flags,
                      .x = seconds,
                      .y = refresh,
                      .start = w->presentation_clock});
    feedback_remove(feedback);
}
static void feedback_discarded(void *data, struct wp_presentation_feedback *proxy) {
    Feedback *feedback = data;
    NtWindow *w = feedback->window;
    emit(w, (NtEvent){.kind = NT_PRESENTED,
                      .code = feedback->id,
                      .start = w->presentation_clock,
                      .end = 1});
    feedback_remove(feedback);
}
static const struct wp_presentation_feedback_listener feedback_listener = {
    .sync_output = feedback_output,
    .presented = feedback_presented,
    .discarded = feedback_discarded};
static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    NtWindow *w = data;
#define BIND(iface, field, required)                                                         \
    if (!strcmp(interface, #iface)) {                                                       \
        if (version < (required)) {                                                        \
            fail(w, "Wayland requires %s version %d (offered %u)", #iface, required, version); \
            return;                                                                        \
        }                                                                                  \
        w->field = wl_registry_bind(registry, name, &iface##_interface, required);           \
    }
    BIND(wl_compositor, compositor, 6)
    else BIND(xdg_wm_base, shell, 6)
    else BIND(wl_seat, seat, 10)
    else BIND(wl_data_device_manager, data_manager, 3)
    else BIND(zwp_primary_selection_device_manager_v1, primary_manager, 1)
    else BIND(zwp_text_input_manager_v3, text_manager, 1)
    else BIND(wp_cursor_shape_manager_v1, cursor_manager, 1)
    else BIND(wp_viewporter, viewporter, 1)
    else BIND(wp_fractional_scale_manager_v1, scale_manager, 1)
    else if (w->tracing && !strcmp(interface, "wp_presentation")) {
        w->presentation = wl_registry_bind(registry, name, &wp_presentation_interface, 1);
        wp_presentation_add_listener(w->presentation, &presentation_listener, w);
    } else if (!strcmp(interface, "wl_output")) {
        if (version < 4) {
            fail(w, "Wayland requires wl_output version 4 (offered %u)", version);
            return;
        }
        Output *o = calloc(1, sizeof *o);
        if (!o) {
            fail(w, "allocating output: %s", strerror(errno));
            return;
        }
        o->window = w;
        o->id = name;
        o->proxy = wl_registry_bind(registry, name, &wl_output_interface, 4);
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
    emit(w, (NtEvent){.kind = NT_RESIZE, .x = w->width, .y = w->height});
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
    if (w->error[0])
        return 0;
    return 1;
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
int nt_window_wait(NtWindow *w, struct pollfd *sources, size_t source_count, int timeout_ms, NtEventFn callback, void *data) {
    if (!w || !callback)
        return -1;
    w->callback = callback;
    w->callback_data = data;
    w->delivered = 0;
    drain_events(w);
    if (w->error[0])
        goto done;
    if (w->delivered)
        timeout_ms = 0;
    while (wl_display_prepare_read(w->display) < 0) {
        if (wl_display_dispatch_pending(w->display) < 0) {
            wayland_error(w, "dispatching queued Wayland events");
            break;
        }
        if (w->error[0])
            goto done;
        if (w->delivered)
            timeout_ms = 0;
    }
    if (w->error[0])
        goto done;
    timeout_ms = poll_timeout(w, timeout_ms);
    int count = 2 + (int)source_count;
    for (Transfer *t = w->transfers; t; t = t->next)
        count++;
    struct pollfd local[16], *fds = local;
    if ((size_t)count > sizeof local / sizeof *local) {
        fds = malloc((size_t)count * sizeof *fds);
        if (!fds) {
            wl_display_cancel_read(w->display);
            fail(w, "allocating native poll descriptors: %s", strerror(errno));
            goto done;
        }
    }
    fds[0] = (struct pollfd){.fd = wl_display_get_fd(w->display), .events = POLLIN};
    if (wl_display_flush(w->display) < 0) {
        if (errno == EAGAIN)
            fds[0].events |= POLLOUT;
        else {
            wl_display_cancel_read(w->display);
            if (fds != local)
                free(fds);
            wayland_error(w, "flushing Wayland events");
            goto done;
        }
    }
    fds[1] = (struct pollfd){.fd = w->frame_fd, .events = POLLIN};
    for (size_t i = 0; i < source_count; ++i)
        fds[i + 2] = (struct pollfd){.fd = sources[i].fd, .events = sources[i].events};
    int at = 2 + (int)source_count;
    for (Transfer *t = w->transfers; t; t = t->next)
        fds[at++] = (struct pollfd){.fd = t->fd, .events = t->writing ? POLLOUT : POLLIN};
    int status = poll(fds, (nfds_t)count, timeout_ms);
    if (status < 0 && errno != EINTR)
        fail(w, "polling terminal events: %s", strerror(errno));
    /* Wayland callbacks may prepend transfers; process the polled list first. */
    process_transfers(w, &fds[2 + source_count]);
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
    if (fds[1].revents & POLLIN) {
        uint64_t expirations;
        if (read(w->frame_fd, &expirations, sizeof expirations) == sizeof expirations) {
            w->frame_deadline_ns = 0;
            emit(w, (NtEvent){.kind = NT_FRAME});
        }
    }
    if (fds[0].revents & (POLLHUP | POLLERR | POLLNVAL))
        fail(w, "display connection closed");
    for (size_t i = 0; i < source_count; ++i)
        sources[i].revents = fds[i + 2].revents;
    if (fds != local)
        free(fds);
done:
    w->callback = NULL;
    w->callback_data = NULL;
    return w->error[0] ? -1 : (int)w->delivered;
}

NtWindow *nt_window_new(int width, int height) {
    NtWindow *w = calloc(1, sizeof *w);
    if (w) w->pending_selection_file[0] = w->pending_selection_file[1] = -1;
    if (!w) {
        snprintf(startup_error, sizeof startup_error, "allocating native window: %s",
                 strerror(errno));
        return NULL;
    }
    w->tracing = getenv("NEXTTY_TRACE") != NULL;
    w->width = width > 0 ? width : 800;
    w->height = height > 0 ? height : 600;
    w->scale = 1;
    w->cursor_w = w->cursor_h = 1;
    w->queued_tail = &w->queued;
    w->frame_fd = -1;
    w->frame_fd = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC | TFD_NONBLOCK);
    if (w->frame_fd < 0) {
        fail(w, "creating frame timer: %s", strerror(errno));
        goto failed;
    }
    setlocale(LC_CTYPE, "");
    signal(SIGPIPE, SIG_IGN);
    w->xkb_context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (!w->xkb_context) {
        fail(w, "creating XKB context");
        goto failed;
    }
    const char *locale = setlocale(LC_CTYPE, NULL);
    struct xkb_compose_table *table = xkb_compose_table_new_from_locale(
        w->xkb_context, locale ? locale : "C", XKB_COMPOSE_COMPILE_NO_FLAGS);
    if (table) {
        w->compose = xkb_compose_state_new(table, XKB_COMPOSE_STATE_NO_FLAGS);
        xkb_compose_table_unref(table);
    }
    if (!wayland_new(w))
        goto failed;
    return w;
failed:
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
        wp_cursor_shape_device_v1_set_shape(w->cursor_device, w->pointer_serial,
            enabled == 2 ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_NS_RESIZE :
            enabled ? WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_EW_RESIZE : WP_CURSOR_SHAPE_DEVICE_V1_SHAPE_DEFAULT);
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
void nt_window_set_selection(NtWindow *w, int primary, const char *bytes, size_t length) {
    primary = !!primary;
    char *copy = malloc(length + 1);
    if (!copy) {
        fail(w, "copying selection: %s", strerror(errno));
        return;
    }
    memcpy(copy, bytes, length);
    copy[length] = 0;
    free(w->pending_selection[primary]);
    if (w->pending_selection_file[primary] >= 0)
        queue_file(w, -1, w->pending_selection_file[primary]);
    w->pending_selection_file[primary] = -1;
    w->pending_selection[primary] = copy;
    w->pending_selection_len[primary] = length;
    wayland_selection(w, primary);
}
void nt_window_set_selection_file(NtWindow *w, int primary, int fd) {
    primary = !!primary;
    if (fcntl(fd, F_SETFD, FD_CLOEXEC) < 0) {
        int error = errno;
        close(fd);
        fail(w, "configuring clipboard file: %s", strerror(error));
        return;
    }
    if (!file_sender_start(w)) {
        close(fd);
        return;
    }
    free(w->pending_selection[primary]);
    w->pending_selection[primary] = NULL;
    if (w->pending_selection_file[primary] >= 0)
        queue_file(w, -1, w->pending_selection_file[primary]);
    w->pending_selection_file[primary] = fd;
    w->pending_selection_len[primary] = 0;
    wayland_selection(w, primary);
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
    if (w->tracing)
        w->frame_id++;
    if (w->presentation) {
        Feedback *feedback = calloc(1, sizeof *feedback);
        if (!feedback) {
            fail(w, "allocating presentation feedback: %s", strerror(errno));
            return;
        }
        feedback->window = w;
        feedback->id = w->frame_id;
        feedback->proxy = wp_presentation_feedback(w->presentation, w->surface);
        if (!feedback->proxy) {
            free(feedback);
            fail(w, "allocating presentation feedback: %s", strerror(errno));
            return;
        }
        feedback->next = w->feedbacks;
        w->feedbacks = feedback;
        wp_presentation_feedback_add_listener(feedback->proxy, &feedback_listener, feedback);
    }
    if (w->tracing && !w->frame) {
        w->frame_callback_id = w->frame_id;
        w->frame = wl_surface_frame(w->surface);
        wl_callback_add_listener(w->frame, &frame_listener, w);
    }
}
void *nt_window_display(NtWindow *w) {
    return w->display;
}
void *nt_window_surface(NtWindow *w) {
    return w->surface;
}
void nt_window_free(NtWindow *w) {
    if (!w)
        return;
    if (w->display) {
        if (w->frame)
            wl_callback_destroy(w->frame);
        while (w->feedbacks)
            feedback_remove(w->feedbacks);
        if (w->presentation)
            wp_presentation_destroy(w->presentation);
        while (w->sources)
            source_remove(w->sources);
        while (w->offers)
            offer_destroy(w->offers);
        if (w->text_input)
            zwp_text_input_v3_destroy(w->text_input);
        if (w->primary_device)
            zwp_primary_selection_device_v1_destroy(w->primary_device);
        if (w->data_device)
            wl_data_device_release(w->data_device);
        if (w->keyboard)
            wl_keyboard_release(w->keyboard);
        if (w->cursor_device)
            wp_cursor_shape_device_v1_destroy(w->cursor_device);
        if (w->pointer)
            wl_pointer_release(w->pointer);
        if (w->seat)
            wl_seat_release(w->seat);
        if (w->fractional_scale)
            wp_fractional_scale_v1_destroy(w->fractional_scale);
        if (w->viewport)
            wp_viewport_destroy(w->viewport);
        if (w->toplevel)
            xdg_toplevel_destroy(w->toplevel);
        if (w->xdg_surface)
            xdg_surface_destroy(w->xdg_surface);
        if (w->surface)
            wl_surface_destroy(w->surface);
        if (w->cursor_manager)
            wp_cursor_shape_manager_v1_destroy(w->cursor_manager);
        if (w->scale_manager)
            wp_fractional_scale_manager_v1_destroy(w->scale_manager);
        if (w->viewporter)
            wp_viewporter_destroy(w->viewporter);
        if (w->primary_manager)
            zwp_primary_selection_device_manager_v1_destroy(w->primary_manager);
        if (w->text_manager)
            zwp_text_input_manager_v3_destroy(w->text_manager);
        if (w->data_manager)
            wl_data_device_manager_destroy(w->data_manager);
        while (w->outputs) {
            Output *o = w->outputs;
            w->outputs = o->next;
            wl_output_destroy(o->proxy);
            free(o);
        }
        if (w->shell)
            xdg_wm_base_destroy(w->shell);
        if (w->compositor)
            wl_compositor_destroy(w->compositor);
        if (w->registry)
            wl_registry_destroy(w->registry);
        wl_display_flush(w->display);
        wl_display_disconnect(w->display);
        xkb_state_unref(w->key_state);
        free(w->pending_commit);
        free(w->pending_preedit);
    }
    while (w->transfers) {
        Transfer *t = w->transfers;
        w->transfers = t->next;
        close(t->fd);
        free(t->bytes);
        free(t);
    }
    if (w->frame_fd >= 0)
        close(w->frame_fd);
    clear_events(w->queued);
    free(w->pending_selection[0]);
    free(w->pending_selection[1]);
    for (int i = 0; i < 2; i++)
        if (w->pending_selection_file[i] >= 0)
            queue_file(w, -1, w->pending_selection_file[i]);
    if (w->file_sender) {
        FileSender *sender = w->file_sender;
        pthread_mutex_lock(&sender->mutex);
        sender->closed = 1;
        int error = file_sender_notify(sender);
        pthread_mutex_unlock(&sender->mutex);
        if (error)
            fprintf(stderr, "closing clipboard file sender: %s\n", strerror(error));
        /* A file syscall may still be blocked; the sender owns its remaining lifetime. */
        file_sender_release(sender);
    }
    xkb_compose_state_unref(w->compose);
    xkb_context_unref(w->xkb_context);
    free(w);
}
