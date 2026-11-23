//! Xbox-style gamepad batteries (read-only, untested on hardware: no Xbox pad at hand).
//! XInput decides *whether* a wireless pad exists: `Windows.Gaming.Input` also lists wired
//! pads and 2.4 GHz dongles that emulate a wired Xbox 360 one (8BitDo & co.) with an invented
//! full battery (observed: Discharging, 1000/1000 mWh), XInput reports those as WIRED.
//! Only when XInput sees battery-powered pads is Gaming.Input asked for the exact percent and
//! charging state; if its list does not match, the four XInput levels are used.

use crate::model::{Device, DeviceKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows::Devices::Power::BatteryReport;
use windows::Gaming::Input::{Gamepad, RawGameController};
use windows::System::Power::BatteryStatus;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::Input::XboxController::*;

const XINPUT_SLOTS: u32 = 4;
const DEFAULT_NAME: &str = "Xbox Controller";
/// `Gamepad::Gamepads()` fills in asynchronously after the first access.
const WARM_UP: Duration = Duration::from_millis(100);
const WARM_UP_TRIES: u32 = 4;

static FIRST: AtomicBool = AtomicBool::new(true);

pub fn poll() -> Vec<Device> {
    let levels = xinput();
    if levels.is_empty() {
        return levels; // nothing battery-powered: Gaming.Input is not even touched
    }
    // SAFETY: per-thread COM setup; S_FALSE / already initialised / a different mode are all fine.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    choose(gaming_input(), levels)
}

/// Exact Gaming.Input readings only when they cover exactly the pads XInput sees.
fn choose(exact: Vec<Device>, levels: Vec<Device>) -> Vec<Device> {
    if exact.len() == levels.len() {
        exact
    } else {
        levels
    }
}

// ---- Windows.Gaming.Input -------------------------------------------------

fn gaming_input() -> Vec<Device> {
    let Ok(mut list) = Gamepad::Gamepads() else {
        return Vec::new();
    };
    if FIRST.swap(false, Ordering::Relaxed) {
        for _ in 0..WARM_UP_TRIES {
            if list.Size().is_ok_and(|n| n > 0) {
                break;
            }
            std::thread::sleep(WARM_UP);
            match Gamepad::Gamepads() {
                Ok(l) => list = l,
                Err(_) => break,
            }
        }
    }
    let mut out = Vec::new();
    for (i, pad) in list.into_iter().enumerate() {
        let Some((percent, charging)) = pad.TryGetBatteryReport().ok().and_then(|r| report_percent(&r)) else { continue };
        let raw = RawGameController::FromGameController(&pad).ok();
        let name = raw.as_ref().and_then(|r| r.DisplayName().ok()).map(|n| n.to_string()).filter(|n| !n.is_empty());
        let id = raw
            .as_ref()
            .and_then(|r| r.NonRoamableId().ok())
            .map(|n| n.to_string().chars().filter(char::is_ascii_graphic).collect::<String>())
            .filter(|n| !n.is_empty());
        out.push(Device {
            id: id.unwrap_or_else(|| format!("gip-{i}")),
            name: name.unwrap_or_else(|| DEFAULT_NAME.into()),
            kind: DeviceKind::Gamepad,
            percent,
            charging: Some(charging),
            parts: Vec::new(),
        });
    }
    out
}

/// (percent, charging) of a battery report; `None` when there is no battery (wired).
fn report_percent(r: &BatteryReport) -> Option<(u8, bool)> {
    let status = r.Status().ok()?;
    let rem = r.RemainingCapacityInMilliwattHours().ok()?.Value().ok();
    let full = r.FullChargeCapacityInMilliwattHours().ok()?.Value().ok();
    battery(status == BatteryStatus::NotPresent, rem, full, status == BatteryStatus::Charging)
}

/// Pure core of `report_percent`. The invented report of wired XInput pads (not charging,
/// 1000/1000 mWh) is dropped too; a genuinely full pad then falls back to the XInput level.
fn battery(not_present: bool, remaining: Option<i32>, full: Option<i32>, charging: bool) -> Option<(u8, bool)> {
    if not_present || (!charging && remaining == Some(1000) && full == Some(1000)) {
        return None;
    }
    Some((mwh_to_percent(remaining?, full?)?, charging))
}

fn mwh_to_percent(remaining: i32, full: i32) -> Option<u8> {
    if full <= 0 || remaining < 0 {
        return None;
    }
    let p = (i64::from(remaining) * 100 + i64::from(full) / 2) / i64::from(full);
    Some(p.clamp(0, 100) as u8)
}

// ---- XInput fallback --------------------------------------------------------

fn xinput() -> Vec<Device> {
    let mut out = Vec::new();
    for slot in 0..XINPUT_SLOTS {
        let mut state = XINPUT_STATE::default();
        let mut info = XINPUT_BATTERY_INFORMATION::default();
        // SAFETY: plain getters with valid out-pointers.
        let ok = unsafe { XInputGetState(slot, &mut state) } == ERROR_SUCCESS.0
            && unsafe { XInputGetBatteryInformation(slot, BATTERY_DEVTYPE_GAMEPAD, &mut info) } == ERROR_SUCCESS.0;
        let Some(percent) = ok.then(|| xinput_percent(info.BatteryType, info.BatteryLevel)).flatten() else { continue };
        out.push(Device {
            id: format!("xinput-{slot}"),
            name: DEFAULT_NAME.into(),
            kind: DeviceKind::Gamepad,
            percent,
            charging: None,
            parts: Vec::new(),
        });
    }
    out
}

/// Wired, unknown and disconnected report no battery; the rest only have four levels.
fn xinput_percent(kind: BATTERY_TYPE, level: BATTERY_LEVEL) -> Option<u8> {
    if matches!(kind, BATTERY_TYPE_WIRED | BATTERY_TYPE_UNKNOWN | BATTERY_TYPE_DISCONNECTED) {
        return None;
    }
    match level {
        BATTERY_LEVEL_EMPTY => Some(5),
        BATTERY_LEVEL_LOW => Some(25),
        BATTERY_LEVEL_MEDIUM => Some(60),
        BATTERY_LEVEL_FULL => Some(100),
        _ => None,
    }
}

// ---- dedupe ----------------------------------------------------------------

/// Bluetooth PnP entries of pads this source already reports (it also knows charging).
/// ponytail: while a pad is present every entry named "Xbox ..." goes, other makes only on an exact name match.
pub fn supersede(bt: &[Device], pads: &[Device]) -> Vec<Device> {
    let covered = |d: &Device| {
        let pad_like = matches!(d.kind, DeviceKind::Gamepad | DeviceKind::Other);
        !pads.is_empty()
            && pad_like
            && (d.name.to_ascii_lowercase().contains("xbox") || pads.iter().any(|p| p.name.eq_ignore_ascii_case(&d.name)))
    };
    bt.iter().filter(|d| !covered(d)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str, kind: DeviceKind) -> Device {
        Device { id: id.into(), name: name.into(), kind, percent: 50, charging: None, parts: Vec::new() }
    }

    fn ids(v: Vec<Device>) -> Vec<String> {
        v.into_iter().map(|d| d.id).collect()
    }

    #[test]
    fn mwh() {
        assert_eq!(mwh_to_percent(500, 1000), Some(50));
        assert_eq!(mwh_to_percent(1200, 1000), Some(100)); // clamped
        assert_eq!(mwh_to_percent(0, 1000), Some(0));
        assert_eq!(mwh_to_percent(1, 3), Some(33));
        assert_eq!(mwh_to_percent(i32::MAX, 1), Some(100)); // no overflow
        assert_eq!(mwh_to_percent(10, 0), None);
        assert_eq!(mwh_to_percent(-1, 1000), None);
    }

    #[test]
    fn report_rules() {
        assert_eq!(battery(false, Some(250), Some(1000), true), Some((25, true)));
        assert_eq!(battery(false, Some(250), Some(1000), false), Some((25, false)));
        assert_eq!(battery(true, Some(250), Some(1000), false), None); // wired / no battery
        assert_eq!(battery(false, None, Some(1000), false), None); // null capacity
        assert_eq!(battery(false, Some(5), None, false), None);
        // invented report of a wired / emulated Xbox 360 pad (8BitDo dongle)
        assert_eq!(battery(false, Some(1000), Some(1000), false), None);
        assert_eq!(battery(false, Some(1000), Some(1000), true), Some((100, true)));
    }

    #[test]
    fn exact_only_when_counts_match() {
        let (a, b) = (dev("a", "Pad", DeviceKind::Gamepad), dev("x", "Xbox Controller", DeviceKind::Gamepad));
        assert_eq!(ids(choose(vec![a.clone()], vec![b.clone()])), ["a"]);
        assert_eq!(ids(choose(vec![], vec![b.clone()])), ["x"]); // Gaming.Input has nothing
        assert_eq!(ids(choose(vec![a.clone(), a], vec![b])), ["x"]); // phantom pad in its list
    }

    #[test]
    fn xinput_levels_and_wired_skip() {
        assert_eq!(xinput_percent(BATTERY_TYPE_ALKALINE, BATTERY_LEVEL_EMPTY), Some(5));
        assert_eq!(xinput_percent(BATTERY_TYPE_ALKALINE, BATTERY_LEVEL_LOW), Some(25));
        assert_eq!(xinput_percent(BATTERY_TYPE_NIMH, BATTERY_LEVEL_MEDIUM), Some(60));
        assert_eq!(xinput_percent(BATTERY_TYPE_NIMH, BATTERY_LEVEL_FULL), Some(100));
        // what the 8BitDo 2.4 GHz dongle reports
        assert_eq!(xinput_percent(BATTERY_TYPE_WIRED, BATTERY_LEVEL_FULL), None);
        assert_eq!(xinput_percent(BATTERY_TYPE_UNKNOWN, BATTERY_LEVEL_FULL), None);
        assert_eq!(xinput_percent(BATTERY_TYPE_DISCONNECTED, BATTERY_LEVEL_FULL), None);
        assert_eq!(xinput_percent(BATTERY_TYPE_ALKALINE, BATTERY_LEVEL(9)), None);
    }

    #[test]
    fn dedupe_against_bluetooth() {
        let bt = vec![
            dev("a", "Xbox Wireless Controller", DeviceKind::Gamepad),
            dev("b", "Xbox Controller", DeviceKind::Other),
            dev("c", "Redmi Buds 6", DeviceKind::Headset),
            dev("d", "Wireless Controller", DeviceKind::Gamepad),
            dev("e", "Xbox Wireless Headset", DeviceKind::Headset), // not a pad: kept
        ];
        let xbox = vec![dev("p", "Xbox Wireless Controller", DeviceKind::Gamepad)];
        assert_eq!(ids(supersede(&bt, &xbox)), ["c", "d", "e"]); // another make and the headset are kept
        assert_eq!(ids(supersede(&bt, &[])), ["a", "b", "c", "d", "e"]); // no pad here: nothing dropped
        let other = vec![dev("p", "wireless controller", DeviceKind::Gamepad)];
        assert_eq!(ids(supersede(&bt, &other)), ["c", "e"]); // exact name, any case
    }
}
