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
use super::liquid::{self, Edge, Shape};
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
    Back,
    Fwd,
}

/// Waveform state: bar heights 0..1 and the (cover-derived) base colour; `None` = neutral grey.
pub struct WaveDraw {
    pub h: [f32; BARS],
    pub rgb: Option<[u8; 3]>,
}

/// The pill emerging from a screen edge (see `liquid`): everything in window-logical px.
pub struct Liquid {
    pub edge: Edge,
    /// The edge line and the rest rect's centre.
    pub line: f32,
    pub centre: (f32, f32),
    pub sh: Shape,
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
    pub can_seek: bool,
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
    /// Left edge of the right-hand text section (it moves right to make room for the waveform).
    pub text_l: f32,
    pub wave: Option<WaveDraw>,
    /// `Some` while the pill is on its way in or out of an edge (replaces `vis` / `vis_scale`).
    pub liquid: Option<Liquid>,
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
    Arc::new(Cover { width: N, height: N, bgra, accent: None })
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
    tiny: IDWriteTextFormat,
    /// Circular skip arrow (forward orientation, radius 9, centred on the origin): open arc + filled head.
    skip_arc: ID2D1PathGeometry,
    skip_head: ID2D1PathGeometry,
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

fn merge(a: RECT, b: RECT) -> RECT {
    let empty = |r: &RECT| r.right <= r.left || r.bottom <= r.top;
    if empty(&a) {
        b
    } else if empty(&b) {
        a
    } else {
        RECT { left: a.left.min(b.left), top: a.top.min(b.top), right: a.right.max(b.right), bottom: a.bottom.max(b.bottom) }
    }
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

            // Skip arrow: 300 degree clockwise arc from -40 to -100 degrees, head past the end.
            let skip_arc = d2d.CreatePathGeometry()?;
            let sink = skip_arc.Open()?;
            sink.BeginFigure(Vector2 { X: 6.894, Y: -5.785 }, D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: Vector2 { X: -1.563, Y: -8.863 },
                size: D2D_SIZE_F { width: 9.0, height: 9.0 },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_LARGE,
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            sink.Close()?;
            let skip_head = d2d.CreatePathGeometry()?;
            let sink = skip_head.Open()?;
            sink.BeginFigure(Vector2 { X: 2.6, Y: -9.6 }, D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLine(Vector2 { X: -1.1, Y: -5.9 });
            sink.AddLine(Vector2 { X: -2.1, Y: -11.9 });
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
            let tiny = format(&dw, ui, 8.5, 700)?;
            tiny.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            tiny.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
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
                tiny,
                skip_arc,
                skip_head,
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

    /// Circular skip arrow with "10" inside, centred on (cx, cy); `fwd` = clockwise (forward), else mirrored.
    fn skip10(&self, cx: f32, cy: f32, fwd: bool, press: f32, hex: u32, a: f32) {
        let local = Matrix3x2 { M11: if fwd { 1.0 } else { -1.0 }, M12: 0.0, M21: 0.0, M22: 1.0, M31: cx, M32: cy };
        let m = mul(&local, &mul(&scale_about(press, cx, cy), &mul(&self.group, &self.base)));
        // SAFETY: valid pointers for the call.
        unsafe {
            self.rt.SetTransform(&m);
            let b = self.color(hex, a);
            self.rt.DrawGeometry(&self.skip_arc, b, 1.6, &self.stroke);
            self.rt.FillGeometry(&self.skip_head, b, None::<&ID2D1Brush>);
        }
        self.set_xf(Some((press, cx, cy)));
        self.text("10", &self.tiny, Rect::around(cx, cy + 0.5, 16.0, 12.0), hex, a);
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
        let px = |r: Rect| RECT {
            left: ((r.l * f.s).floor() as i32).clamp(0, self.w),
            top: ((r.t * f.s).floor() as i32).clamp(0, self.h),
            right: ((r.r * f.s).ceil() as i32).clamp(0, self.w),
            bottom: ((r.b * f.s).ceil() as i32).clamp(0, self.h),
        };
        let cur = match &f.liquid {
            Some(l) => match l.sh.extent(m) {
                Some((n1, th)) => px(liquid::map_rect(l.edge, l.line, l.centre, (0.0, n1), th)),
                None => RECT::default(),
            },
            None => px(Rect { l: f.pill.l - m, t: f.pill.t - m, r: f.pill.r + m, b: f.pill.b + m }),
        };
        let rc = match self.dirty {
            Some(o) => merge(o, cur),
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
            match &f.liquid {
                Some(l) => self.liquid(f, l, &g),
                None => {
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
                    self.content(f, &g, f.pill);
                }
            }
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

    /// Everything inside the pill, clipped to `clip` (window-logical px, under the current group).
    fn content(&self, f: &Frame, g: &Geo, clip: Rect) {
        // SAFETY: balanced with the pop below.
        unsafe { self.rt.PushAxisAlignedClip(&rf(clip), D2D1_ANTIALIAS_MODE_ALIASED) };
        self.row1(f, g);
        if f.content > 0.01 {
            self.block(f, g);
            self.parts_rows(f);
        }
        // SAFETY: matches the push above.
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// Body + neck of the liquid pill as one outline, in edge axes (x = n, y = t).
    fn silhouette(&self, sh: &Shape) -> windows::core::Result<ID2D1Geometry> {
        // SAFETY: plain geometry construction; every sink is closed before its geometry is used.
        unsafe {
            let body = self.d2d.CreateRoundedRectangleGeometry(&D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F { left: sh.near, top: -sh.along / 2.0, right: sh.near + sh.depth, bottom: sh.along / 2.0 },
                radiusX: sh.r,
                radiusY: sh.r,
            })?;
            if sh.near <= 0.0 || sh.c < 0.5 {
                return body.cast();
            }
            // Neck: a band of width c from the edge line into the body, flared by a meniscus at the edge.
            let (h, rs) = (sh.c / 2.0, sh.rs);
            let end = (sh.near + sh.r).max(rs);
            let v = |x: f32, y: f32| Vector2 { X: x, Y: y };
            let q = |cx: f32, cy: f32, x: f32, y: f32| D2D1_QUADRATIC_BEZIER_SEGMENT { point1: v(cx, cy), point2: v(x, y) };
            let neck = self.d2d.CreatePathGeometry()?;
            let sink = neck.Open()?;
            sink.BeginFigure(v(0.0, -h - rs), D2D1_FIGURE_BEGIN_FILLED);
            sink.AddQuadraticBezier(&q(0.0, -h, rs, -h));
            sink.AddLine(v(end, -h));
            sink.AddLine(v(end, h));
            sink.AddLine(v(rs, h));
            sink.AddQuadraticBezier(&q(0.0, h, 0.0, h + rs));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
            let out = self.d2d.CreatePathGeometry()?;
            let sink = out.Open()?;
            body.CombineWithGeometry(&neck, D2D1_COMBINE_MODE_UNION, None, 0.1, &sink)?;
            sink.Close()?;
            out.cast()
        }
    }

    /// The pill on its way through an edge: clipped at the edge line, a body with a neck
    /// stretching back to the edge, and the content riding on (or clipped to) the body.
    fn liquid(&mut self, f: &Frame, l: &Liquid, g: &Geo) {
        let sh = &l.sh;
        let win = (self.w as f32 / f.s, self.h as f32 / f.s);
        self.group = ident();
        self.ga = 1.0;
        // SAFETY: balanced clip, valid pointers.
        unsafe {
            self.set_xf(None);
            self.rt.PushAxisAlignedClip(&rf(liquid::clip_rect(l.edge, l.line, win)), D2D1_ANTIALIAS_MODE_ALIASED);
            if sh.extent(0.0).is_some() {
                let m = liquid::edge_matrix(l.edge, l.line, l.centre);
                let xf = Matrix3x2 { M11: m[0], M12: m[1], M21: m[2], M22: m[3], M31: m[4], M32: m[5] };
                self.rt.SetTransform(&mul(&xf, &self.base));
                if sh.glow > 0.001 {
                    const BANDS: i32 = 3;
                    let bw = GLOW / BANDS as f32;
                    for i in 0..BANDS {
                        let t = (i as f32 + 0.5) / BANDS as f32;
                        let grow = (i as f32 + 0.5) * bw;
                        let rect = D2D_RECT_F {
                            left: sh.near - grow,
                            top: -sh.along / 2.0 - grow,
                            right: sh.near + sh.depth + grow,
                            bottom: sh.along / 2.0 + grow,
                        };
                        let rr = D2D1_ROUNDED_RECT { rect, radiusX: sh.r + grow, radiusY: sh.r + grow };
                        self.rt.DrawRoundedRectangle(&rr, self.color(0xFFFFFF, 0.06 * (1.0 - t) * (1.0 - t) * sh.glow), bw, None);
                    }
                }
                if let Ok(geom) = self.silhouette(sh) {
                    self.rt.FillGeometry(&geom, self.color(0x000000, 1.0), None::<&ID2D1Brush>);
                    self.rt.DrawGeometry(&geom, self.color(0xFFFFFF, 0.06), 1.0, None::<&ID2D1StrokeStyle>);
                }
                if sh.content > 0.01 {
                    let (sx, sy, dx, dy) = sh.content_xf(l.edge);
                    self.group = Matrix3x2 {
                        M11: sx,
                        M12: 0.0,
                        M21: 0.0,
                        M22: sy,
                        M31: l.centre.0 * (1.0 - sx) + dx,
                        M32: l.centre.1 * (1.0 - sy) + dy,
                    };
                    self.ga = sh.content;
                    self.set_xf(None);
                    let clip = if l.edge.vertical() {
                        let b = sh.body_rect(l.edge, l.line, l.centre);
                        Rect { l: f.pill.l.max(b.l), t: f.pill.t.max(b.t), r: f.pill.r.min(b.r), b: f.pill.b.min(b.b) }
                    } else {
                        f.pill
                    };
                    self.content(f, g, clip);
                }
            }
            self.set_xf(None);
            self.rt.PopAxisAlignedClip();
        }
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
        if let Some(w) = &f.wave {
            self.bars(f.pill.l, cy, w);
        }
        // Right section without devices: the current track (or the empty state).
        let dev_presence = f.devs.iter().map(|d| d.e).fold(0.0f32, f32::max);
        let ta = (1.0 - dev_presence).clamp(0.0, 1.0) * (1.0 - f.content).clamp(0.0, 1.0);
        if ta > 0.01 {
            let (l, r) = (f.pill.l + f.text_l, f.pill.r - PAD);
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

    /// Two tones of the waveform: the accent (or neutral grey) and a lighter variant, as 0xRRGGBB.
    fn bar_tones(w: &WaveDraw) -> [u32; 2] {
        let pack = |c: [u8; 3]| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
        match w.rgb {
            Some(c) => [pack(c), pack(c.map(|v| v + ((255 - v) as f32 * 0.4) as u8))],
            None => [0x8E8E93; 2],
        }
    }

    /// The waveform: `BARS` rounded bars centred on `cy`, alternating the two tones.
    fn bars(&self, left: f32, cy: f32, w: &WaveDraw) {
        let tones = Self::bar_tones(w);
        for (i, v) in w.h.iter().enumerate() {
            let h = BAR_W + (BAR_MAX_H - BAR_W) * v.clamp(0.0, 1.0);
            let x = left + BARS_L + i as f32 * (BAR_W + BAR_GAP);
            self.fill(Rect::new(x, cy - h / 2.0, BAR_W, h), BAR_W / 2.0, tones[i % 2], 1.0);
        }
    }

    /// Redraw only the waveform over the settled pill, straight into the window's DIB, and mark
    /// just that rectangle changed. A Direct2D pass (~1 ms of software rendering) would cost more
    /// than the whole job: the region is ~800 px and the pill behind it is opaque black.
    /// Valid only when nothing else moves (scale 1, full opacity); the next full frame redraws
    /// the region from scratch, so the WIC bitmap does not need to see these pixels.
    pub fn draw_wave(&mut self, s: f32, left: f32, cy: f32, w: &WaveDraw) -> bool {
        if self.bits.is_null() {
            return false;
        }
        let rc = RECT {
            left: (((left + BARS_L) * s).floor() as i32 - 1).clamp(0, self.w),
            top: (((cy - BAR_MAX_H / 2.0) * s).floor() as i32 - 1).clamp(0, self.h),
            right: (((left + BARS_L + BARS_W) * s).ceil() as i32 + 1).clamp(0, self.w),
            bottom: (((cy + BAR_MAX_H / 2.0) * s).ceil() as i32 + 1).clamp(0, self.h),
        };
        let tones = Self::bar_tones(w).map(|c| [(c >> 16 & 255) as f32, (c >> 8 & 255) as f32, (c & 255) as f32]);
        let hw = BAR_W * s / 2.0;
        // (centre x, half of the straight part, tone) per bar, in physical px.
        let bars: [(f32, f32, [f32; 3]); BARS] = std::array::from_fn(|i| {
            let hh = (BAR_W + (BAR_MAX_H - BAR_W) * w.h[i].clamp(0.0, 1.0)) * s / 2.0;
            ((left + BARS_L) * s + hw + i as f32 * (BAR_W + BAR_GAP) * s, (hh - hw).max(0.0), tones[i % 2])
        });
        let cyp = cy * s;
        for y in rc.top..rc.bottom {
            let py = y as f32 + 0.5;
            for x in rc.left..rc.right {
                let px = x as f32 + 0.5;
                let mut rgb = [0.0f32; 3];
                for (cx, seg, tone) in &bars {
                    let (dx, dy) = ((px - cx).abs(), ((py - cyp).abs() - seg).max(0.0));
                    // Capsule distance: coverage 1 inside, 0 half a pixel outside.
                    let cov = (0.5 - ((dx * dx + dy * dy).sqrt() - hw)).clamp(0.0, 1.0);
                    for k in 0..3 {
                        rgb[k] += tone[k] * cov;
                    }
                }
                let o = (y as usize * self.w as usize + x as usize) * 4;
                // SAFETY: rc is clamped to the DIB, which holds w * h BGRA pixels.
                unsafe {
                    let d = self.bits.add(o);
                    *d = rgb[2] as u8;
                    *d.add(1) = rgb[1] as u8;
                    *d.add(2) = rgb[0] as u8;
                    *d.add(3) = 255;
                }
            }
        }
        self.changed = rc;
        true
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
        let seek_a = c * dim(f.can_seek);
        let (bx, by) = yo(g.back);
        self.skip10(bx, by, false, Self::press_of(f, Ctl::Back), 0xF2F2F2, seek_a);
        let (px, py) = yo(g.prev);
        self.set_xf(Some((Self::press_of(f, Ctl::Prev), px, py)));
        self.glyph(&self.ico20, '\u{E892}', px, py, 0xF2F2F2, c * dim(f.can_prev));
        let (qx, qy) = yo(g.pp2);
        self.play_pause(qx, qy, 18.0, f.playing_w, c, Self::press_of(f, Ctl::Play2));
        let (nx, ny) = yo(g.next);
        self.set_xf(Some((Self::press_of(f, Ctl::Next), nx, ny)));
        self.glyph(&self.ico20, '\u{E893}', nx, ny, 0xF2F2F2, c * dim(f.can_next));
        let (fx, fy) = yo(g.fwd);
        self.skip10(fx, fy, true, Self::press_of(f, Ctl::Fwd), 0xF2F2F2, seek_a);
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

    /// Test aid: write the presented DIB as a 24-bit BMP, composited over mid-grey so the pill is visible.
    pub fn dump_bmp(&self, path: &std::path::Path) {
        if self.bits.is_null() {
            return;
        }
        let (w, h) = (self.w as usize, self.h as usize);
        let row = (w * 3).div_ceil(4) * 4;
        let mut out = Vec::with_capacity(54 + row * h);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&((54 + row * h) as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes()); // bottom-up
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&[0; 24]);
        for y in (0..h).rev() {
            for x in 0..w {
                // SAFETY: in bounds of the w * h * 4 byte DIB.
                let p = unsafe { std::slice::from_raw_parts(self.bits.add((y * w + x) * 4), 4) };
                let inv = 255 - p[3] as u32;
                for c in &p[..3] {
                    out.push((*c as u32 + 0x50 * inv / 255).min(255) as u8);
                }
            }
            out.resize(out.len() + (row - w * 3), 0);
        }
        let _ = std::fs::write(path, out);
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
