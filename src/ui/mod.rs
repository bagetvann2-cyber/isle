//! The pill window: layered popup, hit-testing, drag, hover, spring-driven motion.
//!
//! One UI thread owns `App` (reached from the window procedure through a global pointer).
//! Frames are paced by a helper thread that blocks in `DwmFlush` only while something
//! animates; when everything has settled the thread parks on a channel and nothing runs.

pub mod layout;
mod render;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicPtr, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{Spring, Tween};
use crate::audio::Meter;
use crate::battery::BatteryHandle;
use crate::media::MediaHandle;
use crate::model::{Cover, Device, MediaCommand, MediaSnapshot, Part, PartKind};
use crate::settings;
use layout::*;
use render::{Ctl, DevDraw, Frame, PartsRow, Renderer, WaveDraw};

pub const WM_APP_MEDIA: u32 = WM_APP + 1;
pub const WM_APP_DEVICES: u32 = WM_APP + 2;
const WM_APP_ZONE: u32 = WM_APP + 5;
const WM_APP_DPI: u32 = WM_APP + 6;
const WM_APP_TRAY: u32 = WM_APP + 7;
/// Registered "TaskbarCreated" message id (Explorer restarted: re-add the tray icon).
static WM_TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
const WM_APP_FRAME: u32 = WM_APP + 3;
const WM_APP_FOREGROUND: u32 = WM_APP + 4;
const WM_APP_WAVE: u32 = WM_APP + 8;

/// Waveform pacing, shared with the `wave` helper thread: it posts `WM_APP_WAVE` every ~33 ms
/// (`WAVE_FAST`, bars moving) or every 150 ms (`WAVE_POLL`, playing but silent) and parks on `WAVE_OFF`.
static WAVE_MODE: AtomicU8 = AtomicU8::new(WAVE_OFF);
const WAVE_OFF: u8 = 0;
const WAVE_POLL: u8 = 1;
const WAVE_FAST: u8 = 2;
/// How often the audio level is sampled (bar targets are refreshed at this rate).
const WAVE_SAMPLE: Duration = Duration::from_millis(110);

const T_PROGRESS: usize = 2;
const T_LEAVE: usize = 3;
const T_DEVCHANGE: usize = 4;
const T_HIDE: usize = 5;

/// User-requested size: everything is drawn at 75% of the reference design.
const UI_SCALE: f32 = 0.75;
/// Reveal zone padding around the compact pill, physical px at 100% DPI.
const ZONE_PAD: f32 = 24.0;
/// Zone rect (l, t, r, b, physical px) shared with the low-level mouse hook proc.
static ZONE: [AtomicI32; 4] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
static INSIDE: AtomicBool = AtomicBool::new(false);
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);

const WM_MOUSELEAVE: u32 = 0x02A3;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// Data the worker threads hand to the UI (paired with a `PostMessageW`).
#[derive(Default)]
pub struct Shared {
    pub media: Mutex<Option<MediaSnapshot>>,
    pub devices: Mutex<Vec<Device>>,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

fn app() -> &'static mut App {
    // SAFETY: set once before the message loop starts; only the UI thread dereferences it,
    // and handlers never keep a reference across a call that pumps messages.
    unsafe { &mut *APP.load(Ordering::Acquire) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Outside,
    Pill,
    Ctl(Ctl),
}

struct Drag {
    start: POINT,
    win_x: i32,
    top_y: i32,
    moved: bool,
    hit: Hit,
}

struct DevView {
    dev: Device,
    /// What the compact pill shows: min(L, R) for earbuds, else the device value.
    disp_pct: u8,
    disp_chg: bool,
    prev_pct: u8,
    pct_t: Tween,
    chg_t: Tween,
    pres: Tween,
    text_w: f32,
    removing: bool,
}

/// Compact-pill value of a device: earbuds show the lower bud, charging if any bud charges.
fn display_of(d: &Device) -> (u8, bool) {
    let buds: Vec<&Part> = d.parts.iter().filter(|p| p.kind != PartKind::Case).collect();
    if buds.is_empty() {
        (d.percent, d.charging == Some(true))
    } else {
        (buds.iter().map(|p| p.percent).min().unwrap_or(d.percent), buds.iter().any(|p| p.charging))
    }
}

impl DevView {
    /// Width with the bolt fully shown (what the pill springs to).
    fn full_w(&self) -> f32 {
        ICON_W + ICON_TEXT_GAP + self.text_w + if self.disp_chg { BOLT_W } else { 0.0 }
    }

    /// Current animated width: the bolt slot opens and closes with its fade.
    fn item_w(&self, now: Instant) -> f32 {
        ICON_W + ICON_TEXT_GAP + self.text_w + BOLT_W * self.chg_t.val(now)
    }
}

struct App {
    hwnd: HWND,
    shared: Arc<Shared>,
    media_tx: Option<MediaHandle>,
    battery: Option<BatteryHandle>,
    last_scan: Option<Instant>,
    r: Option<Renderer>,
    ru: bool,

    // geometry (physical px)
    scale: f32,
    gm: i32,
    win_w: i32,
    win_h: i32,
    win_x: i32,
    top_y: i32,
    frac: f32,
    up: bool,

    // motion
    open: Spring,
    width: Spring,
    vis: Spring,
    press: Spring,
    press_ctl: Option<Ctl>,
    content: Tween,
    rows_t: Tween,
    collapse_at: Option<Instant>,
    text_t: Tween,
    cover_t: Tween,
    pp_t: Tween,
    last_tick: Instant,
    anim_running: bool,
    frame_pending: bool,
    frame_tx: Option<Sender<()>>,
    perf: (u32, f64, f64),
    perf_present: f64,

    // waveform
    wave_on: bool,
    bars: [Spring; BARS],
    /// What the DIB currently shows, to skip identical frames.
    wave_drawn: [f32; BARS],
    wave_t: Instant,
    wave_sample_at: Instant,
    wave_silent: u32,
    wave_rng: u32,
    wave_perf: (u32, f64, f64),
    meter: Meter,
    wave_thread: Option<std::thread::Thread>,

    // interaction
    want_exp: bool,
    tracking: bool,
    leave_pending: bool,
    menu_open: bool,
    progress_timer: bool,
    fs_hidden: bool,
    trimmed: bool,
    tray_icon: Option<HICON>,
    always_visible: bool,
    zone_active: bool,
    mouse_hook: Option<HHOOK>,
    hinst: HINSTANCE,
    shown: bool,
    drag: Option<Drag>,

    // content
    media: Option<MediaSnapshot>,
    prev_title: String,
    prev_artist: String,
    prev_cover: Option<Arc<Cover>>,
    devs: Vec<DevView>,
}

fn work_area(x: i32, y: i32, primary: bool) -> (RECT, u32) {
    // SAFETY: plain monitor queries with valid out pointers.
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, if primary { MONITOR_DEFAULTTOPRIMARY } else { MONITOR_DEFAULTTONEAREST });
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (mi.rcWork, dx)
    }
}

fn wide0(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

impl App {
    fn new(shared: Arc<Shared>) -> App {
        let now = Instant::now();
        let ru = unsafe { GetUserDefaultUILanguage() } & 0x3FF == 0x19;
        let saved = settings::load_pos();
        let (work, dpi) = match saved {
            Some((x, y)) => work_area(x, y, false),
            None => work_area(0, 0, true),
        };
        let mut a = App {
            hwnd: HWND::default(),
            shared,
            media_tx: None,
            battery: None,
            last_scan: None,
            r: None,
            ru,
            scale: dpi as f32 / 96.0 * UI_SCALE,
            gm: 0,
            win_w: 0,
            win_h: 0,
            win_x: 0,
            top_y: 0,
            frac: 0.5,
            up: false,
            open: Spring::new(0.0, 0.42, 0.78),
            width: Spring::new(MIN_W, 0.40, 0.80),
            vis: Spring::new(0.0, 0.30, 0.85),
            press: Spring::new(1.0, 0.18, 0.70),
            press_ctl: None,
            content: Tween::at(0.0, now),
            rows_t: Tween::at(0.0, now),
            collapse_at: None,
            text_t: Tween::at(1.0, now),
            cover_t: Tween::at(1.0, now),
            pp_t: Tween::at(1.0, now),
            last_tick: now,
            anim_running: false,
            frame_pending: false,
            frame_tx: None,
            perf: (0, 0.0, 0.0),
            perf_present: 0.0,
            wave_on: settings::waveform(),
            bars: [Spring::new(0.0, 0.16, 0.6); BARS],
            wave_drawn: [0.0; BARS],
            wave_t: now,
            wave_sample_at: now,
            wave_silent: 0,
            wave_rng: 0x9E37_79B9,
            wave_perf: (0, 0.0, 0.0),
            meter: Meter::default(),
            wave_thread: None,
            want_exp: false,
            tracking: false,
            leave_pending: false,
            menu_open: false,
            progress_timer: false,
            fs_hidden: false,
            trimmed: false,
            tray_icon: None,
            always_visible: settings::always_visible(),
            zone_active: false,
            mouse_hook: None,
            hinst: HINSTANCE::default(),
            shown: false,
            drag: None,
            media: None,
            prev_title: String::new(),
            prev_artist: String::new(),
            prev_cover: None,
            devs: Vec::new(),
        };
        a.vis.target = 1.0;
        a.recompute_size();
        match saved {
            Some((x, y)) => {
                a.frac = 0.0;
                a.win_x = x - a.gm;
                a.top_y = y;
            }
            None => a.place_default(&work),
        }
        a.clamp_to_monitor();
        a
    }

    fn recompute_size(&mut self) {
        let s = self.scale;
        self.gm = (GLOW * s).ceil() as i32;
        self.win_w = (MAX_W * s).ceil() as i32 + 2 * self.gm;
        self.win_h = ((EXP_H + PART_ROW_H * MAX_PART_ROWS as f32) * s).ceil() as i32 + 2 * self.gm;
    }

    fn place_default(&mut self, work: &RECT) {
        self.frac = 0.5;
        self.win_x = (work.left + work.right) / 2 - self.win_w / 2;
        self.top_y = work.top + (12.0 * self.scale).round() as i32;
    }

    fn win_y(&self) -> i32 {
        self.top_y - self.gm - if self.up { (UP_ROOM * self.scale).round() as i32 } else { 0 }
    }

    /// Physical left edge of the pill at its target width.
    fn pill_left(&self) -> i32 {
        self.win_x + self.gm + ((MAX_W - self.width.target) * self.frac * self.scale).round() as i32
    }

    fn clamp_to_monitor(&mut self) {
        let left = self.pill_left();
        let wpx = (self.width.target * self.scale).round() as i32;
        let (work, _) = work_area(left + wpx / 2, self.top_y, false);
        if left < work.left {
            self.win_x += work.left - left;
        } else if left + wpx > work.right {
            self.win_x -= left + wpx - work.right;
        }
        let hpx = (PILL_H * self.scale).round() as i32;
        self.top_y = self.top_y.min(work.bottom - hpx).max(work.top);
        self.update_zone();
    }

    /// Reveal zone for the hook proc: the compact pill (or the expanded one) inflated by 24 px.
    fn update_zone(&self) {
        let s = self.scale;
        let pad = (ZONE_PAD * s / UI_SCALE).round() as i32;
        let left = self.pill_left();
        let w = (self.width.target * s).round() as i32;
        let h = (PILL_H * s).round() as i32;
        let extra = if self.want_exp { ((EXP_H - PILL_H + PART_ROW_H * self.rows_t.target()) * s).round() as i32 } else { 0 };
        let (top, bottom) = if self.up { (self.top_y - extra, self.top_y + h) } else { (self.top_y, self.top_y + h + extra) };
        for (a, v) in ZONE.iter().zip([left - pad, top - pad, left + w + pad, bottom + pad]) {
            a.store(v, Ordering::Relaxed);
        }
    }

    fn point_in_zone(x: i32, y: i32) -> bool {
        x >= ZONE[0].load(Ordering::Relaxed)
            && y >= ZONE[1].load(Ordering::Relaxed)
            && x < ZONE[2].load(Ordering::Relaxed)
            && y < ZONE[3].load(Ordering::Relaxed)
    }

    // ---- animation driver ----

    fn kick(&mut self) {
        if !self.anim_running {
            self.anim_running = true;
            self.last_tick = Instant::now();
        }
        if !self.frame_pending {
            if let Some(tx) = &self.frame_tx {
                self.frame_pending = true;
                let _ = tx.send(());
            }
        }
    }

    fn is_animating(&self, now: Instant) -> bool {
        !self.open.settled(0.004, 0.02)
            || !self.width.settled(0.5, 2.0)
            || !self.vis.settled(0.004, 0.02)
            || (self.press_ctl.is_some() && !self.press.settled(0.002, 0.02))
            || self.collapse_at.is_some()
            || !self.content.done(now)
            || !self.rows_t.done(now)
            || !self.text_t.done(now)
            || !self.cover_t.done(now)
            || !self.pp_t.done(now)
            || self.devs.iter().any(|d| !d.pres.done(now) || !d.pct_t.done(now) || !d.chg_t.done(now))
    }

    fn tick(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.open.step(dt);
        self.width.step(dt);
        self.vis.step(dt);
        self.press.step(dt);
        if self.collapse_at.is_some_and(|t| now >= t) {
            self.collapse_at = None;
            self.open.target = 0.0;
            self.open.tune(0.35, 1.0);
        }
        if self.open.target == 1.0 && self.open.x >= 0.6 && self.content.target() != 1.0 {
            self.content.go(1.0, 0.18, now);
        }
        if self.open.settled(0.004, 0.02) {
            self.open.snap();
        }
        if self.width.settled(0.5, 2.0) {
            self.width.snap();
        }
        if self.vis.settled(0.004, 0.02) {
            self.vis.snap();
        }
        if self.press.settled(0.002, 0.02) {
            self.press.snap();
            if self.press.target == 1.0 {
                self.press_ctl = None;
            }
        }
        self.devs.retain(|d| !(d.removing && d.pres.done(now)));
        if self.vis.target == 0.0 && self.vis.x == 0.0 && self.shown {
            self.shown = false;
            // SAFETY: hides our own window.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.sync_progress_timer();
        }
    }

    fn on_frame(&mut self) {
        self.frame_pending = false;
        let now = Instant::now();
        self.tick(now);
        if self.wave_visible() {
            self.wave_step(now);
        }
        self.paint();
        if self.is_animating(now) {
            self.kick();
        } else {
            self.anim_running = false;
            self.sync_wave();
            self.log_perf();
            if !self.trimmed {
                self.trimmed = true;
                trim_memory();
            }
        }
    }

    fn log_perf(&mut self) {
        let (n, sum, max) = self.perf;
        self.perf = (0, 0.0, 0.0);
        let present = std::mem::take(&mut self.perf_present);
        if n == 0 || !test_knob("ISLE_PERF") {
            return;
        }
        perf_log(&format!("frames {n} avg {:.3} ms (present {:.3}) max {:.3} ms\n", sum / n as f64, present / n as f64, max));
    }

    // ---- waveform ----

    fn playing(&self) -> bool {
        self.media.as_ref().is_some_and(|m| m.playing)
    }

    /// The bars are on screen (or about to be): enabled, island shown and not on its way out, media present.
    fn wave_visible(&self) -> bool {
        self.wave_on && self.shown && self.vis.target > 0.0 && self.media.is_some()
    }

    fn wave_moving(&self) -> bool {
        self.bars.iter().any(|b| b.target > 0.0 || !b.settled(0.004, 0.02))
    }

    /// Set the helper thread's cadence: fast while bars move, slow while playing silently
    /// (to notice sound starting), parked otherwise. Hidden / disabled also zeroes the bars.
    fn sync_wave(&mut self) {
        let visible = self.wave_visible();
        let mode = if !visible {
            WAVE_OFF
        } else if self.wave_moving() {
            WAVE_FAST
        } else if self.playing() {
            WAVE_POLL
        } else {
            WAVE_OFF
        };
        if !visible {
            self.bars.iter_mut().for_each(|b| (b.target, b.x, b.v) = (0.0, 0.0, 0.0));
            self.wave_drawn = [0.0; BARS];
            self.meter.reset(); // endpoint is looked up afresh next time (device may have changed)
        }
        if WAVE_MODE.swap(mode, Ordering::Relaxed) == WAVE_OFF && mode != WAVE_OFF {
            self.wave_t = Instant::now();
            self.wave_sample_at = self.wave_t;
            if let Some(t) = &self.wave_thread {
                t.unpark();
            }
        }
    }

    fn rand01(&mut self) -> f32 {
        let mut x = self.wave_rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.wave_rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Advance the bar springs; every `WAVE_SAMPLE` read the audio level and pick new bar targets.
    fn wave_step(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.wave_t).as_secs_f32();
        self.wave_t = now;
        if self.playing() && now >= self.wave_sample_at {
            self.wave_sample_at = now + WAVE_SAMPLE;
            let peak = if test_knob("ISLE_TEST_WAVE") { 0.1 + 0.5 * self.rand01() } else { self.meter.peak() };
            if peak < 0.01 {
                self.wave_silent = self.wave_silent.saturating_add(1);
                if self.wave_silent == 12 {
                    self.meter.reset(); // silence for a while: maybe the default device changed
                }
                self.bars.iter_mut().for_each(|b| b.target = 0.0);
            } else {
                self.wave_silent = 0;
                // Peak -> 0..1 on a dB scale (-36 dB and below is flat), then a random share per bar.
                let level = ((20.0 * peak.log10() + 36.0) / 36.0).clamp(0.0, 1.0);
                for i in 0..BARS {
                    let r = 0.3 + 0.7 * self.rand01();
                    self.bars[i].target = level * r;
                }
            }
        } else if !self.playing() {
            self.bars.iter_mut().for_each(|b| b.target = 0.0);
        }
        for b in &mut self.bars {
            b.step(dt);
            if b.target == 0.0 && b.settled(0.004, 0.02) {
                b.snap();
            }
        }
    }

    /// Bars-only frame from the helper thread's cadence. Skipped while a regular animation
    /// runs (its frames carry the bars) and when nothing moved.
    fn on_wave(&mut self) {
        if self.anim_running {
            return;
        }
        let now = Instant::now();
        if self.is_animating(now) {
            self.kick();
            return;
        }
        if self.wave_visible() {
            self.wave_step(now);
            if self.bars.iter().zip(&self.wave_drawn).any(|(b, d)| (b.x - d).abs() > 0.002) {
                self.paint_wave();
            }
        }
        self.sync_wave();
    }

    fn wave_draw(&self, now: Instant) -> Option<WaveDraw> {
        let m = self.media.as_ref().filter(|_| self.wave_on)?;
        let cur = m.cover.as_ref().and_then(|c| c.accent);
        let prev = self.prev_cover.as_ref().and_then(|c| c.accent);
        let t = self.cover_t.val(now);
        let rgb = match (prev, cur) {
            (Some(p), Some(c)) if t < 1.0 => Some([0, 1, 2].map(|i| (p[i] as f32 + (c[i] as f32 - p[i] as f32) * t).round() as u8)),
            _ => cur,
        };
        Some(WaveDraw { h: self.bars.map(|b| b.x.clamp(0.0, 1.0)), rgb })
    }

    fn paint_wave(&mut self) {
        if !self.shown {
            return;
        }
        let t0 = Instant::now();
        let (pill, _, _, row1, _) = self.geometry();
        let (Some(w), Some(mut r)) = (self.wave_draw(t0), self.r.take()) else { return };
        if !r.draw_wave(self.scale, pill.l, row1 + PILL_H / 2.0, &w) {
            self.r = Some(r);
            self.paint(); // no DIB to draw into: take the full path
            return;
        }
        let t1 = Instant::now();
        r.present(self.hwnd, self.win_x, self.win_y());
        self.r = Some(r);
        self.wave_drawn = w.h;
        if test_knob("ISLE_PERF") {
            let (ms, pr) = (t0.elapsed().as_secs_f64() * 1000.0, t1.elapsed().as_secs_f64() * 1000.0);
            let p = &mut self.wave_perf;
            *p = (p.0 + 1, p.1 + ms, p.2.max(ms));
            self.perf_present += pr;
            if self.wave_perf.0 == 100 {
                let (n, sum, max) = std::mem::take(&mut self.wave_perf);
                let line = format!(
                    "wave frames {n} avg {:.3} ms (present {:.3}) max {:.3} ms\n",
                    sum / n as f64,
                    self.perf_present / n as f64,
                    max
                );
                self.perf_present = 0.0;
                perf_log(&line);
            }
        }
    }

    // ---- scene ----

    /// (pill rect, radius, geo, row1, block) in window-logical px, from the current spring state.
    fn geometry(&self) -> (Rect, f32, Geo, f32, f32) {
        let s = self.scale;
        let open = self.open.x.clamp(0.0, 1.08);
        let ext = PART_ROW_H * self.rows_t.val(Instant::now());
        let h = PILL_H + (EXP_H - PILL_H + ext) * open;
        let w = self.width.x.max(60.0);
        let gml = self.gm as f32 / s;
        let left = gml + (MAX_W - w) * self.frac;
        let row1 = gml + if self.up { UP_ROOM } else { 0.0 };
        let (top, block) = if self.up { (row1 + PILL_H - h, row1 - PILL_H - ext) } else { (row1, row1 + PILL_H) };
        let radius = (42.0 - 8.0 * open).min(h / 2.0);
        let pill = Rect::new(left, top, w, h);
        (pill, radius, geo(left, w, row1, block), row1, block)
    }

    fn frame(&self, now: Instant) -> Frame {
        let (pill, radius, _, row1, block) = self.geometry();
        let m = self.media.as_ref();
        let progress = m.filter(|m| m.duration_ms > 0).map(|m| {
            let mut pos = m.position_ms;
            if m.playing {
                pos += m.position_at.map_or(0, |t| t.elapsed().as_millis() as u64);
            }
            let pos = pos.min(m.duration_ms);
            (pos as f32 / m.duration_ms as f32, fmt_time(pos), format!("-{}", fmt_time(m.duration_ms - pos)))
        });
        let live: Vec<&DevView> = self.devs.iter().filter(|d| !d.removing).collect();
        let extra = (live.len() > MAX_DEV)
            .then(|| live[MAX_DEV..].iter().map(|d| format!("{} {}%", d.dev.name, d.disp_pct)).collect::<Vec<_>>().join("   "));
        Frame {
            s: self.scale,
            pill,
            radius,
            row1,
            block,
            vis: self.vis.x,
            vis_scale: 0.9 + 0.1 * self.vis.x,
            content: self.content.val(now),
            has_media: m.is_some(),
            playing_w: self.pp_t.val(now),
            can_prev: m.is_some_and(|m| m.can_prev),
            can_next: m.is_some_and(|m| m.can_next),
            cover: m.and_then(|m| m.cover.clone()),
            prev_cover: self.prev_cover.clone(),
            cover_t: self.cover_t.val(now),
            title: m.map(|m| m.title.clone()).unwrap_or_default(),
            prev_title: self.prev_title.clone(),
            artist: m.map(|m| m.artist.clone()).unwrap_or_default(),
            prev_artist: self.prev_artist.clone(),
            text_t: self.text_t.val(now),
            nothing: if self.ru { "Ничего не играет" } else { "Nothing playing" },
            devs: self
                .shown_devs()
                .map(|d| DevDraw {
                    kind: d.dev.kind,
                    pct: d.disp_pct,
                    prev_pct: d.prev_pct,
                    pct_t: d.pct_t.val(now),
                    charging: d.disp_chg,
                    chg: d.chg_t.val(now),
                    text_w: d.text_w,
                    item_w: d.item_w(now),
                    e: d.pres.val(now),
                })
                .collect(),
            extra,
            rows: live
                .iter()
                .filter(|d| !d.dev.parts.is_empty())
                .take(MAX_PART_ROWS)
                .map(|d| PartsRow { kind: d.dev.kind, name: d.dev.name.clone(), parts: d.dev.parts.clone() })
                .collect(),
            rows_f: self.rows_t.val(now),
            press: self.press_ctl.map(|c| (c, self.press.x)),
            progress,
            text_l: text_l(self.wave_on && self.media.is_some()),
            wave: self.wave_draw(now),
        }
    }

    /// Devices that take a slot on the pill: the first three live ones plus any fading out.
    fn shown_devs(&self) -> impl Iterator<Item = &DevView> {
        let mut live = 0;
        self.devs.iter().filter(move |d| {
            if d.removing {
                return true;
            }
            live += 1;
            live <= MAX_DEV
        })
    }

    fn paint(&mut self) {
        if !self.shown {
            return;
        }
        let t0 = Instant::now();
        let f = self.frame(t0);
        self.wave_drawn = self.bars.map(|b| b.x.clamp(0.0, 1.0));
        let Some(mut r) = self.r.take() else { return };
        if r.size() != (self.win_w, self.win_h) {
            let _ = r.resize(self.win_w, self.win_h);
        }
        if !r.draw(&f) {
            // Render target lost: rebuild it and draw this frame again.
            if let Ok(mut n) = Renderer::new(self.win_w, self.win_h) {
                n.draw(&f);
                r = n;
            }
        }
        let t1 = Instant::now();
        r.present(self.hwnd, self.win_x, self.win_y());
        self.perf_present += t1.elapsed().as_secs_f64() * 1000.0;
        self.r = Some(r);
        if self.anim_running {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.perf = (self.perf.0 + 1, self.perf.1 + ms, self.perf.2.max(ms));
        }
    }

    // ---- hit-testing ----

    fn hit_at(&self, px: i32, py: i32) -> Hit {
        if !self.shown || self.vis.target == 0.0 {
            return Hit::Outside;
        }
        let s = self.scale;
        let (x, y) = ((px - self.win_x) as f32 / s, (py - self.win_y()) as f32 / s);
        let (pill, radius, g, _, _) = self.geometry();
        if !in_pill(pill, radius, x, y) {
            return Hit::Outside;
        }
        let near = |p: (f32, f32)| Rect::around(p.0, p.1, 32.0, 32.0).contains(x, y);
        if g.cover.contains(x, y) {
            Hit::Ctl(Ctl::Cover)
        } else if near(g.pp1) {
            Hit::Ctl(Ctl::Play1)
        } else if self.content.target() == 1.0 && self.open.x > 0.9 {
            if near(g.prev) {
                Hit::Ctl(Ctl::Prev)
            } else if near(g.pp2) {
                Hit::Ctl(Ctl::Play2)
            } else if near(g.next) {
                Hit::Ctl(Ctl::Next)
            } else {
                Hit::Pill
            }
        } else {
            Hit::Pill
        }
    }

    fn command(&self, c: Ctl) {
        let cmd = match c {
            Ctl::Cover | Ctl::Next => MediaCommand::Next,
            Ctl::Prev => MediaCommand::Prev,
            Ctl::Play1 | Ctl::Play2 => MediaCommand::TogglePlay,
        };
        if let Some(m) = &self.media_tx {
            m.send(cmd);
        }
    }

    // ---- hover / expand / collapse ----

    fn request_expand(&mut self, now: Instant) {
        if self.want_exp && self.open.target == 1.0 && self.collapse_at.is_none() {
            return; // already open: mouse moves must not cost a frame
        }
        self.collapse_at = None;
        self.want_exp = true;
        if self.content.target() == 0.0 && self.content.val(now) > 0.05 {
            self.content.go(1.0, 0.18, now);
        }
        if self.open.target != 1.0 {
            if self.open.x < 0.02 {
                let (work, _) = work_area(self.pill_left(), self.top_y, false);
                self.up = !expand_down(self.top_y, self.scale, PART_ROW_H * MAX_PART_ROWS as f32, work.top, work.bottom);
            }
            self.open.target = 1.0;
            self.open.tune(0.42, 0.78);
            // Earbud L/R/case levels come from a short BLE scan; the worker serialises scans,
            // this just keeps us from asking more than every 30 s.
            if self.last_scan.is_none_or(|t| now.duration_since(t) > Duration::from_secs(30)) {
                self.last_scan = Some(now);
                if let Some(b) = &self.battery {
                    b.scan_buds();
                }
            }
        }
        self.sync_progress_timer();
        self.update_zone();
        self.kick();
    }

    fn request_collapse(&mut self, now: Instant) {
        if !self.want_exp && self.open.target == 0.0 {
            return;
        }
        self.want_exp = false;
        self.content.go(0.0, 0.10, now);
        self.collapse_at = Some(now + Duration::from_millis(90));
        self.sync_progress_timer();
        self.update_zone();
        self.kick();
    }

    fn sync_progress_timer(&mut self) {
        let want = self.shown && self.want_exp && self.media.as_ref().is_some_and(|m| m.playing && m.duration_ms > 0);
        if want != self.progress_timer {
            self.progress_timer = want;
            // SAFETY: timer on our own window.
            unsafe {
                if want {
                    SetTimer(Some(self.hwnd), T_PROGRESS, 1000, None);
                } else {
                    let _ = KillTimer(Some(self.hwnd), T_PROGRESS);
                }
            }
        }
    }

    fn cursor_in_pill(&self) -> bool {
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        unsafe { GetCursorPos(&mut p).is_ok() && self.hit_at(p.x, p.y) != Hit::Outside }
    }

    // ---- mouse ----

    fn on_mouse_move(&mut self) {
        let now = Instant::now();
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        unsafe {
            let _ = GetCursorPos(&mut p);
            if !self.tracking {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: self.hwnd,
                    dwHoverTime: 0,
                };
                if TrackMouseEvent(&mut t).is_ok() {
                    self.tracking = true;
                }
            }
            if self.leave_pending {
                let _ = KillTimer(Some(self.hwnd), T_LEAVE);
                self.leave_pending = false;
            }
        }
        if let Some(d) = &mut self.drag {
            let (dx, dy) = (p.x - d.start.x, p.y - d.start.y);
            if !d.moved && dx.abs().max(dy.abs()) > 4 {
                d.moved = true;
                // From now on the pill is user-placed: pin its left edge.
                if self.frac != 0.0 {
                    let shift = ((MAX_W - self.width.x) * self.frac * self.scale).round() as i32;
                    self.win_x += shift;
                    d.win_x += shift;
                    self.frac = 0.0;
                }
                self.press.target = 1.0;
            }
            if d.moved {
                self.win_x = d.win_x + dx;
                self.top_y = d.top_y + dy;
                self.clamp_to_monitor();
                self.paint();
            }
            return;
        }
        self.request_expand(now);
    }

    fn on_leave(&mut self) {
        self.tracking = false;
        if self.drag.is_some() {
            return;
        }
        self.leave_pending = true;
        // SAFETY: timer on our own window.
        unsafe {
            SetTimer(Some(self.hwnd), T_LEAVE, 300, None);
        }
    }

    fn on_lbutton_down(&mut self) {
        let mut p = POINT::default();
        // SAFETY: valid out pointer; capture on our own window.
        unsafe {
            let _ = GetCursorPos(&mut p);
            SetCapture(self.hwnd);
        }
        let hit = self.hit_at(p.x, p.y);
        self.drag = Some(Drag { start: p, win_x: self.win_x, top_y: self.top_y, moved: false, hit });
        if let Hit::Ctl(c) = hit {
            self.press_ctl = Some(c);
            self.press.target = 0.92;
            self.press.tune(0.18, 0.70);
            self.kick();
        }
    }

    /// `drag` was taken (and capture released) by the caller, outside any `&mut App`.
    fn on_lbutton_up(&mut self, drag: Option<Drag>) {
        let Some(d) = drag else { return };
        if self.press_ctl.is_some() {
            self.press.target = 1.0;
            self.press.tune(0.30, 0.55);
            self.kick();
        }
        if d.moved {
            settings::save_pos(self.pill_left(), self.top_y);
        } else if let Hit::Ctl(c) = d.hit {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            if self.hit_at(p.x, p.y) == d.hit {
                self.command(c);
            }
        }
        if !self.cursor_in_pill() {
            self.on_leave();
        }
    }

    // ---- data from worker threads ----

    fn on_media(&mut self) {
        let now = Instant::now();
        let mut new = self.shared.media.lock().map(|g| g.clone()).unwrap_or(None);
        if test_knob("ISLE_TEST_NO_MEDIA") {
            new = None; // test knob: force the empty state
        }
        match (&self.media, &new) {
            (Some(o), Some(n)) => {
                if o.title != n.title || o.artist != n.artist {
                    self.prev_title = o.title.clone();
                    self.prev_artist = o.artist.clone();
                    self.text_t = Tween::at(0.0, now);
                    self.text_t.go(1.0, 0.25, now);
                }
                let changed = match (&o.cover, &n.cover) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                };
                if changed {
                    self.prev_cover = o.cover.clone();
                    self.cover_t = Tween::at(0.0, now);
                    self.cover_t.go(1.0, 0.25, now);
                }
                if o.playing != n.playing {
                    self.pp_t.go(if n.playing { 1.0 } else { 0.0 }, 0.20, now);
                }
            }
            (None, Some(n)) => {
                self.prev_title.clear();
                self.prev_artist.clear();
                self.prev_cover = None;
                self.text_t = Tween::at(0.0, now);
                self.text_t.go(1.0, 0.25, now);
                self.cover_t = Tween::at(0.0, now);
                self.cover_t.go(1.0, 0.25, now);
                self.pp_t = Tween::at(if n.playing { 1.0 } else { 0.0 }, now);
            }
            _ => {}
        }
        let material = match (&self.media, &new) {
            (Some(o), Some(n)) => {
                o.title != n.title
                    || o.artist != n.artist
                    || o.playing != n.playing
                    || o.can_prev != n.can_prev
                    || o.can_next != n.can_next
                    || o.duration_ms != n.duration_ms
                    || self.want_exp
            }
            (None, None) => false,
            _ => true,
        };
        self.media = new;
        self.sync_progress_timer();
        self.sync_wave();
        if self.is_animating(now) {
            if self.shown {
                self.kick();
            }
        } else if material {
            self.paint();
        }
    }

    fn on_devices(&mut self) {
        let now = Instant::now();
        let mut new = self.shared.devices.lock().map(|g| g.clone()).unwrap_or_default();
        if test_knob("ISLE_TEST_NO_DEVICES") {
            new.clear(); // test knob
        }
        if test_knob("ISLE_TEST_FAKE_DEVICES") {
            // test knob: the two reference devices
            new = vec![
                Device {
                    id: "t1".into(),
                    name: "Headphones".into(),
                    kind: crate::model::DeviceKind::Headphones,
                    percent: 78,
                    charging: None,
                    parts: Vec::new(),
                },
                Device {
                    id: "t2".into(),
                    name: "Mouse".into(),
                    kind: crate::model::DeviceKind::Mouse,
                    percent: 92,
                    charging: Some(true),
                    parts: Vec::new(),
                },
            ];
        }
        if test_knob("ISLE_FAKE_BUDS") {
            // debug-only: earbuds with parts, left bud charging
            new.push(Device {
                id: "fake-buds".into(),
                name: "Redmi Buds 6".into(),
                kind: crate::model::DeviceKind::Headset,
                percent: 75,
                charging: None,
                parts: vec![
                    Part { kind: PartKind::Left, percent: 80, charging: true },
                    Part { kind: PartKind::Right, percent: 75, charging: false },
                    Part { kind: PartKind::Case, percent: 40, charging: false },
                ],
            });
        }
        let old = std::mem::take(&mut self.devs);
        let mut out: Vec<DevView> = Vec::with_capacity(new.len());
        for d in new {
            let (disp_pct, disp_chg) = display_of(&d);
            let text_w = self.r.as_ref().map_or(40.0, |r| r.measure(&format!("{disp_pct}%")));
            match old.iter().find(|o| o.dev.id == d.id) {
                Some(o) => {
                    let mut v = DevView {
                        dev: d,
                        disp_pct,
                        disp_chg,
                        prev_pct: o.prev_pct,
                        pct_t: o.pct_t,
                        chg_t: o.chg_t,
                        pres: o.pres,
                        text_w,
                        removing: false,
                    };
                    if o.disp_pct != disp_pct {
                        v.prev_pct = o.disp_pct;
                        v.pct_t = Tween::at(0.0, now);
                        v.pct_t.go(1.0, 0.25, now);
                    }
                    if o.disp_chg != disp_chg {
                        v.chg_t.go(if disp_chg { 1.0 } else { 0.0 }, 0.20, now);
                    }
                    if o.removing {
                        v.pres.go(1.0, 0.25, now);
                    }
                    out.push(v);
                }
                None => {
                    let mut pres = Tween::at(0.0, now);
                    pres.go(1.0, 0.30, now);
                    out.push(DevView {
                        prev_pct: disp_pct,
                        disp_pct,
                        disp_chg,
                        pct_t: Tween::at(1.0, now),
                        chg_t: Tween::at(disp_chg as u8 as f32, now),
                        pres,
                        text_w,
                        removing: false,
                        dev: d,
                    });
                }
            }
        }
        for (i, mut o) in old.into_iter().enumerate() {
            if !out.iter().any(|v| v.dev.id == o.dev.id) {
                if !o.removing {
                    o.removing = true;
                    o.pres.go(0.0, 0.25, now);
                }
                out.insert(i.min(out.len()), o);
            }
        }
        self.devs = out;
        self.update_width_target();
        let rows = self.devs.iter().filter(|d| !d.removing && !d.dev.parts.is_empty()).count().min(MAX_PART_ROWS) as f32;
        if self.rows_t.target() != rows {
            self.rows_t.go(rows, 0.25, now);
        }
        self.update_zone();
        if self.shown {
            self.kick();
        }
    }

    fn update_width_target(&mut self) {
        let items: Vec<f32> = self.shown_devs().filter(|d| !d.removing).map(|d| d.full_w()).collect();
        self.width.target = target_width(&items, self.wave_on);
        self.width.tune(0.40, 0.80);
    }

    // ---- system events ----

    fn on_dpi(&mut self, dpi: u32) {
        let left = self.pill_left();
        let center = self.win_x + self.win_w / 2;
        self.scale = dpi as f32 / 96.0 * UI_SCALE;
        self.recompute_size();
        if self.frac == 0.0 {
            self.win_x = left - self.gm;
        } else {
            self.win_x = center - self.win_w / 2;
        }
        self.clamp_to_monitor();
        if let Some(r) = &mut self.r {
            let _ = r.resize(self.win_w, self.win_h);
        }
        self.tray_sync(false);
        self.paint();
    }

    fn on_foreground(&mut self) {
        // SAFETY: queries about the foreground window.
        let hide = unsafe { fullscreen_active(self.hwnd) };
        if hide == self.fs_hidden {
            return;
        }
        self.fs_hidden = hide;
        self.sync_hook(!hide);
        self.apply_visibility();
    }

    fn apply_visibility(&mut self) {
        let want = !self.fs_hidden && (self.always_visible || self.zone_active || self.menu_open || self.drag.is_some());
        if want {
            self.vis.target = 1.0;
            if !self.shown {
                self.shown = true;
                // SAFETY: shows our own window without activating it.
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                }
                self.sync_progress_timer();
            }
        } else {
            self.vis.target = 0.0;
        }
        self.sync_wave();
        self.kick();
    }

    /// Cursor entered / left the reveal zone (posted by the mouse hook).
    fn on_zone(&mut self, inside: bool) {
        // SAFETY: timer on our own window.
        unsafe {
            if inside {
                let _ = KillTimer(Some(self.hwnd), T_HIDE);
                self.zone_active = true;
                self.apply_visibility();
            } else {
                self.request_collapse(Instant::now());
                SetTimer(Some(self.hwnd), T_HIDE, 600, None);
            }
        }
    }

    /// Install the low-level mouse hook only while auto-hide is on.
    /// Not installed while always-visible or while a fullscreen app hides the island (a
    /// gaming mouse would otherwise pay for it). `rehook` replaces a live hook: Windows
    /// silently drops hooks that time out, so leaving fullscreen always reinstalls.
    fn sync_hook(&mut self, rehook: bool) {
        // SAFETY: hook installed/removed on the UI thread that pumps its messages.
        unsafe {
            let want = !self.always_visible && !self.fs_hidden;
            if !want || rehook {
                if let Some(h) = self.mouse_hook.take() {
                    let _ = UnhookWindowsHookEx(h);
                }
            }
            if want && self.mouse_hook.is_none() {
                self.mouse_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll), Some(self.hinst), 0).ok();
            }
        }
        let mut p = POINT::default();
        // SAFETY: valid out pointer.
        let inside = unsafe { GetCursorPos(&mut p).is_ok() } && Self::point_in_zone(p.x, p.y);
        INSIDE.store(inside, Ordering::Relaxed);
    }

    /// Add (or refresh) the notification-area icon.
    fn tray_sync(&mut self, add: bool) {
        // SAFETY: NOTIFYICONDATAW is fully initialised; the icon outlives the call.
        unsafe {
            let icon = make_tray_icon(GetSystemMetrics(SM_CXSMICON).max(16));
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: 1,
                uFlags: NIF_ICON | NIF_TIP | NIF_MESSAGE,
                uCallbackMessage: WM_APP_TRAY,
                hIcon: icon.unwrap_or_default(),
                ..Default::default()
            };
            for (d, c) in nid.szTip.iter_mut().zip("isle".encode_utf16()) {
                *d = c;
            }
            // After an Explorer restart NIM_ADD can fail if the icon survived: modify it instead.
            if !add || !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
            }
            if let Some(old) = std::mem::replace(&mut self.tray_icon, icon) {
                let _ = DestroyIcon(old);
            }
        }
    }

    fn tray_remove(&mut self) {
        // SAFETY: deletes the icon we added.
        unsafe {
            let nid =
                NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: self.hwnd, uID: 1, ..Default::default() };
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            if let Some(i) = self.tray_icon.take() {
                let _ = DestroyIcon(i);
            }
        }
    }

    /// Show the island for a few seconds (tray click), like the startup reveal.
    fn reveal_for(&mut self, ms: u32) {
        self.zone_active = true;
        // SAFETY: timer on our own window.
        unsafe {
            SetTimer(Some(self.hwnd), T_HIDE, ms, None);
        }
        self.apply_visibility();
    }

    fn reset_position(&mut self) {
        settings::clear_pos();
        let (work, _) = work_area(0, 0, true);
        self.place_default(&work);
        self.clamp_to_monitor();
        self.paint();
    }
}

// SAFETY (all `unsafe` below): thin wrappers over Win32 calls with valid arguments.

extern "system" {
    fn GetCurrentProcess() -> isize;
    fn SetProcessWorkingSetSize(process: isize, min: usize, max: usize) -> i32;
}

/// Hand idle pages back to the OS (startup and DirectWrite/COM init touch a lot once).
/// The working set refills only with what the next frame actually needs.
fn trim_memory() {
    // SAFETY: documented kernel32 call; (-1, -1) means "empty the working set".
    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Append a line to `%TEMP%\isle-perf.log` (callers gate this on the `ISLE_PERF` test knob).
fn perf_log(line: &str) {
    if let Some(dir) = std::env::var_os("TEMP") {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::path::Path::new(&dir).join("isle-perf.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }
}

/// Test switches (env vars) only exist in debug builds; release builds ignore them.
fn test_knob(name: &str) -> bool {
    cfg!(debug_assertions) && std::env::var_os(name).is_some()
}

unsafe fn fullscreen_active(own: HWND) -> bool {
    if test_knob("ISLE_NOFS") {
        return false;
    }
    if let Ok(st) = SHQueryUserNotificationState() {
        if st == QUNS_BUSY || st == QUNS_RUNNING_D3D_FULL_SCREEN || st == QUNS_PRESENTATION_MODE {
            return true;
        }
    }
    let fg = GetForegroundWindow();
    if fg.0.is_null() || fg == own {
        return false;
    }
    let mut cls = [0u16; 64];
    let n = GetClassNameW(fg, &mut cls) as usize;
    let cls = String::from_utf16_lossy(&cls[..n]);
    if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return false;
    }
    let mut r = RECT::default();
    if GetWindowRect(fg, &mut r).is_err() {
        return false;
    }
    let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !GetMonitorInfoW(mon, &mut mi).as_bool() {
        return false;
    }
    let m = mi.rcMonitor;
    let covers = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
    // A maximized window on an auto-hide-taskbar setup also covers the monitor; it keeps its caption.
    let style = GetWindowLongW(fg, GWL_STYLE) as u32;
    covers && style & WS_CAPTION.0 != WS_CAPTION.0
}

unsafe extern "system" fn win_event(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    // Out-of-context hooks run inside GetMessage, possibly under a modal menu loop:
    // only post, never touch `App` here.
    let h = HOOK_HWND.load(Ordering::Relaxed);
    if h != 0 {
        let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_FOREGROUND, WPARAM(0), LPARAM(0));
    }
}

/// A small white capsule with a dark edge, drawn at runtime (no resource compiler needed).
fn make_tray_icon(n: i32) -> Option<HICON> {
    // SAFETY: the DIB section is written within its n*n bounds, then copied by CreateIconIndirect.
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: n,
                biHeight: -n,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let mask = CreateBitmap(n, n, 1, 1, None);
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (n * n) as usize);
        let nf = n as f32;
        let (hh, hw, c) = (nf * 0.26, nf * 0.44, nf / 2.0);
        for y in 0..n {
            for x in 0..n {
                let dx = ((x as f32 + 0.5 - c).abs() - (hw - hh)).max(0.0);
                let d = (dx * dx + (y as f32 + 0.5 - c).powi(2)).sqrt() - hh;
                let fill = (0.5 - d).clamp(0.0, 1.0);
                let edge = (0.5 - (d - 1.0)).clamp(0.0, 1.0);
                // premultiplied: white fill over a dark outline
                let a = fill + edge * (1.0 - fill);
                let v = 255.0 * fill + 0x20 as f32 * edge * (1.0 - fill);
                px[(y * n + x) as usize] = ((a * 255.0) as u32) << 24 | (v as u32) << 16 | (v as u32) << 8 | v as u32;
            }
        }
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

/// Low-level mouse hook: runs on every mouse move system-wide, so it only compares the
/// point with the cached zone rect and posts when the inside/outside state flips.
unsafe extern "system" fn mouse_ll(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == 0 && wp.0 as u32 == WM_MOUSEMOVE {
        let pt = (*(lp.0 as *const MSLLHOOKSTRUCT)).pt;
        let inside = App::point_in_zone(pt.x, pt.y);
        if inside != INSIDE.swap(inside, Ordering::Relaxed) {
            let _ = PostMessageW(Some(HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _)), WM_APP_ZONE, WPARAM(inside as usize), LPARAM(0));
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe fn show_menu(hwnd: HWND) {
    let ru = app().ru;
    let (t_auto, t_reset, t_exit, t_vis, t_wave) = if ru {
        ("Запускать вместе с Windows", "Сбросить позицию", "Выход", "Всегда показывать", "Показывать волну")
    } else {
        ("Start with Windows", "Reset position", "Exit", "Always visible", "Show waveform")
    };
    let Ok(menu) = CreatePopupMenu() else { return };
    let (a, b, c, d, e) = (wide0(t_auto), wide0(t_reset), wide0(t_exit), wide0(t_vis), wide0(t_wave));
    let flags = if settings::autostart_enabled() { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, flags, 1, PCWSTR(a.as_ptr()));
    let vflags = if app().always_visible { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, vflags, 4, PCWSTR(d.as_ptr()));
    let wflags = if app().wave_on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, wflags, 5, PCWSTR(e.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, 2, PCWSTR(b.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(c.as_ptr()));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    app().menu_open = true;
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    let a = app();
    a.menu_open = false;
    match cmd.0 {
        1 => settings::set_autostart(!settings::autostart_enabled()),
        2 => a.reset_position(),
        4 => {
            a.always_visible = !a.always_visible;
            settings::set_always_visible(a.always_visible);
            a.sync_hook(false);
            a.zone_active = INSIDE.load(Ordering::Relaxed);
            if !a.zone_active {
                SetTimer(Some(hwnd), T_HIDE, 600, None);
            }
            a.apply_visibility();
        }
        5 => {
            a.wave_on = !a.wave_on;
            settings::set_waveform(a.wave_on);
            a.update_width_target();
            a.update_zone();
            a.sync_wave();
            a.kick();
        }
        3 => {
            let _ = DestroyWindow(hwnd);
            return;
        }
        _ => {}
    }
    if !a.cursor_in_pill() {
        a.on_leave();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if APP.load(Ordering::Acquire).is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let tbc = WM_TASKBAR_CREATED.load(Ordering::Relaxed);
    if tbc != 0 && msg == tbc {
        app().tray_sync(true);
        return LRESULT(0);
    }
    match msg {
        WM_APP_TRAY => {
            match (lp.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP => app().reveal_for(3000),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let (x, y) = (lp.0 as i16 as i32, (lp.0 >> 16) as i16 as i32);
            LRESULT(if app().hit_at(x, y) == Hit::Outside { HTTRANSPARENT as isize } else { HTCLIENT as isize })
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            app().on_mouse_move();
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            app().on_leave();
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            app().on_lbutton_down();
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // ReleaseCapture sends WM_CAPTURECHANGED synchronously: take the drag first and
            // keep it out of any live `&mut App` across the call.
            let d = app().drag.take();
            let _ = ReleaseCapture();
            app().on_lbutton_up(d);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_menu(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            let a = app();
            match wp.0 {
                T_PROGRESS => a.paint(),
                T_LEAVE => {
                    let _ = KillTimer(Some(hwnd), T_LEAVE);
                    a.leave_pending = false;
                    if !a.menu_open && a.drag.is_none() && !a.cursor_in_pill() {
                        a.request_collapse(Instant::now());
                    }
                }
                T_HIDE => {
                    let _ = KillTimer(Some(hwnd), T_HIDE);
                    if !a.menu_open && a.drag.is_none() {
                        a.zone_active = INSIDE.load(Ordering::Relaxed);
                        a.apply_visibility();
                    }
                }
                T_DEVCHANGE => {
                    let _ = KillTimer(Some(hwnd), T_DEVCHANGE);
                    if let Some(b) = &a.battery {
                        b.refresh();
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_FRAME => {
            app().on_frame();
            LRESULT(0)
        }
        WM_APP_WAVE => {
            app().on_wave();
            LRESULT(0)
        }
        WM_APP_MEDIA => {
            app().on_media();
            LRESULT(0)
        }
        WM_APP_DEVICES => {
            app().on_devices();
            LRESULT(0)
        }
        WM_APP_ZONE => {
            app().on_zone(wp.0 != 0);
            LRESULT(0)
        }
        WM_APP_FOREGROUND => {
            app().on_foreground();
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let _ = PostMessageW(Some(hwnd), WM_APP_DPI, WPARAM(wp.0 & 0xFFFF), LPARAM(0));
            LRESULT(0)
        }
        WM_APP_DPI => {
            app().on_dpi(wp.0 as u32);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            let a = app();
            if a.drag.take().is_some() {
                a.press.target = 1.0;
                a.kick();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let a = app();
            a.clamp_to_monitor();
            a.paint();
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            if wp.0 == DBT_DEVNODES_CHANGED {
                SetTimer(Some(hwnd), T_DEVCHANGE, 2000, None);
            }
            LRESULT(1)
        }
        WM_POWERBROADCAST => {
            if wp.0 == PBT_APMRESUMEAUTOMATIC {
                if let Some(b) = &app().battery {
                    b.refresh();
                }
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            app().tray_remove();
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

pub fn run() -> windows::core::Result<()> {
    // SAFETY: standard window-class registration, window creation and message loop.
    unsafe {
        let shared = Arc::new(Shared::default());
        let mut a0 = Box::new(App::new(shared.clone()));
        let hinst = GetModuleHandleW(None)?;
        let class = w!("isle_pill");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let ex = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE;
        let hwnd = CreateWindowExW(
            ex,
            class,
            w!("isle"),
            WS_POPUP,
            a0.win_x,
            a0.win_y(),
            a0.win_w,
            a0.win_h,
            None,
            None,
            Some(hinst.into()),
            None,
        )?;
        a0.hwnd = hwnd;
        // The window may have landed on a monitor with a different DPI than we guessed.
        let dpi = GetDpiForWindow(hwnd);
        let r = Renderer::new(a0.win_w, a0.win_h);
        a0.r = r.ok();
        if dpi != 0 && (dpi as f32 / 96.0 * UI_SCALE - a0.scale).abs() > 0.01 {
            a0.on_dpi(dpi);
        }
        let hw = hwnd.0 as isize;
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("frames".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    if DwmFlush().is_err() {
                        std::thread::sleep(Duration::from_millis(8));
                    }
                    let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_FRAME, WPARAM(0), LPARAM(0));
                }
            })
            .ok();
        a0.frame_tx = Some(tx);
        a0.wave_thread = std::thread::Builder::new()
            .name("wave".into())
            .spawn(move || loop {
                match WAVE_MODE.load(Ordering::Relaxed) {
                    WAVE_OFF => std::thread::park(),
                    m => {
                        std::thread::sleep(Duration::from_millis(if m == WAVE_FAST { 33 } else { 150 }));
                        if WAVE_MODE.load(Ordering::Relaxed) != WAVE_OFF {
                            let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_WAVE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
            })
            .ok()
            .map(|h| h.thread().clone());
        let sh = shared.clone();
        a0.battery = Some(crate::battery::spawn(Box::new(move |devs| {
            if let Ok(mut g) = sh.devices.lock() {
                *g = devs;
            }
            let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_DEVICES, WPARAM(0), LPARAM(0));
        })));
        a0.media_tx = Some(crate::media::spawn(hw, shared));
        APP.store(Box::into_raw(a0), Ordering::Release);

        // First (transparent) frame, then show and animate in.
        let a = app();
        a.fs_hidden = fullscreen_active(hwnd);
        a.hinst = hinst.into();
        HOOK_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        a.update_zone();
        a.sync_hook(false);
        WM_TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);
        a.tray_sync(true);
        // Auto-hide: start revealed for a moment so the user sees where the island lives.
        a.zone_active = true;
        SetTimer(Some(hwnd), T_HIDE, 2500, None);
        a.apply_visibility();
        let hook = SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(win_event), 0, 0, WINEVENT_OUTOFCONTEXT);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWinEvent(hook);
        if let Some(h) = app().mouse_hook.take() {
            let _ = UnhookWindowsHookEx(h);
        }
        Ok(())
    }
}
