//! Per-bud (Left / Right / Case) battery from BLE advertisements. Passive scan
//! only, on demand, a few seconds at a time; nothing is ever sent to a device.
//!
//! Windows' own battery property gives one number. Earbuds that broadcast more
//! do it in vendor formats:
//!  * Google Fast Pair (service data, UUID 0xFE2C): documented in the public
//!    Fast Pair spec, battery field type 3/4, one byte per part.
//!  * Apple AirPods/Beats (manufacturer 0x004C, type 0x07 "proximity pairing"):
//!    reverse engineered (furiousMAC/continuity, OpenPods, AirPodsDesktop).
//!    The layout below is taken from those and checked only against frames
//!    captured from neighbours' devices, never against hardware we own.
//!  * Xiaomi/Redmi (manufacturer id 0xFFFF, header `16 01 FF`): reverse engineered
//!    from live captures of a Redmi Buds 6 only; see `decode_xiaomi`.
//!
//! Samsung etc. broadcast nothing we can use.
//!
//! Fast Pair / Apple adverts use resolvable random addresses, so they cannot be
//! matched to the connected Windows device by MAC; `pick` uses a conservative
//! heuristic for them. The Xiaomi advert carries the public MAC and is matched exactly.

use crate::model::{Part, PartKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::{Duration, Instant};
use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementBytePattern, BluetoothLEAdvertisementFilter, BluetoothLEAdvertisementReceivedEventArgs,
    BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementWatcherStatus, BluetoothLEManufacturerData, BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{Buffer, DataReader, DataWriter, IBuffer};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

const APPLE: u16 = 0x004C;
const XIAOMI_AD: u16 = 0xFFFF;
const FAST_PAIR_UUID: [u8; 2] = [0x2C, 0xFE]; // 0xFE2C little endian
const SERVICE_DATA_16: u8 = 0x16;
/// Longest scan.
pub const SCAN_MAX: Duration = Duration::from_secs(8);
/// A neighbour's earbuds are usually weaker than ours (a few cm to 1 m away).
/// Heuristic only: adverts must also agree with the Windows battery value.
const MIN_RSSI: i16 = -55;
/// Max distance between an advert's bud level and the Windows battery value.
const MAX_LEVEL_DIFF: u8 = 10;
/// Parts older than this are dropped (spec).
pub const MAX_AGE: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    FastPair,
    Apple,
    /// Redmi/Xiaomi buds: carries the device MAC, matched exactly.
    Xiaomi,
}

/// One decoded advert (latest per address).
#[derive(Clone, Debug)]
pub struct Found {
    pub vendor: Vendor,
    /// Public Bluetooth MAC (12 upper-case hex digits) when the advert carries it.
    pub mac: Option<String>,
    pub rssi: i16,
    pub name: String,
    pub parts: Vec<Part>,
}

/// A connected headset as Windows reports it.
#[derive(Clone, Debug)]
pub struct Headset {
    pub id: String,
    pub name: String,
    pub percent: u8,
}

/// Parts attached to a connected device, with the time they were seen.
#[derive(Clone, Debug)]
pub struct Attached {
    pub device_id: String,
    pub parts: Vec<Part>,
    pub at: Instant,
}

// ---- decoding (pure) ------------------------------------------------------

/// Google Fast Pair non-discoverable advertisement. `section` is the whole
/// service-data section including the 2-byte UUID, e.g.
/// `2C FE 00 40 20 8A 80 C0 11 C4 33 14 1E 7F`:
/// UUID, version/flags byte, then fields `(len << 4 | type) + len bytes`:
/// type 0/2 account-key filter, 1 salt, 3/4 battery (show/hide UI).
/// Battery = Left, Right, Case; each byte: bit 7 charging, bits 0..6 percent,
/// 0x7F unknown.
pub fn decode_fast_pair(section: &[u8]) -> Option<Vec<Part>> {
    let (&version, mut rest) = section.strip_prefix(&FAST_PAIR_UUID[..])?.split_first()?;
    if version >> 5 != 0 {
        return None; // only protocol version 0 is known
    }
    while let Some((&header, tail)) = rest.split_first() {
        let (len, kind) = (usize::from(header >> 4), header & 0x0F);
        let field = tail.get(..len)?;
        if matches!(kind, 3 | 4) && len == 3 {
            let parts = [PartKind::Left, PartKind::Right, PartKind::Case]
                .into_iter()
                .zip(field)
                .filter_map(|(k, &b)| {
                    let percent = b & 0x7F;
                    (percent <= 100).then_some(Part { kind: k, percent, charging: b & 0x80 != 0 })
                })
                .collect::<Vec<_>>();
            return (!parts.is_empty()).then_some(parts);
        }
        rest = &tail[len..];
    }
    None
}

/// Apple proximity-pairing message. `data` is the manufacturer data after the
/// company id: `07 19 01 <model:2> <status> <battery> <charge|case> ...`.
/// Layout per OpenPods / AirPodsDesktop (UNVERIFIED on owned hardware):
///  * status bit 1 (0x20 of the first status nibble) clear = buds "flipped":
///    the battery byte then holds Left in the high nibble, Right in the low;
///    otherwise Right high, Left low.
///  * nibble 0..=9 -> n*10+5 %, 10 -> 100 %, 0xF (and above 10) = unknown.
///  * charge nibble (high nibble of the next byte): bit0/bit1 = the two buds
///    (swapped when flipped), bit2 = case. Low nibble = case battery.
pub fn decode_airpods(data: &[u8]) -> Option<Vec<Part>> {
    let &[0x07, 0x19, _, _, _, status, batt, power, ..] = data else { return None };
    if data.len() < 27 {
        return None; // 2 header + 25 payload bytes
    }
    let flipped = status & 0x20 == 0;
    let (hi, lo) = (batt >> 4, batt & 0x0F);
    let (left, right) = if flipped { (hi, lo) } else { (lo, hi) };
    let charge = power >> 4;
    let (cl, cr) = if flipped { (charge & 0b10, charge & 0b01) } else { (charge & 0b01, charge & 0b10) };
    let level = |n: u8| match n {
        0..=9 => Some(n * 10 + 5),
        10 => Some(100),
        _ => None,
    };
    let parts: Vec<Part> = [
        (PartKind::Left, level(left), cl != 0),
        (PartKind::Right, level(right), cr != 0),
        (PartKind::Case, level(power & 0x0F), charge & 0b100 != 0),
    ]
    .into_iter()
    .filter_map(|(kind, p, charging)| p.map(|percent| Part { kind, percent, charging }))
    .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Xiaomi/Redmi buds advertisement (manufacturer id 0xFFFF; `data` is after the
/// company id, 24 bytes). Layout reverse engineered from captures of a Redmi
/// Buds 6 (UNVERIFIED on other Xiaomi models), e.g.
/// `16 01 FF 41 4E E4 E4 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB`:
///  * `[0..3]` = `16 01 FF` signature.
///  * `[3]` status flags (lid / bud-in-ear changes) and `[4]` unknown: ignored.
///  * `[5]`, `[6]`, `[7]` = Left, Right, Case; bit 7 charging, bits 0..6 percent,
///    0x7F unknown. Left/Right order is inferred from toggling buds, not proven.
///  * `[11..17]` = the public MAC `12:34:56:AB:CD:EF` with bytes permuted as
///    `[1, 0, 2, 5, 4, 3]`; `[17]` is a counter; `[18..24]` repeats the MAC block.
///
/// Returns the parts and the MAC (None if the MAC block is inconsistent).
pub fn decode_xiaomi(data: &[u8]) -> Option<(Vec<Part>, Option<String>)> {
    let &[0x16, 0x01, 0xFF, _status, _, left, right, case, _, _, _, m0, m1, m2, m3, m4, m5, _counter, c0, c1, c2, c3, c4, c5] = data else {
        return None;
    };
    let parts: Vec<Part> = [(PartKind::Left, left), (PartKind::Right, right), (PartKind::Case, case)]
        .into_iter()
        .filter_map(|(kind, b)| {
            let percent = b & 0x7F;
            (percent <= 100).then_some(Part { kind, percent, charging: b & 0x80 != 0 })
        })
        .collect();
    let mac = ([m0, m1, m2, m3, m4, m5] == [c0, c1, c2, c3, c4, c5])
        .then(|| [m1, m0, m2, m5, m4, m3].iter().map(|b| format!("{b:02X}")).collect::<String>());
    (!parts.is_empty()).then_some((parts, mac))
}

/// Public MAC from a random-looking advert address: Xiaomi sets the top bit.
fn mac_from_address(addr: u64) -> String {
    format!("{:012X}", addr & !(1 << 47))
}

/// Redmi / Xiaomi / Mi buds advertise their own MAC (exact match); the heuristic vendors
/// must never be attached to them.
fn is_xiaomi_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("redmi")
        || n.contains("xiaomi")
        || n.contains("poco")
        || n == "mi"
        || n.starts_with("mi ")
        || n.starts_with("mi_")
        || n.starts_with("mi-")
        || n.contains(" mi ")
}

fn is_apple_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.contains("airpods") || n.contains("beats")
}

/// The bud level an advert reports: the weaker bud, else the case.
fn level(parts: &[Part]) -> Option<u8> {
    let buds = parts.iter().filter(|p| p.kind != PartKind::Case).map(|p| p.percent).min();
    buds.or_else(|| parts.first().map(|p| p.percent))
}

/// May advert `f` belong to connected headset `h`? Only for the heuristic
/// vendors (Fast Pair, Apple): strong enough, plausible vendor for the name,
/// and its level close to the one Windows reports for `h`. Xiaomi-named headsets
/// never take heuristic matches (a neighbour's Fast Pair advert must not attach).
fn matches(f: &Found, h: &Headset) -> bool {
    f.vendor != Vendor::Xiaomi
        && !is_xiaomi_name(&h.name)
        && f.rssi >= MIN_RSSI
        && (f.vendor == Vendor::Apple) == is_apple_name(&h.name)
        && level(&f.parts).is_some_and(|l| l.abs_diff(h.percent) <= MAX_LEVEL_DIFF)
}

/// Choose which advert belongs to which connected headset.
/// Xiaomi adverts name the MAC, so they match exactly. Fast Pair / Apple adverts
/// must pass `matches` and be unambiguous: with several headsets, a headset only
/// gets parts when it is the only candidate or the advert's name equals its name.
pub fn pick(found: &[Found], headsets: &[Headset]) -> Option<(String, Vec<Part>)> {
    let exact = headsets.iter().filter_map(|h| {
        let f = found.iter().filter(|f| f.vendor == Vendor::Xiaomi && f.mac.as_deref() == Some(h.id.as_str())).max_by_key(|f| f.rssi)?;
        Some((h.id.clone(), f.parts.clone()))
    });
    if let Some(hit) = exact.max_by_key(|(_, p)| p.len()) {
        return Some(hit);
    }
    let best = |h: &Headset, named: bool| {
        found.iter().filter(|f| matches(f, h) && (!named || f.name.eq_ignore_ascii_case(&h.name))).max_by_key(|f| f.rssi)
    };
    let plausible: Vec<&Headset> = headsets.iter().filter(|h| best(h, false).is_some()).collect();
    if let [h] = plausible[..] {
        return best(h, false).map(|f| (h.id.clone(), f.parts.clone()));
    }
    headsets.iter().filter_map(|h| Some((h, best(h, true)?))).max_by_key(|(_, f)| f.rssi).map(|(h, f)| (h.id.clone(), f.parts.clone()))
}

/// Put still-fresh attached parts onto their device.
pub fn apply(devices: &mut [crate::model::Device], attached: &Option<Attached>) {
    if let Some(a) = attached.as_ref().filter(|a| a.at.elapsed() < MAX_AGE) {
        if let Some(d) = devices.iter_mut().find(|d| d.id == a.device_id) {
            d.parts = a.parts.clone();
        }
    }
}

// ---- scanning -------------------------------------------------------------

static SCANNING: AtomicBool = AtomicBool::new(false);

/// Start one scan on its own thread (no-op if one is running or there is no
/// headset); `done` gets everything decoded. Returns whether a scan started.
pub fn spawn_scan(headsets: Vec<Headset>, done: impl FnOnce(Vec<Found>) + Send + 'static) -> bool {
    if headsets.is_empty() || SCANNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    let spawned = std::thread::Builder::new().name("ble-scan".into()).spawn(move || {
        let found = scan(SCAN_MAX, &headsets).unwrap_or_default();
        SCANNING.store(false, Ordering::Release);
        done(found);
    });
    if spawned.is_err() {
        SCANNING.store(false, Ordering::Release);
        return false;
    }
    true
}

fn buffer(bytes: &[u8]) -> windows::core::Result<IBuffer> {
    let w = DataWriter::new()?;
    w.WriteBytes(bytes)?;
    w.DetachBuffer()
}

fn read(b: &IBuffer) -> Vec<u8> {
    let mut v = vec![0u8; b.Length().unwrap_or(0) as usize];
    if let Ok(r) = DataReader::FromBuffer(b) {
        let _ = r.ReadBytes(&mut v);
    }
    v
}

/// Decode what the filtered watcher delivers; `None` for anything else.
fn decode_args(args: &BluetoothLEAdvertisementReceivedEventArgs) -> windows::core::Result<Option<(u64, Found)>> {
    let adv = args.Advertisement()?;
    let (mut parts, mut vendor, mut mac) = (None, Vendor::FastPair, None);
    for s in adv.DataSections()? {
        if s.DataType()? == SERVICE_DATA_16 {
            if let Some(p) = decode_fast_pair(&read(&s.Data()?)) {
                parts = Some(p);
            }
        }
    }
    if parts.is_none() {
        for m in adv.ManufacturerData()? {
            match m.CompanyId()? {
                APPLE => {
                    if let Some(p) = decode_airpods(&read(&m.Data()?)) {
                        (parts, vendor) = (Some(p), Vendor::Apple);
                    }
                }
                XIAOMI_AD => {
                    if let Some((p, payload_mac)) = decode_xiaomi(&read(&m.Data()?)) {
                        // fall back to the address with its top bit cleared if the MAC block is inconsistent
                        mac = Some(payload_mac.unwrap_or_else(|| mac_from_address(args.BluetoothAddress().unwrap_or_default())));
                        (parts, vendor) = (Some(p), Vendor::Xiaomi);
                    }
                }
                _ => {}
            }
        }
    }
    let Some(parts) = parts else { return Ok(None) };
    let name = adv.LocalName()?.to_string();
    Ok(Some((args.BluetoothAddress()?, Found { vendor, mac, rssi: args.RawSignalStrengthInDBm()?, name, parts })))
}

/// One passive watcher whose Windows-side filter (set by `setup`) keeps only one
/// vendor's adverts. Filter conditions are ANDed by Windows, so each vendor needs
/// its own watcher; unfiltered adverts never reach our callback. `on_found` runs on a
/// Windows thread-pool thread: it must only hand the advert over (channel send).
fn start_watcher(
    on_found: impl Fn(u64, Found) + Send + 'static,
    setup: impl FnOnce(&BluetoothLEAdvertisementFilter) -> windows::core::Result<()>,
) -> Option<(BluetoothLEAdvertisementWatcher, i64)> {
    let watcher = BluetoothLEAdvertisementWatcher::new().ok()?;
    watcher.SetScanningMode(BluetoothLEScanningMode::Passive).ok()?;
    setup(&watcher.AdvertisementFilter().ok()?).ok()?; // no filter, no watcher (never scan everything)
    let token = watcher
        .Received(&TypedEventHandler::new(move |_, args: windows::core::Ref<BluetoothLEAdvertisementReceivedEventArgs>| {
            if let Some(Ok(Some((addr, found)))) = args.as_ref().map(decode_args) {
                on_found(addr, found);
            }
            Ok(())
        }))
        .ok()?;
    watcher.Start().ok()?;
    Some((watcher, token))
}

fn manufacturer_filter(company: u16) -> impl FnOnce(&BluetoothLEAdvertisementFilter) -> windows::core::Result<()> {
    move |f| f.Advertisement()?.ManufacturerData()?.Append(&BluetoothLEManufacturerData::Create(company, &Buffer::Create(0)?)?)
}

fn start_vendor(vendor: Vendor, on_found: impl Fn(u64, Found) + Send + 'static) -> Option<(BluetoothLEAdvertisementWatcher, i64)> {
    match vendor {
        Vendor::Apple => start_watcher(on_found, manufacturer_filter(APPLE)),
        Vendor::Xiaomi => start_watcher(on_found, manufacturer_filter(XIAOMI_AD)),
        Vendor::FastPair => start_watcher(on_found, |f| {
            f.BytePatterns()?.Append(&BluetoothLEAdvertisementBytePattern::Create(SERVICE_DATA_16, 0, &buffer(&FAST_PAIR_UUID)?)?)
        }),
    }
}

/// Which vendor's adverts can belong to these headsets (and nothing else): Xiaomi names
/// only advertise as Xiaomi, AirPods/Beats as Apple, everything else may speak Fast Pair.
pub fn vendors_for(headsets: &[Headset]) -> Vec<Vendor> {
    let mut v = Vec::new();
    for h in headsets {
        let k = if is_xiaomi_name(&h.name) {
            Vendor::Xiaomi
        } else if is_apple_name(&h.name) {
            Vendor::Apple
        } else {
            Vendor::FastPair
        };
        if !v.contains(&k) {
            v.push(k);
        }
    }
    v
}

/// Long-running passive watchers for `vendors`, stopped on drop. Start/stop only on a
/// worker thread, never the UI thread.
pub struct LiveWatch {
    vendors: Vec<Vendor>,
    watchers: Vec<(BluetoothLEAdvertisementWatcher, i64)>,
}

impl LiveWatch {
    pub fn vendors(&self) -> &[Vendor] {
        &self.vendors
    }

    /// Windows aborts watchers (radio switched off, adapter removed): then restart.
    pub fn alive(&self) -> bool {
        self.watchers.iter().all(|(w, _)| w.Status() == Ok(BluetoothLEAdvertisementWatcherStatus::Started))
    }
}

impl Drop for LiveWatch {
    fn drop(&mut self) {
        for (w, token) in &self.watchers {
            let _ = w.Stop();
            let _ = w.RemoveReceived(*token);
        }
    }
}

/// Start continuous watchers; every decoded advert goes to `on_found`. `None` if any failed.
pub fn start_live(vendors: &[Vendor], on_found: impl Fn(u64, Found) + Clone + Send + 'static) -> Option<LiveWatch> {
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }; // per thread; S_FALSE / already initialised is fine
    let mut live = LiveWatch { vendors: vendors.to_vec(), watchers: Vec::new() };
    for &v in vendors {
        live.watchers.extend(start_vendor(v, on_found.clone())); // one failing vendor must not silence the rest
    }
    (!live.watchers.is_empty()).then_some(live)
}

/// Blocking passive scan, at most `max`; returns the latest advert per address.
/// Stops early once a strong advert that fits a connected headset is seen.
pub fn scan(max: Duration, headsets: &[Headset]) -> Option<Vec<Found>> {
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }; // S_FALSE / already initialised is fine
    let (tx, rx) = channel::<(u64, Found)>();
    let watchers: Vec<_> = [Vendor::Apple, Vendor::Xiaomi, Vendor::FastPair]
        .into_iter()
        .filter_map(|v| {
            let tx = tx.clone();
            start_vendor(v, move |addr, f| {
                let _ = tx.send((addr, f));
            })
        })
        .collect();
    if watchers.is_empty() {
        return None;
    }
    drop(tx);
    let deadline = Instant::now() + max;
    let mut seen: HashMap<u64, Found> = HashMap::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((addr, f)) => {
                // Only an exact MAC hit ends the scan early: a heuristic match could still be a
                // neighbour's, and the real (exact) advert may arrive a moment later.
                let strong = f.vendor == Vendor::Xiaomi && f.mac.as_deref().is_some_and(|m| headsets.iter().any(|h| h.id == m));
                seen.insert(addr, f);
                if strong {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    for (w, token) in watchers {
        let _ = w.Stop();
        let _ = w.RemoveReceived(token);
    }
    Some(seen.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Device;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace().map(|b| u8::from_str_radix(b, 16).unwrap()).collect()
    }
    fn part(kind: PartKind, percent: u8, charging: bool) -> Part {
        Part { kind, percent, charging }
    }

    #[test]
    fn fast_pair_captured_frames() {
        // captured from a nearby device: L 20 %, R 30 %, case unknown (0x7F)
        let p = decode_fast_pair(&hex("2C FE 00 40 20 8A 80 C0 11 C4 33 14 1E 7F")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 20, false), part(PartKind::Right, 30, false)]);
        let p = decode_fast_pair(&hex("2C FE 00 40 20 C4 44 09 11 55 33 14 14 7F")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 20, false), part(PartKind::Right, 20, false)]);
        // charging bit + case present
        let p = decode_fast_pair(&hex("2C FE 00 00 34 D4 50 28")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 84, true), part(PartKind::Right, 80, false), part(PartKind::Case, 40, false)]);
        // discoverable frame (model id only), truncated / odd frames, wrong uuid
        assert_eq!(decode_fast_pair(&hex("2C FE 00 00")), None);
        assert_eq!(decode_fast_pair(&hex("2C FE 10 30 00 00 00 21 A7 3B")), None);
        assert_eq!(decode_fast_pair(&hex("2C FE 00 33 14")), None);
        assert_eq!(decode_fast_pair(&hex("2C FE")), None);
        assert_eq!(decode_fast_pair(&hex("F1 FC 04 64 AD")), None);
        assert_eq!(decode_fast_pair(&hex("2C FE 00 33 7F 7F 7F")), None); // all unknown
    }

    #[test]
    fn airpods_captured_frames() {
        // status 0x03 (flipped): batt 0x67 -> L 65 %, R 75 %; case nibble F unknown; charge nibble 8: none
        let p = decode_airpods(&hex("07 19 01 0F 20 03 67 8F 01 00 08 25 5A 9E C5 5A 2D 69 DE BF 97 07 D8 A9 2D 78 52")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 65, false), part(PartKind::Right, 75, false)]);
        // status 0x22 (not flipped): batt 0xF9 -> L 95 %, R unknown
        let p = decode_airpods(&hex("07 19 01 0E 20 22 F9 8F 01 00 05 9D 19 8F 4E 9D E6 F6 AC 16 86 5A 24 6F 22 B8 B3")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 95, false)]);
        // synthetic: both buds + case, left charging, case 100 %
        let mut frame = hex("07 19 01 0F 20 03 A5 3A");
        frame.resize(27, 0);
        let p = decode_airpods(&frame).unwrap();
        assert_eq!(p, [part(PartKind::Left, 100, true), part(PartKind::Right, 55, true), part(PartKind::Case, 100, false)]);
        // other continuity messages, short frames
        assert_eq!(decode_airpods(&hex("10 05 3C 1C 3C A3 77")), None);
        assert_eq!(decode_airpods(&hex("07 19 01 0F 20 03 67")), None);
        assert_eq!(decode_airpods(&hex("07 19 01 0F 20 03 FF FF 01 00 08 25 5A 9E C5 5A 2D 69 DE BF 97 07 D8 A9 2D 78 52")), None);
    }

    #[test]
    fn xiaomi_captured_frames() {
        // Redmi Buds 6 in its case: L/R 100 % charging, case 85 %
        let (p, mac) = decode_xiaomi(&hex("16 01 FF 41 4E E4 E4 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 100, true), part(PartKind::Right, 100, true), part(PartKind::Case, 85, false)]);
        assert_eq!(mac.as_deref(), Some("123456ABCDEF"));
        // buds out of the case: 100 % not charging
        let (p, _) = decode_xiaomi(&hex("16 01 FF 01 4E 64 64 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB")).unwrap();
        assert_eq!(p, [part(PartKind::Left, 100, false), part(PartKind::Right, 100, false), part(PartKind::Case, 85, false)]);
        // one bud unknown (0x7F); inconsistent MAC copy -> no mac, parts still decoded
        let (p, mac) = decode_xiaomi(&hex("16 01 FF 00 4E 7F 64 7F 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AC")).unwrap();
        assert_eq!(p, [part(PartKind::Right, 100, false)]);
        assert_eq!(mac, None);
        // case unknown (0x7F, or 0xFF = 0x7F + charging bit): the Case part is omitted
        for case in ["7F", "FF"] {
            let (p, _) =
                decode_xiaomi(&hex(&format!("16 01 FF 00 4E 64 64 {case} 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB"))).unwrap();
            assert_eq!(p, [part(PartKind::Left, 100, false), part(PartKind::Right, 100, false)]);
        }
        // wrong signature / length
        assert!(decode_xiaomi(&hex("16 02 FF 41 4E E4 E4 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB")).is_none());
        assert!(decode_xiaomi(&hex("16 01 FF 41 4E E4 E4 55")).is_none());
        assert_eq!(mac_from_address(0x9234_56AB_CDEF), "123456ABCDEF");
    }

    #[test]
    fn xiaomi_matches_by_mac_only() {
        let (parts, mac) = decode_xiaomi(&hex("16 01 FF 41 4E E4 E4 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB")).unwrap();
        let ad = Found { vendor: Vendor::Xiaomi, mac, rssi: -90, name: String::new(), parts: parts.clone() };
        // exact, even when weak and when several headsets are connected
        let two = [hs("AAAAAAAAAAAA", "Other", 10), hs("123456ABCDEF", "Redmi Buds 6", 10)];
        assert_eq!(pick(std::slice::from_ref(&ad), &two), Some(("123456ABCDEF".into(), parts)));
        // a stranger's Xiaomi buds: no match, and never through the RSSI heuristic
        assert!(pick(&[ad], &[hs("111111111111", "Redmi Buds 6", 100)]).is_none());
    }

    #[test]
    fn live_watches_only_relevant_vendors() {
        assert!(vendors_for(&[]).is_empty());
        assert_eq!(vendors_for(&[hs("A", "Redmi Buds 6", 50)]), [Vendor::Xiaomi]);
        assert_eq!(vendors_for(&[hs("A", "Redmi Buds 6", 50), hs("B", "POCO Buds", 50)]), [Vendor::Xiaomi]);
        assert_eq!(vendors_for(&[hs("A", "Anna's AirPods Pro", 50)]), [Vendor::Apple]);
        assert_eq!(vendors_for(&[hs("A", "Pixel Buds", 50), hs("B", "Redmi Buds 6", 50)]), [Vendor::FastPair, Vendor::Xiaomi]);
    }

    fn found(vendor: Vendor, rssi: i16, name: &str, pct: u8) -> Found {
        Found { vendor, mac: None, rssi, name: name.into(), parts: vec![part(PartKind::Left, pct, false)] }
    }
    fn hs(id: &str, name: &str, percent: u8) -> Headset {
        Headset { id: id.into(), name: name.into(), percent }
    }

    #[test]
    fn matching_rules() {
        let fp = |rssi, pct| found(Vendor::FastPair, rssi, "", pct);
        // none connected -> ignore
        assert!(pick(&[fp(-40, 50)], &[]).is_none());
        // exactly one: strongest agreeing advert wins; weak or disagreeing (a neighbour's) are ignored
        let one = [hs("A", "Pixel Buds", 48)];
        assert_eq!(pick(&[fp(-62, 45), fp(-45, 50), fp(-50, 99)], &one).map(|(i, p)| (i, p[0].percent)), Some(("A".into(), 50)));
        assert!(pick(&[fp(-70, 48)], &one).is_none()); // too weak
        assert!(pick(&[fp(-40, 80)], &one).is_none()); // level far from Windows' 48 %
                                                       // AirPods adverts never go to a non-Apple name, and vice versa
        assert!(pick(&[found(Vendor::Apple, -40, "", 50)], &one).is_none());
        assert!(pick(&[found(Vendor::Apple, -40, "", 50)], &[hs("B", "Anna's AirPods Pro", 50)]).is_some());
        assert!(pick(&[fp(-40, 50)], &[hs("B", "Anna's AirPods Pro", 50)]).is_none());
        // several candidates: only a name match counts; a lone plausible candidate still counts
        let two = [hs("A", "Pixel Buds", 50), hs("C", "JBL Tune", 52)];
        assert!(pick(&[fp(-40, 51)], &two).is_none());
        let named = found(Vendor::FastPair, -50, "jbl tune", 53);
        assert_eq!(pick(&[fp(-40, 51), named], &two).map(|(i, _)| i), Some("C".into()));
        let spread = [hs("A", "Pixel Buds", 50), hs("C", "JBL Tune", 90)];
        assert_eq!(pick(&[fp(-40, 51)], &spread).map(|(i, _)| i), Some("A".into()));
    }

    #[test]
    fn redmi_ignores_neighbours_fast_pair() {
        let redmi = [hs("AABBCCDDEEFF", "Redmi Buds 6", 70)];
        // a neighbour's Fast Pair advert, strong enough and level-agreeing: still no attach
        let neighbour = found(Vendor::FastPair, -55, "", 65);
        assert!(pick(std::slice::from_ref(&neighbour), &redmi).is_none());
        // the exact Xiaomi advert (MAC) does attach, and wins over the neighbour
        let mut exact = found(Vendor::Xiaomi, -70, "", 72);
        exact.mac = Some("AABBCCDDEEFF".into());
        let (id, parts) = pick(&[neighbour, exact], &redmi).expect("exact MAC attaches");
        assert_eq!((id.as_str(), parts[0].percent), ("AABBCCDDEEFF", 72));
        for name in ["Xiaomi Buds 4", "Mi True Wireless", "POCO Buds"] {
            assert!(pick(&[found(Vendor::FastPair, -40, "", 70)], &[hs("A", name, 70)]).is_none(), "{name}");
        }
        // other brands still use the heuristic
        assert!(pick(&[found(Vendor::FastPair, -40, "", 70)], &[hs("A", "Pixel Buds", 70)]).is_some());
    }

    /// Panics abort the whole app (panic = abort): decoders must shrug off any bytes.
    #[test]
    fn decoders_never_panic() {
        let samples = [
            hex("16 01 FF 41 4E E4 E4 55 67 91 A6 34 12 56 EF CD AB E0 34 12 56 EF CD AB"),
            hex("2C FE 00 40 20 8A 80 C0 11 C4 33 14 1E 7F"),
            hex("07 19 01 20 20 AB 99 55 00 00"),
        ];
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        };
        let mut inputs: Vec<Vec<u8>> = Vec::new();
        for s in &samples {
            for n in 0..=s.len() {
                inputs.push(s[..n].to_vec());
            }
        }
        for _ in 0..3000 {
            let len = usize::from(next() % 40);
            inputs.push((0..len).map(|_| next()).collect());
        }
        for d in &inputs {
            let _ = decode_xiaomi(d);
            let _ = decode_fast_pair(d);
            let _ = decode_airpods(d);
            let _ = level(&decode_fast_pair(d).unwrap_or_default());
        }
    }

    #[test]
    fn attached_parts_expire() {
        let dev = |id: &str| Device {
            id: id.into(),
            name: "x".into(),
            kind: crate::model::DeviceKind::Headset,
            percent: 50,
            charging: None,
            parts: Vec::new(),
        };
        let parts = vec![part(PartKind::Left, 20, true)];
        let mut list = [dev("A"), dev("B")];
        let fresh = Some(Attached { device_id: "B".into(), parts: parts.clone(), at: Instant::now() });
        apply(&mut list, &fresh);
        assert!(list[0].parts.is_empty());
        assert_eq!(list[1].parts, parts);
        let mut list = [dev("B")];
        let stale = Instant::now().checked_sub(MAX_AGE + Duration::from_secs(1)).map(|at| Attached { device_id: "B".into(), parts, at });
        apply(&mut list, &stale);
        assert!(list[0].parts.is_empty());
        apply(&mut list, &None);
    }

    /// Live radio test: `cargo test --release live_scan -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_scan() {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let cpu = || {
            let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).ok() };
            let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) as f64 / 1e7;
            t(k) + t(u)
        };
        let (c0, t0) = (cpu(), Instant::now());
        let found = scan(SCAN_MAX, &[hs("x", "Pixel Buds", 50)]).expect("watcher");
        let (cpu_s, wall) = (cpu() - c0, t0.elapsed().as_secs_f64());
        println!("scan: {:.1}s wall, {:.3}s cpu ({:.2}% of one core), {} adverts", wall, cpu_s, cpu_s / wall * 100.0, found.len());
        for f in &found {
            println!("  {:?} rssi={} name={:?} parts={:?}", f.vendor, f.rssi, f.name, f.parts);
        }
    }
}
