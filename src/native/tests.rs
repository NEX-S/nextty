use super::Run;
use std::{
    cell::UnsafeCell,
    ffi::{CStr, CString, c_char, c_void},
    ptr, slice,
    sync::Once,
};

#[derive(Clone, Copy)]
#[repr(C)]
struct Glyph {
    font: *mut c_void,
    glyph: u32,
    x: f32,
    y: f32,
    color: u32,
    color_glyph: u32,
    straight: u32,
}

#[repr(C)]
struct CairoGlyph {
    index: libc::c_ulong,
    x: f64,
    y: f64,
}

unsafe extern "C" {
    fn nt_fonts_new(
        family: *const c_char,
        size: f32,
        error: *mut c_char,
        size_error: usize,
    ) -> *mut c_void;
    fn nt_fonts_free(fonts: *mut c_void);
    fn nt_fonts_scale(fonts: *mut c_void, scale: f64) -> i32;
    fn nt_fonts_shape(
        fonts: *mut c_void,
        text: *const u8,
        length: usize,
        runs: *const Run,
        run_count: usize,
        columns: *const u16,
        glyphs: *mut *mut Glyph,
        count: *mut usize,
    ) -> i32;
    fn FcConfigCreate() -> *mut c_void;
    fn FcConfigAppFontAddFile(config: *mut c_void, path: *const u8) -> i32;
    fn FcConfigBuildFonts(config: *mut c_void) -> i32;
    fn FcConfigSetCurrent(config: *mut c_void) -> i32;
    fn FcConfigDestroy(config: *mut c_void);
    fn nt_font_glyphs_free(glyphs: *mut Glyph, count: usize);
    fn cairo_image_surface_create(format: i32, width: i32, height: i32) -> *mut c_void;
    fn cairo_image_surface_get_data(surface: *mut c_void) -> *const u8;
    fn cairo_image_surface_get_stride(surface: *mut c_void) -> i32;
    fn cairo_surface_status(surface: *mut c_void) -> i32;
    fn cairo_surface_flush(surface: *mut c_void);
    fn cairo_surface_destroy(surface: *mut c_void);
    fn cairo_create(surface: *mut c_void) -> *mut c_void;
    fn cairo_status(context: *mut c_void) -> i32;
    fn cairo_destroy(context: *mut c_void);
    fn cairo_set_source_rgba(context: *mut c_void, r: f64, g: f64, b: f64, a: f64);
    fn cairo_set_scaled_font(context: *mut c_void, font: *mut c_void);
    fn cairo_show_glyphs(context: *mut c_void, glyphs: *const CairoGlyph, count: i32);
}

struct Fonts {
    raw: *mut c_void,
    error: Box<UnsafeCell<[c_char; 512]>>,
}

impl Fonts {
    fn new(family: &CStr) -> Self {
        static FONTS: Once = Once::new();
        FONTS.call_once(|| unsafe {
            let config = FcConfigCreate();
            assert!(!config.is_null());
            for name in ["arabic", "color", "metrics"] {
                let path = CString::new(format!(
                    "{}/tests/fonts/{name}.ttf",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .unwrap();
                assert_ne!(FcConfigAppFontAddFile(config, path.as_ptr().cast()), 0);
            }
            assert_ne!(FcConfigBuildFonts(config), 0);
            assert_ne!(FcConfigSetCurrent(config), 0);
            FcConfigDestroy(config);
        });
        let error = Box::new(UnsafeCell::new([0; 512]));
        let raw = unsafe { nt_fonts_new(family.as_ptr(), 24.0, error.get().cast(), 512) };
        assert!(!raw.is_null(), "{}", unsafe {
            CStr::from_ptr(error.get().cast()).to_string_lossy()
        });
        Self { raw, error }
    }

    fn shape(&mut self, text: &str, runs: &[Run], columns: &[u16]) -> Shaped {
        let mut shaped = Shaped {
            raw: ptr::null_mut(),
            count: 0,
        };
        let result = unsafe {
            nt_fonts_shape(
                self.raw,
                text.as_ptr(),
                text.len(),
                runs.as_ptr(),
                runs.len(),
                if columns.is_empty() {
                    ptr::null()
                } else {
                    columns.as_ptr()
                },
                &mut shaped.raw,
                &mut shaped.count,
            )
        };
        assert_ne!(result, 0, "{}", unsafe {
            CStr::from_ptr(self.error.get().cast()).to_string_lossy()
        });
        shaped
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        unsafe { nt_fonts_free(self.raw) };
    }
}

struct Shaped {
    raw: *mut Glyph,
    count: usize,
}

impl Shaped {
    fn glyphs(&self) -> &[Glyph] {
        if self.count == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(self.raw, self.count) }
        }
    }
}

impl Drop for Shaped {
    fn drop(&mut self) {
        unsafe { nt_font_glyphs_free(self.raw, self.count) };
    }
}

#[test]
fn foreground_boundaries_preserve_arabic_joining() {
    let mut fonts = Fonts::new(c"Nextty Test Arabic");
    let plain = fonts.shape("بتب", &[], &[0, 0, 1, 1, 2, 2]);
    let colored = fonts.shape(
        "بتب",
        &[
            Run {
                start: 0,
                end: 2,
                color: 0xff0000ff,
                flags: 0,
            },
            Run {
                start: 2,
                end: 4,
                color: 0x00ff00ff,
                flags: 0,
            },
            Run {
                start: 4,
                end: 6,
                color: 0xff0000ff,
                flags: 0,
            },
        ],
        &[0, 0, 1, 1, 2, 2],
    );
    let positions = |shaped: &Shaped| {
        let mut glyphs: Vec<_> = shaped
            .glyphs()
            .iter()
            .map(|g| (g.glyph, g.x, g.y, g.font as usize))
            .collect();
        glyphs.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then(a.2.total_cmp(&b.2))
                .then(a.0.cmp(&b.0))
        });
        glyphs
    };
    assert!(!plain.glyphs().is_empty());
    assert_eq!(positions(&plain), positions(&colored));
    assert!(colored.glyphs().iter().any(|g| g.color == 0x00ff00ff));
    let mut joined: Vec<_> = plain.glyphs().iter().map(|g| g.glyph).collect();
    let mut isolated = Vec::new();
    for letter in ["ب", "ت", "ب"] {
        let shaped = fonts.shape(letter, &[], &[0, 0]);
        isolated.extend(shaped.glyphs().iter().map(|g| g.glyph));
    }
    joined.sort_unstable();
    isolated.sort_unstable();
    assert_ne!(joined, isolated, "the whole word must use joining forms");
}

#[test]
fn multibyte_text_keeps_foreground_runs_on_terminal_columns() {
    let mut fonts = Fonts::new(c"Nextty Test Arabic");
    let metric = fonts.shape("mm", &[], &[0, 1]);
    let width = metric.glyphs()[1].x - metric.glyphs()[0].x;
    let colors = [0xff0000ff, 0x00ff00ff];
    let shaped = fonts.shape(
        "🌈mبm",
        &[
            Run {
                start: 4,
                end: 5,
                color: colors[0],
                flags: 0,
            },
            Run {
                start: 7,
                end: 8,
                color: colors[1],
                flags: 0,
            },
        ],
        &[0, 0, 0, 0, 2, 3, 3, 4],
    );
    let positions: Vec<_> = shaped
        .glyphs()
        .iter()
        .filter(|glyph| colors.contains(&glyph.color))
        .map(|glyph| (glyph.color, glyph.x))
        .collect();
    assert_eq!(
        positions,
        [(colors[0], width * 2.), (colors[1], width * 4.)]
    );
}

#[test]
fn colrv1_emoji_raster_contains_color() {
    let mut fonts = Fonts::new(c"Nextty Test Color");
    let shaped = fonts.shape("🌈", &[], &[0, 0, 0, 0]);
    assert!(!shaped.glyphs().is_empty());
    assert!(
        shaped
            .glyphs()
            .iter()
            .any(|g| g.color_glyph != 0 && g.glyph != 0)
    );
    let (status, colored) = unsafe {
        let surface = cairo_image_surface_create(0, 96, 96);
        let cr = cairo_create(surface);
        cairo_set_source_rgba(cr, 1.0, 1.0, 1.0, 1.0);
        for glyph in shaped.glyphs() {
            cairo_set_scaled_font(cr, glyph.font);
            let glyph = CairoGlyph {
                index: glyph.glyph.into(),
                x: f64::from(glyph.x) + 8.0,
                y: f64::from(glyph.y) + 8.0,
            };
            cairo_show_glyphs(cr, &glyph, 1);
        }
        let status = cairo_status(cr);
        cairo_destroy(cr);
        cairo_surface_flush(surface);
        let colored = if status == 0 && cairo_surface_status(surface) == 0 {
            let bytes = slice::from_raw_parts(
                cairo_image_surface_get_data(surface),
                cairo_image_surface_get_stride(surface) as usize * 96,
            );
            bytes
                .chunks_exact(4)
                .filter(|bytes| {
                    let pixel = u32::from_ne_bytes((*bytes).try_into().unwrap());
                    let (a, r, g, b) = (
                        pixel >> 24,
                        pixel >> 16 & 255,
                        pixel >> 8 & 255,
                        pixel & 255,
                    );
                    a != 0 && (r != g || g != b)
                })
                .count()
        } else {
            0
        };
        cairo_surface_destroy(surface);
        (status, colored)
    };
    assert_eq!(status, 0);
    assert!(colored > 0, "COLRv1 glyph must rasterize colored pixels");
}

#[test]
fn ascii_numbers_and_brackets_keep_ltr_columns_after_arabic_shaping() {
    let mut fonts = Fonts::new(c"Nextty Test Arabic");
    let metric = fonts.shape("mm", &[], &[0, 1]);
    let width = metric.glyphs()[1].x - metric.glyphs()[0].x;
    for text in [
        "12345",
        "(12 + 34) / 99.00",
        "{m: 42} [m-7]",
        "m < 12 && 34 > m",
    ] {
        let columns: Vec<_> = (0..text.len()).map(|column| column as u16).collect();
        for map in [columns.as_slice(), &[]] {
            let _arabic = fonts.shape("بتب", &[], &[0, 0, 1, 1, 2, 2]);
            let shaped = fonts.shape(text, &[], map);
            let positions: Vec<_> = shaped.glyphs().iter().map(|glyph| glyph.x).collect();
            let expected: Vec<_> = text
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte != b' ')
                .map(|(column, _)| column as f32 * width)
                .collect();
            assert_eq!(positions, expected, "{text}");
        }
    }
}

#[test]
fn fallback_selection_survives_scale_changes() {
    let mut fonts = Fonts::new(c"Nextty Test Arabic");
    let initial = fonts.shape("ب", &[], &[0, 0]);
    let expected: Vec<_> = initial.glyphs().iter().map(|glyph| glyph.glyph).collect();
    assert!(!expected.is_empty() && expected.iter().all(|glyph| *glyph != 0));
    for scale in [2.0, 1.0] {
        assert_ne!(unsafe { nt_fonts_scale(fonts.raw, scale) }, 0);
        for _ in 0..2 {
            let shaped = fonts.shape("ب", &[], &[0, 0]);
            assert_eq!(
                shaped
                    .glyphs()
                    .iter()
                    .map(|glyph| glyph.glyph)
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }
}
