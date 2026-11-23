//! Direct2D/DirectWrite drawing into a 32-bit top-down DIB, presented with
//! `UpdateLayeredWindow`. Everything is drawn in logical px under a scale transform.

use std::cell::RefCell;
use std::sync::Arc;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{COLORREF, D2DERR_RECREATE_TARGET, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, UpdateLayeredWindowIndirect, ULW_ALPHA, UPDATELAYEREDWINDOWINFO};
use windows_numerics::{Matrix3x2, Vector2};

use super::layout::*;
use crate::model::{Cover, DeviceKind, Part, PartKind};

/// One device slot, already animated: `e` is presence (fade/slide in and out),
/// `pct_t` is the weight of the current percent text in the old -> new crossfade.
pub struct DevDraw {
    pub kind: DeviceKind,
    pub pct: u8,
    pub prev_pct: u8,
    pub pct_t: f32,
    pub charging: bool,
    /// Bolt fade/scale weight (0 = hidden, 1 = shown).
    pub chg: f32,
    pub text_w: f32,
    pub item_w: f32,
    pub e: f32,
}

/// One expanded row for a device that reports earbud parts.
pub struct PartsRow {
    pub kind: DeviceKind,
    pub name: String,
    pub parts: Vec<Part>,
}

/// Which control is being pressed (scaled to `press_scale`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ctl {
    Cover,
    Play1,
    Prev,
    Play2,
    Next,
}

pub struct Frame {
    pub s: f32,
    pub pill: Rect,
    pub radius: f32,
    pub row1: f32,
    pub block: f32,
    pub vis: f32,
    pub vis_scale: f32,
    pub content: f32,
    pub has_media: bool,
    pub playing_w: f32, // weight of the *pause* shape (1 = playing)
    pub can_prev: bool,
    pub can_next: bool,
    pub cover: Option<Arc<Cover>>,
    pub prev_cover: Option<Arc<Cover>>,
    pub cover_t: f32,
    pub title: String,
    pub prev_title: String,
    pub artist: String,
    pub prev_artist: String,
    pub text_t: f32,
    pub nothing: &'static str,
    pub devs: Vec<DevDraw>,
    pub extra: Option<String>,
    pub rows: Vec<PartsRow>,
    pub rows_f: f32,
    pub press: Option<(Ctl, f32)>,
    pub progress: Option<(f32, String, String)>,
}

/// Text layout reused until its string, format or box size changes.
struct CachedLayout {
    fmt: usize,
    text: String,
    size: (f32, f32),
    layout: IDWriteTextLayout,
}

struct CoverBmp {
    src: Arc<Cover>,
    #[allow(dead_code)] // kept alive: the brush references it
    bmp: ID2D1Bitmap,
    brush: ID2D1BitmapBrush,
    w: f32,
    h: f32,
}

/// Dark radial gradient with a faint diagonal sheen, as premultiplied BGRA (opaque).
fn placeholder_cover() -> Arc<Cover> {
    const N: u32 = 104;
    let mut bgra = Vec::with_capacity((N * N * 4) as usize);
    for y in 0..N {
        for x in 0..N {
            let (u, v) = (x as f32 / N as f32, y as f32 / N as f32);
            let d = (((u - 0.38).powi(2) + (v - 0.35).powi(2)).sqrt() / 0.88).min(1.0);
            let base = 0x4A as f32 + (0x1A as f32 - 0x4A as f32) * d;
            let sheen = (1.0 - ((u + v - 1.0).abs() * 2.5).min(1.0)) * 0.10;
            let c = (base + (255.0 - base) * sheen).round() as u8;
            bgra.extend_from_slice(&[c, c, c.saturating_add(2), 255]);
        }
    }
    Arc::new(Cover { width: N, height: N, bgra })
}

pub struct Renderer {
    #[allow(dead_code)] // factories must outlive the render target
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    rt: ID2D1RenderTarget,
    wic: IWICImagingFactory,
    bmp: IWICBitmap,
    bits: *mut u8,
    brush: ID2D1SolidColorBrush,
    stroke: ID2D1StrokeStyle,
    tri: ID2D1PathGeometry,
    bolt: ID2D1PathGeometry,
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    w: i32,
    h: i32,
    pct: IDWriteTextFormat,
    pct_trim: IDWriteTextFormat,
    title: IDWriteTextFormat,
    artist: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_r: IDWriteTextFormat,
    ico24: IDWriteTextFormat,
    ico20: IDWriteTextFormat,
    cur: Option<CoverBmp>,
    prev: Option<CoverBmp>,
    ph: Option<CoverBmp>,
    /// Region touched by the previous frame; DC copies and clears are limited to prev + current.
    dirty: Option<RECT>,
    /// Region of the DIB changed by the latest `draw` and the window position it was last shown at.
    changed: RECT,
    shown_at: Option<(i32, i32)>,
    layouts: RefCell<Vec<CachedLayout>>,
    ga: f32,
    base: Matrix3x2,
    group: Matrix3x2,
}

const GREEN: u32 = 0x30D158;

fn ident() -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 }
}

fn scale_about(sc: f32, cx: f32, cy: f32) -> Matrix3x2 {
    Matrix3x2 { M11: sc, M12: 0.0, M21: 0.0, M22: sc, M31: cx * (1.0 - sc), M32: cy * (1.0 - sc) }
}

/// Apply `a` first, then `b` (row-vector convention).
fn mul(a: &Matrix3x2, b: &Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

fn col(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((hex >> 16) & 255) as f32 / 255.0, g: ((hex >> 8) & 255) as f32 / 255.0, b: (hex & 255) as f32 / 255.0, a }
}

fn rf(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: r.l, top: r.t, right: r.r, bottom: r.b }
}

fn rr(r: Rect, rad: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rf(r), radiusX: rad, radiusY: rad }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn font_exists(dw: &IDWriteFactory, name: PCWSTR) -> bool {
    // SAFETY: plain DirectWrite queries with valid out pointers.
    unsafe {
        let mut coll = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_err() {
            return false;
        }
        let Some(coll) = coll else { return false };
        let (mut idx, mut exists) = (0u32, false.into());
        coll.FindFamilyName(name, &mut idx, &mut exists).is_ok() && exists.as_bool()
    }
}

fn format(dw: &IDWriteFactory, family: PCWSTR, size: f32, weight: i32) -> windows::core::Result<IDWriteTextFormat> {
    // SAFETY: arguments are valid for the duration of the call.
    unsafe {
        let f = dw.CreateTextFormat(
            family,
            None,
            DWRITE_FONT_WEIGHT(weight),
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("en-us"),
        )?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        Ok(f)
    }
}

impl Renderer {
    pub fn new(w_px: i32, h_px: i32) -> windows::core::Result<Renderer> {
        // SAFETY: straightforward COM object creation; all pointers are to live locals.
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            // Render straight into a WIC bitmap (no per-frame DC round trip); dirty rows are
            // copied into the layered-window DIB afterwards.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let (bmp, rt, brush) = Self::target(&d2d, &wic, &props, w_px, h_px)?;
            let sp = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 1.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            };
            let stroke = d2d.CreateStrokeStyle(&sp, None)?;
            // Play triangle, 17 x 20 at the origin; the stroke rounds its corners.
            let tri = d2d.CreatePathGeometry()?;
            let sink = tri.Open()?;
            sink.BeginFigure(Vector2 { X: 1.5, Y: 1.5 }, D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(Vector2 { X: 1.5, Y: 18.5 });
            sink.AddLine(Vector2 { X: 15.5, Y: 10.0 });
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;

            // Lightning bolt, 9 x 15 at the origin.
            let bolt = d2d.CreatePathGeometry()?;
            let sink = bolt.Open()?;
            let pts = [(6.5, 0.0), (0.5, 8.5), (4.3, 8.5), (2.8, 15.0), (8.8, 6.0), (4.9, 6.0), (8.0, 0.0)];
            sink.BeginFigure(Vector2 { X: pts[0].0, Y: pts[0].1 }, D2D1_FIGURE_BEGIN_FILLED);
            for p in &pts[1..] {
                sink.AddLine(Vector2 { X: p.0, Y: p.1 });
            }
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;

            let ui = if font_exists(&dw, w!("Segoe UI Variable Display")) { w!("Segoe UI Variable Display") } else { w!("Segoe UI") };
            let ic = if font_exists(&dw, w!("Segoe Fluent Icons")) { w!("Segoe Fluent Icons") } else { w!("Segoe MDL2 Assets") };
            let title = format(&dw, ui, 15.0, 600)?;
            let sign = dw.CreateEllipsisTrimmingSign(&title)?;
            let trim = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
            title.SetTrimming(&trim, &sign)?;
            let pct_trim = format(&dw, ui, 18.0, 500)?;
            pct_trim.SetTrimming(&trim, &sign)?;
            let artist = format(&dw, ui, 13.0, 400)?;
            artist.SetTrimming(&trim, &sign)?;
            let small = format(&dw, ui, 11.0, 400)?;
            let small_r = format(&dw, ui, 11.0, 400)?;
            small_r.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
            let icon = |sz| -> windows::core::Result<IDWriteTextFormat> {
                let f = format(&dw, ic, sz, 400)?;
                f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                Ok(f)
            };
            let mut r = Renderer {
                pct: format(&dw, ui, 18.0, 500)?,
                pct_trim,
                title,
                artist,
                small,
                small_r,
                ico24: icon(28.0)?,
                ico20: icon(20.0)?,
                d2d,
                dw,
                rt,
                wic,
                bmp,
                bits: std::ptr::null_mut(),
                brush,
                stroke,
                tri,
                bolt,
                dc: HDC::default(),
                dib: HBITMAP::default(),
                old: HGDIOBJ::default(),
                w: 0,
                h: 0,
                cur: None,
                prev: None,
                ph: None,
                dirty: None,
                changed: RECT::default(),
                shown_at: None,
                layouts: RefCell::new(Vec::new()),
                ga: 1.0,
                base: ident(),
                group: ident(),
            };
            r.dc = CreateCompatibleDC(None);
            r.ph = r.cover_bmp(&placeholder_cover());
            r.make_dib(w_px, h_px)?;
            Ok(r)
        }
    }

    fn target(
        d2d: &ID2D1Factory,
        wic: &IWICImagingFactory,
        props: &D2D1_RENDER_TARGET_PROPERTIES,
        w: i32,
        h: i32,
    ) -> windows::core::Result<(IWICBitmap, ID2D1RenderTarget, ID2D1SolidColorBrush)> {
        // SAFETY: plain COM creation with valid arguments.
        unsafe {
            let bmp = wic.CreateBitmap(w as u32, h as u32, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand)?;
            let rt = d2d.CreateWicBitmapRenderTarget(&bmp, props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let brush = rt.CreateSolidColorBrush(&col(0xFFFFFF, 1.0), None)?;
            Ok((bmp, rt, brush))
        }
    }

    /// Rebuild bitmap, render target and DIB. Only on DPI / size change, never during animation.
    pub fn resize(&mut self, w: i32, h: i32) -> windows::core::Result<()> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let (bmp, rt, brush) = Self::target(&self.d2d, &self.wic, &props, w, h)?;
        self.bmp = bmp;
        self.rt = rt;
        self.brush = brush;
        self.cur = None;
        self.prev = None;
        self.ph = self.cover_bmp(&placeholder_cover());
        self.make_dib(w, h)
    }

    fn make_dib(&mut self, w: i32, h: i32) -> windows::core::Result<()> {
        // SAFETY: the DIB is selected out of the DC before deletion.
        unsafe {
            if !self.dib.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.dib.into());
                self.dib = HBITMAP::default();
                self.bits = std::ptr::null_mut();
                self.w = 0;
                self.h = 0;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            self.dib = CreateDIBSection(Some(self.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            self.old = SelectObject(self.dc, self.dib.into());
            self.bits = bits as *mut u8;
            self.w = w;
            self.h = h;
            self.dirty = None;
            self.shown_at = None;
            // Bitmaps belong to the render target, which survives; only the DIB changed.
            Ok(())
        }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    pub fn measure(&self, s: &str) -> f32 {
        self.measure_with(s, &self.pct)
    }

    fn measure_with(&self, s: &str, fmt: &IDWriteTextFormat) -> f32 {
        // SAFETY: layout is created and read within this call.
        unsafe {
            let t = wide(s);
            let Ok(l) = self.dw.CreateTextLayout(&t, fmt, 1000.0, 100.0) else { return 40.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            if l.GetMetrics(&mut m).is_err() {
                return 40.0;
            }
            m.widthIncludingTrailingWhitespace.ceil()
        }
    }

    // ---- primitives (all `&self`: COM objects are interior-mutable) ----

    fn set_xf(&self, local: Option<(f32, f32, f32)>) {
        let m = match local {
            Some((sc, cx, cy)) => mul(&mul(&scale_about(sc, cx, cy), &self.group), &self.base),
            None => mul(&self.group, &self.base),
        };
        // SAFETY: matrix pointer is valid for the call.
        unsafe { self.rt.SetTransform(&m) };
    }

    fn color(&self, hex: u32, a: f32) -> &ID2D1SolidColorBrush {
        // SAFETY: valid color pointer.
        unsafe { self.brush.SetColor(&col(hex, (a * self.ga).clamp(0.0, 1.0))) };
        &self.brush
    }

    fn fill(&self, r: Rect, rad: f32, hex: u32, a: f32) {
        if a <= 0.001 {
            return;
        }
        // SAFETY: valid pointers for the call.
        unsafe { self.rt.FillRoundedRectangle(&rr(r, rad), self.color(hex, a)) };
    }

    fn stroke_rr(&self, r: Rect, rad: f32, hex: u32, a: f32) {
        // SAFETY: valid pointers for the call.
        unsafe { self.rt.DrawRoundedRectangle(&rr(r, rad), self.color(hex, a), 1.0, None) };
    }

    fn text(&self, s: &str, f: &IDWriteTextFormat, r: Rect, hex: u32, a: f32) {
        if a <= 0.001 || s.is_empty() {
            return;
        }
        let key = f.as_raw() as usize;
        let size = (r.w(), r.b - r.t);
        let mut cache = self.layouts.borrow_mut();
        let idx = match cache.iter().position(|c| c.fmt == key && c.size == size && c.text == s) {
            Some(i) => i,
            None => {
                let t = wide(s);
                // SAFETY: valid arguments; the layout keeps its own copy of the string.
                let Ok(layout) = (unsafe { self.dw.CreateTextLayout(&t, f, size.0, size.1) }) else { return };
                if cache.len() >= 48 {
                    cache.clear();
                }
                cache.push(CachedLayout { fmt: key, text: s.to_string(), size, layout });
                cache.len() - 1
            }
        };
        // SAFETY: valid pointers for the call.
        unsafe { self.rt.DrawTextLayout(Vector2 { X: r.l, Y: r.t }, &cache[idx].layout, self.color(hex, a), D2D1_DRAW_TEXT_OPTIONS_CLIP) };
    }

    fn glyph(&self, f: &IDWriteTextFormat, ch: char, cx: f32, cy: f32, hex: u32, a: f32) {
        let mut b = [0u16; 2];
        let s = ch.encode_utf16(&mut b);
        let r = Rect::around(cx, cy, 48.0, 48.0);
        // SAFETY: valid pointers for the call.
        unsafe { self.rt.DrawText(s, f, &rf(r), self.color(hex, a), D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL) };
    }

    /// Pause bars (weight `pause`) crossfaded with the play triangle (1 - pause), height `h`.
    fn play_pause(&self, cx: f32, cy: f32, h: f32, pause: f32, a: f32, press: f32) {
        let hex = 0xF2F2F2;
        let k = h / 20.0;
        if pause > 0.01 {
            let g = 0.8 + 0.2 * pause;
            self.set_xf(Some((press * g, cx, cy)));
            let bw = 6.0 * k;
            let gap = 5.0 * k;
            self.fill(Rect::new(cx - gap / 2.0 - bw, cy - h / 2.0, bw, h), 1.5 * k, hex, a * pause);
            self.fill(Rect::new(cx + gap / 2.0, cy - h / 2.0, bw, h), 1.5 * k, hex, a * pause);
        }
        let play = 1.0 - pause;
        if play > 0.01 {
            let g = 0.8 + 0.2 * play;
            // Triangle geometry is 17 x 20 at the origin; center it on (cx, cy).
            let local = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: cx - 8.5 * k, M32: cy - 10.0 * k };
            let m = mul(&mul(&mul(&local, &scale_about(press * g, cx, cy)), &self.group), &self.base);
            // SAFETY: valid pointers for the call.
            unsafe {
                self.rt.SetTransform(&m);
                let b = self.color(hex, a * play);
                self.rt.FillGeometry(&self.tri, b, None::<&ID2D1Brush>);
                if a * play > 0.95 {
                    self.rt.DrawGeometry(&self.tri, b, 3.0, &self.stroke);
                }
            }
        }
        self.set_xf(None);
    }

    /// Charging bolt centred on (cx, cy); `w` fades and scales it in (0.6 -> 1).
    fn bolt(&self, cx: f32, cy: f32, size: f32, w: f32, hex: u32) {
        if w <= 0.01 {
            return;
        }
        let k = size * (0.6 + 0.4 * w);
        let local = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: cx - BOLT_GLYPH_W / 2.0 * k, M32: cy - BOLT_GLYPH_H / 2.0 * k };
        let m = mul(&local, &mul(&self.group, &self.base));
        // SAFETY: valid pointers for the call.
        unsafe {
            self.rt.SetTransform(&m);
            self.rt.FillGeometry(&self.bolt, self.color(hex, w), None::<&ID2D1Brush>);
        }
        self.set_xf(None);
    }

    fn cover_bmp(&mut self, c: &Arc<Cover>) -> Option<CoverBmp> {
        // SAFETY: the pixel buffer outlives the call (D2D copies it).
        unsafe {
            let p = D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
            };
            let bmp = self
                .rt
                .CreateBitmap(D2D_SIZE_U { width: c.width, height: c.height }, Some(c.bgra.as_ptr() as *const _), c.width * 4, &p)
                .ok()?;
            let bp = D2D1_BITMAP_BRUSH_PROPERTIES {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            };
            let brush = self.rt.CreateBitmapBrush(&bmp, Some(&bp), None).ok()?;
            Some(CoverBmp { src: c.clone(), bmp, brush, w: c.width as f32, h: c.height as f32 })
        }
    }

    /// Keep the two cached bitmaps in step with the (cur, prev) cover Arcs.
    fn sync_covers(&mut self, cur: &Option<Arc<Cover>>, prev: &Option<Arc<Cover>>) {
        // Compare by Arc identity; the cache holds the Arc, so an address can't be reused.
        let same = |b: &Option<CoverBmp>, c: &Option<Arc<Cover>>| match (b, c) {
            (Some(b), Some(c)) => Arc::ptr_eq(&b.src, c),
            (None, None) => true,
            _ => false,
        };
        if !same(&self.cur, cur) {
            // The old "current" is usually the new "previous": reuse it instead of re-uploading.
            if prev.is_some() && same(&self.cur, prev) {
                self.prev = self.cur.take();
            }
            self.cur = cur.as_ref().and_then(|c| self.cover_bmp(c));
        }
        if !same(&self.prev, prev) {
            self.prev = prev.as_ref().and_then(|c| self.cover_bmp(c));
        }
    }

    fn draw_cover_bmp(&self, b: &CoverBmp, r: Rect, a: f32) {
        let sc = (r.w() / b.w).max(r.w() / b.h);
        let m =
            Matrix3x2 { M11: sc, M12: 0.0, M21: 0.0, M22: sc, M31: r.l + (r.w() - b.w * sc) / 2.0, M32: r.t + (r.w() - b.h * sc) / 2.0 };
        // SAFETY: valid pointers for the call.
        unsafe {
            b.brush.SetTransform(&m);
            b.brush.SetOpacity((a * self.ga).clamp(0.0, 1.0));
            self.rt.FillRoundedRectangle(&rr(r, 10.0), &b.brush);
        }
    }

    // ---- scene ----

    pub fn draw(&mut self, f: &Frame) -> bool {
        self.sync_covers(&f.cover, &f.prev_cover);
        // Physical-px bounding box of this frame (pill + glow), merged with the last one.
        let m = GLOW + 2.0;
        let cur = RECT {
            left: (((f.pill.l - m) * f.s).floor() as i32).clamp(0, self.w),
            top: (((f.pill.t - m) * f.s).floor() as i32).clamp(0, self.h),
            right: (((f.pill.r + m) * f.s).ceil() as i32).clamp(0, self.w),
            bottom: (((f.pill.b + m) * f.s).ceil() as i32).clamp(0, self.h),
        };
        let rc = match self.dirty {
            Some(o) => RECT {
                left: o.left.min(cur.left),
                top: o.top.min(cur.top),
                right: o.right.max(cur.right),
                bottom: o.bottom.max(cur.bottom),
            },
            None => RECT { left: 0, top: 0, right: self.w, bottom: self.h },
        };
        self.dirty = Some(cur);
        self.base = Matrix3x2 { M11: f.s, M12: 0.0, M21: 0.0, M22: f.s, M31: 0.0, M32: 0.0 };
        let pc = ((f.pill.l + f.pill.r) / 2.0, (f.pill.t + f.pill.b) / 2.0);
        self.group = scale_about(f.vis_scale, pc.0, pc.1);
        self.ga = f.vis.clamp(0.0, 1.0);
        let g = geo(f.pill.l, f.pill.w(), f.row1, f.block);
        // SAFETY: the DC holds our DIB for the whole BeginDraw..EndDraw span.
        unsafe {
            self.clear_rows(&rc);
            self.changed = rc;
            self.rt.BeginDraw();
            self.set_xf(None);
            // Glow: a few wide strokes with a quadratic falloff.
            const BANDS: i32 = 3;
            let bw = GLOW / BANDS as f32;
            for i in 0..BANDS {
                let t = (i as f32 + 0.5) / BANDS as f32;
                let grow = (i as f32 + 0.5) * bw;
                let r = Rect { l: f.pill.l - grow, t: f.pill.t - grow, r: f.pill.r + grow, b: f.pill.b + grow };
                self.rt.DrawRoundedRectangle(&rr(r, f.radius + grow), self.color(0xFFFFFF, 0.06 * (1.0 - t) * (1.0 - t)), bw, None);
            }
            self.fill(f.pill, f.radius, 0x000000, 1.0);
            self.stroke_rr(
                Rect { l: f.pill.l + 0.5, t: f.pill.t + 0.5, r: f.pill.r - 0.5, b: f.pill.b - 0.5 },
                f.radius - 0.5,
                0xFFFFFF,
                0.06,
            );

            self.rt.PushAxisAlignedClip(&rf(f.pill), D2D1_ANTIALIAS_MODE_ALIASED);
            self.row1(f, &g);
            if f.content > 0.01 {
                self.block(f, &g);
                self.parts_rows(f);
            }
            self.rt.PopAxisAlignedClip();
            self.set_xf(None);

            let res = self.rt.EndDraw(None, None);
            self.copy_rows(&rc);
            if let Err(e) = res {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.dirty = None;
                    return false;
                }
            }
        }
        true
    }

    fn press_of(f: &Frame, c: Ctl) -> f32 {
        match f.press {
            Some((p, s)) if p == c => s,
            _ => 1.0,
        }
    }

    fn row1(&self, f: &Frame, g: &Geo) {
        // Cover (crossfade old -> new on track change).
        let cp = Self::press_of(f, Ctl::Cover);
        self.set_xf(Some((cp, g.cover.l + 26.0, g.cover.t + 26.0)));
        let ph = self.ph.as_ref();
        match (&self.cur, f.has_media) {
            (Some(c), true) => {
                if f.cover_t < 1.0 {
                    if let Some(p) = self.prev.as_ref().or(ph) {
                        self.draw_cover_bmp(p, g.cover, 1.0);
                    }
                }
                self.draw_cover_bmp(c, g.cover, f.cover_t.clamp(0.0, 1.0));
            }
            _ => {
                if let Some(p) = ph {
                    self.draw_cover_bmp(p, g.cover, 1.0);
                }
            }
        }
        // Play / pause.
        let pp = Self::press_of(f, Ctl::Play1);
        let a = if f.has_media { 1.0 } else { 0.30 };
        // Empty state: the play glyph (not pause), dimmed.
        let pause_w = if f.has_media { f.playing_w } else { 0.0 };
        self.play_pause(g.pp1.0, g.pp1.1, 20.0, pause_w, a, pp);

        let cy = f.row1 + PILL_H / 2.0;
        // Right section without devices: the current track (or the empty state).
        let dev_presence = f.devs.iter().map(|d| d.e).fold(0.0f32, f32::max);
        let ta = (1.0 - dev_presence).clamp(0.0, 1.0) * (1.0 - f.content).clamp(0.0, 1.0);
        if ta > 0.01 {
            let (l, r) = (f.pill.l + TEXT_L, f.pill.r - PAD);
            if f.has_media {
                let tr = Rect { l, t: cy - 21.0, r, b: cy + 1.0 };
                let ar = Rect { l, t: cy + 1.0, r, b: cy + 19.0 };
                if f.text_t < 1.0 {
                    self.text(&f.prev_title, &self.pct_trim, tr, 0xD1D1D6, ta * (1.0 - f.text_t));
                    self.text(&f.prev_artist, &self.artist, ar, 0x8E8E93, ta * (1.0 - f.text_t));
                }
                self.text(&f.title, &self.pct_trim, tr, 0xD1D1D6, ta * f.text_t);
                self.text(&f.artist, &self.artist, ar, 0x8E8E93, ta * f.text_t);
            } else {
                self.text(f.nothing, &self.pct_trim, Rect { l, t: cy - 14.0, r, b: cy + 14.0 }, 0x8E8E93, ta);
            }
        }
        if f.devs.is_empty() {
            return;
        }
        // Devices, right-anchored; item presence `e` drives both fade and slide.
        let mut cursor = f.pill.r - PAD;
        let mut presence = 0.0f32;
        let mut last_e = 0.0;
        for d in f.devs.iter().rev() {
            let x = cursor - d.item_w;
            let a = d.e.clamp(0.0, 1.0);
            self.glyph(&self.ico24, icon_char(d.kind), x + ICON_W / 2.0, cy, 0x8E8E93, a);
            let tx = x + ICON_W + ICON_TEXT_GAP;
            let tr = Rect { l: tx, t: cy - 14.0, r: tx + d.text_w + 6.0, b: cy + 14.0 };
            // Green while charging; the low-battery red only applies when not charging.
            let pick = |p: u8| {
                if d.charging {
                    GREEN
                } else if p <= 15 {
                    0xFF453A
                } else {
                    0xD1D1D6
                }
            };
            let color = pick(d.pct);
            let pcolor = pick(d.prev_pct);
            if d.pct_t < 1.0 && d.prev_pct != d.pct {
                self.text(&format!("{}%", d.prev_pct), &self.pct, tr, pcolor, a * (1.0 - d.pct_t));
            }
            self.text(&format!("{}%", d.pct), &self.pct, tr, color, a * if d.prev_pct != d.pct { d.pct_t } else { 1.0 });
            if d.chg > 0.01 {
                self.bolt(tx + d.text_w + 3.0 + BOLT_GLYPH_W / 2.0, cy, 1.0, d.chg, GREEN);
            }
            cursor -= (d.item_w + DEV_GAP) * d.e;
            presence = presence.max(d.e);
            last_e = d.e;
        }
        let left = cursor + DEV_GAP * last_e;
        let dx = (left - DIV_GAP).floor() + 0.5;
        self.fill(Rect::around(dx, cy, 1.0, 36.0), 0.0, 0xFFFFFF, 0.10 * presence.clamp(0.0, 1.0));
    }

    fn block(&self, f: &Frame, g: &Geo) {
        let c = f.content.clamp(0.0, 1.0);
        let dy = 6.0 * (1.0 - c);
        let off = |r: Rect| Rect { l: r.l, t: r.t + dy, r: r.r, b: r.b + dy };
        let dim = |on: bool| if on { 1.0 } else { 0.3 };
        // Title / artist (crossfade on track change), or the empty-state / extra-device line.
        if f.has_media {
            if f.text_t < 1.0 {
                self.text(&f.prev_title, &self.title, off(g.title), 0xF2F2F2, c * (1.0 - f.text_t));
                self.text(&f.prev_artist, &self.artist, off(g.artist), 0x8E8E93, c * (1.0 - f.text_t));
            }
            self.text(&f.title, &self.title, off(g.title), 0xF2F2F2, c * f.text_t);
            self.text(&f.artist, &self.artist, off(g.artist), 0x8E8E93, c * f.text_t);
        } else {
            match &f.extra {
                Some(e) => self.text(e, &self.artist, off(g.title), 0x8E8E93, c),
                None => self.text(f.nothing, &self.title, off(g.title), 0x8E8E93, c),
            }
        }
        if !f.has_media {
            return;
        }
        // Transport.
        let yo = |p: (f32, f32)| (p.0, p.1 + dy);
        let (px, py) = yo(g.prev);
        self.set_xf(Some((Self::press_of(f, Ctl::Prev), px, py)));
        self.glyph(&self.ico20, '\u{E892}', px, py, 0xF2F2F2, c * dim(f.can_prev));
        let (qx, qy) = yo(g.pp2);
        self.play_pause(qx, qy, 18.0, f.playing_w, c, Self::press_of(f, Ctl::Play2));
        let (nx, ny) = yo(g.next);
        self.set_xf(Some((Self::press_of(f, Ctl::Next), nx, ny)));
        self.glyph(&self.ico20, '\u{E893}', nx, ny, 0xF2F2F2, c * dim(f.can_next));
        self.set_xf(None);
        // Progress.
        if let Some((frac, el, rem)) = &f.progress {
            self.text(el, &self.small, off(g.elapsed), 0x8E8E93, c);
            self.text(rem, &self.small_r, off(g.remaining), 0x8E8E93, c);
            let bar = off(g.bar);
            self.fill(bar, 1.5, 0xFFFFFF, 0.15 * c);
            let fw = (bar.w() * frac.clamp(0.0, 1.0)).max(0.0);
            if fw > 0.5 {
                self.fill(Rect { r: bar.l + fw.max(3.0), ..bar }, 1.5, 0xF2F2F2, c);
            }
        }
    }

    /// Earbud rows below the progress bar: icon + name, then `L 80%  R 75%  [case] 40%`.
    fn parts_rows(&self, f: &Frame) {
        let c = f.content.clamp(0.0, 1.0);
        let dy = 6.0 * (1.0 - c);
        let right = f.pill.r - PAD;
        // Only the band the pill has opened for: rows must not paint over the compact row
        // while they animate in (matters when opening upwards).
        let band = D2D_RECT_F { left: f.pill.l, top: f.block + 84.0, right: f.pill.r, bottom: f.block + 84.0 + PART_ROW_H * f.rows_f };
        // SAFETY: balanced with the pop below.
        unsafe { self.rt.PushAxisAlignedClip(&band, D2D1_ANTIALIAS_MODE_ALIASED) };
        for (i, row) in f.rows.iter().enumerate() {
            let a = c * (f.rows_f - i as f32).clamp(0.0, 1.0);
            if a <= 0.01 {
                continue;
            }
            let cy = f.block + 84.0 + PART_ROW_H * (i as f32 + 0.5) + dy;
            // Slots are right-aligned; missing parts are simply omitted.
            const SLOT_W: f32 = 84.0;
            let mut x = right;
            for p in row.parts.iter().rev() {
                x -= SLOT_W;
                let color = if p.charging {
                    GREEN
                } else if p.percent <= 15 {
                    0xFF453A
                } else {
                    0xD1D1D6
                };
                match p.kind {
                    PartKind::Case => {
                        let r = Rect::new(x + 1.0, cy - 4.5, 12.0, 9.0);
                        // SAFETY: valid pointers for the call.
                        unsafe { self.rt.DrawRoundedRectangle(&rr(r, 2.5), self.color(0x8E8E93, a), 1.3, None) };
                    }
                    k => {
                        self.text(if k == PartKind::Left { "L" } else { "R" }, &self.small, Rect::new(x, cy - 9.0, 14.0, 18.0), 0x8E8E93, a)
                    }
                }
                let s = format!("{}%", p.percent);
                let tw = self.measure_with(&s, &self.artist);
                self.text(&s, &self.artist, Rect::new(x + 17.0, cy - 10.0, tw + 4.0, 20.0), color, a);
                if p.charging {
                    self.bolt(x + 17.0 + tw + 3.0 + BOLT_GLYPH_W * 0.4, cy, 0.8, 1.0, GREEN);
                }
            }
            self.glyph(&self.ico20, icon_char(row.kind), f.pill.l + PAD + 10.0, cy, 0x8E8E93, a);
            self.text(
                &row.name,
                &self.artist,
                Rect { l: f.pill.l + PAD + 28.0, t: cy - 10.0, r: (x - 8.0).max(f.pill.l + PAD + 40.0), b: cy + 10.0 },
                0x8E8E93,
                a,
            );
        }
        // SAFETY: matches the push above.
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// Zero the dirty rectangle of the WIC bitmap (transparent black).
    fn clear_rows(&self, rc: &RECT) {
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w <= 0 || h <= 0 {
            return;
        }
        let r = WICRect { X: rc.left, Y: rc.top, Width: w, Height: h };
        // SAFETY: the write lock covers exactly the rows touched below.
        unsafe {
            let Ok(lock) = self.bmp.Lock(&r, WICBitmapLockWrite.0 as u32) else { return };
            let Ok(stride) = lock.GetStride() else { return };
            let (mut len, mut dst) = (0u32, std::ptr::null_mut());
            if lock.GetDataPointer(&mut len, &mut dst).is_err() {
                return;
            }
            for row in 0..h as usize {
                std::ptr::write_bytes(dst.add(row * stride as usize), 0, w as usize * 4);
            }
        }
    }

    /// Copy the dirty rectangle from the WIC bitmap into the DIB the window is presented from.
    fn copy_rows(&self, rc: &RECT) {
        let (x, y) = (rc.left, rc.top);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w <= 0 || h <= 0 || self.bits.is_null() {
            return;
        }
        let r = WICRect { X: x, Y: y, Width: w, Height: h };
        // SAFETY: the lock is held while reading; source and destination rows are in bounds.
        unsafe {
            let Ok(lock) = self.bmp.Lock(&r, WICBitmapLockRead.0 as u32) else { return };
            let Ok(stride) = lock.GetStride() else { return };
            let (mut len, mut src) = (0u32, std::ptr::null_mut());
            if lock.GetDataPointer(&mut len, &mut src).is_err() {
                return;
            }
            let dst_stride = self.w as usize * 4;
            for row in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(row * stride as usize),
                    self.bits.add((y as usize + row) * dst_stride + x as usize * 4),
                    w as usize * 4,
                );
            }
        }
    }

    /// Push the DIB to the layered window at screen position (x, y).
    pub fn present(&mut self, hwnd: HWND, x: i32, y: i32) {
        let pt = POINT { x, y };
        let sz = SIZE { cx: self.w, cy: self.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        // SAFETY: all pointers are to live locals; the DC holds our DIB.
        unsafe {
            if self.shown_at == Some((x, y)) {
                // Same position: let the system copy only what changed.
                let info = UPDATELAYEREDWINDOWINFO {
                    cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                    hdcDst: HDC::default(),
                    pptDst: &pt,
                    psize: &sz,
                    hdcSrc: self.dc,
                    pptSrc: &src,
                    crKey: COLORREF(0),
                    pblend: &blend,
                    dwFlags: ULW_ALPHA,
                    prcDirty: &self.changed,
                };
                if UpdateLayeredWindowIndirect(hwnd, &info).as_bool() {
                    return;
                }
            }
            let _ = UpdateLayeredWindow(hwnd, None, Some(&pt), Some(&sz), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
            self.shown_at = Some((x, y));
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: releasing GDI objects we created.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.dib.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn icon_char(k: DeviceKind) -> char {
    match k {
        DeviceKind::Headphones | DeviceKind::Headset => '\u{E7F6}',
        DeviceKind::Mouse => '\u{E962}',
        DeviceKind::Keyboard => '\u{E765}',
        DeviceKind::Gamepad => '\u{E7FC}',
        DeviceKind::Other => '\u{E702}',
    }
}
