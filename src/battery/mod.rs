//! Battery sources (read-only) + the worker thread that polls them.
//! Each source is a module with a `poll() -> Vec<Device>`; to add one, write the
//! module and add a field/call in `Sources` below.

mod ble;
mod bluetooth;
mod logitech;

use crate::model::{Device, DeviceKind};
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

pub struct BatteryHandle {
    tx: Sender<Msg>,
}

enum Msg {
    Refresh,
    ScanBuds,
    Buds(Vec<ble::Found>),
    Quit,
}

impl BatteryHandle {
    /// Ask the worker to re-poll now (WM_DEVICECHANGE, resume from sleep). Cheap, non-blocking.
    pub fn refresh(&self) {
        let _ = self.tx.send(Msg::Refresh);
    }

    /// Ask for a short (<= 8 s) passive BLE scan for left/right/case battery of
    /// connected earbuds (call when the island expands). Cheap, non-blocking; ignored
    /// while a scan runs or no headset is connected. Result arrives via `on_update`.
    pub fn scan_buds(&self) {
        let _ = self.tx.send(Msg::ScanBuds);
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
        let (mut next_bt, mut next_lg, mut settle_at) = (now, now, None::<Instant>);
        let mut publish = false;
        loop {
            let now = Instant::now();
            let refreshed = settle_at.is_some_and(|t| now >= t);
            if refreshed {
                settle_at = None;
            }
            let (do_bt, do_lg) = (now >= next_bt || refreshed, now >= next_lg || refreshed);
            if do_bt {
                let polled = bluetooth::poll();
                // a headset that was not connected before: look for its per-bud advert
                let new_headset = polled.iter().any(|d| is_headset(d) && !bt.iter().any(|o: &Device| o.id == d.id));
                bt = polled;
                if new_headset {
                    start_scan(&bt, &tx_worker);
                }
                next_bt = now + BLUETOOTH_EVERY;
            }
            if do_lg {
                lg = logitech.poll();
                next_lg = now + LOGITECH_EVERY;
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
            let wake = [Some(next_bt), Some(next_lg), settle_at].into_iter().flatten().min();
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
                    Msg::ScanBuds => start_scan(&bt, &tx_worker),
                    Msg::Buds(found) => {
                        if let Some((device_id, parts)) = ble::pick(&found, &headsets(&bt)) {
                            attached = Some(ble::Attached { device_id, parts, at: Instant::now() });
                            publish = true;
                        }
                    }
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
    BatteryHandle { tx }
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
