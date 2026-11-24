//! Timer / stopwatch state. Pure: every function takes `now` (ms, `GetTickCount64` base).
// TODO(coder2): remove when wired
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    Off,
    Timer { deadline_ms: u64, total_ms: u64 },
    Stopwatch { start_ms: u64 },
}

/// "mm:ss", or "h:mm:ss" from one hour up. Timers count down (ceil to the second), stopwatches up.
pub fn label(c: &Clock, now: u64) -> String {
    let secs = match *c {
        Clock::Off => return String::new(),
        Clock::Timer { deadline_ms, .. } => deadline_ms.saturating_sub(now).div_ceil(1000),
        Clock::Stopwatch { start_ms } => now.saturating_sub(start_ms) / 1000,
    };
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Milliseconds left on a timer (0 once due); `None` for anything else.
pub fn remaining_ms(c: &Clock, now: u64) -> Option<u64> {
    match *c {
        Clock::Timer { deadline_ms, .. } => Some(deadline_ms.saturating_sub(now)),
        _ => None,
    }
}

pub fn expired(c: &Clock, now: u64) -> bool {
    remaining_ms(c, now) == Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(deadline_ms: u64) -> Clock {
        Clock::Timer { deadline_ms, total_ms: deadline_ms }
    }

    #[test]
    fn timer_labels() {
        assert_eq!(label(&timer(3_599_000), 0), "59:59");
        assert_eq!(label(&timer(3_600_000), 0), "1:00:00");
        assert_eq!(label(&timer(3_661_000), 0), "1:01:01");
        assert_eq!(label(&timer(60_000), 0), "01:00");
    }

    #[test]
    fn timer_ceils_and_clamps() {
        assert_eq!(label(&timer(1_001), 0), "00:02");
        assert_eq!(label(&timer(1_000), 0), "00:01");
        assert_eq!(label(&timer(1), 0), "00:01");
        assert_eq!(label(&timer(1_000), 1_000), "00:00");
        assert_eq!(label(&timer(1_000), 9_999), "00:00");
    }

    #[test]
    fn stopwatch() {
        let c = Clock::Stopwatch { start_ms: 1_000 };
        assert_eq!(label(&c, 1_000), "00:00");
        assert_eq!(label(&c, 1_999), "00:00");
        assert_eq!(label(&c, 61_000), "01:00");
        assert_eq!(label(&c, 1_000 + 3_600_000), "1:00:00");
        assert_eq!(label(&c, 0), "00:00"); // clock never runs backwards into a panic
        assert_eq!(remaining_ms(&c, 5_000), None);
        assert!(!expired(&c, u64::MAX));
    }

    #[test]
    fn off_and_expiry() {
        assert_eq!(label(&Clock::Off, 5), "");
        assert!(!expired(&Clock::Off, 5));
        assert_eq!(remaining_ms(&Clock::Off, 5), None);
        let t = timer(10_000);
        assert_eq!(remaining_ms(&t, 4_000), Some(6_000));
        assert!(!expired(&t, 9_999));
        assert!(expired(&t, 10_000));
        assert_eq!(remaining_ms(&t, 20_000), Some(0));
    }
}
