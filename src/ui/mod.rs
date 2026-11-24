//! The pill window: layered popup, hit-testing, drag, hover, spring-driven motion.
//!
//! One UI thread owns `App` (reached from the window procedure through a global pointer).
//! Frames are paced by a helper thread that blocks in `DwmFlush` only while something
//! animates; when everything has settled the thread parks on a channel and nothing runs.

mod banner;
pub mod layout;
mod liquid;
mod render;
mod reveal;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicPtr, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY, POWERBROADCAST_SETTING,
    SYSTEM_POWER_STATUS,
};
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING, GUID_SESSION_DISPLAY_STATUS};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::alerts::{Alert, Queue, Tracker};
use crate::anim::{Spring, Tween};
use crate::audio::Meter;
use crate::battery::{BatteryHandle, LiveMode};
use crate::clock::{self, Clock};
use crate::media::MediaHandle;
use crate::model::{Cover, Device, MediaCommand, MediaSnapshot, Part, PartKind};
use crate::privacy::PrivacyState;
use crate::settings;
use crate::update::{self, Release, Version};
use banner::Item;
use layout::*;
use liquid::{choose_edge, edge_line, shape, Edge, IRect, RestBox};
use render::{BannerDraw, ClockDraw, Ctl, DevDraw, Frame, Liquid, PartsRow, PrivLine, Renderer, WaveDraw};
use reveal::{Ev, Reveal};

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
const WM_APP_PRIVACY: u32 = WM_APP + 9;
const WM_APP_UPDATE: u32 = WM_APP + 10;

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
/// One-shot: saves the Ctrl+wheel size once the wheel has been quiet for `SIZE_SAVE_MS`.
const T_SIZE: usize = 6;
const SIZE_SAVE_MS: u32 = 500;
/// One-shot: the banner has been up long enough.
const T_ALERT: usize = 7;
/// One-shot, re-armed on every second boundary while a clock runs on a visible island.
const T_CLOCK_TICK: usize = 8;
/// One-shot: the timer's deadline (armed whenever a timer runs, shown or not).
const T_CLOCK_DUE: usize = 9;
/// Debug-only: drives the scripted alert tests (`ISLE_TEST_FAKE_CONNECT`, `ISLE_TEST_POWER`).
const T_TEST: usize = 10;
/// One-shot: time for the next update check (first 60 s after start, then 1 h x3 while nothing is found, then every 24 h).
const T_UPDATE: usize = 11;
/// One-shot: the cursor dwelled long enough to reveal the island (armed on enter, killed on leave).
const T_DWELL: usize = 12;
const CHECK_FIRST_MS: u32 = 60_000;
const CHECK_EVERY_MS: u32 = 24 * 3_600_000;
/// How long a banner stays up; update banners get longer, they are worth reading.
const ALERT_MS: u32 = 3000;
const UPDATE_ALERT_MS: u32 = 6000;
const UPDATE_CLICK_GRACE: Duration = Duration::from_millis(700);
/// A failed check is retried after this (the network may simply not be up yet).
const CHECK_RETRY_MS: u32 = 3_600_000;
/// The first checks after start that find nothing are repeated hourly (a stale CDN answer right after a release).
const QUICK_RECHECKS: u32 = 3;
/// A known update is re-offered on a cursor reveal at most this often.
const REOFFER_MS: u64 = 10 * 60_000;

/// Delay until the next check: errors and the first few empty answers retry in 1 h, otherwise 24 h.
/// `nones` counts the empty answers so far (this one included), 0 after a found release.
fn next_check_ms(ok: bool, nones: u32) -> u32 {
    if !ok || (1..=QUICK_RECHECKS).contains(&nones) {
        CHECK_RETRY_MS
    } else {
        CHECK_EVERY_MS
    }
}

/// Whether a cursor reveal may show the update offer again.
fn reoffer_due(last: Option<u64>, now: u64, window: u64) -> bool {
    last.is_none_or(|t| now.saturating_sub(t) >= window)
}
/// Connect alerts are muted this long after resume / unlock (devices reappear then).
const MUTE_MS: u64 = 15_000;

/// Physical px per logical px: monitor DPI times the user's size setting.
fn scale_for(dpi: u32, size_pct: u32) -> f32 {
    dpi as f32 / 96.0 * size_pct as f32 / 100.0
}
/// Minimum distance (logical px) between the pill and every work-area edge.
const EDGE_GAP: f32 = 12.0;
/// Reveal zone padding around the compact pill, logical px.
const ZONE_PAD: f32 = 28.0;
type AtomicRect = [AtomicI32; 4];
const fn atomic_rect() -> AtomicRect {
    [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)]
}
/// Rects (l, t, r, b, physical px) shared with the low-level mouse hook proc: the hold zone (the
/// padded pill, reaching the edge when docked), the compact rest rect (inside the zone) and the
/// hot strip on the screen edge (empty unless docked; also inside the zone).
static ZONE: AtomicRect = atomic_rect();
static REST: AtomicRect = atomic_rect();
static STRIP: AtomicRect = atomic_rect();
static INSIDE: AtomicBool = AtomicBool::new(false);
static IN_REST: AtomicBool = AtomicBool::new(false);
static IN_STRIP: AtomicBool = AtomicBool::new(false);
static HOOK_HWND: AtomicIsize = AtomicIsize::new(0);

const WM_MOUSELEAVE: u32 = 0x02A3;
/// WM_MOUSEWHEEL wParam flag: Ctrl is down.
const MK_CONTROL: usize = 0x0008;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const PBT_APMRESUMEAUTOMATIC: usize = 0x0012;

/// Whether anyone can see the screen: display on, session unlocked, console connected.
/// While not, the island does no periodic work (waveform, BLE watcher, progress timer).
#[derive(Clone, Copy)]
struct Screen {
    display_on: bool,
    locked: bool,
    connected: bool,
}

impl Screen {
    const fn new() -> Self {
        Screen { display_on: true, locked: false, connected: true }
    }

    fn active(self) -> bool {
        self.display_on && !self.locked && self.connected
    }

    /// `GUID_SESSION_DISPLAY_STATUS` payload: 0 off, 1 on, 2 dimmed (still visible).
    fn set_display(&mut self, state: u32) {
        self.display_on = state != 0;
    }

    /// `WM_WTSSESSION_CHANGE` wParam.
    fn session_event(&mut self, ev: u32) {
        match ev {
            WTS_SESSION_LOCK => self.locked = true,
            WTS_SESSION_UNLOCK => self.locked = false,
            WTS_CONSOLE_CONNECT | WTS_REMOTE_CONNECT => self.connected = true,
            WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT => self.connected = false,
            _ => {}
        }
    }
}

/// Data the worker threads hand to the UI (paired with a `PostMessageW`).
#[derive(Default)]
pub struct Shared {
    pub media: Mutex<Option<MediaSnapshot>>,
    pub devices: Mutex<Vec<Device>>,
    pub privacy: Mutex<PrivacyState>,
    pub update: Mutex<Vec<UpdateMsg>>,
}

/// What the update worker threads report (paired with `WM_APP_UPDATE`).
pub enum UpdateMsg {
    /// A check finished: the newer release if any, or `Err` when it could not be done (logged by the worker).
    Checked(Result<Option<Release>, ()>),
    Installed(InstallResult),
}

pub enum InstallResult {
    /// Downloaded, verified, swapped; the new exe is running and holds the mutex.
    Restarted,
    Failed,
    /// The new exe is in place but could not be started: retrying would only destroy the rollback copy.
    SwappedNoRestart,
}

/// The install the user started, shown outside the alert queue (it must show even with notifications off).
#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateUi {
    Working,
    Failed,
    Installed,
}

/// Append a line to the debug log (`ISLE_PERF` knob, debug builds only; release builds log nothing).
fn debug_log(line: &str) {
    if test_knob("ISLE_PERF") {
        perf_log(line);
    }
}

/// Hand an update message to the UI thread.
fn post_update(sh: &Shared, hw: isize, m: UpdateMsg) {
    if let Ok(mut g) = sh.update.lock() {
        g.push(m);
    }
    // SAFETY: posting to our own window; a destroyed window just drops the message.
    unsafe {
        let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
    }
}

/// How long `a` stays on the banner before it goes (sticky ones never time out).
fn alert_ms(a: &Alert) -> u32 {
    match a {
        Alert::UpdateAvailable { .. } | Alert::UpdateFailed | Alert::UpdateInstalled => UPDATE_ALERT_MS,
        _ => ALERT_MS,
    }
}

/// `GetTickCount64`, the time base of `alerts` and `clock`.
fn now_ms() -> u64 {
    // SAFETY: plain query.
    unsafe { GetTickCount64() }
}

/// The one place that decides whether the island is on screen: always-visible, hover (cursor near,
/// menu or drag) or an alert banner, and never while a fullscreen app hides it.
fn want_shown(fs_hidden: bool, always: bool, hover: bool, alert: bool) -> bool {
    !fs_hidden && (always || hover || alert)
}

/// `GetSystemPowerStatus` fields to `(on AC, percent)`; `None` without a usable system battery
/// (desktop PC: flag 128 / 255) or with unknown values.
fn power_sample(ac_line: u8, battery_flag: u8, percent: u8) -> Option<(bool, u8)> {
    if battery_flag == 255 || battery_flag & 128 != 0 || percent > 100 {
        return None;
    }
    match ac_line {
        0 => Some((false, percent)),
        1 => Some((true, percent)),
        _ => None,
    }
}

/// The banner on (or leaving) the compact row.
struct ActiveBanner {
    alert: Alert,
    items: Vec<Item>,
    /// (x, width) per item, and the total width, logical px.
    xs: Vec<(f32, f32)>,
    total: f32,
    /// Stays until clicked (the timer) or finished (the install); the others go after `alert_ms`.
    sticky: bool,
    /// False while it fades out.
    live: bool,
    /// When it came up: an update offer ignores clicks for a moment, so a click aimed at something else never installs.
    shown_at: Instant,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

fn app() -> &'static mut App {
    // SAFETY: set once before the message loop starts; only the UI thread dereferences it,
    // and handlers never keep a reference across a call that pumps messages.
    unsafe { &mut *APP.load(Ordering::Acquire) }
}

/// Step of the back/forward buttons.
const SEEK_STEP_MS: i64 = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Outside,
    Pill,
    /// The clock segment (expanded only): a click stops the clock.
    Clock,
    Ctl(Ctl),
    /// The progress bar's seek strip (expanded, seekable only).
    Bar,
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
    r: Option<Renderer>,
    ru: bool,

    // geometry (physical px)
    dpi: u32,
    size_pct: u32,
    /// Unspent Ctrl+wheel delta (fractions of a notch).
    wheel_acc: i32,
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
    /// Same target as `vis`, softer spring: the pair drives the liquid squash.
    q: Spring,
    /// Edge the pill emerges from (and its screen coordinate), chosen at the start of a show / hide.
    edge: Option<(Edge, i32)>,
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
    /// Cursor-reveal state machine (see `reveal`).
    rv: Reveal,
    mouse_hook: Option<HHOOK>,
    hinst: HINSTANCE,
    shown: bool,
    screen: Screen,
    screen_notify: isize,
    drag: Option<Drag>,

    // content
    media: Option<MediaSnapshot>,
    prev_title: String,
    prev_artist: String,
    prev_cover: Option<Arc<Cover>>,
    devs: Vec<DevView>,

    // alerts
    tracker: Tracker,
    queue: Queue,
    alerts_on: bool,
    banner: Option<ActiveBanner>,
    /// Cross-fade between the normal compact row (0) and the banner (1).
    banner_t: Tween,
    /// A banner is up: part of the visibility predicate.
    alert_active: bool,
    /// The banner's time is up but the cursor is over the island: dismiss once it leaves.
    alert_hold: bool,
    timer_total_ms: u64,
    timer_sound: bool,
    power_notify: [isize; 2],

    // updates
    update_on: bool,
    /// The newest release offered so far (menu item + install target).
    update_rel: Option<Release>,
    /// The last version announced with a banner: each version is announced once per run.
    update_offered: Option<Version>,
    /// `GetTickCount64` of the last update banner pushed (first offer or re-offer).
    offer_at: Option<u64>,
    /// Checks that found nothing so far.
    none_checks: u32,
    update_ui: Option<UpdateUi>,
    check_busy: bool,
    /// `GetTickCount64` of the last finished check.
    last_check: Option<u64>,
    test_n: u32,
    fake_connect: bool,

    // clock and privacy
    clock: Clock,
    /// Width of the clock segment (logical px), springing between 0 and `clk_full`.
    clk: Spring,
    clk_full: f32,
    /// Label shown while the segment fades out after a stop.
    clk_last: String,
    clk_tick: bool,
    privacy: PrivacyState,
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
        let saved = test_pos().or_else(settings::load_pos);
        let (work, dpi) = match saved {
            Some((x, y)) => work_area(x, y, false),
            None => work_area(0, 0, true),
        };
        let mut a = App {
            hwnd: HWND::default(),
            shared,
            media_tx: None,
            battery: None,
            r: None,
            ru,
            dpi,
            size_pct: settings::size_pct(),
            wheel_acc: 0,
            scale: 0.0,
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
            q: Spring::new(0.0, 0.30, 0.85),
            edge: None,
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
            rv: Reveal::default(),
            mouse_hook: None,
            hinst: HINSTANCE::default(),
            shown: false,
            screen: Screen::new(),
            screen_notify: 0,
            drag: None,
            media: None,
            prev_title: String::new(),
            prev_artist: String::new(),
            prev_cover: None,
            devs: Vec::new(),
            tracker: Tracker::new(),
            queue: Queue::default(),
            alerts_on: settings::alerts(),
            banner: None,
            banner_t: Tween::at(0.0, now),
            alert_active: false,
            alert_hold: false,
            timer_total_ms: 0,
            timer_sound: false,
            power_notify: [0; 2],
            update_on: settings::check_updates(),
            update_rel: None,
            update_offered: None,
            offer_at: None,
            none_checks: 0,
            update_ui: None,
            check_busy: false,
            last_check: None,
            test_n: 0,
            fake_connect: false,
            clock: Clock::Off,
            clk: Spring::new(0.0, 0.40, 0.90),
            clk_full: 0.0,
            clk_last: String::new(),
            clk_tick: false,
            privacy: PrivacyState::default(),
        };
        a.scale = scale_for(dpi, a.size_pct);
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
        // A saved spot that had to be pushed inside the gap is stored again, so it stays fixed.
        if test_pos().is_none() && saved.is_some_and(|p| p != (a.pill_left(), a.top_y)) {
            settings::save_pos(a.pill_left(), a.top_y);
        }
        a
    }

    fn recompute_size(&mut self) {
        let s = self.scale;
        // Glow plus a collapsed-pill-high margin: the liquid pill travels between the edge and its rest spot.
        self.gm = ((GLOW + PILL_H) * s).ceil() as i32;
        self.win_w = (MAX_W * s).ceil() as i32 + 2 * self.gm;
        self.win_h = ((EXP_H + PART_ROW_H * MAX_PART_ROWS as f32) * s).ceil() as i32 + 2 * self.gm;
    }

    fn place_default(&mut self, work: &RECT) {
        self.frac = 0.5;
        self.win_x = (work.left + work.right) / 2 - self.win_w / 2;
        self.top_y = work.top + (EDGE_GAP * self.scale).round() as i32;
    }

    fn win_y(&self) -> i32 {
        self.top_y - self.gm - if self.up { (UP_ROOM * self.scale).round() as i32 } else { 0 }
    }

    /// Physical left edge of the pill at its target width.
    fn pill_left(&self) -> i32 {
        self.win_x + self.gm + ((MAX_W - self.width.target) * self.frac * self.scale).round() as i32
    }

    /// Keeps the pill at least `EDGE_GAP` logical px inside the work area on every side (the liquid
    /// show needs a real gap to an edge). Every position change (load, drag, DPI, display, reset) ends here.
    fn clamp_to_monitor(&mut self) {
        let left = self.pill_left();
        let wpx = (self.width.target * self.scale).round() as i32;
        let (work, _) = work_area(left + wpx / 2, self.top_y, false);
        let gap = (EDGE_GAP * self.scale).round() as i32;
        let (lo, hi) = (work.left + gap, work.right - gap - wpx);
        let dx = left.min(hi.max(lo)).max(lo) - left;
        self.win_x += dx;
        let hpx = (PILL_H * self.scale).round() as i32;
        let (lo, hi) = (work.top + gap, work.bottom - gap - hpx);
        self.top_y = self.top_y.min(hi.max(lo)).max(lo);
        self.update_zone();
    }

    /// Rects for the hook proc: the compact pill (or the expanded one) inflated by `ZONE_PAD` is the
    /// hold zone; the compact pill alone is the rest rect. At a screen edge the zone also reaches the
    /// edge and a hot strip there is the reveal trigger.
    fn update_zone(&mut self) {
        let s = self.scale;
        let pad = (ZONE_PAD * s).round() as i32;
        let left = self.pill_left();
        let w = (self.width.target * s).round() as i32;
        let h = (PILL_H * s).round() as i32;
        let extra = if self.want_exp { ((EXP_H - PILL_H + PART_ROW_H * self.rows_t.target()) * s).round() as i32 } else { 0 };
        let (top, bottom) = if self.up { (self.top_y - extra, self.top_y + h) } else { (self.top_y, self.top_y + h + extra) };
        let rest = IRect { l: left, t: self.top_y, r: left + w, b: self.top_y + h };
        let mut zone = IRect { l: left - pad, t: top - pad, r: left + w + pad, b: bottom + pad };
        let mut strip = reveal::EMPTY;
        let dock = self.dock_edge(rest);
        if let Some((e, work, mon)) = dock {
            strip = reveal::edge_strip(e, mon, work, rest, pad);
            zone = reveal::union(zone, strip);
        }
        self.rv.docked = dock.is_some();
        for (a, r) in [(&ZONE, zone), (&REST, rest), (&STRIP, strip)] {
            for (a, v) in a.iter().zip([r.l, r.t, r.r, r.b]) {
                a.store(v, Ordering::Relaxed);
            }
        }
    }

    fn in_rect(a: &AtomicRect, x: i32, y: i32) -> bool {
        x >= a[0].load(Ordering::Relaxed)
            && y >= a[1].load(Ordering::Relaxed)
            && x < a[2].load(Ordering::Relaxed)
            && y < a[3].load(Ordering::Relaxed)
    }

    /// The cursor revealed the island and has left it: click-through until the hide delay passes.
    fn click_through(&self) -> bool {
        self.rv.revealed && !self.rv.in_zone && !self.alert_active && !self.always_visible && !self.menu_open && self.drag.is_none()
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

    /// Both show / hide springs at rest. The liquid shape squashes by `0.6 * (vis - q)`, so it needs a
    /// tighter tolerance than the plain fade for the last frame to match the settled pill.
    fn vis_settled(&self) -> bool {
        let (e, ev) = if self.edge.is_some() { (0.001, 0.01) } else { (0.004, 0.02) };
        self.vis.settled(e, ev) && self.q.settled(e, ev)
    }

    fn is_animating(&self, now: Instant) -> bool {
        !self.open.settled(0.004, 0.02)
            || !self.width.settled(0.5, 2.0)
            || !self.vis_settled()
            || (self.press_ctl.is_some() && !self.press.settled(0.002, 0.02))
            || self.collapse_at.is_some()
            || !self.content.done(now)
            || !self.rows_t.done(now)
            || !self.text_t.done(now)
            || !self.cover_t.done(now)
            || !self.pp_t.done(now)
            || !self.banner_t.done(now)
            || !self.clk.settled(0.5, 2.0)
            || self.devs.iter().any(|d| !d.pres.done(now) || !d.pct_t.done(now) || !d.chg_t.done(now))
    }

    fn tick(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.open.step(dt);
        self.width.step(dt);
        self.clk.step(dt);
        self.vis.step(dt);
        self.q.step(dt);
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
        if self.clk.settled(0.5, 2.0) {
            self.clk.snap();
        }
        if self.banner.as_ref().is_some_and(|b| !b.live) && self.banner_t.done(now) {
            self.banner = None;
        }
        if self.vis_settled() {
            self.vis.snap();
            self.q.snap();
        }
        // Liquid hide: once the body is beyond the edge, the springs' long tails would only repaint nothing.
        if self.edge.is_some() && self.vis.target == 0.0 && self.vis.x < 0.02 && self.q.x < 0.3 {
            self.vis.snap();
            self.q.snap();
        }
        if self.press.settled(0.002, 0.02) {
            self.press.snap();
            if self.press.target == 1.0 {
                self.press_ctl = None;
            }
        }
        self.devs.retain(|d| !(d.removing && d.pres.done(now)));
        if self.vis.target == 0.0 && self.vis.x == 0.0 && self.q.x == 0.0 && self.shown {
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

    /// The island is shown (not on its way out) and the screen is actually lit and unlocked.
    fn on_screen(&self) -> bool {
        self.shown && self.vis.target > 0.0 && self.screen.active()
    }

    /// Display or session state changed: stop or resume the periodic work.
    fn on_screen_change(&mut self, was_active: bool) {
        if self.screen.active() == was_active {
            return;
        }
        self.sync_wave();
        self.sync_live();
        self.sync_progress_timer();
        self.sync_alerts();
        if self.screen.active() {
            self.kick();
        }
    }

    /// The bars are on screen (or about to be): enabled, on screen, media present.
    fn wave_visible(&self) -> bool {
        self.wave_on && self.on_screen() && self.media.is_some()
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

    /// The liquid state of this frame; `None` at rest (the plain renderer takes over) or without an edge.
    fn liquid_frame(&self, pill: Rect, radius: f32) -> Option<Liquid> {
        let (edge, line_px) = self.edge?;
        if self.vis.x == 1.0 && self.q.x == 1.0 {
            return None;
        }
        let s = self.scale;
        let line = if edge.vertical() { (line_px - self.win_x) as f32 / s } else { (line_px - self.win_y()) as f32 / s };
        let g = match edge {
            Edge::Top => pill.t - line,
            Edge::Bottom => line - pill.b,
            Edge::Left => pill.l - line,
            Edge::Right => line - pill.r,
        };
        let b = RestBox { w: pill.w(), h: pill.b - pill.t, hc: PILL_H, g, r: radius, rs_max: liquid::SHOULDER };
        Some(Liquid { edge, line, centre: ((pill.l + pill.r) / 2.0, (pill.t + pill.b) / 2.0), sh: shape(edge, &b, self.vis.x, self.q.x) })
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
            can_seek: m.is_some_and(|m| m.can_seek && m.duration_ms > 0),
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
            liquid: self.liquid_frame(pill, radius),
            banner: self.banner_draw(now),
            clock: self.clock_draw(),
            dots: (!self.privacy.mic.is_empty(), !self.privacy.cam.is_empty()),
            priv_line: banner::privacy_line(&self.privacy, self.ru).map(|text| PrivLine {
                text,
                mic: !self.privacy.mic.is_empty(),
                cam: !self.privacy.cam.is_empty(),
            }),
        }
    }

    fn banner_draw(&self, now: Instant) -> Option<BannerDraw> {
        let b = self.banner.as_ref()?;
        let t = self.banner_t.val(now);
        (t > 0.001).then(|| BannerDraw {
            t,
            items: b.items.iter().zip(&b.xs).map(|(i, &(x, w))| (i.clone(), x, w)).collect(),
            total: b.total,
        })
    }

    fn clock_draw(&self) -> Option<ClockDraw> {
        let running = self.clock != Clock::Off;
        if !running && self.clk.x < 0.5 {
            return None;
        }
        let label = if running { clock::label(&self.clock, now_ms()) } else { self.clk_last.clone() };
        let (d, c) = self.r.as_ref().map_or((12.0, 6.0), |r| r.clock_metrics());
        let (cells, total) = banner::clock_cells(&label, d, c);
        Some(ClockDraw { cells, total, w: self.clk.x.max(0.0), full: self.clk_full })
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
        if test_knob("ISLE_TEST_DUMP") && f.content >= 0.99 && f.has_media && f.progress.is_some() {
            static DUMPED: AtomicBool = AtomicBool::new(false);
            if !DUMPED.swap(true, Ordering::Relaxed) {
                if let Some(d) = std::env::var_os("ISLE_TEST_DUMP") {
                    r.dump_bmp(std::path::Path::new(&d));
                    // Screen px of the seek controls, for injected-click tests.
                    let g = self.geometry().2;
                    let sx = |x: f32| self.win_x as f32 + x * self.scale;
                    let sy = |y: f32| self.win_y() as f32 + (y + 0.0) * self.scale;
                    let txt = format!(
                        "back {} {}
fwd {} {}
bar {} {} {}
",
                        sx(g.back.0) as i32,
                        sy(g.back.1) as i32,
                        sx(g.fwd.0) as i32,
                        sy(g.fwd.1) as i32,
                        sx(g.bar.l) as i32,
                        sx(g.bar.r) as i32,
                        sy((g.bar.t + g.bar.b) / 2.0) as i32
                    );
                    let _ = std::fs::write(std::path::Path::new(&d).with_extension("txt"), txt);
                }
            }
        }
        self.r = Some(r);
        if self.anim_running {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.perf = (self.perf.0 + 1, self.perf.1 + ms, self.perf.2.max(ms));
        }
    }

    // ---- hit-testing ----

    fn hit_at(&self, px: i32, py: i32) -> Hit {
        if !self.shown || self.vis.target == 0.0 || self.click_through() {
            return Hit::Outside;
        }
        let s = self.scale;
        let (x, y) = ((px - self.win_x) as f32 / s, (py - self.win_y()) as f32 / s);
        let (pill, radius, g, row1, _) = self.geometry();
        if !in_pill(pill, radius, x, y) {
            return Hit::Outside;
        }
        let near = |p: (f32, f32)| Rect::around(p.0, p.1, 32.0, 32.0).contains(x, y);
        let seekable = self.media.as_ref().is_some_and(|m| m.can_seek && m.duration_ms > 0);
        // While a banner covers the compact row its controls are not there: a click on it only dismisses (or, for an update offer, starts the install).
        let banner = self.banner_live();
        let expanded = self.content.target() == 1.0 && self.open.x > 0.9;
        let clock = Rect { l: pill.r - PAD - self.clk.x, t: row1, r: pill.r, b: row1 + PILL_H };
        if !banner && g.cover.contains(x, y) {
            Hit::Ctl(Ctl::Cover)
        } else if !banner && near(g.pp1) {
            Hit::Ctl(Ctl::Play1)
        } else if expanded && !banner && self.clock != Clock::Off && clock.contains(x, y) {
            Hit::Clock
        } else if expanded {
            if seekable && near(g.back) {
                Hit::Ctl(Ctl::Back)
            } else if seekable && near(g.fwd) {
                Hit::Ctl(Ctl::Fwd)
            } else if seekable && g.bar_hit.contains(x, y) {
                Hit::Bar
            } else if near(g.prev) {
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
            Ctl::Back => MediaCommand::SeekBy(-SEEK_STEP_MS),
            Ctl::Fwd => MediaCommand::SeekBy(SEEK_STEP_MS),
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
        }
        self.sync_live();
        self.sync_progress_timer();
        self.update_zone();
        self.kick();
    }

    /// Earbud L/R/case levels come from a BLE watcher in the battery worker: continuous
    /// while expanded, duty-cycled while merely shown, off while hidden.
    fn sync_live(&self) {
        let mode = match (self.on_screen(), self.want_exp) {
            (false, _) => LiveMode::Off,
            (true, false) => LiveMode::Duty,
            (true, true) => LiveMode::Continuous,
        };
        if let Some(b) = &self.battery {
            b.set_live(mode);
        }
    }

    fn request_collapse(&mut self, now: Instant) {
        if !self.want_exp && self.open.target == 0.0 {
            return;
        }
        self.want_exp = false;
        self.sync_live();
        self.content.go(0.0, 0.10, now);
        self.collapse_at = Some(now + Duration::from_millis(90));
        self.sync_progress_timer();
        self.update_zone();
        self.kick();
    }

    fn sync_progress_timer(&mut self) {
        let want =
            self.shown && self.screen.active() && self.want_exp && self.media.as_ref().is_some_and(|m| m.playing && m.duration_ms > 0);
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
        self.sync_clock_tick(false);
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
                // The pill is user-placed now: drop any liquid motion (its edge line would go stale).
                self.vis.snap();
                self.q.snap();
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
        self.resume_hide();
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
        } else if d.hit == Hit::Clock {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            if self.hit_at(p.x, p.y) == Hit::Clock {
                self.stop_clock();
            }
        } else if d.hit == Hit::Pill && self.banner_live() {
            self.on_banner_click();
        } else if d.hit == Hit::Bar {
            let mut p = POINT::default();
            // SAFETY: valid out pointer.
            unsafe {
                let _ = GetCursorPos(&mut p);
            }
            if self.hit_at(p.x, p.y) == Hit::Bar {
                let bar = self.geometry().2.bar;
                let x = (p.x - self.win_x) as f32 / self.scale;
                let dur = self.media.as_ref().map_or(0, |m| m.duration_ms);
                let to = (((x - bar.l) / bar.w()).clamp(0.0, 1.0) * dur as f32) as u64;
                if let Some(m) = &self.media_tx {
                    m.send(MediaCommand::SeekTo(to));
                }
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
                    || o.can_seek != n.can_seek
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
        if self.fake_connect {
            // debug-only (ISLE_TEST_FAKE_CONNECT): earbuds that "connect" a few seconds after start
            new.push(Device {
                id: "fake-connect".into(),
                name: "Redmi Buds 6".into(),
                kind: crate::model::DeviceKind::Headset,
                percent: 85,
                charging: None,
                parts: vec![
                    Part { kind: PartKind::Left, percent: 85, charging: false },
                    Part { kind: PartKind::Right, percent: 12, charging: false },
                    Part { kind: PartKind::Case, percent: 60, charging: false },
                ],
            });
        }
        if test_knob("ISLE_FAKE_GAMEPAD") {
            // debug-only: a charging gamepad
            new.push(Device {
                id: "fake-pad".into(),
                name: "Xbox Wireless Controller".into(),
                kind: crate::model::DeviceKind::Gamepad,
                percent: 64,
                charging: Some(true),
                parts: Vec::new(),
            });
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
        let found = self.tracker.on_devices(now_ms(), &new);
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
        self.push_alerts(found);
        if self.shown {
            self.kick();
        }
    }

    fn update_width_target(&mut self) {
        let items: Vec<f32> = self.shown_devs().filter(|d| !d.removing).map(|d| d.full_w()).collect();
        let clock = if self.clock == Clock::Off { 0.0 } else { self.clk_full };
        let base = target_width(&items, self.wave_on, clock);
        self.width.target = match self.banner.as_ref().filter(|b| b.live) {
            Some(b) => banner::pill_width(base, b.total, MAX_W),
            None => base,
        };
        self.width.tune(0.40, 0.80);
    }

    // ---- system events ----

    fn on_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        self.rescale(false);
    }

    /// Recompute everything that derives from `scale`. `anchor`: a size change keeps the gap to the
    /// nearest work-area edge and the centre along it; a DPI change keeps the left edge / centre.
    fn rescale(&mut self, anchor: bool) {
        let old = self.scale;
        let left = self.pill_left();
        let center = self.win_x + self.win_w / 2;
        let tw = self.width.target;
        let wpx = |s: f32| (tw * s).round() as i32;
        let hpx = |s: f32| (PILL_H * s).round() as i32;
        self.scale = scale_for(self.dpi, self.size_pct);
        let s = self.scale;
        self.recompute_size();
        if anchor {
            let pill = IRect { l: left, t: self.top_y, r: left + wpx(old), b: self.top_y + hpx(old) };
            let (work, _) = work_area(left + wpx(old) / 2, self.top_y, false);
            let work = IRect { l: work.left, t: work.top, r: work.right, b: work.bottom };
            let (nl, nt) = anchor_pos(work, pill, s / old, wpx(s), hpx(s));
            self.top_y = nt;
            self.win_x = nl - self.gm - ((MAX_W - tw) * self.frac * s).round() as i32;
        } else if self.frac == 0.0 {
            self.win_x = left - self.gm;
        } else {
            self.win_x = center - self.win_w / 2;
        }
        if self.open.x > 0.0 {
            // Expanded now: the room below / above changed with the scale.
            let (w, _) = work_area(self.pill_left(), self.top_y, false);
            self.up = !expand_down(self.top_y, s, PART_ROW_H * MAX_PART_ROWS as f32, w.top, w.bottom);
        }
        self.clamp_to_monitor();
        if let Some(r) = &mut self.r {
            let _ = r.resize(self.win_w, self.win_h);
        }
        self.tray_sync(false);
        self.paint();
    }

    /// Apply a new size live: finish any show / expand motion first so nothing animates across the resize.
    fn set_size(&mut self, pct: u32) {
        let pct = clamp_pct(pct as i32);
        if pct == self.size_pct {
            return;
        }
        self.size_pct = pct;
        for sp in [&mut self.open, &mut self.width, &mut self.vis, &mut self.q, &mut self.press] {
            sp.snap();
        }
        self.rescale(true);
        self.kick();
    }

    /// Ctrl + wheel over the pill: +-`WHEEL_STEP_PCT` per notch, saved after the wheel goes quiet.
    fn on_wheel(&mut self, delta: i32) {
        if self.drag.is_some() {
            return;
        }
        let (acc, steps) = wheel_steps(self.wheel_acc, delta);
        self.wheel_acc = acc;
        if steps != 0 {
            self.set_size(clamp_pct(self.size_pct as i32 + steps * WHEEL_STEP_PCT));
            // SAFETY: (re)arms a one-shot timer on our own window.
            unsafe {
                SetTimer(Some(self.hwnd), T_SIZE, SIZE_SAVE_MS, None);
            }
        }
    }

    /// Store the size (and, if the user placed the island, its position, which moved with the size).
    fn save_size(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_SIZE);
        }
        settings::set_size_pct(self.size_pct);
        if test_pos().is_none() && settings::load_pos().is_some() {
            settings::save_pos(self.pill_left(), self.top_y);
        }
    }

    fn on_foreground(&mut self) {
        // SAFETY: queries about the foreground window.
        let hide = unsafe { fullscreen_active(self.hwnd) };
        if hide == self.fs_hidden {
            return;
        }
        self.fs_hidden = hide;
        self.sync_hook(!hide);
        self.sync_alerts();
        self.apply_visibility();
    }

    fn apply_visibility(&mut self) {
        let hover = self.zone_active || self.menu_open || self.drag.is_some();
        let want = want_shown(self.fs_hidden, self.always_visible, hover, self.alert_active);
        self.set_vis(want);
        if want && !self.shown {
            self.shown = true;
            // SAFETY: shows our own window without activating it.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
            self.sync_progress_timer();
        }
        self.sync_wave();
        self.sync_live();
        self.sync_clock_tick(false);
        self.kick();
    }

    /// Start or reverse the show / hide motion. The edge is picked only when starting from rest;
    /// a reversal keeps it (and the velocity of both springs).
    fn set_vis(&mut self, show: bool) {
        let t = if show { 1.0 } else { 0.0 };
        if self.vis.target == t {
            return;
        }
        if self.vis_settled() {
            self.edge = self.pick_edge();
        }
        self.vis.target = t;
        self.q.target = t;
        let ((pr, pz), (qr, qz)) = match (self.edge.is_some(), show) {
            (true, true) => ((0.42, 0.78), (0.5, 0.65)),
            (true, false) => ((0.35, 1.0), (0.5, 1.0)),
            (false, _) => ((0.30, 0.85), (0.30, 0.85)),
        };
        self.vis.tune(pr, pz);
        self.q.tune(qr, qz);
    }

    /// The screen edge the pill should emerge from, from its rest rect and the monitor it is on.
    fn pick_edge(&self) -> Option<(Edge, i32)> {
        if test_knob("ISLE_NO_LIQUID") || !client_animations() {
            return None;
        }
        let s = self.scale;
        let (pill, ..) = self.geometry();
        let (ox, oy) = (self.win_x, self.win_y());
        let rest = IRect {
            l: ox + (pill.l * s).round() as i32,
            t: oy + (pill.t * s).round() as i32,
            r: ox + (pill.r * s).round() as i32,
            b: oy + (pill.b * s).round() as i32,
        };
        let (e, work, _) = self.dock_edge(rest)?;
        Some((e, edge_line(e, work)))
    }

    /// The screen edge an island resting at `rest` is docked to (the one `choose_edge` picks), with the
    /// monitor's work area and full rect. `None`: not at an edge.
    fn dock_edge(&self, rest: IRect) -> Option<(Edge, IRect, IRect)> {
        let s = self.scale;
        let r = RECT { left: rest.l, top: rest.t, right: rest.r, bottom: rest.b };
        // SAFETY: plain monitor queries with valid pointers.
        let (work, mon) = unsafe {
            let m = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(m, &mut mi).as_bool() {
                return None;
            }
            let ir = |r: RECT| IRect { l: r.left, t: r.top, r: r.right, b: r.bottom };
            (ir(mi.rcWork), ir(mi.rcMonitor))
        };
        let hc = (PILL_H * s).round() as i32;
        let min_gap = (liquid::MIN_GAP * s).round() as i32;
        // SAFETY: point query only.
        let other = |x, y| unsafe { !MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL).is_invalid() };
        choose_edge(rest, work, mon, hc, min_gap, other).map(|e| (e, work, mon))
    }

    /// A cursor event from the mouse hook or a timer: run the reveal state machine.
    fn on_reveal(&mut self, ev: Ev) {
        let (st, act) = reveal::step(self.rv, ev);
        self.rv = st;
        if ev == Ev::LeaveZone {
            self.request_collapse(Instant::now());
        }
        // SAFETY: timers on our own window.
        unsafe {
            match act {
                reveal::Act::None => {}
                reveal::Act::Keep => {
                    let _ = KillTimer(Some(self.hwnd), T_HIDE);
                }
                reveal::Act::Reveal => {
                    let _ = KillTimer(Some(self.hwnd), T_HIDE);
                    let _ = KillTimer(Some(self.hwnd), T_DWELL);
                    let was = self.zone_active;
                    self.zone_active = true;
                    self.apply_visibility();
                    if !was {
                        self.reoffer_update();
                    }
                }
                reveal::Act::Hide(ms) => {
                    SetTimer(Some(self.hwnd), T_HIDE, ms, None);
                }
                reveal::Act::ArmDwell(ms) => {
                    SetTimer(Some(self.hwnd), T_DWELL, ms, None);
                }
                reveal::Act::CancelDwell => {
                    let _ = KillTimer(Some(self.hwnd), T_DWELL);
                }
            }
        }
    }

    /// A menu or drag held back the hide: start it now if the cursor is away.
    fn resume_hide(&mut self) {
        if self.rv.revealed && !self.rv.in_zone {
            // SAFETY: timer on our own window.
            unsafe {
                SetTimer(Some(self.hwnd), T_HIDE, reveal::HIDE_MS, None);
            }
        }
    }

    /// The hide delay is over (or a tray / startup reveal ran out): hide unless the cursor holds it.
    fn on_hide_timer(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_HIDE);
        }
        if !self.menu_open && self.drag.is_none() {
            self.rv = reveal::step(self.rv, Ev::HideDone).0;
            self.zone_active = self.rv.revealed;
            self.apply_visibility();
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
        let ok = unsafe { GetCursorPos(&mut p).is_ok() };
        let inside = ok && Self::in_rect(&ZONE, p.x, p.y);
        INSIDE.store(inside, Ordering::Relaxed);
        IN_REST.store(inside && Self::in_rect(&REST, p.x, p.y), Ordering::Relaxed);
        IN_STRIP.store(inside && Self::in_rect(&STRIP, p.x, p.y), Ordering::Relaxed);
        self.rv.in_zone = inside;
        if !inside {
            // Nothing holds a cursor reveal any more (the hook was off meanwhile).
            if self.rv.revealed {
                self.zone_active = false;
            }
            self.rv.revealed = false;
            self.rv.dwell = false;
            // SAFETY: timer on our own window.
            unsafe {
                let _ = KillTimer(Some(self.hwnd), T_DWELL);
            }
        }
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
        // This reveal ends on its own timer, not on the cursor.
        self.rv.revealed = false;
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

// ---- alerts, clock, privacy ----

/// What a menu command does to the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClockCmd {
    Timer(u64),
    Stopwatch,
    Stop,
}

/// Menu command ids of the timer submenu: `CLOCK_CMD + minutes`, then the two fixed ones.
const CLOCK_CMD: usize = 400;
const CLOCK_MINUTES: [u64; 6] = [1, 5, 10, 15, 25, 60];
const CLOCK_STOPWATCH: usize = 480;
const CLOCK_STOP: usize = 481;
/// Menu id of the notifications toggle.
const ALERTS_CMD: usize = 6;
/// Menu ids: install the known update, toggle the daily update check.
const UPDATE_CMD: usize = 8;
const UPDATE_PAGE_CMD: usize = 12;
const CHECK_CMD: usize = 9;

fn clock_cmd(id: usize) -> Option<ClockCmd> {
    match id {
        CLOCK_STOPWATCH => Some(ClockCmd::Stopwatch),
        CLOCK_STOP => Some(ClockCmd::Stop),
        _ => CLOCK_MINUTES.iter().find(|&&m| CLOCK_CMD + m as usize == id).map(|&m| ClockCmd::Timer(m)),
    }
}

/// The system notification sound, once. Debug perf runs log whether the call went through.
fn play_notification() {
    // SAFETY: plays a registered sound alias asynchronously; no buffers are involved.
    let ok = unsafe { PlaySoundW(w!("SystemNotification"), None, SND_ALIAS | SND_ASYNC) };
    if test_knob("ISLE_PERF") {
        perf_log(&format!("notification sound: {}\n", ok.as_bool()));
    }
}

impl App {
    fn banner_live(&self) -> bool {
        self.banner.as_ref().is_some_and(|b| b.live)
    }

    /// Banners may be on screen: enabled, the screen is lit and unlocked, no fullscreen app in front.
    fn can_alert(&self) -> bool {
        self.alerts_on && self.screen.active() && !self.fs_hidden
    }

    /// Queue what the tracker found (unless alerts are off) and bring the banner up to date.
    fn push_alerts(&mut self, found: Vec<Alert>) {
        if found.is_empty() {
            return;
        }
        if self.alerts_on {
            let now = now_ms();
            for a in found {
                self.queue.push(a, now);
            }
        }
        self.sync_alerts();
    }

    /// Make the banner match the queue: show the head when allowed, take the banner down when not.
    fn sync_alerts(&mut self) {
        if !self.alerts_on {
            self.queue = Queue::default();
        }
        let can = self.can_alert();
        let head = match self.update_ui {
            // The install's own banner: user-started, so independent of the notifications switch.
            Some(UpdateUi::Working) if self.screen.active() && !self.fs_hidden => Some(Alert::Updating),
            Some(UpdateUi::Failed) if self.screen.active() && !self.fs_hidden => Some(Alert::UpdateFailed),
            Some(UpdateUi::Installed) if self.screen.active() && !self.fs_hidden => Some(Alert::UpdateInstalled),
            _ => self.queue.current(now_ms(), can).cloned(),
        };
        let changed = match head {
            Some(a) if self.banner.as_ref().is_some_and(|b| b.live && b.alert == a) => false,
            Some(a) => {
                if !self.show_banner(a) {
                    // Nothing to show for it (its device is gone): drop it and look at the next one.
                    self.queue.dismiss();
                    self.sync_alerts();
                }
                true
            }
            None if self.banner_live() => {
                self.banner_off();
                true
            }
            None => false,
        };
        if changed {
            self.update_width_target();
            self.update_zone();
            self.apply_visibility();
        }
    }

    /// Put `a` on the compact row. False when it cannot be shown (a device alert whose device left).
    fn show_banner(&mut self, a: Alert) -> bool {
        let dev = match &a {
            Alert::DeviceConnected { id } | Alert::DeviceLow { id, .. } => {
                self.devs.iter().find(|d| &d.dev.id == id && !d.removing).map(|d| &d.dev)
            }
            _ => None,
        };
        let items = banner::banner_items(&a, dev, self.timer_total_ms, self.ru);
        if items.is_empty() {
            return false;
        }
        let r = self.r.as_ref();
        let (xs, total) = banner::layout(&items, |it| match it {
            Item::Glyph(..) => ICON_W,
            Item::Bolt(_) => BOLT_GLYPH_W,
            Item::Case => banner::CASE_W,
            Item::Text { s, small, .. } => r.map_or(s.chars().count() as f32 * 9.0, |r| r.measure_banner(s, *small)),
        });
        let sticky = matches!(a, Alert::TimerDone | Alert::Updating);
        if a == Alert::TimerDone && !self.timer_sound {
            self.timer_sound = true;
            play_notification();
        }
        let ms = alert_ms(&a);
        self.banner = Some(ActiveBanner { alert: a, items, xs, total, sticky, live: true, shown_at: Instant::now() });
        self.banner_t.go(1.0, 0.22, Instant::now());
        self.alert_hold = false;
        self.alert_active = true;
        // SAFETY: timers on our own window.
        unsafe {
            if sticky {
                let _ = KillTimer(Some(self.hwnd), T_ALERT);
            } else {
                SetTimer(Some(self.hwnd), T_ALERT, ms, None);
            }
        }
        self.kick();
        true
    }

    /// Fade the banner out; the queue is left alone (it comes back if it was only held off).
    fn banner_off(&mut self) {
        if let Some(b) = &mut self.banner {
            b.live = false;
        }
        self.banner_t.go(0.0, 0.20, Instant::now());
        self.alert_active = false;
        self.alert_hold = false;
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_ALERT);
        }
        self.kick();
    }

    /// The banner is done (time up, or clicked): drop it from the queue and show the next one.
    fn finish_alert(&mut self) {
        if self.update_ui == Some(UpdateUi::Working) {
            return; // the install banner goes when the install ends
        }
        if self.banner.as_ref().is_some_and(|b| b.live && b.alert == Alert::TimerDone) {
            self.timer_sound = false;
        }
        if self.update_ui.take().is_none() {
            self.queue.dismiss();
        }
        self.banner_off();
        self.sync_alerts();
        self.update_width_target();
        self.update_zone();
        self.apply_visibility();
    }

    fn on_alert_timer(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_ALERT);
        }
        if self.banner.as_ref().is_some_and(|b| b.live && !b.sticky) {
            if self.cursor_in_pill() {
                // Dismissed when the cursor leaves (WM_MOUSELEAVE); the 1 s re-check, which only
                // runs while a hovered banner is up, covers a leave that was never tracked.
                self.alert_hold = true;
                // SAFETY: timer on our own window.
                unsafe {
                    SetTimer(Some(self.hwnd), T_ALERT, 1000, None);
                }
            } else {
                self.finish_alert();
            }
        }
    }

    /// A click on the banner: an update offer starts the install, the install banner ignores it, the rest dismiss.
    fn on_banner_click(&mut self) {
        match self.banner.as_ref().map(|b| &b.alert) {
            Some(Alert::Updating) => {}
            Some(Alert::UpdateAvailable { .. }) => {
                // A click right after the offer appeared (or a double click through queued banners) is not consent.
                if self.banner.as_ref().is_some_and(|b| b.shown_at.elapsed() >= UPDATE_CLICK_GRACE) {
                    self.queue.dismiss();
                    self.start_update();
                }
            }
            Some(Alert::UpdateFailed) => {
                // Same grace as the offer: this banner replaces the one that was under the cursor.
                if self.banner.as_ref().is_some_and(|b| b.shown_at.elapsed() >= UPDATE_CLICK_GRACE) {
                    self.open_update_page();
                    self.finish_alert();
                }
            }
            _ => self.finish_alert(),
        }
    }

    /// Manual fallback: open the release page of the known update in the default browser.
    fn open_update_page(&self) {
        let url = self.update_rel.as_ref().map_or("", |r| r.page_url.as_str());
        let wurl = wide0(update::safe_page_url(url));
        // SAFETY: `wurl` is a live NUL-terminated buffer; no window owns the call.
        unsafe {
            ShellExecuteW(None, w!("open"), PCWSTR(wurl.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }

    // ---- updates ----

    fn arm_update_check(&self, ms: u32) {
        // SAFETY: one-shot timer on our own window.
        unsafe {
            SetTimer(Some(self.hwnd), T_UPDATE, ms, None);
        }
    }

    /// Run `update::check` on a short-lived worker; the result comes back as `WM_APP_UPDATE`.
    fn start_update_check(&mut self) {
        if !self.update_on || self.check_busy || test_knob("ISLE_TEST_UPDATE") {
            return;
        }
        self.check_busy = true;
        let (sh, hw) = (self.shared.clone(), self.hwnd.0 as isize);
        let spawned = std::thread::Builder::new().name("update-check".into()).spawn(move || {
            let found = Self::check_source().map_err(|e| debug_log(&format!("update check failed: {e}\n")));
            post_update(&sh, hw, UpdateMsg::Checked(found));
        });
        if spawned.is_err() {
            // Could not start a worker: try again tomorrow.
            self.check_busy = false;
            self.arm_update_check(CHECK_EVERY_MS);
        }
    }

    /// The release check itself. Debug-only: `ISLE_TEST_LATEST_JSON=<file>` reads a saved `/releases/latest` response
    /// instead of asking GitHub.
    fn check_source() -> Result<Option<Release>, update::Error> {
        if cfg!(debug_assertions) {
            if let Some(p) = std::env::var_os("ISLE_TEST_LATEST_JSON") {
                let json = std::fs::read_to_string(p).map_err(|e| update::Error::Io(e.to_string()))?;
                let rel = update::parse_latest(&json)?;
                return Ok(update::is_newer(&rel.version, &Version::current()).then_some(rel));
            }
        }
        update::check(&Version::current())
    }

    /// The T_UPDATE one-shot fired.
    fn on_update_timer(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_UPDATE);
        }
        self.start_update_check();
    }

    /// Resume from sleep: the one-shot does not count sleep; if the last check is over a day old, check soon
    /// (not at once: the network is usually not back yet).
    fn on_resume_update(&mut self) {
        let stale = self.last_check.is_some_and(|t| now_ms().saturating_sub(t) > CHECK_EVERY_MS as u64);
        if self.update_on && stale && !self.check_busy {
            self.arm_update_check(CHECK_FIRST_MS);
        }
    }

    /// Debug-only fake release (`ISLE_TEST_UPDATE=<version>`): shows the banner and the menu item, installs nothing.
    fn test_release() -> Option<Release> {
        let v = Version::parse(&std::env::var("ISLE_TEST_UPDATE").ok().filter(|_| cfg!(debug_assertions))?)?;
        Some(Release {
            version: v,
            tag: format!("v{v}"),
            exe_url: "https://github.com/bagetvann2-cyber/isle/releases/download/test/isle.exe".into(),
            sha256: None,
            page_url: "https://github.com/bagetvann2-cyber/isle/releases/tag/test".into(),
        })
    }

    /// Remember the newest release and announce each version once per run.
    fn on_release_found(&mut self, rel: Release) {
        let announce = self.update_offered != Some(rel.version);
        self.update_offered = Some(rel.version);
        let alert = Alert::UpdateAvailable { version: rel.version.to_string() };
        self.update_rel = Some(rel);
        if announce {
            self.offer_at = Some(now_ms());
            self.push_alerts(vec![alert]);
        }
    }

    /// The island was revealed by the cursor: show the known update offer again, at most once per `REOFFER_MS`.
    fn reoffer_update(&mut self) {
        let Some(rel) = self.update_rel.as_ref().filter(|_| self.update_on) else { return };
        let window = std::env::var("ISLE_TEST_REOFFER_MS").ok().and_then(|v| v.parse().ok()).filter(|_| cfg!(debug_assertions));
        if self.update_ui.is_some() || !reoffer_due(self.offer_at, now_ms(), window.unwrap_or(REOFFER_MS)) {
            return;
        }
        self.offer_at = Some(now_ms());
        debug_log(
            "update re-offered
",
        );
        let alert = Alert::UpdateAvailable { version: rel.version.to_string() };
        self.push_alerts(vec![alert]);
    }

    /// Install `update_rel` after an explicit click. Never called without one.
    fn start_update(&mut self) {
        let Some(rel) = self.update_rel.clone() else { return };
        if self.update_ui == Some(UpdateUi::Working) {
            return;
        }
        self.update_ui = Some(UpdateUi::Working);
        let (sh, hw) = (self.shared.clone(), self.hwnd.0 as isize);
        let spawned = std::thread::Builder::new().name("update-install".into()).spawn(move || {
            let r = if test_knob("ISLE_TEST_UPDATE") {
                // Test mode: nothing is downloaded; linger so the "Updating" banner can be seen, then fail.
                std::thread::sleep(Duration::from_secs(3));
                debug_log("update failed: test mode, nothing installed\n");
                InstallResult::Failed
            } else {
                match update::download_and_install(&rel) {
                    Err(e) => {
                        debug_log(&format!("update failed: {e}\n"));
                        InstallResult::Failed
                    }
                    Ok(()) => match update::restart() {
                        Ok(()) => {
                            // The new process is waiting for the mutex: release it here, not via the UI thread.
                            crate::release_instance_mutex();
                            InstallResult::Restarted
                        }
                        Err(e) => {
                            debug_log(&format!("update installed but restart failed: {e}\n"));
                            InstallResult::SwappedNoRestart
                        }
                    },
                }
            };
            post_update(&sh, hw, UpdateMsg::Installed(r));
        });
        if spawned.is_err() {
            self.update_ui = Some(UpdateUi::Failed);
        }
        self.sync_alerts();
    }

    /// Handle what the workers reported. True: the new exe is running, the caller must release the mutex and quit.
    fn on_update_msgs(&mut self) -> bool {
        let msgs = self.shared.update.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
        for m in msgs {
            match m {
                UpdateMsg::Checked(found) => {
                    self.check_busy = false;
                    if found.is_ok() {
                        self.last_check = Some(now_ms());
                    }
                    if matches!(found, Ok(None)) {
                        self.none_checks += 1;
                    } else if found.is_ok() {
                        self.none_checks = 0;
                    }
                    if self.update_on {
                        self.arm_update_check(next_check_ms(found.is_ok(), self.none_checks));
                    }
                    if let Ok(Some(rel)) = found {
                        self.on_release_found(rel);
                    }
                }
                UpdateMsg::Installed(InstallResult::Restarted) => return true,
                UpdateMsg::Installed(r) => {
                    let swapped = matches!(r, InstallResult::SwappedNoRestart);
                    if swapped {
                        self.update_rel = None; // already on disk: do not offer (and redo) it
                    }
                    self.update_ui = Some(if swapped { UpdateUi::Installed } else { UpdateUi::Failed });
                    self.sync_alerts();
                }
            }
        }
        false
    }

    /// AC / battery level changed (power-setting notification): read the status and feed the tracker.
    fn on_power_event(&mut self) {
        let mut st = SYSTEM_POWER_STATUS::default();
        // SAFETY: valid out pointer.
        if unsafe { GetSystemPowerStatus(&mut st) }.is_err() {
            return;
        }
        if let Some((ac, pct)) = power_sample(st.ACLineStatus, st.BatteryFlag, st.BatteryLifePercent) {
            self.feed_power(ac, pct);
        }
    }

    fn feed_power(&mut self, ac: bool, pct: u8) {
        let found = self.tracker.on_power(now_ms(), ac, pct);
        self.push_alerts(found);
    }

    /// Microphone / camera users changed (the watcher thread posted it).
    fn on_privacy(&mut self) {
        let new = self.shared.privacy.lock().map(|g| g.clone()).unwrap_or_default();
        if new == self.privacy {
            return;
        }
        self.privacy = new;
        if self.shown {
            if self.is_animating(Instant::now()) {
                self.kick();
            } else {
                self.paint();
            }
        }
    }

    // ---- clock ----

    fn clock_seg_w(&self) -> f32 {
        let label = clock::label(&self.clock, now_ms());
        let (d, c) = self.r.as_ref().map_or((12.0, 6.0), |r| r.clock_metrics());
        banner::clock_seg_w(banner::clock_cells(&label, d, c).1)
    }

    /// Start (or replace) the clock.
    fn start_clock(&mut self, c: Clock) {
        self.clock = c;
        self.clk_full = self.clock_seg_w();
        self.clk.target = self.clk_full;
        self.clk.tune(0.40, 0.90);
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_CLOCK_DUE);
        }
        self.clock_check();
        self.update_width_target();
        self.update_zone();
        self.sync_clock_tick(true);
        self.kick();
    }

    fn stop_clock(&mut self) {
        if self.clock == Clock::Off {
            return;
        }
        self.clk_last = clock::label(&self.clock, now_ms());
        self.clock = Clock::Off;
        self.clk.target = 0.0;
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_CLOCK_DUE);
        }
        self.sync_clock_tick(false);
        self.update_width_target();
        self.update_zone();
        self.kick();
    }

    /// Fire an expired timer now, or arm the one-shot for what is left. Also the recompute after sleep.
    fn clock_check(&mut self) {
        match clock::remaining_ms(&self.clock, now_ms()) {
            Some(0) => self.fire_timer(),
            Some(left) => {
                // SAFETY: one-shot timer on our own window.
                unsafe {
                    SetTimer(Some(self.hwnd), T_CLOCK_DUE, left.min(u32::MAX as u64 - 1) as u32, None);
                }
            }
            None => {}
        }
    }

    fn fire_timer(&mut self) {
        if let Clock::Timer { total_ms, .. } = self.clock {
            self.timer_total_ms = total_ms;
        }
        self.stop_clock();
        if self.alerts_on {
            self.queue.push(Alert::TimerDone, now_ms());
            // A second timer finishing while the first banner is still up: rebuild it (new total, sound).
            if self.banner.as_ref().is_some_and(|b| b.alert == Alert::TimerDone) {
                self.timer_sound = false;
                self.banner = None;
            }
        }
        self.sync_alerts();
    }

    fn on_clock_due(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_CLOCK_DUE);
        }
        self.clock_check();
    }

    /// Second boundary: repaint the label and re-arm for the next one.
    fn on_clock_tick(&mut self) {
        // SetTimer repeats until killed: make this a real one-shot (sync_clock_tick re-arms it).
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_CLOCK_TICK);
        }
        self.clk_tick = false;
        if clock::expired(&self.clock, now_ms()) {
            self.clock_check();
            return;
        }
        // The label may have grown (1:00:00) or shrunk: keep the segment as wide as it needs.
        let w = self.clock_seg_w();
        if self.clock != Clock::Off && (w - self.clk_full).abs() > 0.5 {
            self.clk_full = w;
            self.clk.target = w;
            self.update_width_target();
            self.update_zone();
            self.kick();
        }
        self.paint();
        self.sync_clock_tick(true);
    }

    /// The 1 s tick runs only while a clock runs, the island is shown and the screen is lit.
    fn sync_clock_tick(&mut self, rearm: bool) {
        let want = self.clock != Clock::Off && self.on_screen();
        // SAFETY: timer on our own window.
        unsafe {
            if want && (rearm || !self.clk_tick) {
                let ms = clock::next_tick_ms(&self.clock, now_ms()).clamp(10, 1000);
                SetTimer(Some(self.hwnd), T_CLOCK_TICK, ms as u32, None);
                self.clk_tick = true;
            } else if !want {
                let _ = KillTimer(Some(self.hwnd), T_CLOCK_TICK);
                self.clk_tick = false;
            }
        }
    }

    /// Debug-only scripted steps behind `ISLE_TEST_FAKE_CONNECT` / `ISLE_TEST_POWER`.
    fn on_test_step(&mut self) {
        // SAFETY: timer on our own window.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), T_TEST);
        }
        let n = self.test_n;
        self.test_n += 1;
        let mut more = false;
        if n == 0 {
            if let Some(rel) = Self::test_release() {
                self.on_release_found(rel);
            }
        }
        if test_knob("ISLE_TEST_FAKE_CONNECT") {
            self.fake_connect = n == 0;
            self.on_devices();
            more = n == 0;
        }
        if test_knob("ISLE_TEST_POWER") {
            let steps = [(false, 76), (true, 76), (false, 76), (false, 19)];
            if let Some(&(ac, pct)) = steps.get(n as usize) {
                self.feed_power(ac, pct);
                more = n as usize + 1 < steps.len();
            }
        }
        if more {
            // SAFETY: timer on our own window.
            unsafe {
                SetTimer(Some(self.hwnd), T_TEST, 6000, None);
            }
        }
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
pub(crate) fn test_knob(name: &str) -> bool {
    cfg!(debug_assertions) && std::env::var_os(name).is_some()
}

/// Debug-only: `ISLE_TEST_POS=x,y` places the pill (physical px) without touching the saved position.
fn test_pos() -> Option<(i32, i32)> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let v = std::env::var("ISLE_TEST_POS").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// "Animation effects" in Windows settings (off: the pill keeps the plain fade).
fn client_animations() -> bool {
    let mut on: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL (an i32) through the pointer.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, Some(&mut on as *mut i32 as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
    };
    ok.is_err() || on != 0
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

/// Low-level mouse hook: runs on every mouse move system-wide, so it only compares the point
/// with the cached rects and posts when the zone / rest / edge-strip state flips. The rest rect and
/// the strip lie inside the zone, so outside it one compare is all there is.
unsafe extern "system" fn mouse_ll(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == 0 && wp.0 as u32 == WM_MOUSEMOVE {
        let pt = (*(lp.0 as *const MSLLHOOKSTRUCT)).pt;
        let post = |ev: Ev| {
            let _ = PostMessageW(Some(HWND(HOOK_HWND.load(Ordering::Relaxed) as *mut _)), WM_APP_ZONE, WPARAM(ev as usize), LPARAM(0));
        };
        let zone = App::in_rect(&ZONE, pt.x, pt.y);
        if zone != INSIDE.swap(zone, Ordering::Relaxed) {
            post(if zone { Ev::EnterZone } else { Ev::LeaveZone });
        }
        let rest = zone && App::in_rect(&REST, pt.x, pt.y);
        if rest != IN_REST.swap(rest, Ordering::Relaxed) {
            post(if rest { Ev::EnterRest } else { Ev::LeaveRest });
        }
        let strip = zone && App::in_rect(&STRIP, pt.x, pt.y);
        if strip != IN_STRIP.swap(strip, Ordering::Relaxed) && strip {
            post(Ev::EdgeTouch);
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

/// Menu command id of a size preset: `SIZE_CMD + percent`.
const SIZE_CMD: usize = 100;

unsafe fn show_menu(hwnd: HWND) {
    let ru = app().ru;
    let tr = |ru_s: &str, en_s: &str| wide0(if ru { ru_s } else { en_s });
    let (t_auto, t_reset, t_exit, t_vis, t_wave, t_size) = if ru {
        ("Запускать вместе с Windows", "Сбросить позицию", "Выход", "Всегда показывать", "Показывать волну", "Размер")
    } else {
        ("Start with Windows", "Reset position", "Exit", "Always visible", "Show waveform", "Size")
    };
    let Ok(menu) = CreatePopupMenu() else { return };
    // Only while an update is known; grayed while one is installing.
    let t_upd = app()
        .update_rel
        .as_ref()
        .map(|r| wide0(&if ru { format!("Обновить до {}", r.version) } else { format!("Update to {}", r.version) }));
    let t_page = tr("Открыть страницу обновления", "Open update page");
    if let Some(t) = &t_upd {
        let f = if app().update_ui == Some(UpdateUi::Working) { MF_STRING | MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(menu, f, UPDATE_CMD, PCWSTR(t.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, UPDATE_PAGE_CMD, PCWSTR(t_page.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
    let (a, b, c, d, e) = (wide0(t_auto), wide0(t_reset), wide0(t_exit), wide0(t_vis), wide0(t_wave));
    let flags = if settings::autostart_enabled() { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, flags, 1, PCWSTR(a.as_ptr()));
    let vflags = if app().always_visible { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, vflags, 4, PCWSTR(d.as_ptr()));
    let wflags = if app().wave_on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, wflags, 5, PCWSTR(e.as_ptr()));
    let t_notif = tr("Уведомления", "Notifications");
    let nflags = if app().alerts_on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, nflags, ALERTS_CMD, PCWSTR(t_notif.as_ptr()));
    let t_chk = tr("Проверять обновления", "Check for updates");
    let cflags = if app().update_on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let _ = AppendMenuW(menu, cflags, CHECK_CMD, PCWSTR(t_chk.as_ptr()));
    let (t_timer, t_sw, t_stop) = (tr("Таймер", "Timer"), tr("Секундомер", "Stopwatch"), tr("Остановить", "Stop"));
    let mins: Vec<Vec<u16>> = CLOCK_MINUTES.iter().map(|m| wide0(&if ru { format!("{m} мин") } else { format!("{m} min") })).collect();
    if let Ok(sub) = CreatePopupMenu() {
        for (m, l) in CLOCK_MINUTES.iter().zip(&mins) {
            let _ = AppendMenuW(sub, MF_STRING, CLOCK_CMD + *m as usize, PCWSTR(l.as_ptr()));
        }
        let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(sub, MF_STRING, CLOCK_STOPWATCH, PCWSTR(t_sw.as_ptr()));
        let stop = if app().clock == Clock::Off { MF_STRING | MF_GRAYED } else { MF_STRING };
        let _ = AppendMenuW(sub, stop, CLOCK_STOP, PCWSTR(t_stop.as_ptr()));
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(t_timer.as_ptr()));
    }
    let labels: Vec<Vec<u16>> = SIZE_PRESETS.iter().map(|p| wide0(&format!("{p} %"))).collect();
    let t_size = wide0(t_size);
    if let Ok(sub) = CreatePopupMenu() {
        for (p, l) in SIZE_PRESETS.iter().zip(&labels) {
            let _ = AppendMenuW(sub, MF_STRING, SIZE_CMD + *p as usize, PCWSTR(l.as_ptr()));
        }
        if let Some(i) = SIZE_PRESETS.iter().position(|&p| p == app().size_pct) {
            let _ = CheckMenuRadioItem(sub, 0, SIZE_PRESETS.len() as u32 - 1, i as u32, MF_BYPOSITION.0);
        }
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(t_size.as_ptr()));
    }
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
    a.resume_hide();
    match cmd.0 {
        1 => settings::set_autostart(!settings::autostart_enabled()),
        2 => a.reset_position(),
        4 => {
            a.always_visible = !a.always_visible;
            settings::set_always_visible(a.always_visible);
            a.sync_hook(false);
            // The cursor is on the island (the menu came from it): it stays until the cursor leaves.
            a.rv.revealed = a.rv.in_zone;
            a.zone_active = a.rv.revealed;
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
        c if c as usize == ALERTS_CMD => {
            a.alerts_on = !a.alerts_on;
            settings::set_alerts(a.alerts_on);
            if !a.alerts_on {
                a.timer_sound = false;
            }
            a.sync_alerts();
        }
        c if c as usize == UPDATE_CMD => a.start_update(),
        c if c as usize == UPDATE_PAGE_CMD => a.open_update_page(),
        c if c as usize == CHECK_CMD => {
            a.update_on = !a.update_on;
            settings::set_check_updates(a.update_on);
            if a.update_on {
                if !a.check_busy {
                    a.arm_update_check(5000);
                }
            } else {
                let _ = KillTimer(Some(hwnd), T_UPDATE);
            }
        }
        c if clock_cmd(c as usize).is_some() => {
            let now = now_ms();
            match clock_cmd(c as usize) {
                Some(ClockCmd::Timer(m)) => a.start_clock(Clock::Timer { deadline_ms: now + m * 60_000, total_ms: m * 60_000 }),
                Some(ClockCmd::Stopwatch) => a.start_clock(Clock::Stopwatch { start_ms: now }),
                Some(ClockCmd::Stop) => a.stop_clock(),
                None => {}
            }
        }
        // Not while an install runs: killing the worker between the two renames would leave no isle.exe.
        3 if a.update_ui != Some(UpdateUi::Working) => {
            let _ = DestroyWindow(hwnd);
            return;
        }
        c if (SIZE_CMD + SIZE_MIN as usize..=SIZE_CMD + SIZE_MAX as usize).contains(&(c as usize)) => {
            a.set_size(c as u32 - SIZE_CMD as u32);
            a.save_size();
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
        WM_MOUSEWHEEL => {
            if wp.0 & MK_CONTROL != 0 {
                app().on_wheel((wp.0 >> 16) as u16 as i16 as i32);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let a = app();
            match wp.0 {
                T_SIZE => a.save_size(),
                T_ALERT => a.on_alert_timer(),
                T_UPDATE => a.on_update_timer(),
                T_CLOCK_TICK => a.on_clock_tick(),
                T_CLOCK_DUE => a.on_clock_due(),
                T_TEST => a.on_test_step(),
                T_PROGRESS => a.paint(),
                T_LEAVE => {
                    let _ = KillTimer(Some(hwnd), T_LEAVE);
                    a.leave_pending = false;
                    if !a.menu_open && a.drag.is_none() && !a.cursor_in_pill() {
                        a.request_collapse(Instant::now());
                        if a.alert_hold {
                            a.finish_alert();
                        }
                    }
                }
                T_HIDE => a.on_hide_timer(),
                T_DWELL => {
                    let _ = KillTimer(Some(hwnd), T_DWELL);
                    a.on_reveal(Ev::DwellDone);
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
        WM_APP_PRIVACY => {
            app().on_privacy();
            LRESULT(0)
        }
        WM_APP_UPDATE => {
            if app().on_update_msgs() {
                // The new exe is running and waits for the mutex: release it, then leave (WM_DESTROY removes the tray icon).
                crate::release_instance_mutex();
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_APP_ZONE => {
            if let Some(ev) = Ev::from_code(wp.0) {
                app().on_reveal(ev);
            }
            LRESULT(0)
        }
        // The taskbar moved or resized: the work area (and with it the docked edge) changed.
        WM_SETTINGCHANGE if wp.0 == SPI_SETWORKAREA.0 as usize => {
            app().update_zone();
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
                let a = app();
                a.tracker.mute_until(now_ms() + MUTE_MS);
                if let Some(b) = &a.battery {
                    b.refresh();
                }
                // The timer's one-shot does not count sleep: recompute (and fire if it ran out meanwhile).
                a.clock_check();
                a.on_resume_update();
            } else if wp.0 == PBT_POWERSETTINGCHANGE as usize && lp.0 != 0 {
                // SAFETY: for this event lParam points to a POWERBROADCAST_SETTING for the call's duration.
                let p = lp.0 as *const POWERBROADCAST_SETTING;
                let setting = (*p).PowerSetting;
                if setting == GUID_SESSION_DISPLAY_STATUS && (*p).DataLength as usize >= 4 {
                    let a = app();
                    let was = a.screen.active();
                    a.screen.set_display(std::ptr::addr_of!((*p).Data).cast::<u32>().read_unaligned());
                    a.on_screen_change(was);
                } else if setting == GUID_ACDC_POWER_SOURCE || setting == GUID_BATTERY_PERCENTAGE_REMAINING {
                    app().on_power_event();
                }
            }
            LRESULT(1)
        }
        WM_WTSSESSION_CHANGE => {
            let a = app();
            let was = a.screen.active();
            if wp.0 as u32 == WTS_SESSION_UNLOCK {
                a.tracker.mute_until(now_ms() + MUTE_MS);
            }
            a.screen.session_event(wp.0 as u32);
            a.on_screen_change(was);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = WTSUnRegisterSessionNotification(hwnd);
            let a = app();
            let power = std::mem::take(&mut a.power_notify);
            for n in std::iter::once(std::mem::take(&mut a.screen_notify)).chain(power) {
                if n != 0 {
                    let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(n));
                }
            }
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
        if dpi != 0 && (scale_for(dpi, a0.size_pct) - a0.scale).abs() > 0.01 {
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
        a0.media_tx = Some(crate::media::spawn(hw, shared.clone()));
        APP.store(Box::into_raw(a0), Ordering::Release);

        // First (transparent) frame, then show and animate in.
        let a = app();
        a.fs_hidden = fullscreen_active(hwnd);
        a.hinst = hinst.into();
        HOOK_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        if let Ok(n) = RegisterPowerSettingNotification(HANDLE(hwnd.0), &GUID_SESSION_DISPLAY_STATUS, DEVICE_NOTIFY_WINDOW_HANDLE) {
            a.screen_notify = n.0;
        }
        // AC source and battery level: the first values Windows sends right after registering are the tracker's baseline.
        for (slot, guid) in a.power_notify.iter_mut().zip([&GUID_ACDC_POWER_SOURCE, &GUID_BATTERY_PERCENTAGE_REMAINING]) {
            if let Ok(n) = RegisterPowerSettingNotification(HANDLE(hwnd.0), guid, DEVICE_NOTIFY_WINDOW_HANDLE) {
                *slot = n.0;
            }
        }
        let sh = shared.clone();
        crate::privacy::spawn(move |state| {
            if let Ok(mut g) = sh.privacy.lock() {
                *g = state;
            }
            let _ = PostMessageW(Some(HWND(hw as *mut _)), WM_APP_PRIVACY, WPARAM(0), LPARAM(0));
        });
        if cfg!(debug_assertions) {
            if let Some(secs) = std::env::var("ISLE_TEST_TIMER").ok().and_then(|v| v.parse::<u64>().ok()) {
                a.start_clock(Clock::Timer { deadline_ms: now_ms() + secs * 1000, total_ms: secs * 1000 });
            }
            if test_knob("ISLE_TEST_STOPWATCH") {
                a.start_clock(Clock::Stopwatch { start_ms: now_ms() });
            }
            if test_knob("ISLE_TEST_FAKE_CONNECT") || test_knob("ISLE_TEST_POWER") || test_knob("ISLE_TEST_UPDATE") {
                SetTimer(Some(hwnd), T_TEST, 4000, None);
            }
        }
        if a.update_on {
            // Debug-only `ISLE_TEST_UPDATE_DELAY=<ms>` shortens the first check.
            let delay = std::env::var("ISLE_TEST_UPDATE_DELAY").ok().and_then(|v| v.parse().ok()).filter(|_| cfg!(debug_assertions));
            a.arm_update_check(delay.unwrap_or(CHECK_FIRST_MS));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_predicate() {
        // Hover, always-visible and an alert each show it; nothing shows nothing.
        assert!(!want_shown(false, false, false, false));
        assert!(want_shown(false, true, false, false));
        assert!(want_shown(false, false, true, false));
        assert!(want_shown(false, false, false, true), "an alert reveals a hidden island");
        // A fullscreen app overrides every reason, alerts included.
        assert!(!want_shown(true, true, true, true));
    }

    #[test]
    fn update_schedule() {
        assert_eq!(next_check_ms(false, 0), CHECK_RETRY_MS);
        assert_eq!(next_check_ms(true, 0), CHECK_EVERY_MS); // found a release
        for n in 1..=3 {
            assert_eq!(next_check_ms(true, n), CHECK_RETRY_MS);
        }
        assert_eq!(next_check_ms(true, 4), CHECK_EVERY_MS);
        assert!(reoffer_due(None, 5, 600));
        assert!(!reoffer_due(Some(100), 699, 600));
        assert!(reoffer_due(Some(100), 700, 600));
    }

    #[test]
    fn power_samples() {
        assert_eq!(power_sample(1, 8, 76), Some((true, 76))); // charging
        assert_eq!(power_sample(0, 0, 40), Some((false, 40)));
        assert_eq!(power_sample(0, 128, 0), None); // no system battery (desktop)
        assert_eq!(power_sample(1, 255, 100), None); // unknown status
        assert_eq!(power_sample(1, 1, 255), None); // unknown percent
        assert_eq!(power_sample(255, 1, 50), None); // unknown AC state
        assert_eq!(power_sample(1, 9 | 128, 50), None);
    }

    #[test]
    fn clock_menu_commands() {
        assert_eq!(clock_cmd(CLOCK_CMD + 1), Some(ClockCmd::Timer(1)));
        assert_eq!(clock_cmd(CLOCK_CMD + 60), Some(ClockCmd::Timer(60)));
        assert_eq!(clock_cmd(CLOCK_STOPWATCH), Some(ClockCmd::Stopwatch));
        assert_eq!(clock_cmd(CLOCK_STOP), Some(ClockCmd::Stop));
        assert_eq!(clock_cmd(CLOCK_CMD + 7), None);
        // None of the ids collides with the plain menu items or the size presets.
        let sizes = SIZE_CMD + SIZE_MIN as usize..=SIZE_CMD + SIZE_MAX as usize;
        for id in [CLOCK_STOPWATCH, CLOCK_STOP, ALERTS_CMD].into_iter().chain(CLOCK_MINUTES.iter().map(|m| CLOCK_CMD + *m as usize)) {
            assert!(!sizes.contains(&id) && id > 5, "id {id}");
        }
    }

    #[test]
    fn screen_state() {
        let mut s = Screen::new();
        assert!(s.active());
        s.set_display(2);
        assert!(s.active(), "dimmed is still visible");
        s.set_display(0);
        assert!(!s.active());
        s.set_display(1);
        assert!(s.active());
        s.session_event(WTS_SESSION_LOCK);
        assert!(!s.active());
        s.set_display(0);
        s.session_event(WTS_SESSION_UNLOCK);
        assert!(!s.active(), "display still off");
        s.set_display(1);
        assert!(s.active());
        s.session_event(WTS_CONSOLE_DISCONNECT);
        assert!(!s.active());
        s.session_event(WTS_CONSOLE_CONNECT);
        assert!(s.active());
        s.session_event(WTS_REMOTE_DISCONNECT);
        assert!(!s.active());
        s.session_event(WTS_REMOTE_CONNECT);
        s.session_event(99);
        assert!(s.active());
    }
}
