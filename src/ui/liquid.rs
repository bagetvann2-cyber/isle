//! Liquid show/hide from a screen edge: pure geometry, no Win32.
//!
//! Everything is expressed in edge axes: `n` runs inward from the edge line, `t` runs along
//! it with `t = 0` at the pill's centre. Top/bottom and left/right share one code path and
//! differ only by `edge_matrix` (which maps (n, t) to window coordinates).

use super::layout::Rect;

/// Smallest gap (physical px at 100% DPI) between the pill and an edge for it to be a candidate.
pub const MIN_GAP: f32 = 4.0;
/// Radius of the meniscus where the neck meets the edge, physical px at 100% DPI.
pub const SHOULDER: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

impl Edge {
    /// Left/right edges use the "vertical capsule spreads sideways" emergence.
    pub fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// Integer rect in physical screen px (right / bottom exclusive, like `RECT`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

/// Which edge the pill emerges from, or `None` for the plain scale + fade.
/// `work` / `mon` are the monitor's work area and full rect; `monitor_at(x, y)` tells whether
/// another monitor covers a point (used to skip edges that are seams between monitors).
pub fn choose_edge(rest: IRect, work: IRect, mon: IRect, hc: i32, min_gap: i32, monitor_at: impl Fn(i32, i32) -> bool) -> Option<Edge> {
    let (cx, cy) = ((rest.l + rest.r) / 2, (rest.t + rest.b) / 2);
    let cand = |e: Edge| -> Option<i32> {
        let (gap, flush, beyond) = match e {
            Edge::Top => (rest.t - work.t, work.t == mon.t, (cx, work.t - 1)),
            Edge::Bottom => (work.b - rest.b, work.b == mon.b, (cx, work.b)),
            Edge::Left => (rest.l - work.l, work.l == mon.l, (work.l - 1, cy)),
            Edge::Right => (work.r - rest.r, work.r == mon.r, (work.r, cy)),
        };
        // The side capsule is 2 * hc tall, so it needs hc / 2 of room above and below the pill.
        let room = !matches!(e, Edge::Left | Edge::Right) || (rest.t - work.t >= hc / 2 && work.b - rest.b >= hc / 2);
        (room && gap >= min_gap && gap <= hc && !(flush && monitor_at(beyond.0, beyond.1))).then_some(gap)
    };
    // Smaller gap wins within a pair; ties go to the first (top / left).
    let best = |a: Edge, b: Edge| match (cand(a), cand(b)) {
        (Some(x), Some(y)) => Some(if y < x { b } else { a }),
        (Some(_), None) => Some(a),
        (None, y) => y.map(|_| b),
    };
    best(Edge::Top, Edge::Bottom).or_else(|| best(Edge::Left, Edge::Right))
}

/// The screen coordinate of the edge line (a side of the work area).
pub fn edge_line(e: Edge, work: IRect) -> i32 {
    match e {
        Edge::Top => work.t,
        Edge::Bottom => work.b,
        Edge::Left => work.l,
        Edge::Right => work.r,
    }
}

/// The resting pill, in one unit (logical px).
#[derive(Clone, Copy, Debug)]
pub struct RestBox {
    pub w: f32,
    pub h: f32,
    /// Collapsed pill height.
    pub hc: f32,
    /// Gap between the rest rect and the edge line.
    pub g: f32,
    /// Corner radius at rest.
    pub r: f32,
    pub rs_max: f32,
}

/// One frame of the liquid pill in edge axes (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    /// Body side nearest the edge, and extents of the body in n and t (t centred on 0).
    pub near: f32,
    pub depth: f32,
    pub along: f32,
    pub r: f32,
    /// Contact width on the edge line (0 = none). A neck of this width is drawn only while detached
    /// (`near > 0`); before that the body itself crosses the line. And the meniscus radius.
    pub c: f32,
    pub rs: f32,
    /// Glow alpha factor.
    pub glow: f32,
    /// Squash factor of the body (content follows it) and the content's n shift.
    pub sigma: f32,
    pub shift: f32,
    /// Content alpha.
    pub content: f32,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `p` is the fast show spring, `q` the same target with a softer spring (they differ mid-flight).
pub fn shape(edge: Edge, b: &RestBox, p: f32, q: f32) -> Shape {
    let (near, depth, along, sigma, content) = if edge.vertical() {
        // A vertical capsule is pushed out of the edge, detaches, then spreads into the pill.
        let m = ((q - 0.3) / 0.7).max(0.0);
        (
            lerp(-(b.hc + 4.0), b.g, (p / 0.6).clamp(0.0, 1.0)),
            lerp(b.hc, b.w, m),
            lerp(2.0 * b.hc, b.h, m),
            1.0,
            ((m - 0.7) / 0.3).clamp(0.0, 1.0),
        )
    } else {
        let s = (1.0 + 0.6 * (p - q)).clamp(0.88, 1.15);
        (lerp(-(b.h + 4.0), b.g, p), b.h * s, b.w / s, s, 1.0)
    };
    let (depth, along) = (depth.max(0.0), along.max(0.0));
    let r = b.r.min(depth / 2.0).min(along / 2.0).max(0.0);
    let c = if near <= 0.0 {
        // Chord of the body on the edge line: how far the line is outside the straight part.
        let (lo, hi) = (near + r, near + depth - r);
        let dd = if lo > 0.0 { lo } else { (-hi).max(0.0) };
        if dd > r {
            0.0
        } else {
            along - 2.0 * r + 2.0 * (r * r - dd * dd).max(0.0).sqrt()
        }
    } else {
        // Thins quickly (squared) as the body lets go, like a neck about to snap.
        // No gap to neck across (g < 1 px): no neck at all.
        let t = if b.g < 1.0 { 0.0 } else { (1.0 - near / (0.8 * b.g)).clamp(0.0, 1.0) };
        (along - 2.0 * r) * t * t
    };
    // The body sits where the rest rect would, shifted along n and squashed about its centre.
    let shift = near - (b.g + b.h / 2.0 - b.h * sigma / 2.0);
    Shape {
        near,
        depth,
        along,
        r,
        c,
        // The meniscus belongs to a detached neck: while the body still crosses the edge line it would
        // only add horns at the corners, so it grows in over the first pixels after detaching.
        rs: if near > 0.0 { b.rs_max.min(near + depth) * (near / (0.25 * b.g.max(1.0))).clamp(0.0, 1.0) } else { 0.0 },
        glow: smoothstep(0.8, 1.0, p),
        sigma,
        shift: if edge.vertical() { 0.0 } else { shift },
        content,
    }
}

impl Shape {
    /// Visible extent in edge axes, grown by `margin`: `(n1, half_t)`, n running from the
    /// edge line to `n1`. `None` when nothing of the body is on the near side of the line.
    pub fn extent(&self, margin: f32) -> Option<(f32, f32)> {
        let n1 = self.near + self.depth + margin;
        (n1 > 0.0).then(|| (n1, (self.along / 2.0).max(self.c / 2.0 + self.rs) + margin))
    }

    /// The body's bounding box in window coordinates (for clipping the content).
    pub fn body_rect(&self, edge: Edge, line: f32, centre: (f32, f32)) -> Rect {
        map_rect(edge, line, centre, (self.near, self.near + self.depth), self.along / 2.0)
    }

    /// Scale about the rest centre (sx, sy) and translation (dx, dy) that carry the content.
    pub fn content_xf(&self, edge: Edge) -> (f32, f32, f32, f32) {
        let (s, d) = (self.sigma, self.shift);
        match edge {
            Edge::Top => (1.0 / s, s, 0.0, d),
            Edge::Bottom => (1.0 / s, s, 0.0, -d),
            Edge::Left => (s, 1.0 / s, d, 0.0),
            Edge::Right => (s, 1.0 / s, -d, 0.0),
        }
    }
}

/// Window-coordinate bounding box of the n range `n` and |t| <= `half_t`.
pub fn map_rect(edge: Edge, line: f32, centre: (f32, f32), n: (f32, f32), half_t: f32) -> Rect {
    let (a, b) = (to_xy(edge, line, centre, n.0, -half_t), to_xy(edge, line, centre, n.1, half_t));
    Rect { l: a.0.min(b.0), t: a.1.min(b.1), r: a.0.max(b.0), b: a.1.max(b.1) }
}

/// Edge axes (n, t) -> window coordinates.
pub fn to_xy(edge: Edge, line: f32, centre: (f32, f32), n: f32, t: f32) -> (f32, f32) {
    match edge {
        Edge::Top => (centre.0 + t, line + n),
        Edge::Bottom => (centre.0 + t, line - n),
        Edge::Left => (line + n, centre.1 + t),
        Edge::Right => (line - n, centre.1 + t),
    }
}

/// `to_xy` as a 3x2 row-vector matrix `[m11 m12 m21 m22 m31 m32]`, so geometry built in
/// (n, t) can be drawn under it.
pub fn edge_matrix(edge: Edge, line: f32, centre: (f32, f32)) -> [f32; 6] {
    match edge {
        Edge::Top => [0.0, 1.0, 1.0, 0.0, centre.0, line],
        Edge::Bottom => [0.0, -1.0, 1.0, 0.0, centre.0, line],
        Edge::Left => [1.0, 0.0, 0.0, 1.0, line, centre.1],
        Edge::Right => [-1.0, 0.0, 0.0, 1.0, line, centre.1],
    }
}

/// Half-plane on the screen side of the edge line, clamped to the window: nothing is drawn beyond the edge.
pub fn clip_rect(edge: Edge, line: f32, win: (f32, f32)) -> Rect {
    let all = Rect { l: 0.0, t: 0.0, r: win.0, b: win.1 };
    match edge {
        Edge::Top => Rect { t: line, ..all },
        Edge::Bottom => Rect { b: line, ..all },
        Edge::Left => Rect { l: line, ..all },
        Edge::Right => Rect { r: line, ..all },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: RestBox = RestBox { w: 324.0, h: 84.0, hc: 84.0, g: 12.0, r: 42.0, rs_max: 10.67 };
    const EDGES: [Edge; 4] = [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right];

    #[test]
    fn rest_is_exact() {
        for e in EDGES {
            let s = shape(e, &B, 1.0, 1.0);
            assert_eq!(
                (s.near, s.depth, s.along, s.r),
                (B.g, if e.vertical() { B.w } else { B.h }, if e.vertical() { B.h } else { B.w }, B.r),
                "{e:?}"
            );
            assert_eq!(s.c, 0.0, "{e:?}");
            assert_eq!((s.sigma, s.shift, s.content, s.glow), (1.0, 0.0, 1.0, 1.0), "{e:?}");
        }
    }

    #[test]
    fn hidden_is_beyond_the_edge() {
        for e in EDGES {
            let s = shape(e, &B, 0.0, 0.0);
            assert!(s.near + s.depth <= 0.0, "{e:?} {s:?}");
            assert_eq!((s.c, s.rs, s.extent(0.0)), (0.0, 0.0, None), "{e:?}");
        }
    }

    #[test]
    fn no_jumps_when_p_steps() {
        for e in EDGES {
            let mut prev = shape(e, &B, 0.0, 0.0);
            // q trails p, like the real springs.
            for i in 1..=110 {
                let (p, q) = (i as f32 * 0.01, (i as f32 * 0.01 - 0.08).max(0.0));
                let s = shape(e, &B, p, q);
                assert!((s.near - prev.near).abs() <= 3.0, "{e:?} near at {p}: {s:?} {prev:?}");
                assert!((s.depth - prev.depth).abs() <= 9.0 && (s.along - prev.along).abs() <= 9.0, "{e:?} size at {p}");
                // The flat side of the stadium crosses the edge line all at once, so c may appear in one step.
                assert!(prev.c == 0.0 || (s.c - prev.c).abs() <= 0.2 * B.w, "{e:?} c at {p}: {} -> {}", prev.c, s.c);
                assert!((s.shift - prev.shift).abs() <= 6.0 && (s.sigma - prev.sigma).abs() <= 0.03, "{e:?} xf at {p}");
                prev = s;
            }
        }
    }

    #[test]
    fn contact_width_is_continuous_across_the_edge_line() {
        // The chord formula (body crossing the line) must meet the neck formula (body detached).
        let at = |near: f32| {
            let b = RestBox { g: 12.0, ..B };
            let (depth, along) = (b.h, b.w);
            let p = (near + (b.h + 4.0)) / (b.g + b.h + 4.0);
            let s = shape(Edge::Top, &b, p, p);
            assert!((s.near - near).abs() < 1e-3 && s.depth == depth && s.along == along);
            s.c
        };
        assert!((at(-1e-3) - at(1e-3)).abs() < 1.0, "{} vs {}", at(-1e-3), at(1e-3));
    }

    #[test]
    fn mirrors() {
        for (p, q) in [(0.0, 0.0), (0.3, 0.1), (0.7, 0.5), (1.0, 1.0), (1.08, 0.97)] {
            assert_eq!(shape(Edge::Top, &B, p, q), shape(Edge::Bottom, &B, p, q));
            assert_eq!(shape(Edge::Left, &B, p, q), shape(Edge::Right, &B, p, q));
        }
        // Mirrored maps are reflections of each other about the edge line / centre.
        let (c, line) = ((100.0, 50.0), 20.0);
        let (a, b) = (to_xy(Edge::Top, line, c, 7.0, 3.0), to_xy(Edge::Bottom, line, c, 7.0, 3.0));
        assert_eq!((a.0, a.1 - line), (b.0, line - b.1));
        let (a, b) = (to_xy(Edge::Left, line, c, 7.0, 3.0), to_xy(Edge::Right, line, c, 7.0, 3.0));
        assert_eq!((a.1, a.0 - line), (b.1, line - b.0));
        // The matrix agrees with the point map.
        for e in EDGES {
            let m = edge_matrix(e, line, c);
            let (n, t) = (7.0, 3.0);
            let xy = (n * m[0] + t * m[2] + m[4], n * m[1] + t * m[3] + m[5]);
            assert_eq!(xy, to_xy(e, line, c, n, t), "{e:?}");
        }
    }

    #[test]
    fn rest_rect_maps_onto_itself() {
        // Top edge at y = 0, pill 324 x 84 at (40, 12): the body rect must be the rest rect.
        let c = (40.0 + 162.0, 12.0 + 42.0);
        let s = shape(Edge::Top, &B, 1.0, 1.0);
        assert_eq!(s.body_rect(Edge::Top, 0.0, c), Rect { l: 40.0, t: 12.0, r: 364.0, b: 96.0 });
        // Right edge at x = 400, same pill rotated: gap 12 -> body at 400 - 12 - 324.
        let s = shape(Edge::Right, &B, 1.0, 1.0);
        assert_eq!(s.body_rect(Edge::Right, 400.0, c), Rect { l: 64.0, t: 12.0, r: 388.0, b: 96.0 });
    }

    const WORK: IRect = IRect { l: 0, t: 0, r: 1536, b: 816 };
    const MON: IRect = IRect { l: 0, t: 0, r: 1536, b: 864 };
    const HC: i32 = 79;
    const MG: i32 = 5;
    fn pill(l: i32, t: i32) -> IRect {
        IRect { l, t, r: l + 405, b: t + 79 }
    }
    fn pick(rest: IRect, work: IRect, mon: IRect, other: impl Fn(i32, i32) -> bool) -> Option<Edge> {
        choose_edge(rest, work, mon, HC, MG, other)
    }

    #[test]
    fn edge_choice() {
        let alone = |_, _| false;
        // Default placement: a little below the top edge.
        assert_eq!(pick(pill(565, 11), WORK, MON, alone), Some(Edge::Top));
        // Bottom taskbar: the work area's bottom is the taskbar border.
        assert_eq!(pick(pill(565, 816 - 79 - 20), WORK, MON, alone), Some(Edge::Bottom));
        // Gap larger than hc, or smaller than the minimum: not a candidate.
        assert_eq!(pick(pill(565, 80), WORK, MON, alone), None);
        assert_eq!(pick(pill(565, 4), WORK, MON, alone), None);
        assert_eq!(pick(pill(565, 5), WORK, MON, alone), Some(Edge::Top));
        assert_eq!(pick(pill(565, 79), WORK, MON, alone), Some(Edge::Top));
        // Side edges, mid-height.
        assert_eq!(pick(pill(20, 300), WORK, MON, alone), Some(Edge::Left));
        assert_eq!(pick(pill(1536 - 405 - 20, 300), WORK, MON, alone), Some(Edge::Right));
        // Corner: top / bottom first.
        assert_eq!(pick(pill(20, 11), WORK, MON, alone), Some(Edge::Top));
        assert_eq!(pick(pill(1536 - 405 - 20, 816 - 79 - 11), WORK, MON, alone), Some(Edge::Bottom));
        // Smaller gap wins within a pair.
        let tall = IRect { b: 150, ..WORK };
        let mon = IRect { b: 150, ..MON };
        assert_eq!(pick(pill(565, 20), tall, mon, alone), Some(Edge::Top));
        assert_eq!(pick(pill(565, 60), tall, mon, alone), Some(Edge::Bottom));
    }

    #[test]
    fn clamped_gap_picks_top() {
        // The clamp keeps the gap >= 12, which qualifies; a flush (0) pill never does.
        assert_eq!(pick(pill(565, 12), WORK, MON, |_, _| false), Some(Edge::Top));
        assert_eq!(pick(pill(565, 0), WORK, MON, |_, _| false), None);
    }

    #[test]
    fn zero_gap_is_finite() {
        let b = RestBox { g: 0.0, ..B };
        for e in EDGES {
            for i in 0..=108 {
                let p = i as f32 * 0.01;
                let s = shape(e, &b, p, (p - 0.08).max(0.0));
                for v in [s.near, s.depth, s.along, s.r, s.c, s.rs, s.glow, s.sigma, s.shift, s.content] {
                    assert!(v.is_finite(), "{e:?} {p}: {s:?}");
                }
            }
        }
    }

    #[test]
    fn seams_are_skipped() {
        // A monitor above: the top side of the work area is a seam, so no top edge ...
        let above = |x: i32, y: i32| (0..1536).contains(&x) && y == -1;
        assert_eq!(pick(pill(565, 11), WORK, MON, above), None);
        // ... but a docked top taskbar (work area inside the monitor) is not a seam.
        let work = IRect { t: 48, ..WORK };
        assert_eq!(pick(pill(565, 48 + 11), work, MON, |_, _| true), Some(Edge::Top));
        // The probe sits opposite the pill centre: a monitor beyond a different stretch doesn't count.
        let far = |x: i32, y: i32| x > 1400 && y == -1;
        assert_eq!(pick(pill(565, 11), WORK, MON, far), Some(Edge::Top));
        // No side edge when the capsule would stick out above or below the work area.
        assert_eq!(pick(pill(20, 10), WORK, MON, |_, _| false), Some(Edge::Top));
        assert_eq!(pick(pill(20, 3), WORK, MON, |_, _| false), None);
        // A seam on the left falls through to nothing (pill is mid-height); the right still works.
        let left = |x: i32, _| x == -1;
        assert_eq!(pick(pill(20, 300), WORK, MON, left), None);
        assert_eq!(pick(pill(1536 - 405 - 20, 300), WORK, MON, left), Some(Edge::Right));
    }
}
