pub(crate) mod trace;

use crate::error::{Error, Result};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    ptr::NonNull,
};

#[cfg(feature = "tui")]
use std::{fs::File, os::fd::IntoRawFd};

pub const SHIFT: u32 = 1;
pub const CONTROL: u32 = 2;
pub const ALT: u32 = 4;
pub const SUPER: u32 = 8;
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
pub const FRAME: i32 = 11;
pub const EXPOSE: i32 = 12;
pub const PRESENTED: i32 = 13;
pub const WINDOW_STATE: i32 = 14;
pub const FRAME_RATE: i32 = 15;
pub const POINTER_LEAVE: i32 = 16;

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
    end: c_int,
}

pub struct Event {
    pub kind: i32,
    pub code: u32,
    pub modifiers: u32,
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub start: i32,
    pub end: i32,
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Run {
    pub start: u32,
    pub end: u32,
    pub color: u32,
    pub flags: u32,
}

unsafe extern "C" {
    fn nt_window_new(width: c_int, height: c_int) -> *mut c_void;
    fn nt_window_free(window: *mut c_void);
    fn nt_window_error(window: *mut c_void) -> *const c_char;
    fn nt_window_wait(
        window: *mut c_void,
        sources: *mut libc::pollfd,
        count: usize,
        timeout: c_int,
        callback: unsafe extern "C" fn(*mut c_void, *const RawEvent),
        data: *mut c_void,
    ) -> c_int;
    fn nt_window_size(window: *mut c_void, width: *mut c_int, height: *mut c_int, scale: *mut f64);
    fn nt_window_title(window: *mut c_void, title: *const c_char);
    #[cfg(feature = "tui")]
    fn nt_window_minimize(window: *mut c_void);
    #[cfg(feature = "tui")]
    fn nt_window_maximize(window: *mut c_void, maximized: c_int);
    #[cfg(any(feature = "agent", feature = "monitor"))]
    fn nt_window_resize_cursor(window: *mut c_void, enabled: c_int);
    fn nt_window_cursor_rect(window: *mut c_void, x: c_int, y: c_int, width: c_int, height: c_int);
    fn nt_window_set_selection(
        window: *mut c_void,
        primary: c_int,
        text: *const c_char,
        len: usize,
    );
    fn nt_window_paste(window: *mut c_void, primary: c_int);
    #[cfg(feature = "tui")]
    fn nt_window_set_selection_file(window: *mut c_void, primary: c_int, fd: c_int);
    fn nt_window_frame_ready(window: *mut c_void) -> c_int;
    fn nt_renderer_new(window: *mut c_void, family: *const c_char, font_size: f32) -> *mut c_void;
    fn nt_renderer_free(renderer: *mut c_void);
    fn nt_renderer_error(renderer: *mut c_void) -> *const c_char;
    fn nt_renderer_metrics(renderer: *mut c_void, width: *mut f32, height: *mut f32);
    fn nt_line_new(
        renderer: *mut c_void,
        text: *const c_char,
        len: usize,
        runs: *const Run,
        count: usize,
        columns: *const u16,
    ) -> *mut c_void;
    fn nt_line_free(line: *mut c_void);
    fn nt_renderer_begin(renderer: *mut c_void, background: u32) -> c_int;
    #[cfg(feature = "tui")]
    fn nt_renderer_clear(renderer: *mut c_void, background: u32) -> c_int;
    fn nt_renderer_clip(renderer: *mut c_void, x: f32, y: f32, width: f32, height: f32) -> c_int;
    fn nt_renderer_rect(
        renderer: *mut c_void,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        color: u32,
        wave: c_int,
    );
    #[cfg(feature = "monitor")]
    fn nt_renderer_stroke(
        renderer: *mut c_void,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
        color: u32,
    );
    #[cfg(feature = "files")]
    fn nt_renderer_round_rect(
        renderer: *mut c_void,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        color: u32,
    );
    #[cfg(feature = "tui")]
    fn nt_renderer_tab(
        renderer: *mut c_void,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        inset: f32,
        color: u32,
    );
    fn nt_renderer_line(renderer: *mut c_void, line: *mut c_void, x: f32, y: f32, factor: f32);
    fn nt_renderer_present(renderer: *mut c_void) -> c_int;
    fn nt_renderer_ready(renderer: *mut c_void) -> c_int;
    fn nt_renderer_finish(renderer: *mut c_void) -> c_int;
    fn nt_renderer_fd(renderer: *mut c_void) -> c_int;
    fn nt_renderer_swap_time(renderer: *mut c_void) -> f64;
}

fn message(pointer: *const c_char) -> Error {
    Error::msg(
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned(),
    )
}

pub struct Window(NonNull<c_void>);
pub struct Renderer(NonNull<c_void>);
pub struct Line(NonNull<c_void>);

impl Window {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        NonNull::new(unsafe { nt_window_new(width, height) })
            .map(Self)
            .ok_or_else(|| message(unsafe { nt_window_error(std::ptr::null_mut()) }))
    }

    pub fn wait(
        &self,
        sources: &mut [libc::pollfd],
        timeout: i32,
        events: &mut Vec<Event>,
    ) -> Result<()> {
        for source in sources.iter_mut() {
            source.revents = 0;
        }
        struct Collected<'a> {
            events: &'a mut Vec<Event>,
            error: Option<Error>,
        }
        unsafe extern "C" fn collect(data: *mut c_void, event: *const RawEvent) {
            let output = unsafe { &mut *data.cast::<Collected<'_>>() };
            let event = unsafe { &*event };
            let text = if event.length == 0 {
                String::new()
            } else {
                let bytes = unsafe { std::slice::from_raw_parts(event.text.cast(), event.length) };
                match std::str::from_utf8(bytes) {
                    Ok(text) => text.to_owned(),
                    Err(error) => {
                        output.error = Some(Error::context("platform text is not UTF-8", error));
                        return;
                    }
                }
            };
            output.events.push(Event {
                kind: event.kind,
                code: event.code,
                modifiers: event.modifiers,
                x: event.x,
                y: event.y,
                text,
                start: event.start,
                end: event.end,
            });
        }
        let mut output = Collected {
            events,
            error: None,
        };
        let result = unsafe {
            nt_window_wait(
                self.0.as_ptr(),
                sources.as_mut_ptr(),
                sources.len(),
                timeout,
                collect,
                (&mut output as *mut Collected<'_>).cast(),
            )
        };
        if let Some(error) = output.error {
            return Err(error);
        }
        if result < 0 {
            return Err(message(unsafe { nt_window_error(self.0.as_ptr()) }));
        }
        Ok(())
    }

    pub fn size(&self) -> (i32, i32, f64) {
        let (mut width, mut height, mut scale) = (0, 0, 0.);
        unsafe { nt_window_size(self.0.as_ptr(), &mut width, &mut height, &mut scale) };
        (width, height, scale)
    }
    pub fn ready(&self) -> bool {
        unsafe { nt_window_frame_ready(self.0.as_ptr()) != 0 }
    }
    pub fn title(&self, text: &str) -> Result<()> {
        let text = CString::new(text).map_err(|error| Error::context("window title", error))?;
        unsafe { nt_window_title(self.0.as_ptr(), text.as_ptr()) };
        Ok(())
    }
    #[cfg(feature = "tui")]
    pub fn minimize(&self) {
        unsafe { nt_window_minimize(self.0.as_ptr()) };
    }
    #[cfg(feature = "tui")]
    pub fn maximize(&self, maximized: bool) {
        unsafe { nt_window_maximize(self.0.as_ptr(), maximized.into()) };
    }
    #[cfg(feature = "agent")]
    pub fn resize_cursor(&self, enabled: bool) {
        unsafe { nt_window_resize_cursor(self.0.as_ptr(), enabled.into()) };
    }
    #[cfg(all(feature = "tui", feature = "monitor"))]
    pub fn vertical_resize_cursor(&self, enabled: bool) {
        unsafe { nt_window_resize_cursor(self.0.as_ptr(), if enabled { 2 } else { 0 }) };
    }

    pub fn cursor_rect(&self, x: i32, y: i32, width: i32, height: i32) {
        unsafe { nt_window_cursor_rect(self.0.as_ptr(), x, y, width, height) };
    }
    pub fn selection(&self, primary: bool, text: &str) {
        unsafe {
            nt_window_set_selection(
                self.0.as_ptr(),
                primary.into(),
                text.as_ptr().cast(),
                text.len(),
            )
        };
    }
    pub fn paste(&self, primary: bool) {
        unsafe { nt_window_paste(self.0.as_ptr(), primary.into()) }
    }
    #[cfg(feature = "tui")]
    pub fn selection_file(&self, primary: bool, file: File) {
        unsafe { nt_window_set_selection_file(self.0.as_ptr(), primary.into(), file.into_raw_fd()) }
    }
}

impl Renderer {
    pub fn new(window: &Window, family: &str, font_size: f32) -> Result<Self> {
        let family = CString::new(family).map_err(|error| Error::context("font_family", error))?;
        NonNull::new(unsafe { nt_renderer_new(window.0.as_ptr(), family.as_ptr(), font_size) })
            .map(Self)
            .ok_or_else(|| message(unsafe { nt_renderer_error(std::ptr::null_mut()) }))
    }
    pub fn metrics(&self) -> (f32, f32) {
        let (mut width, mut height) = (0., 0.);
        unsafe { nt_renderer_metrics(self.0.as_ptr(), &mut width, &mut height) };
        (width, height)
    }
    pub fn shape(&self, text: &str, runs: &[Run], columns: &[u16]) -> Result<Line> {
        NonNull::new(unsafe {
            nt_line_new(
                self.0.as_ptr(),
                text.as_ptr().cast(),
                text.len(),
                runs.as_ptr(),
                runs.len(),
                if columns.is_empty() {
                    std::ptr::null()
                } else {
                    columns.as_ptr()
                },
            )
        })
        .map(Line)
        .ok_or_else(|| message(unsafe { nt_renderer_error(self.0.as_ptr()) }))
    }
    pub fn begin(&self, background: u32) -> Result<()> {
        self.check(unsafe { nt_renderer_begin(self.0.as_ptr(), background) })
    }
    #[cfg(feature = "tui")]
    pub fn clear(&self, background: u32) -> Result<()> {
        self.check(unsafe { nt_renderer_clear(self.0.as_ptr(), background) })
    }
    pub fn clip(&self, x: f32, y: f32, width: f32, height: f32) -> Result<()> {
        self.check(unsafe { nt_renderer_clip(self.0.as_ptr(), x, y, width, height) })
    }
    pub fn rect(&self, x: f32, y: f32, width: f32, height: f32, color: u32, wave: bool) {
        unsafe { nt_renderer_rect(self.0.as_ptr(), x, y, width, height, color, wave.into()) };
    }
    #[cfg(feature = "monitor")]
    pub fn stroke(&self, from: (f32, f32), to: (f32, f32), width: f32, color: u32) {
        unsafe { nt_renderer_stroke(self.0.as_ptr(), from.0, from.1, to.0, to.1, width, color) };
    }
    #[cfg(feature = "files")]
    pub fn rounded_rect(&self, position: (f32, f32), size: (f32, f32), radius: f32, color: u32) {
        unsafe {
            nt_renderer_round_rect(
                self.0.as_ptr(),
                position.0,
                position.1,
                size.0,
                size.1,
                radius,
                color,
            )
        };
    }
    #[cfg(feature = "tui")]
    pub fn tab(&self, position: (f32, f32), size: (f32, f32), inset: f32, color: u32) {
        unsafe {
            nt_renderer_tab(
                self.0.as_ptr(),
                position.0,
                position.1,
                size.0,
                size.1,
                inset,
                color,
            )
        };
    }
    pub fn line(&self, line: &Line, x: f32, y: f32) {
        unsafe { nt_renderer_line(self.0.as_ptr(), line.0.as_ptr(), x, y, 1.) };
    }
    #[cfg(feature = "tui")]
    pub fn line_scaled(&self, line: &Line, x: f32, y: f32, factor: f32) {
        unsafe { nt_renderer_line(self.0.as_ptr(), line.0.as_ptr(), x, y, factor) };
    }
    pub fn fd(&self) -> i32 {
        unsafe { nt_renderer_fd(self.0.as_ptr()) }
    }
    pub fn ready(&self) -> Result<bool> {
        match unsafe { nt_renderer_ready(self.0.as_ptr()) } {
            -1 => Err(message(unsafe { nt_renderer_error(self.0.as_ptr()) })),
            ready => Ok(ready != 0),
        }
    }
    pub fn swap_time(&self) -> f64 {
        unsafe { nt_renderer_swap_time(self.0.as_ptr()) }
    }
    pub fn finish(&self) -> Result<()> {
        self.check(unsafe { nt_renderer_finish(self.0.as_ptr()) })
    }
    pub fn present(&self) -> Result<()> {
        self.check(unsafe { nt_renderer_present(self.0.as_ptr()) })
    }
    fn check(&self, result: i32) -> Result<()> {
        if result == 0 {
            Err(message(unsafe { nt_renderer_error(self.0.as_ptr()) }))
        } else {
            Ok(())
        }
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        unsafe { nt_line_free(self.0.as_ptr()) }
    }
}
impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe { nt_renderer_free(self.0.as_ptr()) }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        unsafe { nt_window_free(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests;
