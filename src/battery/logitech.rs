//! Logitech HID++ 2.0 battery (experimental, untested on hardware).
//!
//! Strictly read-only: the only requests ever written are the ones in
//! `READ_ONLY` plus the root `ping` / `getFeature`. Every packet is a fixed
//! 20-byte long report (0x11); replies are read into a fixed buffer.
//! Works over receivers (Unifying/Bolt/Lightspeed, device index 1..=6) and for
//! directly connected devices (index 0xFF). No Logitech HID device → returns
//! immediately with an empty list.

use super::log;
use crate::model::{Device, DeviceKind};
use hidapi::{HidApi, HidDevice};
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VID: u16 = 0x046D;
const USAGE_PAGE: u16 = 0xFF00;
const USAGE_LONG: u16 = 0x0002; // collection carrying report 0x11
const REPORT_LONG: u8 = 0x11;
const LEN: usize = 20;
const SWID: u8 = 0x0A;
const PING_DATA: u8 = 0x5A;
const TIMEOUT: Duration = Duration::from_millis(300);
const DIRECT: u8 = 0xFF;
const RECEIVER_SLOTS: std::ops::RangeInclusive<u8> = 1..=6;
const RECEIVER_PIDS: [u16; 12] = [0xC52B, 0xC532, 0xC534, 0xC539, 0xC53A, 0xC53D, 0xC53F, 0xC541, 0xC545, 0xC547, 0xC548, 0xC54D];

const F_DEVICE_NAME: u16 = 0x0005;
const F_BATTERY_STATUS: u16 = 0x1000;
const F_UNIFIED_BATTERY: u16 = 0x1004;
const F_BATTERY_VOLTAGE: u16 = 0x1001;
/// Battery features in priority order.
const BATTERY_FEATURES: [u16; 3] = [F_UNIFIED_BATTERY, F_BATTERY_STATUS, F_BATTERY_VOLTAGE];
/// The only (feature id, function) pairs ever sent, all getters.
const READ_ONLY: [(u16, u8); 6] = [
    (F_UNIFIED_BATTERY, 1), // getStatus
    (F_BATTERY_STATUS, 0),  // getBatteryLevelStatus
    (F_BATTERY_VOLTAGE, 0), // getBatteryVoltage
    (F_DEVICE_NAME, 0),     // getCount
    (F_DEVICE_NAME, 1),     // getDeviceName(chunk)
    (F_DEVICE_NAME, 2),     // getType
];
const MAX_NAME: usize = 64;

/// hidapi allows one `HidApi` at a time.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
struct Feat {
    idx: u8,
    id: u16,
}

#[derive(Clone)]
struct Known {
    name: String,
    kind: DeviceKind,
}

/// Keeps resolved feature indexes between polls.
#[derive(Default)]
pub struct Poller {
    known: HashMap<(String, u8), Known>,
}

pub fn poll() -> Vec<Device> {
    Poller::default().poll()
}

impl Poller {
    pub fn poll(&mut self) -> Vec<Device> {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Cheap guard: no Logitech USB HID node present -> never touch hidapi.
        if !super::device_ids("HID").iter().any(|id| id.to_ascii_uppercase().contains("VID_046D")) {
            self.known.clear();
            return Vec::new();
        }
        let Ok(api) = HidApi::new() else {
            return Vec::new();
        };
        let mut channels: Vec<(CString, u16, String)> = Vec::new();
        for info in api.device_list() {
            if info.vendor_id() == VID && info.usage_page() == USAGE_PAGE && info.usage() == USAGE_LONG {
                let path = info.path().to_owned();
                if !channels.iter().any(|c| c.0 == path) {
                    channels.push((path, info.product_id(), info.product_string().unwrap_or_default().to_string()));
                }
            }
        }
        if channels.is_empty() {
            self.known.clear();
            return Vec::new();
        }
        let live: Vec<String> = channels.iter().map(|c| c.0.to_string_lossy().into_owned()).collect();
        self.known.retain(|(p, _), _| live.contains(p));

        let mut out = Vec::new();
        for (path, pid, product) in &channels {
            let Ok(h) = api.open_path(path) else {
                log("logitech: open failed");
                continue;
            };
            let path_s = path.to_string_lossy().into_owned();
            let slots: Vec<u8> = if is_receiver(*pid, product) { RECEIVER_SLOTS.collect() } else { vec![DIRECT] };
            for idx in slots {
                if let Some(d) = self.read_device(&h, &path_s, idx, product) {
                    out.push(d);
                }
            }
        }
        out
    }

    fn read_device(&mut self, h: &HidDevice, path: &str, idx: u8, product: &str) -> Option<Device> {
        let key = (path.to_string(), idx);
        // The battery feature index is re-resolved every poll: a slot may have been re-paired.
        let feat = find_battery(h, idx)?;
        let known = match self.known.get(&key) {
            Some(k) => k.clone(),
            None => identify(h, idx, product),
        };
        let Some((percent, charging)) = read_battery(h, idx, feat) else {
            self.known.remove(&key); // offline or asleep: re-probe next time
            return None;
        };
        self.known.insert(key, known.clone());
        Some(Device {
            id: format!("hidpp:{path}:{idx:02X}"),
            name: known.name,
            kind: known.kind,
            percent,
            charging: Some(charging),
            parts: Vec::new(),
        })
    }
}

fn is_receiver(pid: u16, product: &str) -> bool {
    RECEIVER_PIDS.contains(&pid) || product.to_ascii_lowercase().contains("receiver")
}

// ---- packets (pure) -------------------------------------------------------

fn packet(dev: u8, feat_idx: u8, func: u8, params: &[u8]) -> [u8; LEN] {
    let mut p = [0u8; LEN];
    p[0] = REPORT_LONG;
    p[1] = dev;
    p[2] = feat_idx;
    p[3] = (func << 4) | SWID;
    for (d, s) in p[4..].iter_mut().zip(params) {
        *d = *s;
    }
    p
}

/// Root feature (index 0), function 1.
fn ping_packet(dev: u8) -> [u8; LEN] {
    packet(dev, 0, 1, &[0, 0, PING_DATA])
}

/// Root feature (index 0), function 0: feature id -> index.
fn get_feature_packet(dev: u8, id: u16) -> [u8; LEN] {
    packet(dev, 0, 0, &id.to_be_bytes())
}

/// A getter of an already resolved feature; `None` for anything not in `READ_ONLY`.
fn query_packet(dev: u8, f: Feat, func: u8, params: &[u8]) -> Option<[u8; LEN]> {
    (f.idx != 0 && READ_ONLY.contains(&(f.id, func))).then(|| packet(dev, f.idx, func, params))
}

enum Reply {
    Ok([u8; LEN]),
    Error,
    Unrelated,
}

/// Classify a received report against the request that was sent.
fn parse_reply(buf: &[u8], req: &[u8; LEN]) -> Reply {
    if buf.len() < 7 || !matches!(buf[0], 0x10 | REPORT_LONG) {
        return Reply::Unrelated;
    }
    if buf[1] != req[1] && !(req[1] == DIRECT && buf[1] == 0) {
        return Reply::Unrelated;
    }
    if matches!(buf[2], 0xFF | 0x8F) && buf[3] == req[2] && buf[4] == req[3] {
        return Reply::Error; // HID++ 2.0 / 1.0 error
    }
    if buf[2] == req[2] && buf[3] == req[3] {
        let mut r = [0u8; LEN];
        let n = buf.len().min(LEN);
        r[..n].copy_from_slice(&buf[..n]);
        return Reply::Ok(r);
    }
    Reply::Unrelated
}

/// Send one request, wait up to `TIMEOUT` for its reply (notifications are skipped).
fn exchange(h: &HidDevice, req: &[u8; LEN]) -> Option<[u8; LEN]> {
    let mut buf = [0u8; 64];
    // Drop stale replies/notifications so a late answer can't be taken for this one.
    for _ in 0..32 {
        if !matches!(h.read_timeout(&mut buf, 0), Ok(n) if n > 0) {
            break;
        }
    }
    h.write(req).ok()?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        let ms = i32::try_from(left.as_millis()).unwrap_or(300).max(1);
        let n = h.read_timeout(&mut buf, ms).ok()?;
        if n == 0 {
            return None;
        }
        match parse_reply(&buf[..n.min(buf.len())], req) {
            Reply::Ok(r) => return Some(r),
            Reply::Error => return None,
            Reply::Unrelated => {}
        }
    }
}

fn query(h: &HidDevice, dev: u8, f: Feat, func: u8, params: &[u8]) -> Option<[u8; LEN]> {
    exchange(h, &query_packet(dev, f, func, params)?)
}

/// `None` = no (valid) reply: callers must stop talking to this device, because a
/// late reply could otherwise be mistaken for the answer to the next request.
/// `Some(None)` = the device answered that it lacks the feature.
fn resolve(h: &HidDevice, dev: u8, id: u16) -> Option<Option<Feat>> {
    let r = exchange(h, &get_feature_packet(dev, id))?;
    Some((r[4] != 0).then_some(Feat { idx: r[4], id }))
}

// ---- discovery and reading ------------------------------------------------

/// Is there an HID++ 2.0 device with a battery feature at `dev`?
fn find_battery(h: &HidDevice, dev: u8) -> Option<Feat> {
    let ping = exchange(h, &ping_packet(dev))?;
    if ping[4] < 2 || ping[6] != PING_DATA {
        return None; // HID++ 1.0 or unrelated
    }
    for id in BATTERY_FEATURES {
        if let Some(f) = resolve(h, dev, id)? {
            return Some(f);
        }
    }
    None
}

/// Name and kind via feature 0x0005; falls back to the HID product string.
fn identify(h: &HidDevice, dev: u8, product: &str) -> Known {
    let name_feat = resolve(h, dev, F_DEVICE_NAME).flatten();
    let name = name_feat.and_then(|f| read_name(h, dev, f)).unwrap_or_else(|| fallback_name(product));
    let kind = name_feat.and_then(|f| query(h, dev, f, 2, &[])).map_or(DeviceKind::Other, |r| kind_from_type(r[4]));
    Known { name, kind }
}

fn read_name(h: &HidDevice, dev: u8, f: Feat) -> Option<String> {
    let count = usize::from(query(h, dev, f, 0, &[])?[4]).min(MAX_NAME);
    let mut bytes: Vec<u8> = Vec::with_capacity(count);
    while bytes.len() < count {
        let r = query(h, dev, f, 1, &[u8::try_from(bytes.len()).ok()?])?;
        let chunk = r.get(4..)?;
        let take = chunk.len().min(count - bytes.len());
        bytes.extend_from_slice(chunk.get(..take)?);
    }
    let s = String::from_utf8_lossy(&bytes);
    let s = s.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    (!s.is_empty()).then(|| s.to_string())
}

fn fallback_name(product: &str) -> String {
    if product.is_empty() || is_receiver(0, product) {
        "Logitech device".into()
    } else {
        product.to_string()
    }
}

fn read_battery(h: &HidDevice, dev: u8, f: Feat) -> Option<(u8, bool)> {
    let func = u8::from(f.id == F_UNIFIED_BATTERY);
    battery_from_reply(f.id, &query(h, dev, f, func, &[])?)
}

/// Decode a battery getter reply: (percent, charging).
fn battery_from_reply(feature: u16, r: &[u8; LEN]) -> Option<(u8, bool)> {
    match feature {
        // r[4] state of charge %, r[5] level flags, r[6] charging status (1..=3 = on charger)
        F_UNIFIED_BATTERY => {
            let pct = if r[4] > 0 { r[4] } else { level_to_percent(r[5])? };
            Some((pct.min(100), matches!(r[6], 1..=3)))
        }
        // r[4] discharge level %, r[6] status (1..=4 = recharging / almost full / full / slow)
        F_BATTERY_STATUS => Some((r[4].min(100), matches!(r[6], 1..=4))),
        // r[4..6] millivolts (big endian), r[6] flags (bit 7 = charging)
        F_BATTERY_VOLTAGE => Some((voltage_to_percent(u16::from_be_bytes([r[4], r[5]])), r[6] & 0x80 != 0)),
        _ => None,
    }
}

/// Unified Battery level flags (critical/low/good/full) -> rough percent.
fn level_to_percent(flags: u8) -> Option<u8> {
    match flags {
        f if f & 0x08 != 0 => Some(90),
        f if f & 0x04 != 0 => Some(50),
        f if f & 0x02 != 0 => Some(20),
        f if f & 0x01 != 0 => Some(5),
        _ => None,
    }
}

/// Li-ion curve (mV, %) used by Logitech/Solaar; linear between points.
const VOLTAGE_CURVE: [(u16, u8); 13] = [
    (4186, 100),
    (4067, 90),
    (3989, 80),
    (3922, 70),
    (3859, 60),
    (3811, 50),
    (3778, 40),
    (3751, 30),
    (3717, 20),
    (3671, 10),
    (3646, 5),
    (3579, 2),
    (3500, 0),
];

fn voltage_to_percent(mv: u16) -> u8 {
    if mv >= VOLTAGE_CURVE[0].0 {
        return 100;
    }
    for w in VOLTAGE_CURVE.windows(2) {
        let ((hi_mv, hi), (lo_mv, lo)) = (w[0], w[1]);
        if mv >= lo_mv {
            let span = u32::from(hi_mv - lo_mv);
            let part = u32::from(mv - lo_mv) * u32::from(hi - lo);
            return lo + u8::try_from((part + span / 2) / span).unwrap_or(0);
        }
    }
    0
}

/// HID++ 0x0005 device type -> kind.
fn kind_from_type(t: u8) -> DeviceKind {
    match t {
        0 | 2 => DeviceKind::Keyboard,
        3..=5 => DeviceKind::Mouse,
        8 => DeviceKind::Headset,
        11 | 12 | 17 => DeviceKind::Gamepad,
        _ => DeviceKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_fixed_long_reports() {
        let p = ping_packet(2);
        assert_eq!(&p[..7], &[0x11, 2, 0, 0x1A, 0, 0, PING_DATA]);
        assert_eq!(p.len(), 20);
        let g = get_feature_packet(DIRECT, 0x1004);
        assert_eq!(&g[..6], &[0x11, 0xFF, 0, 0x0A, 0x10, 0x04]);
        let f = Feat { idx: 7, id: F_UNIFIED_BATTERY };
        assert_eq!(query_packet(1, f, 1, &[]).map(|p| (p[2], p[3])), Some((7, 0x1A)));
    }

    #[test]
    fn only_read_queries_can_be_built() {
        for id in [F_UNIFIED_BATTERY, F_BATTERY_STATUS, F_BATTERY_VOLTAGE, F_DEVICE_NAME, 0x2201, 0x0000] {
            for func in 0..16u8 {
                let allowed = READ_ONLY.contains(&(id, func));
                assert_eq!(query_packet(1, Feat { idx: 5, id }, func, &[]).is_some(), allowed, "{id:04X}/{func}");
            }
        }
        // root index can never be used through the generic builder
        assert!(query_packet(1, Feat { idx: 0, id: F_UNIFIED_BATTERY }, 1, &[]).is_none());
    }

    #[test]
    fn reply_matching() {
        let req = get_feature_packet(1, 0x1004);
        let mut ok = [0u8; 20];
        ok[..5].copy_from_slice(&[0x11, 1, 0, 0x0A, 9]);
        assert!(matches!(parse_reply(&ok, &req), Reply::Ok(r) if r[4] == 9));
        let mut err = [0u8; 20];
        err[..6].copy_from_slice(&[0x11, 1, 0xFF, 0, 0x0A, 2]);
        assert!(matches!(parse_reply(&err, &req), Reply::Error));
        let mut other = ok;
        other[3] = 0x1A; // different function
        assert!(matches!(parse_reply(&other, &req), Reply::Unrelated));
        other = ok;
        other[1] = 3; // other device
        assert!(matches!(parse_reply(&other, &req), Reply::Unrelated));
        assert!(matches!(parse_reply(&[0x11, 1], &req), Reply::Unrelated));
        // direct devices may answer with index 0
        let direct = get_feature_packet(DIRECT, 0x1004);
        let mut d = ok;
        d[1] = 0;
        assert!(matches!(parse_reply(&d, &direct), Reply::Ok(_)));
    }

    #[test]
    fn battery_decoding() {
        let mut r = [0u8; 20];
        r[4] = 73;
        r[6] = 1;
        assert_eq!(battery_from_reply(F_UNIFIED_BATTERY, &r), Some((73, true)));
        assert_eq!(battery_from_reply(F_BATTERY_STATUS, &r), Some((73, true)));
        r[4] = 0;
        r[5] = 0x04;
        r[6] = 0;
        assert_eq!(battery_from_reply(F_UNIFIED_BATTERY, &r), Some((50, false)));
        r[5] = 0;
        assert_eq!(battery_from_reply(F_UNIFIED_BATTERY, &r), None);
        r[4..7].copy_from_slice(&[0x0E, 0xE3, 0x80]); // 3811 mV, charging
        assert_eq!(battery_from_reply(F_BATTERY_VOLTAGE, &r), Some((50, true)));
        assert_eq!(battery_from_reply(0x1234, &r), None);
    }

    #[test]
    fn voltage_curve() {
        assert_eq!(voltage_to_percent(4300), 100);
        assert_eq!(voltage_to_percent(4186), 100);
        assert_eq!(voltage_to_percent(3811), 50);
        assert_eq!(voltage_to_percent(3500), 0);
        assert_eq!(voltage_to_percent(3000), 0);
        assert_eq!(voltage_to_percent(0), 0);
        // monotonic
        let mut prev = 0;
        for mv in 3400..=4300u16 {
            let p = voltage_to_percent(mv);
            assert!(p >= prev && p <= 100);
            prev = p;
        }
        // midpoint between 3811 (50) and 3859 (60)
        assert_eq!(voltage_to_percent(3835), 55);
    }

    #[test]
    fn kinds_and_receivers() {
        assert_eq!(kind_from_type(3), DeviceKind::Mouse);
        assert_eq!(kind_from_type(0), DeviceKind::Keyboard);
        assert_eq!(kind_from_type(8), DeviceKind::Headset);
        assert_eq!(kind_from_type(7), DeviceKind::Other);
        assert!(is_receiver(0xC548, ""));
        assert!(is_receiver(0x0001, "USB Receiver"));
        assert!(!is_receiver(0xC08B, "G502 HERO"));
        assert_eq!(fallback_name("USB Receiver"), "Logitech device");
    }
}
