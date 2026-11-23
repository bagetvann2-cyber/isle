//! "Live" per-bud scanning policy: when the BLE watcher should run and when
//! attached parts have gone stale. Pure (no I/O, time passed in) so it is testable.
//!
//! Earbuds only advertise their levels with the case lid open; with both buds in
//! the ears they stay silent while still connected. So a live watcher runs only while
//! someone looks at the island, and silence of a running watcher means "buds in ears".

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveMode {
    /// Island hidden: no watcher, nothing is cleared (we just do not know).
    Off,
    /// Island shown but collapsed: continuous for `BURST` after the change, then a short
    /// window every `CYCLE`.
    Duty,
    /// Island expanded: continuous.
    Continuous,
}

/// Continuous scanning after a switch into `Duty` (the user has just looked at it).
const BURST: Duration = Duration::from_secs(60);
const CYCLE: Duration = Duration::from_secs(30);
const WINDOW: Duration = Duration::from_secs(4);
/// No matching advert for this long while scanning: parts are dropped.
pub const SILENCE: Duration = Duration::from_secs(30);
/// ...but only after at least this much actual scan time without a match (a duty window
/// or a fresh watcher must get a chance to hear the ~1 s adverts first).
const MIN_SCANNED: Duration = Duration::from_secs(3);

pub struct Live {
    mode: LiveMode,
    burst_until: Instant,
    /// The watcher has been running since (None = not running).
    scan_since: Option<Instant>,
    /// Scan time since the last match, not counting the current run.
    scanned: Duration,
}

impl Live {
    pub fn new(now: Instant) -> Self {
        Live { mode: LiveMode::Off, burst_until: now, scan_since: None, scanned: Duration::ZERO }
    }

    pub fn set_mode(&mut self, mode: LiveMode, now: Instant) {
        if mode != self.mode && mode != LiveMode::Off {
            self.burst_until = now + BURST;
        }
        self.mode = mode;
    }

    /// Should a watcher run at `now`?
    pub fn wants_scan(&self, now: Instant) -> bool {
        match self.mode {
            LiveMode::Off => false,
            LiveMode::Continuous => true,
            LiveMode::Duty => now < self.burst_until || self.phase(now) >= CYCLE - WINDOW,
        }
    }

    /// Position in the duty cycle, counted from the end of the burst.
    fn phase(&self, now: Instant) -> Duration {
        let ms = now.saturating_duration_since(self.burst_until).as_millis() % CYCLE.as_millis();
        Duration::from_millis(ms as u64)
    }

    /// Next instant at which `wants_scan` flips by itself (duty cycle only).
    pub fn next_edge(&self, now: Instant) -> Option<Instant> {
        if self.mode != LiveMode::Duty {
            return None;
        }
        if now < self.burst_until {
            return Some(self.burst_until);
        }
        let (p, on_at) = (self.phase(now), CYCLE - WINDOW);
        Some(now + if p >= on_at { CYCLE - p } else { on_at - p })
    }

    /// Record whether the watcher is actually running now.
    pub fn set_scanning(&mut self, on: bool, now: Instant) {
        match (on, self.scan_since) {
            (true, None) => self.scan_since = Some(now),
            (false, Some(s)) => {
                self.scanned += now.saturating_duration_since(s);
                self.scan_since = None;
            }
            _ => {}
        }
    }

    /// A matching advert arrived.
    pub fn matched(&mut self, now: Instant) {
        self.scanned = Duration::ZERO;
        if self.scan_since.is_some() {
            self.scan_since = Some(now);
        }
    }

    fn scanned(&self, now: Instant) -> Duration {
        self.scanned + self.scan_since.map_or(Duration::ZERO, |s| now.saturating_duration_since(s))
    }

    /// Parts last matched at `at` are stale: silent for `SILENCE` although we listened.
    pub fn stale(&self, at: Instant, now: Instant) -> bool {
        self.stale_at(at, now).is_some_and(|t| now >= t)
    }

    /// When parts matched at `at` become stale if nothing arrives (None = not scanning).
    pub fn stale_at(&self, at: Instant, now: Instant) -> Option<Instant> {
        self.scan_since?;
        Some((at + SILENCE).max(now + MIN_SCANNED.saturating_sub(self.scanned(now))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn silence_while_scanning_clears_but_off_keeps() {
        let t0 = Instant::now();
        let mut l = Live::new(t0);
        l.set_mode(LiveMode::Continuous, t0);
        l.set_scanning(true, t0);
        l.matched(t0);
        assert!(!l.stale(t0, t0 + s(29)));
        assert!(l.stale(t0, t0 + s(30)));
        assert_eq!(l.stale_at(t0, t0 + s(5)), Some(t0 + s(30)));
        // a fresh advert restarts the clock
        l.matched(t0 + s(20));
        assert!(!l.stale(t0 + s(20), t0 + s(45)));
        assert!(l.stale(t0 + s(20), t0 + s(50)));
        // Off: not scanning, so never stale however old
        l.set_mode(LiveMode::Off, t0 + s(60));
        l.set_scanning(false, t0 + s(60));
        assert!(!l.wants_scan(t0 + s(60)));
        assert!(!l.stale(t0, t0 + s(600)));
        assert_eq!(l.stale_at(t0, t0 + s(600)), None);
    }

    #[test]
    fn fresh_watcher_gets_time_to_hear_before_clearing_old_parts() {
        let t0 = Instant::now();
        let mut l = Live::new(t0);
        l.set_mode(LiveMode::Continuous, t0 + s(500));
        l.set_scanning(true, t0 + s(500));
        // parts are 500 s old, but the watcher has only just started
        assert!(!l.stale(t0, t0 + s(501)));
        assert!(l.stale(t0, t0 + s(503)));
        assert_eq!(l.stale_at(t0, t0 + s(500)), Some(t0 + s(503)));
    }

    #[test]
    fn scan_time_accumulates_across_duty_windows() {
        let t0 = Instant::now();
        let mut l = Live::new(t0);
        l.set_mode(LiveMode::Duty, t0);
        l.matched(t0);
        // 4 s window at 86..90 s with no advert: > 30 s since the match and >= 3 s scanned
        l.set_scanning(true, t0 + s(86));
        assert!(!l.stale(t0, t0 + s(87)));
        assert!(l.stale(t0, t0 + s(89)));
        l.set_scanning(false, t0 + s(90));
        assert!(!l.stale(t0, t0 + s(95))); // not scanning now: undecided, no clock
    }

    #[test]
    fn duty_cycle_and_modes() {
        let t0 = Instant::now();
        let mut l = Live::new(t0);
        assert!(!l.wants_scan(t0));
        assert_eq!(l.next_edge(t0), None);
        l.set_mode(LiveMode::Duty, t0);
        // first minute continuous
        assert!(l.wants_scan(t0) && l.wants_scan(t0 + s(59)));
        assert_eq!(l.next_edge(t0 + s(10)), Some(t0 + s(60)));
        // then a 4 s window at the end of every 30 s
        assert!(!l.wants_scan(t0 + s(60)));
        assert!(!l.wants_scan(t0 + s(85)));
        assert!(l.wants_scan(t0 + s(86)));
        assert!(l.wants_scan(t0 + s(89)));
        assert!(!l.wants_scan(t0 + s(90)));
        assert!(l.wants_scan(t0 + s(116)));
        assert_eq!(l.next_edge(t0 + s(60)), Some(t0 + s(86)));
        assert_eq!(l.next_edge(t0 + s(87)), Some(t0 + s(90)));
        assert_eq!(l.next_edge(t0 + s(90)), Some(t0 + s(116)));
        // expanding: continuous with no edges; collapsing again grants a new burst
        l.set_mode(LiveMode::Continuous, t0 + s(200));
        assert!(l.wants_scan(t0 + s(500)));
        assert_eq!(l.next_edge(t0 + s(500)), None);
        l.set_mode(LiveMode::Duty, t0 + s(500));
        assert!(l.wants_scan(t0 + s(559)) && !l.wants_scan(t0 + s(561)));
        // hiding: nothing runs; the same mode again does not extend the burst
        l.set_mode(LiveMode::Off, t0 + s(520));
        assert!(!l.wants_scan(t0 + s(521)));
        l.set_mode(LiveMode::Duty, t0 + s(700));
        l.set_mode(LiveMode::Duty, t0 + s(750));
        assert!(l.wants_scan(t0 + s(759)) && !l.wants_scan(t0 + s(761)));
    }
}
