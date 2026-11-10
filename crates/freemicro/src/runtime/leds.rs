//! Agent state -> LED frames. Port of `renderers/micro_leds.py`. Pure frame
//! building ([`LedRenderer::build`]) is separate from the write
//! ([`LedRenderer::render`]), so tests assert frames without a pad.
//!
//! The renderer never holds the pad: every call that writes takes the
//! pad-owner thread's `&mut dyn PadIo`. Lighting notices (write failed /
//! recovered) are queued and drained by the run loop with
//! [`LedRenderer::take_notices`].

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use super::lighting::{
    preview_message, preview_zone, rgbcfg_message, rgbcfg_side, thread_entry, thstatus_message,
    Zone,
};
use super::lighting_config::{ActivityLight, LightingConfig, StateLight};
use crate::device::PadIo;
use crate::herdr::Herdr;
use crate::input::config::PadConfig;
use crate::state::agentkeys::{AgentKeyOwnership, AgentKeysConfig, AgentSlot, SlotResolver};
use crate::state::{slots as slot_cache, AgentState, DecayPolicy, SessionState};

pub const WRITE_RETRY_BACKOFF: [f64; 4] = [1.0, 2.0, 5.0, 10.0];
pub const FLASH_SECONDS: f64 = 0.4;
pub const BLINK_HALF_MAX: f64 = 0.75;
pub const BLINK_HALF_MIN: f64 = 0.30;
/// `<home>/status.json`: cached `device.status` (battery) shared with Python.
pub const STATUS_FILENAME: &str = "status.json";
const ALERT_STATES: [AgentState; 2] = [AgentState::Waiting, AgentState::Error];
const STATE_FLASH: &str = "state";

type SlotKey = Vec<(String, Option<AgentState>)>;
type Model = (AgentState, Option<SlotKey>, Option<ActivityLight>);

/// Whether a blinking light is on at `now`: rate from `speed`, phase from
/// the shared clock.
fn blink_on(speed: f64, now: f64) -> bool {
    let half = BLINK_HALF_MAX - (BLINK_HALF_MAX - BLINK_HALF_MIN) * speed.clamp(0.0, 1.0);
    ((now / half).floor() as i64) % 2 == 0
}

/// Every zone some binding's light can claim (Python `activity_zones`).
fn activity_zones(config: &PadConfig) -> Vec<Zone> {
    let mut zones = Vec::new();
    let actions = config.bindings.values().chain(config.chords.values());
    for light in actions.filter_map(|a| a.light.as_ref()) {
        for zone in &light.zones {
            if !zones.contains(zone) {
                zones.push(*zone);
            }
        }
    }
    zones
}

/// The frame minus `lights.preview`'s per-call `id`, which is correlation
/// metadata, not part of what the pad shows.
fn dedupe_key(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            if let Some(obj) = m.as_object_mut() {
                obj.remove("id");
            }
            m
        })
        .collect()
}

pub struct LedRenderer {
    lighting: LightingConfig,
    agent_keys: AgentKeysConfig,
    /// Zones a binding's light can claim (from the config, not the moment).
    activity_zones: Vec<Zone>,
    decay: DecayPolicy,
    /// Built lazily, seeded from `slots.json`.
    resolver: Option<SlotResolver>,
    /// `<home>/slots.json`, published on every assignment change.
    slots_path: PathBuf,
    status_path: PathBuf,
    published: Vec<String>,
    /// herdr agents replace sessions while herdr runs. `None` in tests.
    pub herdr: Option<Herdr>,
    overlay: Option<ActivityLight>,
    /// Last frame sent; identical frames are not re-sent.
    frame: Option<Vec<Value>>,
    model: Option<Model>,
    dimmed: bool,
    /// The Mac is locked: dark until unlocked, whatever the agents do.
    locked: bool,
    dim_asserted: bool,
    activity_at: f64,
    failures: usize,
    failed_at: f64,
    retry_at: f64,
    released: bool,
    sends: u64,
    request_id: u32,
    prev_state: Option<AgentState>,
    prev_slot_states: HashMap<String, AgentState>,
    flash_until: HashMap<String, f64>,
    battery_low: bool,
    battery_checked_at: Option<f64>,
    notices: Vec<String>,
}

impl LedRenderer {
    pub fn new(config: &PadConfig, decay: DecayPolicy, home: PathBuf) -> LedRenderer {
        LedRenderer::from_parts(
            config.lighting.clone(),
            config.agent_keys.clone(),
            activity_zones(config),
            decay,
            home,
        )
    }

    pub fn from_parts(
        lighting: LightingConfig,
        agent_keys: AgentKeysConfig,
        activity_zones: Vec<Zone>,
        decay: DecayPolicy,
        home: PathBuf,
    ) -> LedRenderer {
        LedRenderer {
            lighting,
            agent_keys,
            activity_zones,
            decay,
            resolver: None,
            slots_path: slot_cache::slots_path(&home),
            status_path: home.join(STATUS_FILENAME),
            published: Vec::new(),
            herdr: None,
            overlay: None,
            frame: None,
            model: None,
            dimmed: false,
            locked: false,
            dim_asserted: false,
            activity_at: f64::NEG_INFINITY,
            failures: 0,
            failed_at: 0.0,
            retry_at: 0.0,
            released: false,
            sends: 0,
            request_id: 0,
            prev_state: None,
            prev_slot_states: HashMap::new(),
            flash_until: HashMap::new(),
            battery_low: false,
            battery_checked_at: None,
            notices: Vec::new(),
        }
    }

    pub fn lighting(&self) -> &LightingConfig {
        &self.lighting
    }

    /// Go dark while the Mac is locked; light up again on unlock.
    pub fn set_locked(&mut self, locked: bool) {
        if self.locked != locked {
            self.locked = locked;
            self.invalidate();
        }
    }

    pub fn dimmed(&self) -> bool {
        self.dimmed
    }

    /// Swap in a reloaded config. Lighting switched off hands the pad back
    /// the way the old config asked.
    pub fn apply_config(
        &mut self,
        config: &PadConfig,
        io: Option<&mut dyn PadIo>,
        ownership: &AgentKeyOwnership,
        now: f64,
    ) {
        let previous = std::mem::replace(&mut self.lighting, config.lighting.clone());
        self.agent_keys = config.agent_keys.clone();
        self.activity_zones = activity_zones(config);
        self.resolver = None;
        self.published.clear();
        self.frame = None;
        self.model = None;
        self.wake(now);
        if let Some(io) = io {
            if previous.enabled && !self.lighting.enabled {
                self.hand_back_with(io, &previous, &ownership.keys());
            }
        }
    }

    pub fn set_overlay(&mut self, overlay: Option<ActivityLight>) -> bool {
        if overlay == self.overlay {
            return false;
        }
        self.overlay = overlay;
        true
    }

    /// The user did something: restart the inactivity timer. Never writes.
    pub fn note_activity(&mut self, now: f64) {
        self.wake(now);
    }

    /// Forget the cached frame so the next render re-sends. Not activity:
    /// a dimmed pad re-sends the dark frame.
    pub fn invalidate(&mut self) {
        self.frame = None;
        self.dim_asserted = false;
    }

    pub fn sends(&self) -> u64 {
        self.sends
    }

    /// Lighting notices since the last call (write failed / recovered).
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// The six slots to light, or `None` to mirror one state.
    fn slots(
        &mut self,
        sessions: &[SessionState],
        owned: &[usize],
        wall: f64,
    ) -> Option<Vec<AgentSlot>> {
        if self.agent_keys.mirrors() || !self.lighting.drives(Zone::AgentKeys) {
            return None;
        }
        let resolver = self.resolver.get_or_insert_with(|| {
            let mut resolver = SlotResolver::new(self.agent_keys.clone(), self.decay);
            resolver.seed(&slot_cache::load(&self.slots_path));
            resolver
        });
        let sessions = match &self.herdr {
            Some(herdr) => herdr.agent_sessions(sessions, wall),
            None => sessions.to_vec(),
        };
        let slots = resolver.resolve(&sessions, wall, Some(owned));
        let assignment = resolver.previous().to_vec();
        if assignment != self.published && slot_cache::save(&self.slots_path, &assignment) {
            self.published = assignment;
        }
        Some(slots)
    }

    /// The messages that would show `state` / `sessions` now.
    pub fn messages_for(
        &mut self,
        state: AgentState,
        sessions: &[SessionState],
        ownership: &AgentKeyOwnership,
        now: f64,
        wall: f64,
    ) -> Vec<Value> {
        let owned = ownership.keys();
        let slots = self.slots(sessions, &owned, wall);
        self.build(state, slots.as_deref(), &owned, Some(now))
    }

    /// Every message for this state, in send order. Pure apart from the
    /// preview request id. `now = None` = no animation (on-phase colours).
    pub fn build(
        &mut self,
        state: AgentState,
        slots: Option<&[AgentSlot]>,
        owned: &[usize],
        now: Option<f64>,
    ) -> Vec<Value> {
        let base = self.lighting.light_for(state);
        let extra = self.extra_paint_zones();
        let battery = &self.lighting.battery;
        let battery_light =
            (now.is_some() && self.battery_low && battery.enabled).then_some(battery.light);
        let battery_zone = battery.zone;
        let overlay = self.overlay.clone();
        let eff = |light: StateLight, key: Option<&str>| match now {
            None => light,
            Some(now) => self.effective_look(light, key, now),
        };
        let look = |zone: Zone| -> Option<StateLight> {
            if let Some(overlay) = overlay.as_ref().filter(|o| o.zones.contains(&zone)) {
                return Some(eff(overlay.light, None));
            }
            if let Some(light) = battery_light.filter(|_| battery_zone == zone) {
                return Some(eff(light, None));
            }
            if self.lighting.drives(zone) {
                return Some(eff(base, Some(STATE_FLASH)));
            }
            extra.contains(&zone).then_some(StateLight::DARK)
        };
        let keys = look(Zone::Backlight);
        let ambient = look(Zone::Underglow);

        let agent: Option<Result<StateLight, Vec<Value>>> = if let Some(o) = overlay
            .as_ref()
            .filter(|o| o.zones.contains(&Zone::AgentKeys))
        {
            Some(Ok(eff(o.light, None)))
        } else if let Some(light) = battery_light.filter(|_| battery_zone == Zone::AgentKeys) {
            Some(Ok(eff(light, None)))
        } else if self.lighting.drives(Zone::AgentKeys) {
            match slots {
                Some(slots) => Some(Err(slots
                    .iter()
                    .filter(|s| s.owned)
                    .map(|s| self.slot_entry(s, now))
                    .collect())),
                None => Some(Ok(eff(base, Some(STATE_FLASH)))),
            }
        } else if extra.contains(&Zone::AgentKeys) {
            Some(Ok(StateLight::DARK))
        } else {
            None
        };

        let mut messages = Vec::new();
        if keys.is_some() || ambient.is_some() {
            messages.push(self.zone_message(keys, ambient, &self.lighting.method.clone()));
        }
        let mut entries = match agent {
            Some(Ok(light)) => agent_entries(light, owned),
            Some(Err(entries)) => entries,
            None => Vec::new(),
        };
        // A Creator Micro 2 lights its underglow from entry 5 (AG05's LED):
        // no other message reaches it, so a fixed underglow takes that key.
        if let Some(fixed) = self.lighting.underglow.filter(|_| !entries.is_empty()) {
            entries.retain(|e| e["id"] != 5);
            entries.extend(agent_entries(fixed, &[5]));
        }
        if !entries.is_empty() {
            messages.push(thstatus_message(entries));
        }
        messages
    }

    /// Build and send if changed. False on a write failure (backs off).
    pub fn render(
        &mut self,
        io: &mut dyn PadIo,
        state: AgentState,
        sessions: &[SessionState],
        ownership: &AgentKeyOwnership,
        now: f64,
        wall: f64,
    ) -> bool {
        if !self.lighting.enabled {
            return true;
        }
        let owned = ownership.keys();
        let slots = self.slots(sessions, &owned, wall);
        self.render_slots(io, state, slots.as_deref(), &owned, now)
    }

    /// [`LedRenderer::render`] after slot resolution.
    pub fn render_slots(
        &mut self,
        io: &mut dyn PadIo,
        state: AgentState,
        slots: Option<&[AgentSlot]>,
        owned: &[usize],
        now: f64,
    ) -> bool {
        let model: Model = (
            state,
            slots.map(|s| s.iter().map(|s| (s.path.clone(), s.state())).collect()),
            self.overlay.clone(),
        );
        if self.model.as_ref() != Some(&model) {
            self.model = Some(model);
            self.wake(now);
            self.note_flash_entries(state, slots, now);
        }
        self.refresh_battery(now);
        if self.dimmed || self.locked {
            if !self.dim_asserted {
                let blank = self.look_messages(StateLight::DARK, &self.lighting.clone(), owned);
                self.dim_asserted = self.send(io, &blank, now);
                return self.dim_asserted;
            }
            return true;
        }
        let messages = self.build(state, slots, owned, Some(now));
        let key = dedupe_key(&messages);
        if self.frame.as_ref() != Some(&key) {
            let sent = self.send(io, &messages, now);
            if sent {
                self.frame = Some(key);
            }
            return sent;
        }
        if self.should_dim(state, slots, now) {
            self.dimmed = true;
            self.frame = None;
            let blank = self.look_messages(StateLight::DARK, &self.lighting.clone(), owned);
            self.dim_asserted = self.send(io, &blank, now);
            return self.dim_asserted;
        }
        true
    }

    /// Apply `lighting.on_exit` before the process lets go of the pad.
    pub fn hand_back(&mut self, io: &mut dyn PadIo, ownership: &AgentKeyOwnership) -> bool {
        let lighting = self.lighting.clone();
        self.hand_back_with(io, &lighting, &ownership.keys())
    }

    /// At most once; never fails. Bypasses the write backoff on purpose.
    fn hand_back_with(
        &mut self,
        io: &mut dyn PadIo,
        lighting: &LightingConfig,
        owned: &[usize],
    ) -> bool {
        if self.released || !lighting.enabled {
            return false;
        }
        self.released = true;
        self.frame = None;
        self.dim_asserted = false;
        let exit = match lighting.on_exit.as_str() {
            "leave" => return true,
            "off" => StateLight::DARK,
            _ => {
                let idle = lighting.light_for(AgentState::Idle);
                StateLight {
                    effect: 4,
                    speed: 0.15,
                    magic: None,
                    blink: false,
                    ..idle
                }
            }
        };
        for message in self.look_messages(exit, lighting, owned) {
            if io.send(&message).is_err() {
                break;
            }
        }
        true
    }

    fn wake(&mut self, now: f64) {
        self.activity_at = now;
        if self.dimmed {
            self.dimmed = false;
            self.dim_asserted = false;
            self.frame = None;
        }
    }

    fn should_dim(&self, state: AgentState, slots: Option<&[AgentSlot]>, now: f64) -> bool {
        let lighting = &self.lighting;
        if lighting.auto_dim_seconds <= 0.0 || self.overlay.is_some() {
            return false;
        }
        if now - self.activity_at < lighting.auto_dim_seconds {
            return false;
        }
        let asks_for_you = ALERT_STATES.contains(&state)
            || slots.is_some_and(|slots| {
                slots
                    .iter()
                    .any(|s| s.state().is_some_and(|st| ALERT_STATES.contains(&st)))
            });
        lighting.auto_dim_alerts || !asks_for_you
    }

    fn effective_look(&self, base: StateLight, flash_key: Option<&str>, now: f64) -> StateLight {
        if let Some(until) = flash_key.and_then(|k| self.flash_until.get(k)) {
            if now < *until {
                let elapsed = now - (until - FLASH_SECONDS);
                return if elapsed < FLASH_SECONDS / 2.0 {
                    base
                } else {
                    StateLight::DARK
                };
            }
        }
        if base.blink && !blink_on(base.speed, now) {
            return StateLight::DARK;
        }
        base
    }

    /// Zones painted beyond the state zones: activity-light zones and the
    /// battery cue's zone.
    fn extra_paint_zones(&self) -> Vec<Zone> {
        let mut zones: Vec<Zone> = self
            .activity_zones
            .iter()
            .copied()
            .filter(|z| !self.lighting.drives(*z))
            .collect();
        let battery = &self.lighting.battery;
        if battery.enabled && !self.lighting.drives(battery.zone) && !zones.contains(&battery.zone)
        {
            zones.push(battery.zone);
        }
        zones
    }

    fn note_flash_entries(&mut self, state: AgentState, slots: Option<&[AgentSlot]>, now: f64) {
        let flash_on = &self.lighting.flash_on;
        if !flash_on.is_empty() {
            if self.prev_state.is_some_and(|p| p != state) && flash_on.contains(&state) {
                self.flash_until
                    .insert(STATE_FLASH.into(), now + FLASH_SECONDS);
            }
            for slot in slots.unwrap_or_default() {
                let Some(current) = slot.state() else {
                    continue;
                };
                let was = self.prev_slot_states.get(&slot.path);
                if was.is_some_and(|w| *w != current) && flash_on.contains(&current) {
                    self.flash_until
                        .insert(slot.path.clone(), now + FLASH_SECONDS);
                }
            }
        }
        self.prev_state = Some(state);
        self.prev_slot_states = slots
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.state().map(|st| (s.path.clone(), st)))
            .collect();
    }

    fn refresh_battery(&mut self, now: f64) {
        let battery = &self.lighting.battery;
        if !battery.enabled {
            self.battery_low = false;
            return;
        }
        if self
            .battery_checked_at
            .is_some_and(|last| now - last < battery.poll_seconds)
        {
            return;
        }
        self.battery_checked_at = Some(now);
        let data: Value = std::fs::read_to_string(&self.status_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let charging = match data.get("is_charging") {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            _ => false,
        };
        self.battery_low = !charging
            && data
                .get("battery")
                .and_then(Value::as_f64)
                .is_some_and(|level| level.trunc() <= f64::from(battery.threshold));
    }

    fn slot_entry(&self, slot: &AgentSlot, now: Option<f64>) -> Value {
        let Some(state) = slot.state() else {
            return thread_entry(slot.index, 0, 0, 0.0, 0.0);
        };
        let base = self.lighting.light_for(state);
        let light = match now {
            None => base,
            Some(now) => self.effective_look(base, Some(&slot.path), now),
        };
        thread_entry(
            slot.index,
            light.color,
            light.effect,
            light.brightness,
            light.speed,
        )
    }

    fn zone_message(
        &mut self,
        keys: Option<StateLight>,
        ambient: Option<StateLight>,
        method: &str,
    ) -> Value {
        if method == "rgbcfg" {
            let side = |l: StateLight| rgbcfg_side(l.color, l.effect, l.brightness, l.speed);
            return rgbcfg_message(ambient.map(side), keys.map(side));
        }
        self.request_id += 1;
        let zone = |l: StateLight| preview_zone(l.color, l.effect, l.brightness, l.speed, l.magic);
        preview_message(keys.map(zone), ambient.map(zone), self.request_id)
    }

    /// One look across every zone we drive (blank and exit frames).
    fn look_messages(
        &mut self,
        light: StateLight,
        lighting: &LightingConfig,
        owned: &[usize],
    ) -> Vec<Value> {
        let extra = self.extra_paint_zones();
        let drives = |z: Zone| lighting.drives(z) || extra.contains(&z);
        let mut messages = Vec::new();
        let (keys, ambient) = (drives(Zone::Backlight), drives(Zone::Underglow));
        if keys || ambient {
            messages.push(self.zone_message(
                keys.then_some(light),
                ambient.then_some(light),
                &lighting.method,
            ));
        }
        if drives(Zone::AgentKeys) {
            let entries = agent_entries(light, owned);
            if !entries.is_empty() {
                messages.push(thstatus_message(entries));
            }
        }
        messages
    }

    /// One frame on the wire, or note why it did not go. Never terminal.
    fn send(&mut self, io: &mut dyn PadIo, messages: &[Value], now: f64) -> bool {
        if self.failures > 0 && now < self.retry_at {
            return false;
        }
        for message in messages {
            if let Err(err) = io.send(message) {
                self.note_failure(&err.to_string(), now);
                return false;
            }
        }
        if self.failures > 0 {
            let failures = std::mem::take(&mut self.failures);
            self.retry_at = 0.0;
            let elapsed = (now - self.failed_at).max(0.0);
            let plural = if failures == 1 { "" } else { "s" };
            self.notices.push(format!(
                "writes are going through again after {failures} failure{plural} ({elapsed:.0}s);\nthe current colours were just sent again. This pad acks writes it\nthrows away, so this is not proof the LEDs changed. Glance at them."
            ));
        }
        self.sends += 1;
        self.released = false;
        true
    }

    fn note_failure(&mut self, error: &str, now: f64) {
        self.failures += 1;
        let step = WRITE_RETRY_BACKOFF[(self.failures - 1).min(WRITE_RETRY_BACKOFF.len() - 1)];
        self.retry_at = now + step;
        self.frame = None;
        self.dim_asserted = false;
        if self.failures == 1 {
            self.failed_at = now;
            self.notices.push(format!(
                "write failed: {error}\nThe pad still shows its last colour. Retrying in 1s, 2s, 5s,\nthen every 10s until one lands."
            ));
        }
    }
}

/// One look on every owned Agent Key (a partial `thstatus` for a subset).
fn agent_entries(light: StateLight, owned: &[usize]) -> Vec<Value> {
    owned
        .iter()
        .map(|i| thread_entry(*i, light.color, light.effect, light.brightness, light.speed))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::fake::FakePad;
    use crate::device::DeviceError;
    use crate::state::agentkeys::Project;
    use crate::testutil::TempDir;
    use serde_json::json;

    const ALL: [usize; 6] = [0, 1, 2, 3, 4, 5];

    fn renderer(home: &TempDir, lighting: LightingConfig, zones: Vec<Zone>) -> LedRenderer {
        LedRenderer::from_parts(
            lighting,
            AgentKeysConfig::default(),
            zones,
            DecayPolicy::default(),
            home.path().to_path_buf(),
        )
    }

    fn enabled(zones: Vec<Zone>) -> LightingConfig {
        LightingConfig {
            enabled: true,
            zones,
            ..LightingConfig::default()
        }
    }

    fn slot(index: usize, path: &str, state: Option<AgentState>) -> AgentSlot {
        AgentSlot {
            index,
            path: path.into(),
            project: state.map(|state| Project {
                path: path.into(),
                state,
                last_active: 0.0,
                sessions: Vec::new(),
                label: path.into(),
            }),
            pinned: false,
            owned: true,
        }
    }

    fn six(states: [Option<AgentState>; 6]) -> Vec<AgentSlot> {
        states
            .iter()
            .enumerate()
            .map(|(i, s)| slot(i, &format!("/p{i}"), *s))
            .collect()
    }

    #[test]
    fn render_uses_rgbcfg_for_the_live_zones() {
        let home = TempDir::new();
        let mut leds = renderer(
            &home,
            enabled(vec![Zone::Backlight, Zone::Underglow]),
            vec![],
        );
        let msgs = leds.build(AgentState::Working, None, &ALL, None);
        assert_eq!(
            msgs,
            vec![json!({"m": "v.oai.rgbcfg", "p": {
                "ambient": {"e": 1, "b": 1.0, "s": 0.0, "c": 0x304FFE},
                "keys": {"e": 1, "b": 1.0, "s": 0.0, "c": 0x304FFE},
            }})]
        );
    }

    #[test]
    fn a_fixed_underglow_light_takes_agent_key_5() {
        // On a Creator Micro 2, thstatus entry 5 lights AG05 and the underglow.
        let home = TempDir::new();
        let mut lighting = enabled(vec![Zone::AgentKeys]);
        lighting.underglow = Some(StateLight::solid(0xD97757));
        let mut leds = renderer(&home, lighting, vec![]);
        let slots = six([Some(AgentState::Working); 6]);
        let msgs = leds.build(AgentState::Working, Some(&slots), &ALL, None);
        assert_eq!(msgs.len(), 1, "no rgbcfg: the firmware ignores it");
        let entries = msgs[0]["p"].as_array().unwrap();
        let colors: Vec<_> = entries
            .iter()
            .map(|e| (e["id"].clone(), e["c"].clone()))
            .collect();
        assert_eq!(colors.len(), 6);
        assert_eq!(colors[4], (json!(4), json!(0x304FFE)));
        assert_eq!(colors[5], (json!(5), json!(0xD97757)));
    }

    #[test]
    fn a_locked_mac_keeps_the_pad_dark_until_unlocked() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut pad = FakePad::creator_micro_2();
        let lit = |pad: &FakePad| pad.sent.last().unwrap()["p"][0]["e"].clone();
        leds.render_slots(&mut pad, AgentState::Working, None, &ALL, 0.0);
        assert_eq!(lit(&pad), 1);
        leds.set_locked(true);
        leds.render_slots(&mut pad, AgentState::Working, None, &ALL, 1.0);
        assert_eq!(lit(&pad), 0);
        // A state change while locked does not light it.
        let sent = pad.sent.len();
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 2.0);
        assert_eq!(pad.sent.len(), sent);
        leds.set_locked(false);
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 3.0);
        assert_eq!(lit(&pad), 1);
    }

    #[test]
    fn preview_is_still_selectable_and_its_id_does_not_defeat_dedupe() {
        let home = TempDir::new();
        let mut lighting = enabled(vec![Zone::Underglow]);
        lighting.method = "preview".into();
        let mut leds = renderer(&home, lighting, vec![]);
        let mut pad = FakePad::creator_micro_2();
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.0);
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.5);
        assert_eq!(pad.sent.len(), 1);
        assert_eq!(pad.sent[0]["m"], "lights.preview");
        assert_eq!(pad.sent[0]["p"]["underglow"]["color"], 0xFFFFFF);
    }

    #[test]
    fn slots_color_each_key_and_empty_ones_are_dark() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let slots = six([Some(AgentState::Waiting), None, None, None, None, None]);
        let msgs = leds.build(AgentState::Waiting, Some(&slots), &ALL, None);
        let entries = msgs[0]["p"].as_array().unwrap();
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[0]["c"], 0xFF6D00);
        assert_eq!(
            entries[1],
            json!({"id": 1, "c": 0, "b": 0.0, "e": 0, "s": 0.0})
        );
    }

    #[test]
    fn a_subset_sends_a_partial_thstatus_for_owned_keys_only() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut slots = six([None; 6]);
        for s in &mut slots[..3] {
            s.owned = false;
        }
        let msgs = leds.build(AgentState::Idle, Some(&slots), &[3, 4, 5], None);
        let ids: Vec<_> = msgs[0]["p"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].clone())
            .collect();
        assert_eq!(ids, vec![json!(3), json!(4), json!(5)]);
        let mirrored = leds.build(AgentState::Idle, None, &[3, 4, 5], None);
        assert_eq!(mirrored[0]["p"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn an_overlay_wins_its_zone_and_an_activity_zone_is_otherwise_dark() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![Zone::Underglow]);
        let dark = leds.build(AgentState::Idle, None, &ALL, Some(0.0));
        assert_eq!(dark[0]["p"]["ambient"]["c"], 0);
        assert!(dark[0]["p"].get("keys").is_none());
        leds.set_overlay(Some(ActivityLight {
            light: StateLight {
                effect: 2,
                speed: 0.4,
                ..StateLight::solid(0x2E8B57)
            },
            zones: vec![Zone::Underglow],
            timeout_seconds: 120.0,
        }));
        let lit = leds.build(AgentState::Idle, None, &ALL, Some(0.0));
        assert_eq!(
            lit[0]["p"]["ambient"],
            json!({"e": 2, "b": 1.0, "s": 0.4, "c": 0x2E8B57})
        );
        assert_eq!(lit[1]["p"][0]["c"], 0xFFFFFF);
    }

    #[test]
    fn repeated_renders_do_not_resend() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut pad = FakePad::creator_micro_2();
        for t in [0.0, 1.0, 2.0] {
            leds.render_slots(&mut pad, AgentState::Working, None, &ALL, t);
        }
        assert_eq!(pad.sent.len(), 1);
        leds.invalidate();
        leds.render_slots(&mut pad, AgentState::Working, None, &ALL, 3.0);
        assert_eq!(pad.sent.len(), 2);
    }

    #[test]
    fn a_failed_write_retries_on_the_backoff_ladder_and_says_so_once() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut pad = FakePad::creator_micro_2();
        pad.fail = Some(DeviceError("gone".into()));
        assert!(!leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.0));
        assert!(!leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.5));
        assert!(pad.sent.is_empty());
        let notices = leds.take_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].starts_with("write failed: gone"));
        assert!(leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 1.0));
        assert!(
            leds.take_notices()[0].starts_with("writes are going through again after 1 failure")
        );
    }

    #[test]
    fn the_pad_blanks_after_inactivity_unless_something_asks_for_you() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut pad = FakePad::creator_micro_2();
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.0);
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 179.0);
        assert!(!leds.dimmed());
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 180.0);
        assert!(leds.dimmed());
        assert_eq!(pad.sent.last().unwrap()["p"][0]["e"], 0);
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 181.0);
        assert_eq!(pad.sent.len(), 2);
        leds.note_activity(182.0);
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 182.0);
        assert_eq!(pad.sent.len(), 3);
        assert!(!leds.dimmed());

        let mut alert = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        alert.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 0.0);
        alert.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 500.0);
        assert!(!alert.dimmed());
    }

    #[test]
    fn a_blink_state_toggles_between_colour_and_off() {
        let home = TempDir::new();
        let mut lighting = enabled(vec![Zone::AgentKeys]);
        lighting.theme = "high-contrast".into();
        let mut leds = renderer(&home, lighting, vec![]);
        let on = leds.build(AgentState::Error, None, &ALL, Some(0.1));
        let off = leds.build(AgentState::Error, None, &ALL, Some(0.6));
        assert_eq!(on[0]["p"][0]["c"], 0xD55E00);
        assert_eq!(off[0]["p"][0]["c"], 0);
    }

    #[test]
    fn entering_a_flash_state_pulses_but_first_sight_does_not() {
        let home = TempDir::new();
        let mut leds = renderer(&home, enabled(vec![Zone::AgentKeys]), vec![]);
        let mut pad = FakePad::creator_micro_2();
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 0.0);
        assert_eq!(pad.sent[0]["p"][0]["c"], 0xFF6D00);
        leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 1.0);
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 2.0);
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 2.3);
        leds.render_slots(&mut pad, AgentState::Waiting, None, &ALL, 2.5);
        let colors: Vec<_> = pad.sent.iter().map(|m| m["p"][0]["c"].clone()).collect();
        assert_eq!(
            colors,
            vec![
                json!(0xFF6D00),
                json!(0xFFFFFF),
                json!(0xFF6D00),
                json!(0),
                json!(0xFF6D00)
            ]
        );
    }

    #[test]
    fn a_low_battery_tints_the_underglow_unless_charging() {
        for (status, low) in [
            (json!({"battery": 10, "is_charging": false}), true),
            (json!({"battery": 10, "is_charging": true}), false),
            (json!({"battery": 80}), false),
        ] {
            let home = TempDir::new();
            std::fs::write(home.path().join(STATUS_FILENAME), status.to_string()).unwrap();
            let mut lighting = enabled(vec![Zone::AgentKeys]);
            lighting.battery.enabled = true;
            let mut leds = renderer(&home, lighting, vec![]);
            let mut pad = FakePad::creator_micro_2();
            leds.render_slots(&mut pad, AgentState::Idle, None, &ALL, 0.0);
            let ambient = &pad.sent[0]["p"]["ambient"];
            assert_eq!(ambient["c"] == json!(0xFF6D00), low, "{status}");
        }
    }

    #[test]
    fn hand_back_follows_on_exit_once() {
        for (mode, sent) in [("off", 1), ("leave", 0), ("breath", 1)] {
            let home = TempDir::new();
            let mut lighting = enabled(vec![Zone::AgentKeys]);
            lighting.on_exit = mode.into();
            let mut leds = renderer(&home, lighting.clone(), vec![]);
            let mut pad = FakePad::creator_micro_2();
            leds.hand_back_with(&mut pad, &lighting, &ALL);
            leds.hand_back_with(&mut pad, &lighting, &ALL);
            assert_eq!(pad.sent.len(), sent, "{mode}");
            if mode == "breath" {
                assert_eq!(
                    pad.sent[0]["p"][0],
                    json!({"id": 0, "c": 0xFFFFFF, "b": 1.0, "e": 4, "s": 0.15})
                );
            }
        }
    }
}
