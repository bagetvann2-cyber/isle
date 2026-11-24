//! Alert detection (device connect / low battery / AC change / laptop low) and a small
//! display queue. Pure state machines: every call takes `now` (ms, `GetTickCount64` base),
//! nothing here reads a clock or runs a timer.

use crate::model::{Device, PartKind};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Alert {
    DeviceConnected {
        id: String,
    },
    DeviceLow {
        id: String,
        level: u8,
    },
    Power {
        plugged: bool,
        percent: u8,
    },
    LaptopLow {
        percent: u8,
    },
    TimerDone,
    /// A newer release is out (`version` as displayed, e.g. "0.3.0"). Shown without a sound.
    UpdateAvailable {
        version: String,
    },
    /// Banner-only states of a user-started install; the UI shows them itself, they never enter the queue.
    Updating,
    UpdateFailed,
    /// The new exe is on disk but could not be started: the user restarts isle by hand.
    UpdateInstalled,
}

const CONNECT_COOLDOWN_MS: u64 = 60_000;
const HYSTERESIS: u8 = 5;
const DEVICE_LOW: (u8, u8) = (15, 5);
const LAPTOP_LOW: (u8, u8) = (20, 10);

/// Two-threshold "fire once per crossing" latch. A threshold re-arms once the level
/// climbs `HYSTERESIS` above it.
#[derive(Clone, Copy)]
struct Latch {
    hi: bool,
    lo: bool,
}

impl Latch {
    /// State for a level seen for the first time: thresholds already crossed stay quiet.
    fn at(level: u8, (hi, lo): (u8, u8)) -> Self {
        Latch { hi: level > hi, lo: level > lo }
    }

    /// Feed a level; true when a threshold was just crossed downwards (one alert even if both).
    fn step(&mut self, level: u8, (hi, lo): (u8, u8)) -> bool {
        self.hi |= level > hi + HYSTERESIS;
        self.lo |= level > lo + HYSTERESIS;
        let fire = (level <= hi && self.hi) || (level <= lo && self.lo);
        self.hi &= level > hi;
        self.lo &= level > lo;
        fire
    }
}

/// The percent the UI shows: weakest earbud if there are L/R parts, else the device level.
fn compact_percent(d: &Device) -> u8 {
    d.parts.iter().filter(|p| p.kind != PartKind::Case).map(|p| p.percent).min().unwrap_or(d.percent)
}

pub struct Tracker {
    /// `None` until the first device list (baseline).
    seen: Option<HashSet<String>>,
    low: HashMap<String, Latch>,
    last_connect: HashMap<String, u64>,
    muted_until: u64,
    /// `None` until the first power sample (baseline).
    ac: Option<bool>,
    laptop: Latch,
}

impl Tracker {
    pub fn new() -> Self {
        Tracker {
            seen: None,
            low: HashMap::new(),
            last_connect: HashMap::new(),
            muted_until: 0,
            ac: None,
            laptop: Latch { hi: true, lo: true },
        }
    }

    /// Suppress connect alerts until `ms` (call with now+15000 after resume / unlock,
    /// when devices reappear).
    pub fn mute_until(&mut self, ms: u64) {
        self.muted_until = self.muted_until.max(ms);
    }

    pub fn on_devices(&mut self, now: u64, devs: &[Device]) -> Vec<Alert> {
        let mut out = Vec::new();
        self.last_connect.retain(|_, t| now.saturating_sub(*t) < CONNECT_COOLDOWN_MS);
        let baseline = self.seen.is_none();
        let prev = self.seen.take().unwrap_or_default();
        // Forget devices that went away, so a reconnect starts fresh.
        self.low.retain(|id, _| devs.iter().any(|d| &d.id == id));
        for d in devs {
            let level = compact_percent(d);
            match self.low.get_mut(&d.id) {
                Some(latch) => {
                    // Charging devices keep their latch fed (re-arm) but never fire.
                    if latch.step(level, DEVICE_LOW) && d.charging != Some(true) {
                        out.push(Alert::DeviceLow { id: d.id.clone(), level });
                    }
                }
                None => {
                    self.low.insert(d.id.clone(), Latch::at(level, DEVICE_LOW));
                }
            }
            if !baseline && !prev.contains(&d.id) && now >= self.muted_until && !self.last_connect.contains_key(&d.id) {
                self.last_connect.insert(d.id.clone(), now);
                out.push(Alert::DeviceConnected { id: d.id.clone() });
            }
        }
        self.seen = Some(devs.iter().map(|d| d.id.clone()).collect());
        out
    }

    pub fn on_power(&mut self, _now: u64, ac: bool, percent: u8) -> Vec<Alert> {
        let mut out = Vec::new();
        let changed = self.ac.is_some_and(|prev| prev != ac);
        if self.ac.is_none() || changed {
            // Baseline / transition: levels already below a threshold stay quiet.
            self.laptop = if ac { Latch { hi: true, lo: true } } else { Latch::at(percent, LAPTOP_LOW) };
            if changed {
                out.push(Alert::Power { plugged: ac, percent });
            }
        } else if !ac && self.laptop.step(percent, LAPTOP_LOW) {
            out.push(Alert::LaptopLow { percent });
        }
        self.ac = Some(ac);
        out
    }
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_QUEUED: usize = 4;
const TTL_SHORT_MS: u64 = 60_000;
const TTL_LONG_MS: u64 = 300_000;

/// `None` = never expires.
fn ttl(a: &Alert) -> Option<u64> {
    match a {
        Alert::DeviceConnected { .. } | Alert::Power { .. } => Some(TTL_SHORT_MS),
        Alert::DeviceLow { .. } | Alert::LaptopLow { .. } => Some(TTL_LONG_MS),
        Alert::TimerDone => None,
        // Offered for 5 min; if the island could not show it by then, the menu item is still there.
        Alert::UpdateAvailable { .. } => Some(TTL_LONG_MS),
        Alert::Updating | Alert::UpdateFailed | Alert::UpdateInstalled => Some(TTL_SHORT_MS),
    }
}

/// Same kind (and same device, where there is one): a newer alert replaces the queued one.
fn same_slot(a: &Alert, b: &Alert) -> bool {
    use Alert::*;
    match (a, b) {
        (DeviceConnected { id: x }, DeviceConnected { id: y }) | (DeviceLow { id: x, .. }, DeviceLow { id: y, .. }) => x == y,
        (Power { .. }, Power { .. })
        | (LaptopLow { .. }, LaptopLow { .. })
        | (TimerDone, TimerDone)
        | (Updating, Updating)
        | (UpdateFailed, UpdateFailed)
        | (UpdateInstalled, UpdateInstalled) => true,
        // Only the newest offer matters: a newer version replaces a queued older one.
        (UpdateAvailable { .. }, UpdateAvailable { .. }) => true,
        _ => false,
    }
}

#[derive(Default)]
pub struct Queue {
    items: Vec<(Alert, u64)>, // alert, pushed-at
    /// What `current` last handed out: `dismiss` removes this alert, not whatever is at the
    /// head by then (eviction or expiry may have shifted the queue meanwhile).
    shown: Option<Alert>,
}

impl Queue {
    pub fn push(&mut self, a: Alert, now: u64) {
        if let Some(slot) = self.items.iter_mut().find(|(q, _)| same_slot(q, &a)) {
            *slot = (a, now);
            return;
        }
        self.items.push((a, now));
        if self.items.len() > MAX_QUEUED {
            if let Some(i) = self.items.iter().position(|(q, _)| *q != Alert::TimerDone) {
                self.items.remove(i);
            }
        }
    }

    /// Head of the queue, if it should be on screen now. Expired entries are dropped
    /// even when `can_show` is false.
    pub fn current(&mut self, now: u64, can_show: bool) -> Option<&Alert> {
        self.items.retain(|(a, at)| ttl(a).is_none_or(|t| now.saturating_sub(*at) < t));
        if !can_show {
            return None;
        }
        // Stay on the alert already on screen (repeated calls are idempotent) until dismissed.
        let at = self.shown.as_ref().and_then(|s| self.items.iter().position(|(q, _)| same_slot(q, s))).unwrap_or(0);
        let a = &self.items.get(at)?.0;
        self.shown = Some(a.clone());
        Some(a)
    }

    /// Drop the alert `current` last returned (UI calls this after ~3 s, or on click for
    /// `TimerDone`). If that one is already gone (evicted / expired), nothing is removed.
    pub fn dismiss(&mut self) {
        match self.shown.take() {
            Some(a) => self.items.retain(|(q, _)| !same_slot(q, &a)),
            None if !self.items.is_empty() => {
                self.items.remove(0);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceKind, Part};

    fn dev(id: &str, pct: u8) -> Device {
        Device { id: id.into(), name: id.into(), kind: DeviceKind::Mouse, percent: pct, charging: None, parts: vec![] }
    }
    fn buds(id: &str, l: u8, r: u8, case: u8) -> Device {
        let p = |kind, percent| Part { kind, percent, charging: false };
        Device {
            parts: vec![p(PartKind::Left, l), p(PartKind::Right, r), p(PartKind::Case, case)],
            kind: DeviceKind::Headphones,
            ..dev(id, 100)
        }
    }
    fn conn(id: &str) -> Alert {
        Alert::DeviceConnected { id: id.into() }
    }
    fn low(id: &str, level: u8) -> Alert {
        Alert::DeviceLow { id: id.into(), level }
    }

    #[test]
    fn baseline_is_silent_then_connect_fires() {
        let mut t = Tracker::new();
        assert!(t.on_devices(0, &[dev("a", 80)]).is_empty());
        assert!(t.on_devices(1, &[dev("a", 80)]).is_empty());
        assert_eq!(t.on_devices(2, &[dev("a", 80), dev("b", 50)]), vec![conn("b")]);
        assert!(t.on_devices(3, &[dev("a", 80), dev("b", 50)]).is_empty());
    }

    #[test]
    fn empty_baseline_counts() {
        let mut t = Tracker::new();
        assert!(t.on_devices(0, &[]).is_empty());
        assert_eq!(t.on_devices(1, &[dev("a", 80)]), vec![conn("a")]);
    }

    #[test]
    fn mute_window() {
        let mut t = Tracker::new();
        t.on_devices(0, &[]);
        t.mute_until(15_000);
        assert!(t.on_devices(10_000, &[dev("a", 80)]).is_empty());
        // Muted connect is not remembered: after a reconnect past the window it fires.
        t.on_devices(11_000, &[]);
        assert_eq!(t.on_devices(15_000, &[dev("a", 80)]), vec![conn("a")]);
    }

    #[test]
    fn per_device_cooldown() {
        let mut t = Tracker::new();
        t.on_devices(0, &[]);
        assert_eq!(t.on_devices(1_000, &[dev("a", 80)]), vec![conn("a")]);
        t.on_devices(2_000, &[]);
        assert!(t.on_devices(30_000, &[dev("a", 80)]).is_empty()); // flapping
        assert_eq!(t.on_devices(30_000, &[dev("a", 80), dev("b", 80)]), vec![conn("b")]); // other id unaffected
        t.on_devices(31_000, &[]);
        assert_eq!(t.on_devices(61_001, &[dev("a", 80)]), vec![conn("a")]);
    }

    #[test]
    fn low_crossings_and_hysteresis() {
        let mut t = Tracker::new();
        t.on_devices(0, &[dev("a", 50)]);
        assert!(t.on_devices(1, &[dev("a", 16)]).is_empty());
        assert_eq!(t.on_devices(2, &[dev("a", 15)]), vec![low("a", 15)]);
        assert!(t.on_devices(3, &[dev("a", 14)]).is_empty()); // once per crossing
        assert!(t.on_devices(4, &[dev("a", 18)]).is_empty());
        assert!(t.on_devices(5, &[dev("a", 12)]).is_empty()); // not re-armed (<= 20)
        assert_eq!(t.on_devices(6, &[dev("a", 5)]), vec![low("a", 5)]);
        assert!(t.on_devices(7, &[dev("a", 3)]).is_empty());
        assert!(t.on_devices(8, &[dev("a", 9)]).is_empty());
        assert!(t.on_devices(9, &[dev("a", 5)]).is_empty()); // 9 <= 10, not re-armed
        t.on_devices(10, &[dev("a", 11)]); // above 10: 5-latch re-arms
        assert_eq!(t.on_devices(11, &[dev("a", 4)]), vec![low("a", 4)]);
        t.on_devices(12, &[dev("a", 21)]); // above 20: 15-latch re-arms
        assert_eq!(t.on_devices(13, &[dev("a", 14)]), vec![low("a", 14)]);
    }

    #[test]
    fn big_drop_fires_once() {
        let mut t = Tracker::new();
        t.on_devices(0, &[dev("a", 50)]);
        assert_eq!(t.on_devices(1, &[dev("a", 4)]), vec![low("a", 4)]);
        assert!(t.on_devices(2, &[dev("a", 3)]).is_empty());
    }

    #[test]
    fn charging_never_fires_and_rearms() {
        let mut t = Tracker::new();
        t.on_devices(0, &[dev("a", 50)]);
        let mut c = dev("a", 10);
        c.charging = Some(true);
        assert!(t.on_devices(1, &[c.clone()]).is_empty());
        c.percent = 40;
        assert!(t.on_devices(2, &[c]).is_empty());
        assert_eq!(t.on_devices(3, &[dev("a", 14)]), vec![low("a", 14)]);
    }

    #[test]
    fn connecting_low_has_no_low_alert() {
        let mut t = Tracker::new();
        t.on_devices(0, &[]);
        assert_eq!(t.on_devices(1, &[dev("a", 10)]), vec![conn("a")]);
        assert!(t.on_devices(2, &[dev("a", 9)]).is_empty());
        assert_eq!(t.on_devices(3, &[dev("a", 5)]), vec![low("a", 5)]);
        // Baseline-low devices are quiet too.
        let mut t = Tracker::new();
        assert!(t.on_devices(0, &[dev("b", 3)]).is_empty());
        assert!(t.on_devices(1, &[dev("b", 2)]).is_empty());
    }

    #[test]
    fn buds_use_weakest_earbud() {
        assert_eq!(compact_percent(&buds("x", 40, 20, 5)), 20);
        assert_eq!(compact_percent(&dev("x", 33)), 33);
        let mut t = Tracker::new();
        t.on_devices(0, &[buds("x", 60, 60, 90)]);
        // Case at 5% is ignored; the right bud drops to 14.
        assert_eq!(t.on_devices(1, &[buds("x", 60, 14, 5)]), vec![low("x", 14)]);
    }

    #[test]
    fn reconnect_resets_low_state() {
        let mut t = Tracker::new();
        t.on_devices(0, &[dev("a", 50)]);
        assert_eq!(t.on_devices(1, &[dev("a", 14)]), vec![low("a", 14)]);
        t.on_devices(2, &[]);
        // Back (after cooldown) at 14: connect banner only, no low alert.
        assert_eq!(t.on_devices(70_000, &[dev("a", 14)]), vec![conn("a")]);
    }

    #[test]
    fn power_baseline_and_plug() {
        let mut t = Tracker::new();
        assert!(t.on_power(0, true, 80).is_empty());
        assert!(t.on_power(1, true, 81).is_empty());
        assert_eq!(t.on_power(2, false, 81), vec![Alert::Power { plugged: false, percent: 81 }]);
        assert_eq!(t.on_power(3, true, 81), vec![Alert::Power { plugged: true, percent: 81 }]);
    }

    #[test]
    fn laptop_low_crossings_and_rearm_on_ac() {
        let mut t = Tracker::new();
        t.on_power(0, false, 50);
        assert!(t.on_power(1, false, 21).is_empty());
        assert_eq!(t.on_power(2, false, 20), vec![Alert::LaptopLow { percent: 20 }]);
        assert!(t.on_power(3, false, 19).is_empty());
        assert_eq!(t.on_power(4, false, 10), vec![Alert::LaptopLow { percent: 10 }]);
        assert!(t.on_power(5, false, 9).is_empty());
        // Plug in: Power alert only; unplug at 30: Power alert, thresholds armed again.
        assert_eq!(t.on_power(6, true, 9), vec![Alert::Power { plugged: true, percent: 9 }]);
        assert!(t.on_power(7, true, 8).is_empty());
        assert_eq!(t.on_power(8, false, 30), vec![Alert::Power { plugged: false, percent: 30 }]);
        assert_eq!(t.on_power(9, false, 19), vec![Alert::LaptopLow { percent: 19 }]);
        // Hysteresis on battery: back above 25 re-arms the 20 threshold.
        assert!(t.on_power(10, false, 22).is_empty());
        assert!(t.on_power(11, false, 18).is_empty());
        t.on_power(12, false, 26);
        assert_eq!(t.on_power(13, false, 20), vec![Alert::LaptopLow { percent: 20 }]);
    }

    #[test]
    fn unplug_while_low_is_quiet() {
        let mut t = Tracker::new();
        t.on_power(0, true, 9);
        assert_eq!(t.on_power(1, false, 9), vec![Alert::Power { plugged: false, percent: 9 }]);
        assert!(t.on_power(2, false, 8).is_empty());
    }

    #[test]
    fn laptop_baseline_low_is_quiet() {
        let mut t = Tracker::new();
        assert!(t.on_power(0, false, 15).is_empty());
        assert!(t.on_power(1, false, 14).is_empty());
        assert_eq!(t.on_power(2, false, 10), vec![Alert::LaptopLow { percent: 10 }]);
    }

    #[test]
    fn queue_fifo_and_dismiss() {
        let mut q = Queue::default();
        assert!(q.current(0, true).is_none());
        q.push(conn("a"), 0);
        q.push(conn("b"), 0);
        assert_eq!(q.current(1, true), Some(&conn("a")));
        q.dismiss();
        assert_eq!(q.current(1, true), Some(&conn("b")));
        q.dismiss();
        assert!(q.current(1, true).is_none());
        q.dismiss(); // empty: no panic
    }

    #[test]
    fn queue_merge() {
        let mut q = Queue::default();
        q.push(low("a", 15), 0);
        q.push(conn("a"), 0);
        q.push(low("a", 5), 10); // replaces the queued low in place
        assert_eq!(q.current(10, true), Some(&low("a", 5)));
        q.dismiss();
        assert_eq!(q.current(10, true), Some(&conn("a")));
        q.dismiss();
        q.push(Alert::Power { plugged: true, percent: 50 }, 0);
        q.push(Alert::Power { plugged: false, percent: 49 }, 1);
        assert_eq!(q.current(1, true), Some(&Alert::Power { plugged: false, percent: 49 }));
        q.dismiss();
        assert!(q.current(1, true).is_none());
    }

    #[test]
    fn queue_cap_drops_oldest_non_timer() {
        let mut q = Queue::default();
        q.push(Alert::TimerDone, 0);
        for id in ["a", "b", "c", "d"] {
            q.push(conn(id), 0);
        }
        // 5 pushed, cap 4: oldest non-timer ("a") went, the timer stayed at the head.
        assert_eq!(q.current(1, true), Some(&Alert::TimerDone));
        q.dismiss();
        assert_eq!(q.current(1, true), Some(&conn("b")));
        let mut q = Queue::default();
        for id in ["a", "b", "c", "d", "e"] {
            q.push(conn(id), 0);
        }
        assert_eq!(q.current(1, true), Some(&conn("b")));
    }

    #[test]
    fn queue_expiry() {
        let mut q = Queue::default();
        q.push(conn("a"), 0);
        q.push(Alert::Power { plugged: true, percent: 1 }, 0);
        q.push(low("a", 15), 0);
        q.push(Alert::LaptopLow { percent: 20 }, 0);
        q.push(Alert::TimerDone, 0);
        assert!(q.current(59_999, true).is_some());
        assert_eq!(q.current(60_000, true), Some(&low("a", 15))); // short-lived ones gone
        q.dismiss();
        assert_eq!(q.current(299_999, true), Some(&Alert::LaptopLow { percent: 20 }));
        q.dismiss();
        assert_eq!(q.current(300_000, true), Some(&Alert::TimerDone));
        assert_eq!(q.current(u64::MAX, true), Some(&Alert::TimerDone)); // never expires
    }

    #[test]
    fn dismiss_removes_the_shown_alert_even_if_evicted() {
        let mut q = Queue::default();
        for id in ["a", "b", "c", "d"] {
            q.push(conn(id), 0);
        }
        assert_eq!(q.current(1, true), Some(&conn("a")));
        q.push(conn("e"), 2); // full: evicts "a" while it is on screen
        q.dismiss(); // must not eat "b", which was never shown
        assert_eq!(q.current(3, true), Some(&conn("b")));
        // Repeated calls keep returning the shown alert, not a newer head.
        let mut q = Queue::default();
        q.push(conn("a"), 0);
        assert_eq!(q.current(1, true), Some(&conn("a")));
        q.push(Alert::TimerDone, 2);
        assert_eq!(q.current(3, true), Some(&conn("a")));
        q.dismiss();
        assert_eq!(q.current(4, true), Some(&Alert::TimerDone));
    }

    fn upd(v: &str) -> Alert {
        Alert::UpdateAvailable { version: v.into() }
    }

    #[test]
    fn update_offer_merges_and_expires() {
        let mut q = Queue::default();
        q.push(upd("0.3.0"), 0);
        q.push(upd("0.3.0"), 10); // same version: one entry, timestamp refreshed
        q.push(upd("0.4.0"), 20); // newer version replaces it in place
        assert_eq!(q.current(20, true), Some(&upd("0.4.0")));
        q.dismiss();
        assert!(q.current(20, true).is_none());
        // Expires after 5 min even if never shown, but not before.
        q.push(upd("0.4.0"), 0);
        assert_eq!(q.current(299_999, false), None);
        assert_eq!(q.current(299_999, true), Some(&upd("0.4.0")));
        assert!(q.current(300_000, true).is_none());
        q.dismiss(); // forget the expired one that was on screen
                     // Queues behind other alerts.
        q.push(conn("a"), 0);
        q.push(upd("0.5.0"), 1);
        assert_eq!(q.current(2, true), Some(&conn("a")));
        q.dismiss();
        assert_eq!(q.current(2, true), Some(&upd("0.5.0")));
    }

    #[test]
    fn queue_can_show_false() {
        let mut q = Queue::default();
        q.push(conn("a"), 0);
        assert!(q.current(1, false).is_none());
        assert_eq!(q.current(2, true), Some(&conn("a"))); // kept for later
        assert!(q.current(70_000, false).is_none());
        assert!(q.current(70_001, true).is_none()); // expired while hidden
    }
}
