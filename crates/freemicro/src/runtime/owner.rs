//! Keeping our colours against the other writer (the ChatGPT app), and the
//! activity overlay. Port of `lighting_owner.py` (`ActivityOverlay`,
//! `LightingOwner`, `vendor_app_running`, `coexist_advice`). Config reload is
//! the run loop's job here, not the owner's.

use super::leds::LedRenderer;
use super::lighting::Zone;
use super::lighting_config::{ActivityLight, LightingConfig};

pub const QUIET_SECONDS: f64 = 0.1;
pub const COEXIST_HEARTBEAT_SECONDS: f64 = 3.0;
pub const VENDOR_APP: &str = "ChatGPT";
/// The one zone the vendor app leaves dark.
pub const VENDOR_QUIET_ZONES: [Zone; 1] = [Zone::Backlight];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ActivityTimeout,
    ActivityDropped,
    Reconnect,
    VendorQuit,
    VendorStarted,
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LightingEvent {
    pub reason: Reason,
    pub message: String,
}

impl LightingEvent {
    fn new(reason: Reason, message: impl Into<String>) -> LightingEvent {
        LightingEvent {
            reason,
            message: message.into(),
        }
    }
    /// Whether this event re-sends the frame.
    pub fn reasserted(&self) -> bool {
        matches!(
            self.reason,
            Reason::Reconnect | Reason::VendorQuit | Reason::Heartbeat
        )
    }
    /// Periodic by definition: printed only with `-v`.
    pub fn verbose_only(&self) -> bool {
        self.reason == Reason::Heartbeat
    }
}

/// What a live binding asks the pad to show, with a hard timeout.
#[derive(Debug, Default)]
pub struct ActivityOverlay {
    active: Vec<(String, ActivityLight, f64)>,
}

impl ActivityOverlay {
    /// `light` Some = pressed at `now`, None = released.
    pub fn note(&mut self, input_id: &str, light: Option<ActivityLight>, now: f64) {
        self.active.retain(|(id, ..)| id != input_id);
        if let Some(light) = light {
            let deadline = now + light.timeout_seconds;
            self.active.push((input_id.to_string(), light, deadline));
        }
    }
    /// The newest live light.
    pub fn current(&self) -> Option<ActivityLight> {
        self.active.last().map(|(_, light, _)| light.clone())
    }
    /// Drop holds past their timeout.
    pub fn poll(&mut self, now: f64) -> Option<LightingEvent> {
        let expired: Vec<String> = self
            .active
            .iter()
            .filter(|(.., deadline)| *deadline <= now)
            .map(|(id, ..)| id.clone())
            .collect();
        if expired.is_empty() {
            return None;
        }
        self.active.retain(|(.., deadline)| *deadline > now);
        Some(LightingEvent::new(
            Reason::ActivityTimeout,
            format!(
                "{} never reported a key-up, so its light has been taken\ndown from the clock. The pad drops on sleep and on range; press and\nrelease the key to prove it is back.",
                expired.join(", ")
            ),
        ))
    }
    pub fn clear(&mut self, reason: &str) -> Option<LightingEvent> {
        if self.active.is_empty() {
            return None;
        }
        let dropped: Vec<String> = self.active.drain(..).map(|(id, ..)| id).collect();
        let suffix = if reason.is_empty() {
            String::new()
        } else {
            format!(" ({reason})")
        };
        Some(LightingEvent::new(
            Reason::ActivityDropped,
            format!("cleared the light held by {}{suffix}", dropped.join(", ")),
        ))
    }
    pub fn next_deadline(&self) -> Option<f64> {
        self.active
            .iter()
            .map(|(.., deadline)| *deadline)
            .min_by(f64::total_cmp)
    }
}

/// Re-asserts our frame when the vendor app starts or quits, on reconnect,
/// and on an optional heartbeat; never while input is arriving.
pub struct LightingOwner {
    vendor_probe: Box<dyn FnMut() -> bool>,
    /// `None` until the first probe, so the first look is never a transition.
    vendor_running: Option<bool>,
    next_probe: f64,
    beat_at: f64,
    sends: u64,
    attachments: u32,
    last_input: f64,
}

impl LightingOwner {
    pub fn new(vendor_probe: Box<dyn FnMut() -> bool>) -> LightingOwner {
        LightingOwner {
            vendor_probe,
            vendor_running: None,
            next_probe: f64::NEG_INFINITY,
            beat_at: 0.0,
            sends: 0,
            attachments: 0,
            last_input: f64::NEG_INFINITY,
        }
    }
    /// A pad (re)attached: the renderer's cache no longer describes the pad.
    /// The first attach is silent.
    pub fn attach(&mut self, leds: &mut LedRenderer, now: f64) -> Option<LightingEvent> {
        self.sends = leds.sends();
        self.beat_at = now;
        self.attachments += 1;
        if self.attachments == 1 {
            return None;
        }
        leds.invalidate();
        Some(LightingEvent::new(
            Reason::Reconnect,
            "reasserted lighting (pad reconnected)",
        ))
    }
    pub fn note_input(&mut self, now: f64) {
        self.last_input = now;
    }
    pub fn busy(&self, now: f64) -> bool {
        now - self.last_input < QUIET_SECONDS
    }
    /// `subset` = `agent_keys.subset` (a coexistence key set is configured).
    pub fn poll(
        &mut self,
        leds: &mut LedRenderer,
        lighting: &LightingConfig,
        subset: bool,
        now: f64,
    ) -> Vec<LightingEvent> {
        let reassert = lighting.reassert;
        if !reassert.enabled || self.busy(now) {
            return Vec::new();
        }
        if leds.sends() != self.sends {
            self.sends = leds.sends();
            self.beat_at = now;
        }
        let mut events = Vec::new();
        if now >= self.next_probe {
            self.next_probe = now + reassert.poll_seconds.max(0.25);
            events.extend(self.check_vendor());
        }
        if !events.iter().any(LightingEvent::reasserted) {
            let coexist = subset && self.vendor_running == Some(true);
            let interval = if reassert.heartbeat_seconds > 0.0 {
                reassert.heartbeat_seconds
            } else if coexist {
                COEXIST_HEARTBEAT_SECONDS
            } else {
                0.0
            };
            if interval > 0.0 && now - self.beat_at >= interval {
                let message = if reassert.heartbeat_seconds <= 0.0 && coexist {
                    "reasserted lighting (holding your Agent Keys; Codex keeps the rest)"
                } else {
                    "reasserted lighting (heartbeat)"
                };
                events.push(LightingEvent::new(Reason::Heartbeat, message));
            }
        }
        if events.iter().any(LightingEvent::reasserted) {
            self.beat_at = now;
            leds.invalidate();
        }
        events
    }

    fn check_vendor(&mut self) -> Option<LightingEvent> {
        let running = (self.vendor_probe)();
        let was = self.vendor_running.replace(running);
        match was {
            Some(true) if !running => Some(LightingEvent::new(
                Reason::VendorQuit,
                "reasserted lighting (ChatGPT quit)",
            )),
            Some(false) if running => Some(LightingEvent::new(
                Reason::VendorStarted,
                "ChatGPT started - it drives these LEDs too; FreeMicro will reassert when it quits",
            )),
            _ => None,
        }
    }
}

/// `pgrep -x ChatGPT` for the desktop app.
pub fn vendor_app_running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", VENDOR_APP])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Lighting only on zones the vendor app leaves alone.
pub fn owns_only_quiet_zones(lighting: &LightingConfig) -> bool {
    !lighting.zones.is_empty()
        && lighting
            .zones
            .iter()
            .all(|z| VENDOR_QUIET_ZONES.contains(z))
}

/// Startup advice lines about sharing the pad with the vendor app.
pub fn coexist_advice(lighting: &LightingConfig, vendor_running: bool) -> String {
    if !vendor_running || !lighting.enabled || owns_only_quiet_zones(lighting) {
        return String::new();
    }
    "ChatGPT is running and drives these LEDs too. FreeMicro will reassert its\n\
     colours when ChatGPT quits. To never collide in the first place:\n  \
     freemicro lights --coexist    own only the key backlight, which\n                                \
     ChatGPT leaves dark (one glow under\n                                \
     the caps, not six per-key lights)"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::lighting_config::StateLight;
    use crate::state::DecayPolicy;
    use crate::testutil::TempDir;
    use std::cell::Cell;
    use std::rc::Rc;

    fn light(timeout: f64) -> ActivityLight {
        ActivityLight {
            light: StateLight::solid(0x2E8B57),
            zones: vec![Zone::Underglow],
            timeout_seconds: timeout,
        }
    }

    #[test]
    fn overlay_shows_the_newest_hold_and_releases() {
        let mut overlay = ActivityOverlay::default();
        overlay.note("ACT06", Some(light(10.0)), 0.0);
        overlay.note("ACT07", Some(light(5.0)), 1.0);
        assert_eq!(overlay.current().unwrap().timeout_seconds, 5.0);
        assert_eq!(overlay.next_deadline(), Some(6.0));
        overlay.note("ACT07", None, 2.0);
        assert_eq!(overlay.current().unwrap().timeout_seconds, 10.0);
    }

    #[test]
    fn overlay_times_out_a_lost_release() {
        let mut overlay = ActivityOverlay::default();
        overlay.note("ACT06", Some(light(2.0)), 0.0);
        assert!(overlay.poll(1.9).is_none());
        let event = overlay.poll(2.0).unwrap();
        assert_eq!(event.reason, Reason::ActivityTimeout);
        assert!(event.message.starts_with("ACT06 never reported"));
        assert!(overlay.current().is_none());
    }

    #[test]
    fn overlay_clear_names_what_it_dropped() {
        let mut overlay = ActivityOverlay::default();
        assert!(overlay.clear("x").is_none());
        overlay.note("ACT06", Some(light(2.0)), 0.0);
        let event = overlay.clear("the pad disconnected").unwrap();
        assert_eq!(
            event.message,
            "cleared the light held by ACT06 (the pad disconnected)"
        );
    }

    #[test]
    fn the_backlight_is_the_only_zone_the_vendor_leaves_alone() {
        let mut lighting = LightingConfig {
            enabled: true,
            ..LightingConfig::default()
        };
        assert!(!coexist_advice(&lighting, true).is_empty());
        assert!(coexist_advice(&lighting, false).is_empty());
        lighting.zones = vec![Zone::Backlight];
        assert!(owns_only_quiet_zones(&lighting));
        assert!(coexist_advice(&lighting, true).is_empty());
    }

    fn renderer(home: &TempDir) -> LedRenderer {
        LedRenderer::from_parts(
            LightingConfig::default(),
            Default::default(),
            Vec::new(),
            DecayPolicy::default(),
            home.path().to_path_buf(),
        )
    }

    fn owner_with(running: Rc<Cell<bool>>) -> LightingOwner {
        LightingOwner::new(Box::new(move || running.get()))
    }

    #[test]
    fn vendor_quit_reasserts_and_start_only_reports() {
        let home = TempDir::new();
        let mut leds = renderer(&home);
        let running = Rc::new(Cell::new(true));
        let mut owner = owner_with(running.clone());
        let lighting = LightingConfig::default();
        assert!(owner.poll(&mut leds, &lighting, false, 0.0).is_empty());
        running.set(false);
        let events = owner.poll(&mut leds, &lighting, false, 3.0);
        assert_eq!(events[0].reason, Reason::VendorQuit);
        running.set(true);
        let events = owner.poll(&mut leds, &lighting, false, 6.0);
        assert_eq!(events[0].reason, Reason::VendorStarted);
        assert!(!events[0].reasserted());
    }

    #[test]
    fn the_vendor_probe_is_rate_limited_and_quiet_while_keys_arrive() {
        let home = TempDir::new();
        let mut leds = renderer(&home);
        let calls = Rc::new(Cell::new(0));
        let counter = calls.clone();
        let mut owner = LightingOwner::new(Box::new(move || {
            counter.set(counter.get() + 1);
            false
        }));
        let lighting = LightingConfig::default();
        owner.poll(&mut leds, &lighting, false, 0.0);
        owner.poll(&mut leds, &lighting, false, 1.0);
        owner.note_input(3.0);
        owner.poll(&mut leds, &lighting, false, 3.05);
        assert_eq!(calls.get(), 1);
        owner.poll(&mut leds, &lighting, false, 3.2);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn heartbeat_is_off_by_default_and_on_for_a_subset_with_codex() {
        let home = TempDir::new();
        let mut leds = renderer(&home);
        let mut owner = owner_with(Rc::new(Cell::new(true)));
        let lighting = LightingConfig::default();
        owner.attach(&mut leds, 0.0);
        assert!(owner.poll(&mut leds, &lighting, false, 10.0).is_empty());
        let events = owner.poll(&mut leds, &lighting, true, 20.0);
        assert_eq!(events[0].reason, Reason::Heartbeat);
        assert!(events[0].message.contains("holding your Agent Keys"));
    }

    #[test]
    fn an_explicit_heartbeat_fires_on_its_interval() {
        let home = TempDir::new();
        let mut leds = renderer(&home);
        let mut owner = owner_with(Rc::new(Cell::new(false)));
        let mut lighting = LightingConfig::default();
        lighting.reassert.heartbeat_seconds = 5.0;
        owner.attach(&mut leds, 0.0);
        assert!(owner.poll(&mut leds, &lighting, false, 4.0).is_empty());
        let events = owner.poll(&mut leds, &lighting, false, 5.0);
        assert_eq!(events[0].message, "reasserted lighting (heartbeat)");
        assert!(events[0].verbose_only());
        lighting.reassert.enabled = false;
        assert!(owner.poll(&mut leds, &lighting, false, 20.0).is_empty());
    }

    #[test]
    fn reconnecting_reasserts_but_the_first_connect_is_silent() {
        let home = TempDir::new();
        let mut leds = renderer(&home);
        let mut owner = owner_with(Rc::new(Cell::new(false)));
        assert!(owner.attach(&mut leds, 0.0).is_none());
        assert_eq!(
            owner.attach(&mut leds, 1.0).unwrap().reason,
            Reason::Reconnect
        );
    }
}
