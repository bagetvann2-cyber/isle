//! Pure geometry in logical pixels (100% scale). No Win32 here, so it is unit-testable.

use super::liquid::IRect;

pub const PILL_H: f32 = 84.0;
pub const EXP_H: f32 = 168.0;
pub const GLOW: f32 = 14.0;
pub const MIN_W: f32 = 432.0;
pub const MAX_W: f32 = 610.0; // 3 devices with "100%" and a charging bolt each, plus the waveform
pub const MAX_DEV: usize = 3;
pub const PAD: f32 = 24.0;
pub const LEFT_END: f32 = 115.0; // right edge of the play/pause control
/// Left edge of the right-hand section (track text when there are no devices), without / with the waveform.
const TEXT_L: f32 = 139.0;
const TEXT_L_WAVE: f32 = 190.0;
/// Waveform: 6 rounded bars in the left section, `BARS_L` from the pill's left edge.
pub const BARS: usize = 6;
pub const BAR_W: f32 = 3.5;
pub const BAR_GAP: f32 = 2.5;
pub const BAR_MAX_H: f32 = 24.0;
pub const BARS_L: f32 = LEFT_END + 18.0;
pub const BARS_W: f32 = BARS as f32 * BAR_W + (BARS - 1) as f32 * BAR_GAP;
pub const DEV_GAP: f32 = 34.0;
pub const DIV_GAP: f32 = 37.0;
pub const ICON_W: f32 = 25.0;
pub const ICON_TEXT_GAP: f32 = 12.0;
/// Charging bolt: 9 x 15 logical px (about 7 x 11 at the 0.75 UI scale) plus a gap before it.
pub const BOLT_W: f32 = 13.0;
pub const BOLT_GLYPH_W: f32 = 9.0;
pub const BOLT_GLYPH_H: f32 = 15.0;
/// Extra expanded height per device that reports earbud parts, and how many rows fit.
pub const PART_ROW_H: f32 = 28.0;
pub const MAX_PART_ROWS: usize = 2;
/// Room the expanded pill needs above the compact one when it opens upwards (worst case).
pub const UP_ROOM: f32 = EXP_H - PILL_H + PART_ROW_H * MAX_PART_ROWS as f32;
/// Room between the play control and the divider: bare, or with the waveform (18 px each side).
const MIN_DIV_LEFT_GAP: f32 = 40.0;
const MIN_DIV_LEFT_GAP_WAVE: f32 = 70.0;

pub fn text_l(wave: bool) -> f32 {
    if wave {
        TEXT_L_WAVE
    } else {
        TEXT_L
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn new(l: f32, t: f32, w: f32, h: f32) -> Rect {
        Rect { l, t, r: l + w, b: t + h }
    }
    pub fn around(cx: f32, cy: f32, w: f32, h: f32) -> Rect {
        Rect::new(cx - w / 2.0, cy - h / 2.0, w, h)
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
}

/// Pill width for the given (fully present) device item widths; `extra` is room kept free at the
/// right end (the clock segment). Without devices the pill keeps its minimum width.
pub fn target_width(items: &[f32], wave: bool, extra: f32) -> f32 {
    if items.is_empty() {
        return MIN_W;
    }
    let n = items.len() as f32;
    let gap = if wave { MIN_DIV_LEFT_GAP_WAVE } else { MIN_DIV_LEFT_GAP };
    let need = LEFT_END + gap + 1.0 + DIV_GAP + items.iter().sum::<f32>() + DEV_GAP * (n - 1.0) + PAD + extra;
    need.clamp(MIN_W, MAX_W)
}

/// Rounded-rectangle containment (for hit-testing the capsule).
pub fn in_pill(r: Rect, radius: f32, x: f32, y: f32) -> bool {
    if !r.contains(x, y) {
        return false;
    }
    let rad = radius.min(r.w() / 2.0).min((r.b - r.t) / 2.0);
    let dx = (r.l + rad - x).max(x - (r.r - rad)).max(0.0);
    let dy = (r.t + rad - y).max(y - (r.b - rad)).max(0.0);
    dx * dx + dy * dy <= rad * rad
}

/// Expand downwards when the expanded pill fits below the compact one, else upwards.
pub fn expand_down(top: i32, scale: f32, extra_logical: f32, work_top: i32, work_bottom: i32) -> bool {
    let extra = ((EXP_H - PILL_H + extra_logical) * scale).round() as i32;
    top + (PILL_H * scale).round() as i32 + extra <= work_bottom || top - extra < work_top
}

/// Positions of everything interactive/drawn, derived from the pill's left edge and the
/// two vertical anchors (`row1` = top of the always-visible row, `block` = top of the
/// expandable block, which sits either below or above row 1).
#[derive(Clone, Copy, Debug)]
pub struct Geo {
    pub cover: Rect,
    pub pp1: (f32, f32),
    pub back: (f32, f32),
    pub prev: (f32, f32),
    pub pp2: (f32, f32),
    pub next: (f32, f32),
    pub fwd: (f32, f32),
    pub title: Rect,
    pub artist: Rect,
    pub elapsed: Rect,
    pub bar: Rect,
    /// Click target for seeking: the bar widened to a comfortable height.
    pub bar_hit: Rect,
    pub remaining: Rect,
}

pub fn geo(left: f32, width: f32, row1: f32, block: f32) -> Geo {
    let cy1 = row1 + PILL_H / 2.0;
    let right = left + width - PAD;
    let fwd = (right - 16.0, block + 27.0);
    let next = (fwd.0 - 34.0, fwd.1);
    let pp2 = (next.0 - 34.0, fwd.1);
    let prev = (pp2.0 - 34.0, fwd.1);
    let back = (prev.0 - 34.0, fwd.1);
    let text_r = (back.0 - 22.0).max(left + PAD + 40.0);
    let bar_y = block + 64.0;
    Geo {
        cover: Rect::new(left + PAD, cy1 - 26.0, 52.0, 52.0),
        pp1: (left + 98.0 + 8.5, cy1),
        back,
        prev,
        pp2,
        next,
        fwd,
        title: Rect { l: left + PAD, t: block + 8.0, r: text_r, b: block + 28.0 },
        artist: Rect { l: left + PAD, t: block + 28.0, r: text_r, b: block + 46.0 },
        elapsed: Rect { l: left + PAD, t: bar_y - 9.0, r: left + PAD + 36.0, b: bar_y + 9.0 },
        bar: Rect { l: left + PAD + 44.0, t: bar_y - 1.5, r: right - 46.0, b: bar_y + 1.5 },
        bar_hit: Rect { l: left + PAD + 40.0, t: bar_y - 10.0, r: right - 42.0, b: bar_y + 10.0 },
        remaining: Rect { l: right - 40.0, t: bar_y - 9.0, r: right, b: bar_y + 9.0 },
    }
}

pub fn fmt_time(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// UI size, percent of the reference design (the effective scale is `pct / 100`).
pub const SIZE_DEFAULT: u32 = 85;
pub const SIZE_MIN: u32 = 60;
pub const SIZE_MAX: u32 = 150;
pub const SIZE_PRESETS: [u32; 5] = [75, 85, 100, 115, 130];
/// Ctrl + wheel step per notch.
pub const WHEEL_STEP_PCT: i32 = 5;
const WHEEL_DELTA: i32 = 120;

pub fn clamp_pct(p: i32) -> u32 {
    p.clamp(SIZE_MIN as i32, SIZE_MAX as i32) as u32
}

/// Adds a wheel delta to the carried remainder (reversing direction drops it): returns
/// `(remainder, whole notches)`, so precision touchpads accumulate until a notch is full.
pub fn wheel_steps(acc: i32, delta: i32) -> (i32, i32) {
    let acc = if acc != 0 && (acc > 0) != (delta > 0) { 0 } else { acc };
    let t = acc + delta;
    (t % WHEEL_DELTA, t / WHEEL_DELTA)
}

/// New pill left / top after a rescale by `k`: keeps the gap (scaled) to the nearest work-area edge
/// and the pill's centre along that edge. `pill` is the old rect, `new_w` / `new_h` the new pill size.
pub fn anchor_pos(work: IRect, pill: IRect, k: f32, new_w: i32, new_h: i32) -> (i32, i32) {
    let gap = |g: i32| (g as f32 * k).round() as i32;
    let (cx, cy) = ((pill.l + pill.r) / 2, (pill.t + pill.b) / 2);
    let gaps = [pill.t - work.t, work.b - pill.b, pill.l - work.l, work.r - pill.r];
    let near = (0..4).min_by_key(|&i| gaps[i]).unwrap_or(0);
    let (x, y) = (cx - new_w / 2, cy - new_h / 2);
    match near {
        0 => (x, work.t + gap(gaps[0])),
        1 => (x, work.b - gap(gaps[1]) - new_h),
        2 => (work.l + gap(gaps[2]), y),
        _ => (work.r - gap(gaps[3]) - new_w, y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel() {
        assert_eq!(wheel_steps(0, 120), (0, 1));
        assert_eq!(wheel_steps(0, -240), (0, -2));
        assert_eq!(wheel_steps(0, 30), (30, 0)); // touchpad: partial deltas accumulate
        assert_eq!(wheel_steps(90, 30), (0, 1));
        assert_eq!(wheel_steps(90, -30), (-30, 0)); // direction change drops the remainder
        assert_eq!(wheel_steps(-60, -90), (-30, -1));
    }

    #[test]
    fn pct_clamp() {
        assert_eq!(clamp_pct(10), SIZE_MIN);
        assert_eq!(clamp_pct(999), SIZE_MAX);
        assert_eq!(clamp_pct(85), 85);
        assert!(SIZE_PRESETS.contains(&SIZE_DEFAULT));
    }

    #[test]
    fn anchor() {
        let work = IRect { l: 0, t: 0, r: 1920, b: 1040 };
        let r = |l, t, w, h| IRect { l, t, r: l + w, b: t + h };
        // top-docked, centred: stays at the top centre, gap scales
        assert_eq!(anchor_pos(work, r(768, 10, 384, 64), 1.5, 576, 96), (672, 15));
        // bottom-docked: bottom gap kept
        assert_eq!(anchor_pos(work, r(768, 966, 384, 64), 2.0, 768, 128), (576, 1040 - 20 - 128));
        // left-docked: centre y kept
        assert_eq!(anchor_pos(work, r(10, 500, 384, 64), 0.5, 192, 32), (5, 516));
        // right-docked
        assert_eq!(anchor_pos(work, r(1526, 500, 384, 64), 1.0, 384, 64), (1526, 500));
        // nearest wins: top gap 10 < left gap 40
        assert_eq!(anchor_pos(work, r(40, 10, 384, 64), 1.0, 384, 64), (40, 10));
    }

    #[test]
    fn widths() {
        for wave in [false, true] {
            assert_eq!(target_width(&[], wave, 0.0), MIN_W);
            assert_eq!(target_width(&[], wave, 90.0), MIN_W);
            assert_eq!(target_width(&[69.0, 82.0], wave, 0.0), MIN_W); // 78% + charging 92%
            assert!(target_width(&[95.0, 95.0, 95.0], wave, 0.0) > MIN_W);
            assert!(target_width(&[95.0, 95.0, 95.0], wave, 90.0) <= MAX_W);
            // The clock segment widens a pill that has devices.
            assert!(target_width(&[95.0, 95.0, 95.0], false, 20.0) > target_width(&[95.0, 95.0, 95.0], false, 0.0));
        }
        // The waveform needs its own room only when devices crowd the pill.
        assert!(target_width(&[95.0, 95.0, 95.0], true, 0.0) > target_width(&[95.0, 95.0, 95.0], false, 0.0));
        assert!(BARS_L + BARS_W < text_l(true));
    }

    #[test]
    fn capsule_hit() {
        let r = Rect::new(0.0, 0.0, 432.0, 84.0);
        assert!(in_pill(r, 42.0, 216.0, 42.0));
        assert!(!in_pill(r, 42.0, 2.0, 2.0)); // corner is outside the capsule
        assert!(in_pill(r, 42.0, 42.0, 2.0));
    }

    #[test]
    fn direction() {
        assert!(expand_down(12, 1.0, 0.0, 0, 1040));
        assert!(!expand_down(1000, 1.0, 0.0, 0, 1040));
        assert!(expand_down(5, 1.0, 0.0, 0, 100)); // no room either way: stay down
    }

    #[test]
    fn time() {
        assert_eq!(fmt_time(61_000), "1:01");
        assert_eq!(fmt_time(3_725_000), "1:02:05");
    }
}
