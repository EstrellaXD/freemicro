//! Push-to-talk latch and double-tap state machines. Pure; time is passed
//! in. Port of `input/latch.py`.

pub const LATCH_WINDOW_SECONDS: f64 = 0.35;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Start,
    Stop,
    Fire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LatchPhase {
    #[default]
    Idle,
    Pressed,
    Waiting,
    Latched,
    Suppressing,
}

/// One MIC key: hold to talk, tap-tap to latch, tap to stop. `Start` and
/// `Stop` are each one tap of the dictation app's toggle shortcut.
#[derive(Debug, Clone, Default)]
pub struct LatchMachine {
    pub window: f64,
    pub phase: LatchPhase,
    /// When the last press went down.
    since: f64,
    due: Option<f64>,
}

impl LatchMachine {
    pub fn new(window: f64) -> LatchMachine {
        LatchMachine {
            window,
            ..LatchMachine::default()
        }
    }
    pub fn recording(&self) -> bool {
        matches!(
            self.phase,
            LatchPhase::Pressed | LatchPhase::Waiting | LatchPhase::Latched
        )
    }
    fn start(&mut self, now: f64) -> Vec<Signal> {
        self.phase = LatchPhase::Pressed;
        self.since = now;
        self.due = None;
        vec![Signal::Start]
    }
    pub fn press(&mut self, now: f64) -> Vec<Signal> {
        match self.phase {
            LatchPhase::Idle => self.start(now),
            LatchPhase::Waiting => {
                // The second tap inside the window: latch on, indefinitely.
                self.phase = LatchPhase::Latched;
                self.due = None;
                vec![]
            }
            LatchPhase::Latched => {
                self.phase = LatchPhase::Suppressing;
                self.due = Some(now + self.window);
                vec![Signal::Stop]
            }
            LatchPhase::Suppressing => {
                if self.due.is_some_and(|due| now >= due) {
                    self.start(now)
                } else {
                    vec![]
                }
            }
            LatchPhase::Pressed => vec![],
        }
    }
    pub fn release(&mut self, now: f64) -> Vec<Signal> {
        if self.phase != LatchPhase::Pressed {
            return vec![];
        }
        if now - self.since >= self.window {
            self.phase = LatchPhase::Idle;
            self.due = None;
            return vec![Signal::Stop];
        }
        self.phase = LatchPhase::Waiting;
        self.due = Some(now + self.window);
        vec![]
    }
    pub fn tick(&mut self, now: f64) -> Vec<Signal> {
        match self.due {
            Some(due) if now >= due => {}
            _ => return vec![],
        }
        self.due = None;
        match self.phase {
            LatchPhase::Waiting => {
                self.phase = LatchPhase::Idle;
                vec![Signal::Stop]
            }
            LatchPhase::Suppressing => {
                self.phase = LatchPhase::Idle;
                vec![]
            }
            _ => vec![],
        }
    }
    pub fn force_stop(&mut self, _now: f64) -> Vec<Signal> {
        let was = self.recording();
        self.phase = LatchPhase::Idle;
        self.due = None;
        if was {
            vec![Signal::Stop]
        } else {
            vec![]
        }
    }
    /// When `tick` next has work.
    pub fn deadline(&self) -> Option<f64> {
        self.due
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TapPhase {
    #[default]
    Idle,
    Pressed,
    Waiting,
    /// Second tap down, `Fire` sent; its key-up ends the pair.
    Armed,
}

/// Watches a physical `hold` for two quick taps; fires on the second press.
#[derive(Debug, Clone, Default)]
pub struct DoubleTapMachine {
    pub window: f64,
    phase: TapPhase,
    pressed_at: f64,
    due: Option<f64>,
}

impl DoubleTapMachine {
    pub fn new(window: f64) -> DoubleTapMachine {
        DoubleTapMachine {
            window,
            ..DoubleTapMachine::default()
        }
    }
    pub fn press(&mut self, now: f64) -> Vec<Signal> {
        if self.phase == TapPhase::Waiting && self.due.is_some_and(|due| now < due) {
            self.phase = TapPhase::Armed;
            self.due = None;
            return vec![Signal::Fire];
        }
        // A first tap, or a press after a lapsed window: a fresh pair.
        self.phase = TapPhase::Pressed;
        self.pressed_at = now;
        self.due = None;
        vec![]
    }
    pub fn release(&mut self, now: f64) -> Vec<Signal> {
        match self.phase {
            TapPhase::Pressed => {
                if now - self.pressed_at < self.window {
                    self.phase = TapPhase::Waiting;
                    self.due = Some(now + self.window);
                } else {
                    self.phase = TapPhase::Idle;
                    self.due = None;
                }
            }
            TapPhase::Armed => {
                self.phase = TapPhase::Idle;
                self.due = None;
            }
            _ => {}
        }
        vec![]
    }
    pub fn tick(&mut self, now: f64) -> Vec<Signal> {
        if self.phase == TapPhase::Waiting && self.due.is_some_and(|due| now >= due) {
            self.phase = TapPhase::Idle;
            self.due = None;
        }
        vec![]
    }
    pub fn deadline(&self) -> Option<f64> {
        self.due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Signal::*;

    fn machine() -> LatchMachine {
        LatchMachine::new(LATCH_WINDOW_SECONDS)
    }

    #[test]
    fn latch_hold_to_talk_stops_on_a_late_release() {
        let mut m = machine();
        assert_eq!(m.press(0.0), vec![Start]);
        assert!(m.recording());
        assert_eq!(m.release(1.0), vec![Stop]);
        assert_eq!(m.phase, LatchPhase::Idle);
        assert_eq!(m.deadline(), None);
    }

    #[test]
    fn latch_tap_tap_latches_and_a_tap_stops_with_debounce() {
        let mut m = machine();
        m.press(0.0);
        assert_eq!(m.release(0.1), vec![]);
        assert_eq!(m.phase, LatchPhase::Waiting);
        assert_eq!(m.deadline(), Some(0.1 + LATCH_WINDOW_SECONDS));
        assert_eq!(m.press(0.2), vec![]);
        assert_eq!(m.phase, LatchPhase::Latched);
        assert_eq!(m.deadline(), None);
        assert_eq!(m.release(0.3), vec![]);
        assert_eq!(m.press(10.0), vec![Stop]);
        assert!(!m.recording());
        // A second stop tap inside the window is swallowed.
        assert_eq!(m.press(10.1), vec![]);
        // After the window a press is a fresh session.
        assert_eq!(m.press(10.5), vec![Start]);
    }

    #[test]
    fn latch_tick_resolves_waiting_and_suppressing_windows() {
        let mut m = machine();
        m.press(0.0);
        m.release(0.1);
        assert_eq!(m.tick(0.2), vec![]);
        assert_eq!(m.tick(0.5), vec![Stop]);
        assert_eq!(m.phase, LatchPhase::Idle);

        let mut m = machine();
        m.press(0.0);
        m.release(0.1);
        m.press(0.2);
        m.press(1.0);
        assert_eq!(m.tick(1.4), vec![]);
        assert_eq!(m.phase, LatchPhase::Idle);
    }

    #[test]
    fn latch_force_stop_emits_only_when_recording() {
        let mut m = machine();
        assert_eq!(m.force_stop(0.0), vec![]);
        m.press(0.0);
        assert_eq!(m.force_stop(0.1), vec![Stop]);
        assert_eq!(m.release(0.2), vec![]);
    }

    #[test]
    fn double_tap_fires_on_the_second_press_once_per_pair() {
        let mut m = DoubleTapMachine::new(LATCH_WINDOW_SECONDS);
        // tap, tap, tap, tap: fires on press 2 and press 4.
        let mut fired = Vec::new();
        for i in 0..4 {
            let t = i as f64 * 0.1;
            fired.push(m.press(t) == vec![Fire]);
            m.release(t + 0.05);
        }
        assert_eq!(fired, vec![false, true, false, true]);
    }

    #[test]
    fn double_tap_ignores_slow_taps_and_long_holds() {
        let mut m = DoubleTapMachine::new(LATCH_WINDOW_SECONDS);
        m.press(0.0);
        m.release(0.1);
        assert_eq!(m.press(1.0), vec![]);
        m.release(2.0); // a long hold opens no window
        assert_eq!(m.deadline(), None);
        assert_eq!(m.press(2.1), vec![]);
        m.release(2.15);
        m.tick(3.0);
        assert_eq!(m.press(3.1), vec![]);
    }
}
