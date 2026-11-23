//! Battery sources (read-only) + the worker thread that polls them.
//! Each source is a module with a `poll() -> Vec<Device>`; to add one, write the
//! module and add a field/call in `Sources` below.

mod ble;
mod bluetooth;
mod live;
mod logitech;

pub use live::LiveMode;

use crate::model::{Device, DeviceKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_GETIDLIST_FILTER_ENUMERATOR, CM_GETIDLIST_FILTER_PRESENT, CR_BUFFER_SMALL,
    CR_SUCCESS,
};

const BLUETOOTH_EVERY: Duration = Duration::from_secs(60);
const LOGITECH_EVERY: Duration = Duration::from_secs(300);
/// After a refresh request, poll once more: Windows publishes the battery
/// property a few seconds after a device connects.
const SETTLE: Duration = Duration::from_secs(5);
const ID_FILTER: u32 = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
const MAX_ID_LIST: u32 = 1 << 20;

/// Heuristic (Fast Pair / Apple) adverts older than this are forgotten.
const SEEN_MAX: Duration = Duration::from_secs(30);
/// Slack so a timed wake-up never lands a hair before its deadline.
const SLACK: Duration = Duration::from_millis(20);

pub struct BatteryHandle {
    tx: Sender<Msg>,
    mode: AtomicU8,
}

enum Msg {
    Refresh,
    Live(LiveMode),
    /// One decoded advert from the live watcher.
    Advert(u64, ble::Found),
    /// Result of a one-shot scan.
    Buds(Vec<ble::Found>),
    Quit,
}

impl BatteryHandle {
    /// Ask the worker to re-poll now (WM_DEVICECHANGE, resume from sleep). Cheap, non-blocking.
    pub fn refresh(&self) {
        let _ = self.tx.send(Msg::Refresh);
    }

    /// Tell the worker how closely someone is looking: hidden -> `Off`, shown collapsed
    /// -> `Duty`, expanded -> `Continuous`. While not `Off` and a supported headset is
    /// connected, a passive BLE watcher reads left/right/case levels from its adverts
    /// (results via `on_update`). Cheap, non-blocking; repeated calls with the same mode
    /// send nothing.
    pub fn set_live(&self, mode: LiveMode) {
        if self.mode.swap(mode as u8, Ordering::Relaxed) != mode as u8 {
            let _ = self.tx.send(Msg::Live(mode));
        }
    }
}

impl Drop for BatteryHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
    }
}

fn is_headset(d: &Device) -> bool {
    matches!(d.kind, DeviceKind::Headphones | DeviceKind::Headset)
}

fn headsets(bt: &[Device]) -> Vec<ble::Headset> {
    bt.iter().filter(|d| is_headset(d)).map(|d| ble::Headset { id: d.id.clone(), name: d.name.clone(), percent: d.percent }).collect()
}

/// Start a BLE scan if a headset is connected; the result comes back as `Msg::Buds`.
fn start_scan(bt: &[Device], tx: &Sender<Msg>) {
    let tx = tx.clone();
    ble::spawn_scan(headsets(bt), move |found| {
        let _ = tx.send(Msg::Buds(found));
    });
}

type Seen = HashMap<u64, (Instant, ble::Found)>;

/// Record parts from an advert (a hit also restarts the staleness clock).
fn attach(hit: Option<(String, Vec<crate::model::Part>)>, attached: &mut Option<ble::Attached>, live: &mut live::Live, publish: &mut bool) {
    let Some((device_id, parts)) = hit else { return };
    let now = Instant::now();
    live.matched(now);
    *publish |= attached.as_ref().is_none_or(|a| a.device_id != device_id || a.parts != parts);
    *attached = Some(ble::Attached { device_id, parts, at: now });
}

/// Make the live watcher match the policy: running only for the vendors of the connected
/// headsets and only while `live` wants a scan. Runs on the worker thread.
fn sync_watch(watch: &mut Option<ble::LiveWatch>, live: &mut live::Live, seen: &mut Seen, bt: &[Device], tx: &Sender<Msg>) {
    let now = Instant::now();
    let want = if live.wants_scan(now) { ble::vendors_for(&headsets(bt)) } else { Vec::new() };
    if watch.as_ref().is_some_and(|w| !w.alive() || w.vendors() != want) {
        *watch = None;
        seen.clear(); // adverts from before a gap must not vouch for the present
    }
    if watch.is_none() && !want.is_empty() {
        let tx = tx.clone();
        *watch = ble::start_live(&want, move |addr, f| {
            let _ = tx.send(Msg::Advert(addr, f));
        });
    }
    live.set_scanning(watch.is_some(), now);
}

/// Starts one background thread. Calls `on_update` with the full current list
/// whenever it changes (not on every poll). List sorted: Headphones/Headset first,
/// then Mouse, Keyboard, Gamepad, Other; then by name.
pub fn spawn(on_update: Box<dyn Fn(Vec<Device>) + Send>) -> BatteryHandle {
    let (tx, rx) = channel::<Msg>();
    let tx_worker = tx.clone();
    let started = std::thread::Builder::new().name("battery".into()).spawn(move || {
        let mut logitech = logitech::Poller::default();
        let (mut bt, mut lg) = (Vec::new(), Vec::new());
        let mut last: Option<Vec<Device>> = None;
        let mut attached: Option<ble::Attached> = None;
        let now = Instant::now();
        let mut live = live::Live::new(now);
        let mut watch: Option<ble::LiveWatch> = None;
        let mut seen = Seen::new();
        let (mut next_bt, mut next_lg, mut settle_at) = (now, now, None::<Instant>);
        let mut publish = false;
        loop {
            let now = Instant::now();
            let refreshed = settle_at.is_some_and(|t| now >= t);
            if refreshed {
                settle_at = None;
            }
            let (do_bt, do_lg) = (now >= next_bt || refreshed, now >= next_lg || refreshed);
            sync_watch(&mut watch, &mut live, &mut seen, &bt, &tx_worker); // before slow polls: hiding must stop it promptly
            if do_bt {
                let polled = bluetooth::poll();
                // a headset that was not connected before: look for its per-bud advert
                let new_headset = polled.iter().any(|d| is_headset(d) && !bt.iter().any(|o: &Device| o.id == d.id));
                bt = polled;
                // (a live watcher already listens; otherwise take one short one-shot look)
                if new_headset && !live.wants_scan(now) {
                    start_scan(&bt, &tx_worker);
                }
                next_bt = now + BLUETOOTH_EVERY;
            }
            if do_lg {
                lg = logitech.poll();
                next_lg = now + LOGITECH_EVERY;
            }
            sync_watch(&mut watch, &mut live, &mut seen, &bt, &tx_worker);
            let now = Instant::now(); // after a possible watcher start
            if let Some(a) = attached.as_ref().filter(|a| live.stale(a.at, now)) {
                log(&format!("battery: parts of {} are stale", a.device_id));
                attached = None; // nothing heard from a running watcher: buds are in the ears
                publish = true;
            }
            if do_bt || do_lg || publish {
                publish = false;
                let mut list = merge(vec![bt.clone(), lg.clone()]);
                ble::apply(&mut list, &attached);
                if last.as_ref() != Some(&list) {
                    last = Some(list.clone());
                    on_update(list);
                }
            }
            let stale_at = attached.as_ref().and_then(|a| live.stale_at(a.at, now)).map(|t| t + SLACK);
            let edge = live.next_edge(now).map(|t| t + SLACK);
            let wake = [Some(next_bt), Some(next_lg), settle_at, stale_at, edge].into_iter().flatten().min();
            let wait = wake.map_or(BLUETOOTH_EVERY, |t| t.saturating_duration_since(Instant::now()));
            let mut msgs = Vec::new();
            match rx.recv_timeout(wait) {
                Ok(m) => {
                    msgs.push(m);
                    msgs.extend(rx.try_iter()); // coalesce bursts
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let mut refresh = false;
            for m in msgs {
                match m {
                    Msg::Refresh => refresh = true,
                    Msg::Live(mode) => live.set_mode(mode, Instant::now()),
                    Msg::Advert(addr, f) => {
                        let now = Instant::now();
                        seen.retain(|_, (t, _)| now.duration_since(*t) < SEEN_MAX);
                        seen.insert(addr, (now, f.clone()));
                        let found: Vec<ble::Found> = seen.values().map(|(_, f)| f.clone()).collect();
                        // only an advert that itself fits a headset proves the buds are talking now;
                        // the cache just disambiguates between candidates
                        let hs = headsets(&bt);
                        if ble::pick(std::slice::from_ref(&f), &hs).is_some() {
                            attach(ble::pick(&found, &hs), &mut attached, &mut live, &mut publish);
                        }
                    }
                    Msg::Buds(found) => attach(ble::pick(&found, &headsets(&bt)), &mut attached, &mut live, &mut publish),
                    Msg::Quit => return,
                }
            }
            if refresh {
                let now = Instant::now();
                next_bt = now; // poll immediately, then once more after SETTLE
                next_lg = now;
                settle_at = Some(now + SETTLE);
            }
        }
    });
    if started.is_err() {
        log("battery: cannot start worker thread");
    }
    BatteryHandle { tx, mode: AtomicU8::new(LiveMode::Off as u8) }
}

/// Synchronous one-shot poll of every source, for `--dump-batteries`.
pub fn snapshot_now() -> Vec<Device> {
    merge(vec![bluetooth::poll(), logitech::poll()])
}

/// Dedupe by `id` (first source wins) and sort per the public contract.
fn merge(sources: Vec<Vec<Device>>) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for d in sources.into_iter().flatten() {
        if !out.iter().any(|o| o.id == d.id) {
            out.push(d);
        }
    }
    out.sort_by(|a, b| (rank(a.kind), a.name.to_lowercase(), &a.id).cmp(&(rank(b.kind), b.name.to_lowercase(), &b.id)));
    out
}

fn rank(k: DeviceKind) -> u8 {
    match k {
        DeviceKind::Headphones | DeviceKind::Headset => 0,
        DeviceKind::Mouse => 1,
        DeviceKind::Keyboard => 2,
        DeviceKind::Gamepad => 3,
        DeviceKind::Other => 4,
    }
}

/// Instance ids of present device nodes under one enumerator ("BTHENUM", "HID", ...).
pub(crate) fn device_ids(enumerator: &str) -> Vec<String> {
    let filter = wide(enumerator);
    let filter = PCWSTR(filter.as_ptr());
    // The list can grow between the size query and the fetch (CR_BUFFER_SMALL): retry.
    for _ in 0..3 {
        let mut len = 0u32;
        let sized = unsafe { CM_Get_Device_ID_List_SizeW(&mut len, filter, ID_FILTER) };
        if sized != CR_SUCCESS || !(2..=MAX_ID_LIST).contains(&len) {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        match unsafe { CM_Get_Device_ID_ListW(filter, &mut buf, ID_FILTER) } {
            CR_SUCCESS => {
                return buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect();
            }
            CR_BUFFER_SMALL => {}
            _ => break,
        }
    }
    log(&format!("id list failed for {enumerator}"));
    Vec::new()
}

/// NUL-terminated UTF-16.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Debug builds only: `OutputDebugStringW`.
#[cfg(debug_assertions)]
pub(crate) fn log(msg: &str) {
    let w = wide(&format!(
        "isle: {msg}
"
    ));
    unsafe { windows::Win32::System::Diagnostics::Debug::OutputDebugStringW(PCWSTR(w.as_ptr())) };
}
#[cfg(not(debug_assertions))]
pub(crate) fn log(_msg: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str, kind: DeviceKind, percent: u8) -> Device {
        Device { id: id.into(), name: name.into(), kind, percent, charging: None, parts: Vec::new() }
    }

    #[test]
    fn merge_dedupes_first_wins_and_sorts() {
        let bt = vec![
            dev("k", "Zed Keys", DeviceKind::Keyboard, 50),
            dev("m", "Mouse B", DeviceKind::Mouse, 40),
            dev("h", "Buds", DeviceKind::Headset, 90),
        ];
        let lg = vec![
            dev("m", "Dup of m", DeviceKind::Mouse, 1),
            dev("m2", "mouse a", DeviceKind::Mouse, 70),
            dev("o", "Thing", DeviceKind::Other, 10),
            dev("g", "Pad", DeviceKind::Gamepad, 20),
            dev("h2", "Cans", DeviceKind::Headphones, 30),
        ];
        let out = merge(vec![bt, lg]);
        let ids: Vec<&str> = out.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["h", "h2", "m2", "m", "k", "g", "o"]);
        assert_eq!(out.iter().find(|d| d.id == "m").map(|d| d.percent), Some(40));
    }
}

#[cfg(test)]
mod live_cpu {
    use super::*;
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn cpu() -> f64 {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).ok() };
        let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
        t(k) + t(u)
    }

    /// Radio test: `cargo test --release live_cpu -- --ignored --nocapture`.
    /// Measures the watchers themselves (no headset needed) and the idle worker.
    #[test]
    #[ignore]
    fn live_cpu() {
        let report = |label: &str, c: f64, w: f64, n: usize| {
            println!("{label}: {w:.0}s wall, {:.0} ms cpu ({:.3}% of one core), {n} adverts", c * 1000.0, c / w * 100.0)
        };
        let count = |vendors: &[ble::Vendor], secs: u64| {
            let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let n2 = n.clone();
            let (c0, t0) = (cpu(), Instant::now());
            let w = ble::start_live(vendors, move |_, _| {
                n2.fetch_add(1, Ordering::Relaxed);
            })
            .expect("watcher");
            std::thread::sleep(Duration::from_secs(secs));
            drop(w);
            (cpu() - c0, t0.elapsed().as_secs_f64(), n.load(Ordering::Relaxed))
        };
        let (c, w, n) = count(&[ble::Vendor::Xiaomi], 20);
        report("continuous, Xiaomi only", c, w, n);
        let (c, w, n) = count(&[ble::Vendor::FastPair, ble::Vendor::Apple, ble::Vendor::Xiaomi], 20);
        report("continuous, all vendors", c, w, n);
        // duty windows: start, 4 s, stop, 26 s pause (x2)
        let (c0, t0) = (cpu(), Instant::now());
        let mut n = 0;
        for _ in 0..2 {
            n += count(&[ble::Vendor::Xiaomi], 4).2;
            std::thread::sleep(Duration::from_secs(26));
        }
        report("duty, Xiaomi, 2 cycles", cpu() - c0, t0.elapsed().as_secs_f64(), n);
        // worker with the island hidden: nothing may run
        let h = spawn(Box::new(|_| {}));
        std::thread::sleep(Duration::from_secs(3));
        let (c0, t0) = (cpu(), Instant::now());
        h.set_live(LiveMode::Off);
        std::thread::sleep(Duration::from_secs(20));
        report("worker idle, Off", cpu() - c0, t0.elapsed().as_secs_f64(), 0);
    }
}
