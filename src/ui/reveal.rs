//! Cursor reveal: the edge strip the hook watches and the (state, event) -> action machine. No Win32.
//!
//! An island docked at a screen edge is revealed only by touching that edge (inside its span) or by
//! resting the cursor on it; any other island by dwelling a moment in its zone. Passing through
//! quickly (the cursor heading for a browser tab under the island) reveals nothing.

use super::liquid::{Edge, IRect};

/// Width of the hot strip along the screen edge, physical px.
pub const EDGE_PX: i32 = 2;
/// Dwell inside the rest rect (docked island).
pub const DWELL_REST_MS: u32 = 400;
/// Dwell inside the padded zone (island not at an edge).
pub const DWELL_ZONE_MS: u32 = 200;
/// Delay between the cursor leaving the zone and the hide.
pub const HIDE_MS: u32 = 300;

pub const EMPTY: IRect = IRect { l: 0, t: 0, r: 0, b: 0 };

/// The hot strip for a docked island: `EDGE_PX` thick on the screen edge, as long as the rest
/// rect's span along that edge plus `pad` on both sides. Top / left / right use the monitor
/// edge; the bottom one starts at the work-area bottom when a taskbar is there (and runs down over it).
pub fn edge_strip(edge: Edge, mon: IRect, work: IRect, rest: IRect, pad: i32) -> IRect {
    let (l, r, t, b) = (rest.l - pad, rest.r + pad, rest.t - pad, rest.b + pad);
    match edge {
        Edge::Top => IRect { l, r, t: mon.t, b: mon.t + EDGE_PX },
        Edge::Bottom => {
            let line = work.b.min(mon.b);
            // Down to the monitor bottom: with a taskbar the cursor runs on into it, a fast move may skip 2 px.
            IRect { l, r, t: line - EDGE_PX, b: mon.b }
        }
        Edge::Left => IRect { l: mon.l, r: mon.l + EDGE_PX, t, b },
        Edge::Right => IRect { l: mon.r - EDGE_PX, r: mon.r, t, b },
    }
}

/// Smallest rect holding both (the hold zone of a docked island reaches the edge).
pub fn union(a: IRect, b: IRect) -> IRect {
    IRect { l: a.l.min(b.l), t: a.t.min(b.t), r: a.r.max(b.r), b: a.b.max(b.b) }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reveal {
    /// The island sits at a screen edge (hot-edge mode).
    pub docked: bool,
    /// The cursor is inside the hold zone.
    pub in_zone: bool,
    /// The cursor revealed it; stays until the cursor leaves and the hide delay passes.
    pub revealed: bool,
    /// A dwell timer is armed.
    pub dwell: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ev {
    EnterZone,
    LeaveZone,
    EnterRest,
    LeaveRest,
    EdgeTouch,
    DwellDone,
    HideDone,
}

impl Ev {
    /// Wire code for the hook's `PostMessage` wParam.
    pub fn from_code(c: usize) -> Option<Ev> {
        use Ev::*;
        [EnterZone, LeaveZone, EnterRest, LeaveRest, EdgeTouch, DwellDone, HideDone].get(c).copied()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    None,
    /// Show it (and cancel a pending hide).
    Reveal,
    /// Already revealed and the cursor is back: cancel a pending hide.
    Keep,
    /// Cursor left a revealed island: hide after this many ms.
    Hide(u32),
    ArmDwell(u32),
    CancelDwell,
}

pub fn step(mut s: Reveal, ev: Ev) -> (Reveal, Act) {
    let act = match ev {
        Ev::EnterZone => {
            s.in_zone = true;
            if s.revealed {
                Act::Keep
            } else if !s.docked && !s.dwell {
                s.dwell = true;
                Act::ArmDwell(DWELL_ZONE_MS)
            } else {
                Act::None
            }
        }
        Ev::LeaveZone => {
            s.in_zone = false;
            if s.revealed {
                Act::Hide(HIDE_MS)
            } else if std::mem::take(&mut s.dwell) {
                Act::CancelDwell
            } else {
                Act::None
            }
        }
        Ev::EnterRest if s.docked && s.in_zone && !s.revealed && !s.dwell => {
            s.dwell = true;
            Act::ArmDwell(DWELL_REST_MS)
        }
        Ev::LeaveRest if s.docked && std::mem::take(&mut s.dwell) => Act::CancelDwell,
        Ev::EdgeTouch if s.docked && !s.revealed => {
            s.in_zone = true;
            s.dwell = false;
            s.revealed = true;
            Act::Reveal
        }
        Ev::EdgeTouch if s.docked => Act::Keep,
        Ev::DwellDone => {
            let ok = s.dwell && s.in_zone && !s.revealed;
            s.dwell = false;
            s.revealed |= ok;
            if ok {
                Act::Reveal
            } else {
                Act::None
            }
        }
        Ev::HideDone => {
            // `Hide` is only issued while revealed; a tray / startup reveal ends here and the cursor may hold it.
            s.revealed = s.in_zone;
            Act::None
        }
        _ => Act::None,
    };
    (s, act)
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: fn(i32, i32, i32, i32) -> IRect = |l, t, r, b| IRect { l, t, r, b };

    #[test]
    fn strips() {
        let mon = R(0, 0, 1920, 1080);
        let work = R(0, 0, 1920, 1040); // taskbar at the bottom
        let rest = R(800, 12, 1100, 56);
        assert_eq!(edge_strip(Edge::Top, mon, work, rest, 28), R(772, 0, 1128, 2));
        let low = R(800, 980, 1100, 1024);
        assert_eq!(
            edge_strip(Edge::Bottom, mon, work, low, 28),
            R(772, 1038, 1128, 1080),
            "taskbar: from work bottom to the monitor bottom"
        );
        let full = R(0, 0, 1920, 1080);
        assert_eq!(edge_strip(Edge::Bottom, mon, full, low, 28), R(772, 1078, 1128, 1080), "no taskbar: monitor bottom");
        let side = R(12, 400, 60, 700);
        assert_eq!(edge_strip(Edge::Left, mon, work, side, 28), R(0, 372, 2, 728));
        let side = R(1860, 400, 1908, 700);
        assert_eq!(edge_strip(Edge::Right, mon, work, side, 28), R(1918, 372, 1920, 728));
        // Top edge with a top taskbar still uses the monitor edge.
        let work_top = R(0, 40, 1920, 1080);
        assert_eq!(edge_strip(Edge::Top, mon, work_top, R(800, 52, 1100, 96), 28).t, 0);
        assert_eq!(union(R(0, 0, 10, 10), R(5, -3, 8, 20)), R(0, -3, 10, 20));
    }

    fn run(mut s: Reveal, evs: &[Ev]) -> (Reveal, Vec<Act>) {
        let mut acts = Vec::new();
        for &e in evs {
            let (n, a) = step(s, e);
            s = n;
            acts.push(a);
        }
        (s, acts)
    }

    const DOCKED: Reveal = Reveal { docked: true, in_zone: false, revealed: false, dwell: false };
    const FREE: Reveal = Reveal { docked: false, in_zone: false, revealed: false, dwell: false };

    #[test]
    fn docked_pass_through_reveals_nothing() {
        let (s, a) = run(DOCKED, &[Ev::EnterZone, Ev::LeaveZone]);
        assert_eq!(a, [Act::None, Act::None]);
        assert!(!s.revealed && !s.in_zone);
        // Crossing the rest rect quickly arms and cancels the dwell.
        let (s, a) = run(DOCKED, &[Ev::EnterZone, Ev::EnterRest, Ev::LeaveRest, Ev::LeaveZone]);
        assert_eq!(a, [Act::None, Act::ArmDwell(400), Act::CancelDwell, Act::None]);
        assert!(!s.revealed && !s.dwell);
        // A stale timer firing after the cancel does nothing.
        assert_eq!(step(s, Ev::DwellDone).1, Act::None);
    }

    #[test]
    fn docked_reveal_by_edge_or_rest_dwell() {
        let (s, a) = run(DOCKED, &[Ev::EnterZone, Ev::EdgeTouch]);
        assert_eq!(a[1], Act::Reveal);
        assert!(s.revealed);
        let (s, a) = run(DOCKED, &[Ev::EnterZone, Ev::EnterRest, Ev::DwellDone]);
        assert_eq!(a[2], Act::Reveal);
        assert!(s.revealed && !s.dwell);
        // The edge touch also cancels a running dwell.
        let (s, _) = run(DOCKED, &[Ev::EnterZone, Ev::EnterRest, Ev::EdgeTouch]);
        assert!(s.revealed && !s.dwell);
        // The edge counts without a zone event first.
        assert_eq!(step(DOCKED, Ev::EdgeTouch).1, Act::Reveal);
    }

    #[test]
    fn hold_and_hide() {
        let (s, _) = run(DOCKED, &[Ev::EnterZone, Ev::EdgeTouch]);
        let (s2, a) = step(s, Ev::LeaveZone);
        assert_eq!(a, Act::Hide(300));
        assert!(s2.revealed, "stays until the hide fires");
        // Back in time: keep it.
        assert_eq!(step(s2, Ev::EnterZone).1, Act::Keep);
        // The hide fires with the cursor outside: revealed ends. Inside: it stays.
        assert!(!step(s2, Ev::HideDone).0.revealed);
        assert!(step(step(s2, Ev::EnterZone).0, Ev::HideDone).0.revealed);
    }

    #[test]
    fn free_island_dwells_in_zone() {
        let (s, a) = run(FREE, &[Ev::EnterZone, Ev::DwellDone]);
        assert_eq!(a, [Act::ArmDwell(200), Act::Reveal]);
        assert!(s.revealed);
        // Leaving before the dwell ends cancels; resting on the rest rect changes nothing.
        let (s, a) = run(FREE, &[Ev::EnterZone, Ev::EnterRest, Ev::LeaveRest, Ev::LeaveZone]);
        assert_eq!(a, [Act::ArmDwell(200), Act::None, Act::None, Act::CancelDwell]);
        assert!(!s.dwell);
        // Edge touches mean nothing without a dock.
        assert_eq!(step(FREE, Ev::EdgeTouch).1, Act::None);
    }

    #[test]
    fn codes_round_trip() {
        for c in 0..7 {
            assert!(Ev::from_code(c).is_some());
        }
        assert_eq!(Ev::from_code(7), None);
        assert_eq!(Ev::from_code(1), Some(Ev::LeaveZone));
    }
}
