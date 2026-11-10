//! Joystick-as-mouse: speed curve, precision mode, tap-to-click. Pure; the
//! bridge calls [`PointerEngine::tick`] from its deadline. Port of
//! `input/pointer.py` (no PointerLoop thread).
//!
//! The pad sends a sample only when the stick changes, so motion is
//! integrated on a tick clock from the last sample. A sample older than
//! [`DEFAULT_STALE_SECONDS`] is a stop (a lost release fails safe). Angle 0.75
//! is up: `(cos, sin)` of `2*pi*a` is taken straight as screen coordinates.

use super::config::JoystickConfig;

pub const DEFAULT_STALE_SECONDS: f64 = 4.0;
pub const DEFAULT_MAX_STEP: f64 = 0.05;
pub const TAP_MAX_SECONDS: f64 = 0.2;
pub const TAP_MAX_TRAVEL_PX: f64 = 30.0;

/// Pixels per second for a deflection past the deadzone.
pub fn curve_speed(distance: f64, deadzone: f64, max_speed: f64, gamma: f64) -> f64 {
    let span = 1.0 - deadzone;
    if span <= 0.0 {
        return if distance > deadzone { max_speed } else { 0.0 };
    }
    if distance <= deadzone {
        return 0.0;
    }
    let reach = ((distance - deadzone) / span).min(1.0);
    max_speed * reach.powf(gamma)
}

/// Unit screen vector for `angle` (0..1 clockwise from +x; screen y down).
pub fn screen_vector(angle: f64, invert_y: bool) -> (f64, f64) {
    let theta = 2.0 * std::f64::consts::PI * angle.rem_euclid(1.0);
    let dy = theta.sin();
    (theta.cos(), if invert_y { -dy } else { dy })
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PointerVector {
    pub vx: f64,
    pub vy: f64,
}

impl PointerVector {
    pub fn moving(&self) -> bool {
        self.vx != 0.0 || self.vy != 0.0
    }
}

#[derive(Debug, Clone)]
pub struct PointerEngine {
    pub config: JoystickConfig,
    angle: f64,
    distance: f64,
    sampled_at: Option<f64>,
    last_tick: Option<f64>,
    acc_x: f64,
    acc_y: f64,
    precision: bool,
    tap_armed: bool,
    tap_start: f64,
    tap_travel: f64,
    pending_taps: u32,
}

impl PointerEngine {
    pub fn new(config: JoystickConfig) -> PointerEngine {
        PointerEngine {
            config,
            angle: 0.0,
            distance: 0.0,
            sampled_at: None,
            last_tick: None,
            acc_x: 0.0,
            acc_y: 0.0,
            precision: false,
            tap_armed: false,
            tap_start: 0.0,
            tap_travel: 0.0,
            pending_taps: 0,
        }
    }
    pub fn configure(&mut self, config: JoystickConfig) {
        self.config = config;
    }
    pub fn update(&mut self, angle: f64, distance: f64, now: f64) {
        self.angle = angle;
        self.distance = distance;
        self.sampled_at = Some(now);
        self.tap_sample(distance, now);
    }
    /// Arm on a push past the action deadzone; a return to centre in time
    /// and within the pixel budget is a tap.
    fn tap_sample(&mut self, distance: f64, now: f64) {
        if !self.config.tap_click || !self.config.pointing() {
            self.tap_armed = false;
            return;
        }
        if !self.tap_armed {
            if distance >= self.config.deadzone {
                self.tap_armed = true;
                self.tap_start = now;
                self.tap_travel = 0.0;
            }
            return;
        }
        if now - self.tap_start > TAP_MAX_SECONDS || self.tap_travel > TAP_MAX_TRAVEL_PX {
            self.tap_armed = false;
        } else if distance <= self.config.pointer_deadzone {
            self.tap_armed = false;
            self.pending_taps += 1;
        }
    }
    fn tap_tick(&mut self, dx: i32, dy: i32, now: f64) {
        if !self.tap_armed {
            return;
        }
        if dx != 0 || dy != 0 {
            self.tap_travel += f64::from(dx).hypot(f64::from(dy));
        }
        if now - self.tap_start > TAP_MAX_SECONDS || self.tap_travel > TAP_MAX_TRAVEL_PX {
            self.tap_armed = false;
        }
    }
    pub fn set_precision(&mut self, on: bool) {
        self.precision = on;
    }
    pub fn reset(&mut self) {
        self.distance = 0.0;
        self.sampled_at = None;
        self.last_tick = None;
        self.acc_x = 0.0;
        self.acc_y = 0.0;
        self.precision = false;
        self.tap_armed = false;
        self.tap_travel = 0.0;
        self.pending_taps = 0;
    }
    fn vector(&self, now: f64) -> PointerVector {
        match self.sampled_at {
            Some(at) if now - at <= DEFAULT_STALE_SECONDS => {}
            _ => return PointerVector::default(),
        }
        let config = &self.config;
        let mut speed = curve_speed(
            self.distance,
            config.pointer_deadzone,
            config.max_speed,
            config.gamma,
        );
        if self.precision {
            speed *= config.precision_scale;
        }
        if speed <= 0.0 {
            return PointerVector::default();
        }
        let (ux, uy) = screen_vector(self.angle - config.origin, config.invert_y);
        PointerVector {
            vx: ux * speed,
            vy: uy * speed,
        }
    }
    pub fn moving(&self, now: f64) -> bool {
        self.vector(now).moving()
    }
    /// Integer pixel step for this tick.
    pub fn tick(&mut self, now: f64) -> (i32, i32) {
        let vector = self.vector(now);
        let previous = self.last_tick.replace(now);
        if !vector.moving() {
            // Drop the remainder so it cannot leak into the next flick.
            self.acc_x = 0.0;
            self.acc_y = 0.0;
            self.tap_tick(0, 0, now);
            return (0, 0);
        }
        let dt = match previous {
            Some(previous) if now > previous => (now - previous).min(DEFAULT_MAX_STEP),
            _ => {
                // The first tick only sets the baseline.
                self.tap_tick(0, 0, now);
                return (0, 0);
            }
        };
        self.acc_x += vector.vx * dt;
        self.acc_y += vector.vy * dt;
        // Truncate toward zero so the remainder keeps the travel's sign.
        let (dx, dy) = (self.acc_x.trunc(), self.acc_y.trunc());
        self.acc_x -= dx;
        self.acc_y -= dy;
        let (dx, dy) = (dx as i32, dy as i32);
        self.tap_tick(dx, dy, now);
        (dx, dy)
    }
    /// Pending tap clicks.
    pub fn take_tap(&mut self) -> u32 {
        std::mem::take(&mut self.pending_taps)
    }
    /// Next tick time while moving (1 / tick_hz), else `None`.
    pub fn deadline(&self, now: f64) -> Option<f64> {
        if !self.moving(now) {
            return None;
        }
        Some(match self.last_tick {
            Some(last) => last + 1.0 / self.config.tick_hz,
            None => now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> PointerEngine {
        PointerEngine::new(JoystickConfig::default())
    }

    #[test]
    fn curve_speed_is_zero_inside_the_deadzone_and_max_at_the_edge() {
        let cases = [(0.05, 0.0), (0.1, 0.0), (1.0, 1200.0), (2.0, 1200.0)];
        for (distance, want) in cases {
            assert_eq!(curve_speed(distance, 0.1, 1200.0, 2.0), want, "{distance}");
        }
        let mid = curve_speed(0.55, 0.1, 1200.0, 2.0);
        assert!((mid - 300.0).abs() < 1e-9);
    }

    #[test]
    fn screen_vector_puts_up_at_three_quarters() {
        let cases = [
            (0.0, false, (1.0, 0.0)),
            (0.25, false, (0.0, 1.0)),
            (0.75, false, (0.0, -1.0)),
            (0.75, true, (0.0, 1.0)),
        ];
        for (angle, invert, (x, y)) in cases {
            let (vx, vy) = screen_vector(angle, invert);
            assert!((vx - x).abs() < 1e-9 && (vy - y).abs() < 1e-9, "{angle}");
        }
    }

    #[test]
    fn tick_integrates_after_a_baseline_and_carries_subpixels() {
        let mut e = engine();
        e.update(0.0, 1.0, 0.0);
        assert_eq!(e.deadline(0.0), Some(0.0));
        assert_eq!(e.tick(0.0), (0, 0));
        // 1200 px/s * 0.01 s = 12 px.
        assert_eq!(e.tick(0.01), (12, 0));
        // dt is capped at 50 ms: 60 px, not 1200.
        assert_eq!(e.tick(1.01), (60, 0));
        let mut slow = engine();
        slow.update(0.0, 0.2, 0.0); // ~14.8 px/s: 0.15 px per 10 ms
        slow.tick(0.0);
        let total: i32 = (1..=10).map(|i| slow.tick(f64::from(i) * 0.01).0).sum();
        assert_eq!(total, 1);
    }

    #[test]
    fn a_stale_sample_stops_the_pointer() {
        let mut e = engine();
        e.update(0.0, 1.0, 0.0);
        e.tick(0.0);
        assert!(e.moving(3.9));
        assert!(!e.moving(4.1));
        assert_eq!(e.tick(4.1), (0, 0));
        assert_eq!(e.deadline(4.1), None);
    }

    #[test]
    fn precision_scales_speed() {
        let mut e = engine();
        e.set_precision(true);
        e.update(0.0, 1.0, 0.0);
        e.tick(0.0);
        assert_eq!(e.tick(0.01), (3, 0));
    }

    #[test]
    fn a_quick_push_and_return_is_a_tap_and_a_slow_one_is_not() {
        let mut e = engine();
        e.update(0.0, 0.9, 0.0);
        e.update(0.0, 0.0, 0.1);
        assert_eq!(e.take_tap(), 1);
        assert_eq!(e.take_tap(), 0);
        e.update(0.0, 0.9, 1.0);
        e.update(0.0, 0.0, 1.3);
        assert_eq!(e.take_tap(), 0);
        // Travel past the budget disarms the tap.
        e.update(0.0, 1.0, 2.0);
        e.tick(2.0);
        e.tick(2.05);
        e.update(0.0, 0.0, 2.06);
        assert_eq!(e.take_tap(), 0);
        let mut off = PointerEngine::new(JoystickConfig {
            tap_click: false,
            ..JoystickConfig::default()
        });
        off.update(0.0, 0.9, 0.0);
        off.update(0.0, 0.0, 0.1);
        assert_eq!(off.take_tap(), 0);
    }
}
