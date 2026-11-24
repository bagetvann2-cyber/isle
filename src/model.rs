use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Headphones,
    Headset,
    Mouse,
    Keyboard,
    Gamepad,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKind {
    Left,
    Right,
    Case,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Part {
    pub kind: PartKind,
    pub percent: u8,
    pub charging: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub id: String, // stable key (MAC / HID path), used for ordering + dedupe
    pub name: String,
    pub kind: DeviceKind,
    pub percent: u8, // 0..=100
    pub charging: Option<bool>,
    pub parts: Vec<Part>, // empty = single-battery device
}

#[derive(Clone, Debug)]
pub struct Cover {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>, // premultiplied BGRA
    /// Vibrant dominant colour (sRGB), computed once at decode; `None` for grey covers.
    pub accent: Option<[u8; 3]>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaSnapshot {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
    pub can_seek: bool,
    pub position_ms: u64,
    pub duration_ms: u64,                        // 0 = no timeline
    pub position_at: Option<std::time::Instant>, // when position_ms was sampled
    pub cover: Option<Arc<Cover>>,
}

#[derive(Clone, Copy, Debug)]
pub enum MediaCommand {
    TogglePlay,
    Next,
    Prev,
    /// Relative seek in ms (negative = back), from the current interpolated position.
    SeekBy(i64),
    /// Absolute seek in ms.
    SeekTo(u64),
}

impl MediaSnapshot {
    /// Position now: the sampled one plus the time elapsed since, while playing; clamped to the duration.
    pub fn position_now(&self, now: std::time::Instant) -> u64 {
        let mut pos = self.position_ms;
        if self.playing {
            pos += self.position_at.map_or(0, |t| now.saturating_duration_since(t).as_millis() as u64);
        }
        if self.duration_ms > 0 {
            pos = pos.min(self.duration_ms);
        }
        pos
    }

    /// Target of a relative seek, clamped to `[0, duration_ms]`.
    pub fn seek_by_target(&self, delta_ms: i64, now: std::time::Instant) -> u64 {
        let t = (self.position_now(now) as i64).saturating_add(delta_ms).max(0) as u64;
        t.min(self.duration_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn snap(playing: bool, pos: u64, dur: u64, at: Instant) -> MediaSnapshot {
        MediaSnapshot { playing, position_ms: pos, duration_ms: dur, position_at: Some(at), ..Default::default() }
    }

    #[test]
    fn seek_targets() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_secs(3);
        // Playing: 3 s of interpolation on top of the sampled position.
        assert_eq!(snap(true, 50_000, 200_000, t0).seek_by_target(10_000, now), 63_000);
        assert_eq!(snap(true, 50_000, 200_000, t0).seek_by_target(-10_000, now), 43_000);
        // Paused: no interpolation.
        assert_eq!(snap(false, 50_000, 200_000, t0).seek_by_target(10_000, now), 60_000);
        // Clamped at both ends.
        assert_eq!(snap(false, 4_000, 200_000, t0).seek_by_target(-10_000, now), 0);
        assert_eq!(snap(true, 195_000, 200_000, t0).seek_by_target(10_000, now), 200_000);
        assert_eq!(snap(false, 0, 200_000, t0).seek_by_target(i64::MIN, now), 0);
        // A "now" before the sample must not underflow.
        assert_eq!(snap(true, 5_000, 200_000, now).seek_by_target(0, t0), 5_000);
    }
}
