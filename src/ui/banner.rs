//! Pure content of the one-line alert banner, the clock segment and the privacy line:
//! what to show (items, texts, colours) and how wide it is. No Win32 or drawing here.

use super::display_of;
use super::layout::PAD;
use super::render::icon_char;
use crate::alerts::Alert;
use crate::model::{Device, PartKind};
use crate::privacy::PrivacyState;

pub const WHITE: u32 = 0xF2_F2F2;
pub const LIGHT: u32 = 0xD1_D1D6;
pub const GREY: u32 = 0x8E_8E93;
pub const RED: u32 = 0xFF_453A;
pub const GREEN: u32 = 0x30_D158;
pub const ORANGE: u32 = 0xFF_9F0A;

/// Gap between two banner items, logical px.
const GAP: f32 = 10.0;
/// Width of the little earbud-case outline.
pub const CASE_W: f32 = 12.0;
/// Room the clock segment needs besides its label: divider and breathing space.
const CLOCK_GAP: f32 = 30.0;
/// Device names longer than this are cut with an ellipsis in the banner.
const NAME_MAX: usize = 22;
/// A level at or below this is drawn red.
const LOW: u8 = 15;

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// Icon-font glyph.
    Glyph(char, u32),
    Bolt(u32),
    /// Earbud-case outline.
    Case,
    Text {
        s: String,
        color: u32,
        small: bool,
    },
}

fn text(s: impl Into<String>, color: u32) -> Item {
    Item::Text { s: s.into(), color, small: false }
}

fn small(s: impl Into<String>, color: u32) -> Item {
    Item::Text { s: s.into(), color, small: true }
}

fn short(name: &str) -> String {
    if name.chars().count() <= NAME_MAX {
        name.to_string()
    } else {
        name.chars().take(NAME_MAX - 1).chain(Some('\u{2026}')).collect()
    }
}

pub fn pct(p: u8) -> String {
    format!("{p} %")
}

fn level_color(p: u8, charging: bool) -> u32 {
    if charging {
        GREEN
    } else if p <= LOW {
        RED
    } else {
        LIGHT
    }
}

/// Icon + name + percent (+ bolt) and, for earbuds, L / R / case figures.
fn device_items(d: &Device, level: Option<u8>) -> Vec<Item> {
    let (shown, chg) = display_of(d);
    let mut v = vec![Item::Glyph(icon_char(d.kind), GREY), text(short(&d.name), WHITE)];
    match level {
        // Low-battery alert: the red percent, then what it means (added by the caller).
        Some(l) => v.push(text(pct(l), RED)),
        None => {
            v.push(text("\u{B7}", GREY));
            v.push(text(pct(shown), level_color(shown, chg)));
            if chg {
                v.push(Item::Bolt(GREEN));
            }
        }
    }
    for p in &d.parts {
        let color = level_color(p.percent, p.charging);
        match p.kind {
            PartKind::Left => v.push(small(format!("L {}", p.percent), color)),
            PartKind::Right => v.push(small(format!("R {}", p.percent), color)),
            PartKind::Case => {
                v.push(Item::Case);
                v.push(small(p.percent.to_string(), color));
            }
        }
    }
    v
}

/// Items of the banner for `a`. `dev` is the device an alert names (looked up by the caller);
/// without it a device alert yields nothing (the caller skips it). `timer_total_ms` is what the
/// finished timer was set to.
pub fn banner_items(a: &Alert, dev: Option<&Device>, timer_total_ms: u64, ru: bool) -> Vec<Item> {
    match a {
        Alert::DeviceConnected { .. } => dev.map(|d| device_items(d, None)).unwrap_or_default(),
        Alert::DeviceLow { level, .. } => dev
            .map(|d| {
                // The parts would repeat the percent: the low alert is just name + red level + text.
                let mut v = device_items(&Device { parts: Vec::new(), ..d.clone() }, Some(*level));
                v.push(text(if ru { "Низкий заряд" } else { "Low battery" }, GREY));
                v
            })
            .unwrap_or_default(),
        Alert::Power { plugged, percent } => {
            let (icon, label) = match (plugged, ru) {
                (true, true) => (Item::Bolt(GREEN), "Зарядка"),
                (true, false) => (Item::Bolt(GREEN), "Charging"),
                (false, true) => (Item::Glyph('\u{E83F}', LIGHT), "От батареи"),
                (false, false) => (Item::Glyph('\u{E83F}', LIGHT), "On battery"),
            };
            vec![icon, text(format!("{label} \u{B7} {}", pct(*percent)), WHITE)]
        }
        Alert::LaptopLow { percent } => {
            let label = if ru { "Батарея ноутбука" } else { "Laptop battery" };
            vec![Item::Glyph('\u{E850}', RED), text(format!("{label} {}", pct(*percent)), RED)]
        }
        Alert::TimerDone => {
            let total = crate::clock::label(&crate::clock::Clock::Timer { deadline_ms: timer_total_ms, total_ms: timer_total_ms }, 0);
            vec![Item::Glyph('\u{E916}', ORANGE), text(if ru { "Таймер завершён" } else { "Timer done" }, WHITE), text(total, GREY)]
        }
    }
}

/// X offset and width of every item, and the total width, given a text measurer.
pub fn layout(items: &[Item], measure: impl Fn(&Item) -> f32) -> (Vec<(f32, f32)>, f32) {
    let mut x = 0.0;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let w = measure(it);
        out.push((x, w));
        x += w + GAP;
    }
    (out, (x - GAP).max(0.0))
}

/// Pill width needed by a banner of `total` logical px: its text plus padding, at least `base`.
pub fn pill_width(base: f32, total: f32, max: f32) -> f32 {
    base.max(total + 2.0 * PAD).min(max)
}

/// Fixed-width cells for a clock label: every digit gets the widest digit's width, so the
/// label never jitters as the digits change. Returns `(char, x, width)` per char and the total.
pub fn clock_cells(label: &str, digit_w: f32, colon_w: f32) -> (Vec<(char, f32, f32)>, f32) {
    let mut x = 0.0;
    let mut out = Vec::with_capacity(label.len());
    for ch in label.chars() {
        let w = if ch.is_ascii_digit() { digit_w } else { colon_w };
        out.push((ch, x, w));
        x += w;
    }
    (out, x)
}

/// Width of the clock segment for a label `total` px wide.
pub fn clock_seg_w(total: f32) -> f32 {
    total + CLOCK_GAP
}

/// Expanded view: "Микрофон: A, B" / "Камера: C", both joined by a middle dot.
pub fn privacy_line(p: &PrivacyState, ru: bool) -> Option<String> {
    let part = |names: &[String], label: &str| (!names.is_empty()).then(|| format!("{label}: {}", names.join(", ")));
    let (m, c) = if ru { ("Микрофон", "Камера") } else { ("Microphone", "Camera") };
    let parts: Vec<String> = [part(&p.mic, m), part(&p.cam, c)].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join("  \u{B7}  "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceKind, Part};

    fn dev(name: &str, pct: u8) -> Device {
        Device { id: "d".into(), name: name.into(), kind: DeviceKind::Mouse, percent: pct, charging: None, parts: vec![] }
    }
    fn buds() -> Device {
        let p = |kind, percent, charging| Part { kind, percent, charging };
        Device {
            kind: DeviceKind::Headset,
            parts: vec![p(PartKind::Left, 80, true), p(PartKind::Right, 12, false), p(PartKind::Case, 40, false)],
            ..dev("Redmi Buds 6", 75)
        }
    }
    fn texts(v: &[Item]) -> Vec<(String, u32)> {
        v.iter()
            .filter_map(|i| match i {
                Item::Text { s, color, .. } => Some((s.clone(), *color)),
                _ => None,
            })
            .collect()
    }
    fn conn() -> Alert {
        Alert::DeviceConnected { id: "d".into() }
    }

    #[test]
    fn connected_plain_device() {
        let v = banner_items(&conn(), Some(&dev("Mouse", 85)), 0, false);
        assert_eq!(texts(&v), vec![("Mouse".into(), WHITE), ("\u{B7}".into(), GREY), ("85 %".into(), LIGHT)]);
        assert!(matches!(v[0], Item::Glyph(..)));
        // Low level is red, charging green with a bolt.
        let low = banner_items(&conn(), Some(&dev("Mouse", 9)), 0, false);
        assert_eq!(texts(&low).last().unwrap(), &("9 %".to_string(), RED));
        let mut c = dev("Mouse", 9);
        c.charging = Some(true);
        let v = banner_items(&conn(), Some(&c), 0, false);
        assert_eq!(texts(&v).last().unwrap(), &("9 %".to_string(), GREEN));
        assert!(v.contains(&Item::Bolt(GREEN)));
    }

    #[test]
    fn connected_buds_show_parts_low_in_red() {
        let v = banner_items(&conn(), Some(&buds()), 0, true);
        // The main percent is the weakest bud (12), green because a bud is charging (as on the compact pill); R 12 is red.
        assert_eq!(
            texts(&v),
            vec![
                ("Redmi Buds 6".into(), WHITE),
                ("\u{B7}".into(), GREY),
                ("12 %".into(), GREEN),
                ("L 80".into(), GREEN),
                ("R 12".into(), RED),
                ("40".into(), LIGHT)
            ]
        );
        assert!(v.contains(&Item::Case));
        assert!(v.contains(&Item::Bolt(GREEN))); // a bud is charging
    }

    #[test]
    fn missing_device_gives_nothing() {
        assert!(banner_items(&conn(), None, 0, false).is_empty());
        assert!(banner_items(&Alert::DeviceLow { id: "d".into(), level: 5 }, None, 0, false).is_empty());
    }

    #[test]
    fn low_alert_is_red_percent_and_words() {
        let a = Alert::DeviceLow { id: "d".into(), level: 14 };
        let t = texts(&banner_items(&a, Some(&buds()), 0, true));
        assert_eq!(t, vec![("Redmi Buds 6".into(), WHITE), ("14 %".into(), RED), ("Низкий заряд".into(), GREY)]);
        assert_eq!(texts(&banner_items(&a, Some(&dev("M", 14)), 0, false)).last().unwrap().0, "Low battery");
    }

    #[test]
    fn power_and_laptop_texts() {
        let p = |plugged, percent| texts(&banner_items(&Alert::Power { plugged, percent }, None, 0, true)).remove(0).0;
        assert_eq!(p(true, 76), "Зарядка \u{B7} 76 %");
        assert_eq!(p(false, 76), "От батареи \u{B7} 76 %");
        let en = banner_items(&Alert::Power { plugged: true, percent: 5 }, None, 0, false);
        assert_eq!(texts(&en)[0].0, "Charging \u{B7} 5 %");
        assert!(matches!(en[0], Item::Bolt(_)));
        let l = banner_items(&Alert::LaptopLow { percent: 10 }, None, 0, true);
        assert_eq!(texts(&l), vec![("Батарея ноутбука 10 %".into(), RED)]);
    }

    #[test]
    fn timer_done_shows_total() {
        let v = banner_items(&Alert::TimerDone, None, 300_000, false);
        assert_eq!(texts(&v), vec![("Timer done".into(), WHITE), ("05:00".into(), GREY)]);
        let v = banner_items(&Alert::TimerDone, None, 3_600_000, true);
        assert_eq!(texts(&v)[1].0, "1:00:00");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(short("short"), "short");
        let s = short("A very long wireless headphones name");
        assert_eq!(s.chars().count(), NAME_MAX);
        assert!(s.ends_with('\u{2026}'));
    }

    #[test]
    fn layout_and_pill_width() {
        let items = [Item::Bolt(GREEN), text("ab", WHITE), text("cd", WHITE)];
        let (xs, total) = layout(&items, |i| if matches!(i, Item::Bolt(_)) { 9.0 } else { 20.0 });
        assert_eq!(xs, vec![(0.0, 9.0), (19.0, 20.0), (49.0, 20.0)]);
        assert_eq!(total, 69.0);
        assert_eq!(layout(&[], |_| 1.0), (vec![], 0.0));
        // Short banners keep the normal width; long ones grow it, up to the maximum.
        assert_eq!(pill_width(432.0, 69.0, 610.0), 432.0);
        assert_eq!(pill_width(432.0, 500.0, 610.0), 548.0);
        assert_eq!(pill_width(432.0, 900.0, 610.0), 610.0);
    }

    #[test]
    fn clock_cells_are_fixed_width() {
        let (a, wa) = clock_cells("01:11", 12.0, 6.0);
        let (b, wb) = clock_cells("08:00", 12.0, 6.0);
        assert_eq!(wa, wb);
        assert_eq!(wa, 4.0 * 12.0 + 6.0);
        // Same char positions regardless of the digits.
        assert_eq!(a.iter().map(|c| c.1).collect::<Vec<_>>(), b.iter().map(|c| c.1).collect::<Vec<_>>());
        assert_eq!(a[2], (':', 24.0, 6.0));
        let (h, wh) = clock_cells("1:00:00", 12.0, 6.0);
        assert_eq!(h.len(), 7);
        assert_eq!(wh, 5.0 * 12.0 + 12.0);
        assert_eq!(clock_cells("", 12.0, 6.0).1, 0.0);
        assert_eq!(clock_seg_w(66.0), 66.0 + CLOCK_GAP);
    }

    #[test]
    fn privacy_lines() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let none = PrivacyState::default();
        assert_eq!(privacy_line(&none, true), None);
        let mic = PrivacyState { mic: s(&["Telegram", "Edge"]), cam: vec![] };
        assert_eq!(privacy_line(&mic, true).unwrap(), "Микрофон: Telegram, Edge");
        let cam = PrivacyState { mic: vec![], cam: s(&["Zoom"]) };
        assert_eq!(privacy_line(&cam, false).unwrap(), "Camera: Zoom");
        let both = PrivacyState { mic: s(&["Zoom"]), cam: s(&["Zoom"]) };
        assert_eq!(privacy_line(&both, false).unwrap(), "Microphone: Zoom  \u{B7}  Camera: Zoom");
    }
}
