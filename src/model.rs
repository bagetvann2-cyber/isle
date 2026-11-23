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
    pub bgra: Vec<u8>,
} // premultiplied BGRA

#[derive(Clone, Debug, Default)]
pub struct MediaSnapshot {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    pub can_prev: bool,
    pub can_next: bool,
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
}
