//! Windows media transport controls (GSMTC) on one dedicated MTA thread.
//!
//! The thread blocks on a channel; WinRT event handlers only enqueue a "dirty" message.
//! Bursts are coalesced with a 150 ms trailing debounce, then the whole snapshot is re-read.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{Interface, RuntimeType};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::*;
use windows::Storage::Streams::{DataReader, IInputStream};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::model::{Cover, MediaCommand, MediaSnapshot};
use crate::ui::{Shared, WM_APP_MEDIA};

const DEBOUNCE: Duration = Duration::from_millis(150);
const COVER_MAX: u32 = 160; // short side; 52 logical px at up to 3x

enum Msg {
    Dirty { props: bool },
    Cmd(MediaCommand),
}

pub struct MediaHandle {
    tx: Sender<Msg>,
}

impl MediaHandle {
    pub fn send(&self, c: MediaCommand) {
        let _ = self.tx.send(Msg::Cmd(c));
    }
}

pub fn spawn(hwnd: isize, shared: Arc<Shared>) -> MediaHandle {
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    let _ = std::thread::Builder::new().name("media".into()).spawn(move || {
        let _ = run(hwnd, &shared, rx, tx2);
    });
    MediaHandle { tx }
}

/// WinRT event handler that only enqueues a dirty message (it runs on a pool thread).
fn handler<S: RuntimeType + 'static, A: RuntimeType + 'static>(tx: &Sender<Msg>, props: bool) -> TypedEventHandler<S, A> {
    let tx = tx.clone();
    TypedEventHandler::new(move |_, _| {
        let _ = tx.send(Msg::Dirty { props });
        Ok(())
    })
}

struct Subscribed {
    session: GlobalSystemMediaTransportControlsSession,
    /// COM identity of the session (two tabs of one browser share an AUMID).
    ident: windows::core::IUnknown,
    tokens: [Option<i64>; 3],
}

struct Worker {
    mgr: GlobalSystemMediaTransportControlsSessionManager,
    tx: Sender<Msg>,
    sub: Option<Subscribed>,
    wic: Option<IWICImagingFactory>,
    cover: Option<Arc<Cover>>,
    cover_hash: u64,
    resubs: u32,
}

fn run(hwnd: isize, shared: &Shared, rx: mpsc::Receiver<Msg>, tx: Sender<Msg>) -> windows::core::Result<()> {
    // SAFETY: first COM call on this thread; multithreaded apartment so `.join()` may block.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    // The service may not be up yet right after logon: retry a few times before giving up.
    let mut attempt = 0;
    let mgr = loop {
        match GlobalSystemMediaTransportControlsSessionManager::RequestAsync().and_then(|op| op.join()) {
            Ok(m) => break m,
            Err(e) if attempt >= 4 => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    };
    mgr.SessionsChanged(&handler(&tx, true))?;
    mgr.CurrentSessionChanged(&handler(&tx, true))?;
    let mut w = Worker { mgr, tx, sub: None, wic: None, cover: None, cover_hash: 0, resubs: 0 };
    w.publish(hwnd, shared, true);
    loop {
        let mut props = false;
        match rx.recv() {
            Ok(Msg::Dirty { props: p }) => props |= p,
            Ok(Msg::Cmd(c)) => {
                w.command(hwnd, shared, c);
                continue;
            }
            Err(_) => return Ok(()),
        }
        loop {
            match rx.recv_timeout(DEBOUNCE) {
                Ok(Msg::Dirty { props: p }) => props |= p,
                Ok(Msg::Cmd(c)) => w.command(hwnd, shared, c),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
        w.publish(hwnd, shared, props);
    }
}

impl Worker {
    fn command(&self, hwnd: isize, shared: &Shared, c: MediaCommand) {
        let Some(s) = &self.sub else { return };
        let _ = match c {
            MediaCommand::TogglePlay => s.session.TryTogglePlayPauseAsync().map(|_| ()),
            MediaCommand::Next => s.session.TrySkipNextAsync().map(|_| ()),
            MediaCommand::Prev => s.session.TrySkipPreviousAsync().map(|_| ()),
            MediaCommand::SeekBy(_) | MediaCommand::SeekTo(_) => {
                let now = Instant::now();
                let target = match shared.media.lock() {
                    Ok(g) => match (&*g, c) {
                        (Some(m), MediaCommand::SeekBy(d)) if m.duration_ms > 0 => m.seek_by_target(d, now),
                        (Some(m), MediaCommand::SeekTo(t)) if m.duration_ms > 0 => t.min(m.duration_ms),
                        _ => return,
                    },
                    Err(_) => return,
                };
                // The position is relative to the timeline start, which `read` subtracted: add it back.
                let start = s.session.GetTimelineProperties().and_then(|t| t.StartTime()).map_or(0, |t| t.Duration.max(0));
                let ticks = start.saturating_add(target as i64 * 10_000);
                let ok = s.session.TryChangePlaybackPositionAsync(ticks).and_then(|op| op.join()).unwrap_or(false);
                if ok {
                    // Jump the bar now; TimelinePropertiesChanged will correct it.
                    if let Ok(mut g) = shared.media.lock() {
                        if let Some(m) = g.as_mut() {
                            m.position_ms = target;
                            m.position_at = Some(Instant::now());
                        }
                    }
                    // SAFETY: posting to a window handle; failure just means the window is gone.
                    unsafe {
                        let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_MEDIA, WPARAM(0), LPARAM(0));
                    }
                }
                Ok(())
            }
        };
    }

    /// First session that is playing, else the system's current one.
    fn pick(&self) -> Option<GlobalSystemMediaTransportControlsSession> {
        if let Ok(list) = self.mgr.GetSessions() {
            for i in 0..list.Size().unwrap_or(0) {
                let Ok(s) = list.GetAt(i) else { continue };
                let playing = s
                    .GetPlaybackInfo()
                    .and_then(|p| p.PlaybackStatus())
                    .is_ok_and(|st| st == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing);
                if playing {
                    return Some(s);
                }
            }
        }
        self.mgr.GetCurrentSession().ok()
    }

    fn unsubscribe(&mut self) {
        if let Some(s) = self.sub.take() {
            if let Some(t) = s.tokens[0] {
                let _ = s.session.RemoveMediaPropertiesChanged(t);
            }
            if let Some(t) = s.tokens[1] {
                let _ = s.session.RemovePlaybackInfoChanged(t);
            }
            if let Some(t) = s.tokens[2] {
                let _ = s.session.RemoveTimelinePropertiesChanged(t);
            }
        }
    }

    /// Returns true when the subscribed session changed.
    fn select(&mut self) -> bool {
        let Some(s) = self.pick() else {
            let had = self.sub.is_some();
            self.unsubscribe();
            return had;
        };
        let Ok(ident) = s.cast::<windows::core::IUnknown>() else { return false };
        if self.sub.as_ref().is_some_and(|c| c.ident == ident) {
            return false;
        }
        self.unsubscribe();
        let tokens = [
            s.MediaPropertiesChanged(&handler(&self.tx, true)).ok(),
            s.PlaybackInfoChanged(&handler(&self.tx, false)).ok(),
            s.TimelinePropertiesChanged(&handler(&self.tx, false)).ok(),
        ];
        // Keep the session even if a subscription failed: the tokens we hold get removed
        // later, and an unchanged identity never triggers a resubscribe.
        self.sub = Some(Subscribed { session: s, ident, tokens });
        self.resubs += 1;
        if let (Some(dir), true) = (std::env::var_os("TEMP"), cfg!(debug_assertions) && std::env::var_os("ISLE_PERF").is_some()) {
            let line = format!("media resubscribes: {}\n", self.resubs);
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(std::path::Path::new(&dir).join("isle-perf.log"))
                .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        }
        true
    }

    fn publish(&mut self, hwnd: isize, shared: &Shared, props: bool) {
        let switched = self.select();
        let snap = self.sub.as_ref().map(|s| s.session.clone()).and_then(|s| self.read(&s, props || switched));
        if snap.is_none() {
            self.cover = None;
            self.cover_hash = 0;
        }
        if let Ok(mut g) = shared.media.lock() {
            *g = snap;
        }
        // SAFETY: posting to a window handle; failure just means the window is gone.
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_APP_MEDIA, WPARAM(0), LPARAM(0));
        }
    }

    fn read(&mut self, s: &GlobalSystemMediaTransportControlsSession, props: bool) -> Option<MediaSnapshot> {
        let mp = s.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
        let info = s.GetPlaybackInfo().ok()?;
        let playing = info.PlaybackStatus().ok()? == GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing;
        let ctl = info.Controls().ok()?;
        if props || self.cover.is_none() {
            self.load_cover(&mp);
        }
        let (mut position_ms, mut duration_ms) = (0u64, 0u64);
        if let Ok(tl) = s.GetTimelineProperties() {
            let ms = |t: windows::Foundation::TimeSpan| t.Duration.max(0) as u64 / 10_000;
            let start = tl.StartTime().map(ms).unwrap_or(0);
            let end = tl.EndTime().map(ms).unwrap_or(0);
            duration_ms = end.saturating_sub(start);
            position_ms = tl.Position().map(ms).unwrap_or(0).saturating_sub(start);
            if playing {
                // The position was valid at LastUpdatedTime (100 ns since 1601): catch up.
                if let Ok(t) = tl.LastUpdatedTime() {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0) + 11_644_473_600_000;
                    let last = t.UniversalTime / 10_000;
                    if last > 0 && now > last {
                        position_ms += (now - last) as u64;
                    }
                }
            }
            if duration_ms > 0 {
                position_ms = position_ms.min(duration_ms);
            }
        }
        Some(MediaSnapshot {
            title: mp.Title().map(|t| t.to_string()).unwrap_or_default(),
            artist: mp.Artist().map(|t| t.to_string()).unwrap_or_default(),
            playing,
            can_prev: ctl.IsPreviousEnabled().unwrap_or(false),
            can_next: ctl.IsNextEnabled().unwrap_or(false),
            can_seek: ctl.IsPlaybackPositionEnabled().unwrap_or(false),
            position_ms,
            duration_ms,
            position_at: Some(Instant::now()),
            cover: self.cover.clone(),
        })
    }

    /// Decode only when the thumbnail bytes actually changed.
    fn load_cover(&mut self, mp: &GlobalSystemMediaTransportControlsSessionMediaProperties) {
        let bytes = mp.Thumbnail().ok().and_then(|t| read_stream(&t));
        let Some(bytes) = bytes else {
            self.cover = None;
            self.cover_hash = 0;
            return;
        };
        let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)) ^ bytes.len() as u64;
        if hash == self.cover_hash && self.cover.is_some() {
            return;
        }
        if self.wic.is_none() {
            // SAFETY: COM is initialised on this thread.
            self.wic = unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok() };
        }
        self.cover = self.wic.as_ref().and_then(|f| decode(f, &bytes).ok()).map(Arc::new);
        self.cover_hash = hash;
    }
}

fn read_stream(r: &windows::Storage::Streams::IRandomAccessStreamReference) -> Option<Vec<u8>> {
    let stream = r.OpenReadAsync().ok()?.join().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > 16 << 20 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream.cast::<IInputStream>().ok()?).ok()?;
    reader.LoadAsync(size as u32).ok()?.join().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    Some(buf)
}

/// Any WIC-supported image -> premultiplied BGRA, short side capped at `COVER_MAX`.
fn decode(f: &IWICImagingFactory, bytes: &[u8]) -> windows::core::Result<Cover> {
    // SAFETY: `bytes` outlives every WIC object created from it in this function.
    unsafe {
        let stream = f.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let dec = f.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let frame = dec.GetFrame(0)?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h)?;
        if w == 0 || h == 0 || w.max(h) > 8192 {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070057u32 as i32)));
        }
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(&frame, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let short = w.min(h).max(1);
        let (tw, th) =
            if short > COVER_MAX { ((w * COVER_MAX / short).clamp(1, 4096), (h * COVER_MAX / short).clamp(1, 4096)) } else { (w, h) };
        let src: IWICBitmapSource = if (tw, th) != (w, h) {
            let sc = f.CreateBitmapScaler()?;
            sc.Initialize(&conv, tw, th, WICBitmapInterpolationModeFant)?;
            sc.cast()?
        } else {
            conv.cast()?
        };
        let mut bgra = vec![0u8; (tw * th * 4) as usize];
        src.CopyPixels(std::ptr::null(), tw * 4, &mut bgra)?;
        let accent = crate::accent::pick(&bgra);
        Ok(Cover { width: tw, height: th, bgra, accent })
    }
}
