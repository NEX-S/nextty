pub(crate) mod trace;

use std::io::{self, Result, Write};
use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::File,
    os::fd::IntoRawFd,
};

pub use crate::input::{ALT, CONTROL, SHIFT, SUPER};
pub const KEY: i32 = 1;
pub const TEXT: i32 = 2;
pub const PREEDIT: i32 = 3;
pub const MOTION: i32 = 4;
pub const BUTTON: i32 = 5;
pub const SCROLL: i32 = 6;
pub const FOCUS: i32 = 7;
pub const RESIZE: i32 = 8;
pub const CLOSE: i32 = 9;
pub const PASTE: i32 = 10;
pub const EXPOSE: i32 = 11;
pub const PRESENTED: i32 = 12;
pub const WINDOW_STATE: i32 = 13;
pub const POINTER_LEAVE: i32 = 14;

const RECT: c_int = 0;
const UNDERCURL: c_int = 3;
const TAB: c_int = 4;
const STROKE: c_int = 5;
const STROKE_RISING: c_int = 6;

#[repr(C)]
struct RawEvent {
    kind: c_int,
    code: u32,
    modifiers: u32,
    x: f64,
    y: f64,
    text: *const c_char,
    length: usize,
    start: c_int,
}

pub struct Event {
    pub kind: i32,
    pub code: u32,
    pub modifiers: u32,
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub start: i32,
}

#[derive(Clone, Copy, PartialEq)]
#[repr(C)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub color: u32,
    pub flags: u32,
}

type Events = RefCell<Vec<Event>>;

unsafe extern "C" {
    fn nt_window_new(
        width: c_int,
        height: c_int,
        callback: unsafe extern "C" fn(*mut c_void, *const RawEvent),
        data: *mut c_void,
    ) -> *mut c_void;
    fn nt_window_error(window: *mut c_void) -> *const c_char;
    fn nt_window_wait(
        window: *mut c_void,
        sources: *mut libc::pollfd,
        count: usize,
        timeout: c_int,
    ) -> c_int;
    fn nt_window_size(window: *mut c_void, width: *mut c_int, height: *mut c_int, scale: *mut f64);
    fn nt_window_title(window: *mut c_void, title: *const c_char);
    fn nt_window_minimize(window: *mut c_void);
    fn nt_window_maximize(window: *mut c_void, maximized: c_int);
    fn nt_window_resize_cursor(window: *mut c_void, enabled: c_int);
    fn nt_window_cursor_rect(window: *mut c_void, x: c_int, y: c_int, width: c_int, height: c_int);
    fn nt_window_set_selection(window: *mut c_void, primary: c_int, file: c_int);
    fn nt_window_paste(window: *mut c_void, primary: c_int);
    fn nt_window_frame_ready(window: *mut c_void) -> c_int;
    fn nt_renderer_new(window: *mut c_void, family: *const c_char, font_size: f32) -> *mut c_void;
    fn nt_renderer_error(renderer: *mut c_void) -> *const c_char;
    fn nt_renderer_metrics(renderer: *mut c_void, width: *mut f32, height: *mut f32);
    fn nt_line_new(
        text: *const c_char,
        len: usize,
        runs: *const Run,
        count: usize,
        columns: *const u16,
    ) -> *mut c_void;
    fn nt_line_free(line: *mut c_void);
    fn nt_renderer_begin(renderer: *mut c_void, background: u32);
    fn nt_renderer_clear(renderer: *mut c_void, background: u32);
    fn nt_renderer_clip(renderer: *mut c_void, x: f32, y: f32, width: f32, height: f32);
    fn nt_renderer_quad(
        renderer: *mut c_void,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: u32,
        mode: c_int,
        param: f32,
    );
    fn nt_renderer_line(renderer: *mut c_void, line: *mut c_void, x: f32, y: f32);
    fn nt_renderer_present(renderer: *mut c_void);
    fn nt_renderer_ready(renderer: *mut c_void) -> c_int;
    fn nt_renderer_finish(renderer: *mut c_void) -> c_int;
    fn nt_renderer_fd(renderer: *mut c_void) -> c_int;
}

fn error(message: *const c_char) -> io::Error {
    io::Error::other(
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn check(message: *const c_char) -> Result<()> {
    match unsafe { *message } {
        0 => Ok(()),
        _ => Err(error(message)),
    }
}

unsafe extern "C" fn collect(data: *mut c_void, event: *const RawEvent) {
    let event = unsafe { &*event };
    let text = if event.length == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe {
            std::slice::from_raw_parts(event.text.cast(), event.length)
        })
        .into_owned()
    };
    let events = unsafe { &*data.cast::<Events>() };
    events.borrow_mut().push(Event {
        kind: event.kind,
        code: event.code,
        modifiers: event.modifiers,
        x: event.x,
        y: event.y,
        text,
        start: event.start,
    });
}

/// The window and renderer live as long as the process, whose exit releases them.
pub struct Window {
    raw: *mut c_void,
    events: &'static Events,
}
pub struct Renderer(*mut c_void);
pub struct Line(*mut c_void);

impl Window {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        let events: &'static Events = Box::leak(Box::default());
        let data = std::ptr::from_ref(events).cast_mut().cast();
        let raw = unsafe { nt_window_new(width, height, collect, data) };
        check(unsafe { nt_window_error(raw) })?;
        Ok(Self { raw, events })
    }

    /// Polls `sources` along with the window, then moves the window's events into `events`.
    pub fn wait(
        &self,
        sources: &mut [libc::pollfd],
        timeout: i32,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        unsafe { nt_window_wait(self.raw, sources.as_mut_ptr(), sources.len(), timeout) };
        events.append(&mut self.events.borrow_mut());
        check(unsafe { nt_window_error(self.raw) })
    }

    pub fn size(&self) -> (i32, i32, f64) {
        let (mut width, mut height, mut scale) = (0, 0, 0.);
        unsafe { nt_window_size(self.raw, &mut width, &mut height, &mut scale) };
        (width, height, scale)
    }
    pub fn ready(&self) -> bool {
        unsafe { nt_window_frame_ready(self.raw) != 0 }
    }
    pub fn title(&self, text: &str) -> Result<()> {
        let text = CString::new(text)?;
        unsafe { nt_window_title(self.raw, text.as_ptr()) };
        Ok(())
    }

    pub fn minimize(&self) {
        unsafe { nt_window_minimize(self.raw) };
    }

    pub fn maximize(&self, maximized: bool) {
        unsafe { nt_window_maximize(self.raw, maximized.into()) };
    }
    pub fn resize_cursor(&self, horizontal: bool, vertical: bool) {
        let shape = if vertical { 2 } else { i32::from(horizontal) };
        unsafe { nt_window_resize_cursor(self.raw, shape) };
    }

    pub fn cursor_rect(&self, x: i32, y: i32, width: i32, height: i32) {
        unsafe { nt_window_cursor_rect(self.raw, x, y, width, height) };
    }
    pub fn selection(&self, primary: bool, text: &str) -> Result<()> {
        let mut file = crate::sys::memfd(c"nextty-selection")?;
        file.write_all(text.as_bytes())?;
        self.selection_file(primary, file);
        Ok(())
    }

    pub fn selection_file(&self, primary: bool, file: File) {
        unsafe { nt_window_set_selection(self.raw, primary.into(), file.into_raw_fd()) }
    }

    /// The selection arrives as a `PASTE` event.
    pub fn paste(&self, primary: bool) {
        unsafe { nt_window_paste(self.raw, primary.into()) }
    }
}

impl Renderer {
    pub fn new(window: &Window, family: &str, font_size: f32) -> Result<Self> {
        let family = CString::new(family)?;
        let renderer = Self(unsafe { nt_renderer_new(window.raw, family.as_ptr(), font_size) });
        check(unsafe { nt_renderer_error(renderer.0) })?;
        Ok(renderer)
    }
    pub fn metrics(&self) -> (f32, f32) {
        let (mut width, mut height) = (0., 0.);
        unsafe { nt_renderer_metrics(self.0, &mut width, &mut height) };
        (width, height)
    }
    pub fn begin(&self, background: u32) {
        unsafe { nt_renderer_begin(self.0, background) };
    }

    pub fn clear(&self, background: u32) {
        unsafe { nt_renderer_clear(self.0, background) };
    }
    pub fn clip(&self, x: f32, y: f32, width: f32, height: f32) {
        unsafe { nt_renderer_clip(self.0, x, y, width, height) };
    }
    fn quad(
        &self,
        (x, y, width, height): (f32, f32, f32, f32),
        color: u32,
        mode: c_int,
        param: f32,
    ) {
        unsafe { nt_renderer_quad(self.0, x, y, width, height, color, mode, param) };
    }
    pub fn rect(&self, x: f32, y: f32, width: f32, height: f32, color: u32) {
        self.quad((x, y, width, height), color, RECT, 0.);
    }
    pub fn undercurl(&self, x: f32, y: f32, width: f32, height: f32, color: u32) {
        self.quad((x, y, width, height), color, UNDERCURL, 0.);
    }

    /// A round-capped segment from `from` to `to`.
    pub fn stroke(&self, from: (f32, f32), to: (f32, f32), width: f32, color: u32) {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let radius = width * 0.5;
        let padding = radius + 1.;
        let mode = if dx * dy < 0. { STROKE_RISING } else { STROKE };
        let bounds = (
            from.0.min(to.0) - padding,
            from.1.min(to.1) - padding,
            dx.abs() + padding * 2.,
            dy.abs() + padding * 2.,
        );
        self.quad(bounds, color, mode, radius);
    }

    /// A tab whose sides slant inwards by `inset` towards the top.
    pub fn tab(&self, position: (f32, f32), size: (f32, f32), inset: f32, color: u32) {
        let bounds = (position.0, position.1, size.0, size.1);
        self.quad(bounds, color, TAB, inset.min(size.0 * 0.5));
    }
    pub fn line(&self, line: &Line, x: f32, y: f32) {
        unsafe { nt_renderer_line(self.0, line.0, x, y) };
    }
    pub fn fd(&self) -> i32 {
        unsafe { nt_renderer_fd(self.0) }
    }
    pub fn ready(&self) -> Result<bool> {
        match unsafe { nt_renderer_ready(self.0) } {
            -1 => Err(error(unsafe { nt_renderer_error(self.0) })),
            ready => Ok(ready != 0),
        }
    }
    pub fn finish(&self) -> Result<()> {
        match unsafe { nt_renderer_finish(self.0) } {
            0 => Err(error(unsafe { nt_renderer_error(self.0) })),
            _ => Ok(()),
        }
    }
    pub fn present(&self) {
        unsafe { nt_renderer_present(self.0) };
    }
}

impl Line {
    /// Shaped on the render thread when first drawn; `columns` is empty for one column per byte.
    pub fn new(text: &str, runs: &[Run], columns: &[u16]) -> Self {
        let columns = if columns.is_empty() {
            std::ptr::null()
        } else {
            columns.as_ptr()
        };
        Self(unsafe {
            nt_line_new(
                text.as_ptr().cast(),
                text.len(),
                runs.as_ptr(),
                runs.len(),
                columns,
            )
        })
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        unsafe { nt_line_free(self.0) }
    }
}
impl Drop for Renderer {
    /// Exiting while the render thread submits a frame would race the driver's own teardown.
    fn drop(&mut self) {
        unsafe { nt_renderer_finish(self.0) };
    }
}

#[cfg(test)]
mod tests;
