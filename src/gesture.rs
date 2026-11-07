//! Velocity tracking and fling physics.

/// Least-squares velocity over the last ~100 ms of samples.
#[derive(Default, Debug)]
pub struct VelocityTracker {
    samples: Vec<(f64, f64, f64)>,
}

const HORIZON: f64 = 0.1;

impl VelocityTracker {
    pub fn reset(&mut self) {
        self.samples.clear();
    }

    pub fn add(&mut self, t: f64, x: f64, y: f64) {
        self.samples.retain(|s| t - s.0 <= HORIZON && s.0 <= t);
        self.samples.push((t, x, y));
    }

    /// Velocity in units per second at time `now`; zero if the pointer paused.
    pub fn velocity(&self, now: f64) -> (f64, f64) {
        let Some(last) = self.samples.last() else { return (0.0, 0.0) };
        if now - last.0 > 0.05 {
            return (0.0, 0.0);
        }
        let pts: Vec<_> = self.samples.iter().filter(|s| last.0 - s.0 <= HORIZON).collect();
        if pts.len() < 2 {
            return (0.0, 0.0);
        }
        let n = pts.len() as f64;
        let mt = pts.iter().map(|s| s.0).sum::<f64>() / n;
        let mx = pts.iter().map(|s| s.1).sum::<f64>() / n;
        let my = pts.iter().map(|s| s.2).sum::<f64>() / n;
        let var = pts.iter().map(|s| (s.0 - mt).powi(2)).sum::<f64>();
        if var < 1e-9 {
            return (0.0, 0.0);
        }
        let vx = pts.iter().map(|s| (s.0 - mt) * (s.1 - mx)).sum::<f64>() / var;
        let vy = pts.iter().map(|s| (s.0 - mt) * (s.2 - my)).sum::<f64>() / var;
        (vx, vy)
    }
}

/// Fling that reproduces Android's `OverScroller` distance and duration for a
/// given initial velocity, using a curve with the matching start velocity.
#[derive(Clone, Debug)]
pub struct Fling {
    start: f64,
    duration: f64,
    distance: f64,
    dir: (f64, f64),
    travelled: f64,
}

const INFLEXION: f64 = 0.35;
const DECELERATION_RATE: f64 = 2.358_201_8; // ln(0.78) / ln(0.9)

impl Fling {
    /// `vx`, `vy` in screen px/s. Returns `None` for velocities too small to fling.
    pub fn new(now: f64, vx: f64, vy: f64, density: f64) -> Option<Fling> {
        let max = 8000.0 * density;
        let speed = vx.hypot(vy);
        if speed < 50.0 * density {
            return None;
        }
        let dir = (vx / speed, vy / speed);
        let speed = speed.min(max);
        let ppi = 160.0 * density;
        let coef = 0.015 * 9.80665 * 39.37 * ppi * 0.84;
        let l = (INFLEXION * speed / coef).ln();
        let distance = coef * (DECELERATION_RATE / (DECELERATION_RATE - 1.0) * l).exp();
        let duration = (l / (DECELERATION_RATE - 1.0)).exp();
        Some(Fling { start: now, duration, distance, dir, travelled: 0.0 })
    }

    /// Screen-space displacement since the previous call, and whether the fling is still running.
    pub fn step(&mut self, now: f64) -> ((f64, f64), bool) {
        let u = ((now - self.start) / self.duration).clamp(0.0, 1.0);
        let d = self.distance * (1.0 - (1.0 - u).powf(1.0 / INFLEXION));
        let delta = d - self.travelled;
        self.travelled = d;
        ((self.dir.0 * delta, self.dir.1 * delta), u < 1.0)
    }

    /// Stops motion along one axis (e.g. after hitting the edge).
    pub fn stop_axis(&mut self, x: bool) {
        if x {
            self.dir.0 = 0.0;
        } else {
            self.dir.1 = 0.0;
        }
    }

    pub fn is_dead(&self) -> bool {
        self.dir.0 == 0.0 && self.dir.1 == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn velocity_of_linear_motion() {
        let mut v = VelocityTracker::default();
        for i in 0..10 {
            let t = i as f64 * 0.008;
            v.add(t, 100.0 + 2000.0 * t, 50.0 - 500.0 * t);
        }
        let (vx, vy) = v.velocity(0.072);
        assert!((vx - 2000.0).abs() < 1.0, "{vx}");
        assert!((vy + 500.0).abs() < 1.0, "{vy}");
        assert_eq!(v.velocity(0.5), (0.0, 0.0));
    }

    #[test]
    fn fling_matches_android_scale() {
        let mut f = Fling::new(0.0, 0.0, 5000.0, 2.625).unwrap();
        // Android travels ~1.5k px over ~0.9 s for 5000 px/s at 420 dpi.
        assert!(f.distance > 1200.0 && f.distance < 2000.0, "{}", f.distance);
        assert!(f.duration > 0.7 && f.duration < 1.1, "{}", f.duration);
        let mut total = 0.0;
        let mut t = 0.0;
        loop {
            t += 1.0 / 120.0;
            let ((_, dy), running) = f.step(t);
            total += dy;
            if !running {
                break;
            }
        }
        assert!((total - f.distance).abs() < 1e-6);
        // Initial velocity is preserved.
        let mut g = Fling::new(0.0, 0.0, 5000.0, 2.625).unwrap();
        let ((_, dy), _) = g.step(0.001);
        assert!((dy / 0.001 - 5000.0).abs() < 50.0, "{}", dy / 0.001);
        assert!(Fling::new(0.0, 10.0, 0.0, 2.0).is_none());
    }
}
