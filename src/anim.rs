//! Time-based animation primitives: an interruptible spring and a short tween.
//! Nothing here owns a timer; the UI paces frames and calls `step` / `val`.

use std::time::Instant;

/// Damped spring (unit mass). `response` = period in seconds, `zeta` = damping ratio.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
    pub target: f32,
    k: f32,
    c: f32,
}

impl Spring {
    pub fn new(x: f32, response: f32, zeta: f32) -> Self {
        let mut s = Spring { x, v: 0.0, target: x, k: 0.0, c: 0.0 };
        s.tune(response, zeta);
        s
    }

    /// Retargeting only changes the parameters; position and velocity are kept.
    pub fn tune(&mut self, response: f32, zeta: f32) {
        let w = std::f32::consts::TAU / response;
        self.k = w * w;
        self.c = 2.0 * zeta * w;
    }

    /// Fixed sub-steps keep the integrator stable at any frame time.
    pub fn step(&mut self, dt: f32) {
        let mut left = dt.clamp(0.0, 0.1);
        while left > 0.0 {
            let h = left.min(1.0 / 240.0);
            let a = self.k * (self.target - self.x) - self.c * self.v;
            self.v += a * h;
            self.x += self.v * h;
            left -= h;
        }
    }

    /// `eps` / `eps_v` are in the spring's own units.
    pub fn settled(&self, eps: f32, eps_v: f32) -> bool {
        (self.x - self.target).abs() < eps && self.v.abs() < eps_v
    }

    pub fn snap(&mut self) {
        self.x = self.target;
        self.v = 0.0;
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// Eased 0..1 interpolation between two values over `dur` seconds.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    t0: Instant,
    dur: f32,
}

impl Tween {
    pub fn at(v: f32, now: Instant) -> Self {
        Tween { from: v, to: v, t0: now, dur: 0.001 }
    }

    pub fn val(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.t0).as_secs_f32() / self.dur;
        self.from + (self.to - self.from) * ease_out_cubic(t)
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.t0).as_secs_f32() >= self.dur
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    /// Start moving to `to` from wherever the tween is right now.
    pub fn go(&mut self, to: f32, dur: f32, now: Instant) {
        self.from = self.val(now);
        self.to = to;
        self.t0 = now;
        self.dur = dur.max(0.001);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mut s: Spring) -> (f32, f32) {
        let (mut peak, mut t) = (0.0f32, 0.0f32);
        while t < 3.0 && !s.settled(0.0005, 0.001) {
            s.step(1.0 / 144.0);
            peak = peak.max(s.x);
            t += 1.0 / 144.0;
        }
        (peak, t)
    }

    #[test]
    fn expand_overshoots_and_settles_collapse_does_not() {
        let mut up = Spring::new(0.0, 0.42, 0.78);
        up.target = 1.0;
        let (peak, t) = run(up);
        assert!(peak > 1.0 && peak < 1.1 && t < 1.5, "peak {peak} t {t}");
        let mut down = Spring::new(1.0, 0.35, 1.0);
        down.target = 0.0;
        let (_, t) = run(down);
        assert!(t < 1.5);
    }

    #[test]
    fn retarget_keeps_velocity() {
        let mut s = Spring::new(0.0, 0.42, 0.78);
        s.target = 1.0;
        for _ in 0..10 {
            s.step(1.0 / 60.0);
        }
        let (x, v) = (s.x, s.v);
        s.target = 0.0;
        s.tune(0.35, 1.0);
        assert_eq!((s.x, s.v), (x, v));
    }
}
