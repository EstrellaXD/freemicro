//! Pad events in, actions out: press/release, chords (settle window), layers,
//! hold caps, latches, double taps, joystick directions and pointer. Port of
//! `input/bridge.py`.
//!
//! No threads. Python's SettleTimer and PointerLoop become
//! [`Bridge::next_deadline`] + [`Bridge::expire`], driven by the run loop's
//! poll timeout on the pad-owner thread.
//!
//! Two keys at once:
//! - A press is suppressed while a `hold` has real modifiers down, unless its
//!   kind is modifier-safe (it would type a shortcut otherwise).
//! - A chord (`"AG00+AG01"`) fires instead of its members. A key in no chord
//!   fires at once; a chord key with no solo binding stands by at no cost; a
//!   chord key with a solo binding waits `chords.settle_ms` for a partner.

use std::collections::BTreeMap;

use serde_json::Value;

use super::actions::{
    cares_about_release, double_tap_combo, is_latching, is_layer, layer_name, perform, py_float,
    py_str, release, Action, ActionEnv, ActionKind, Backend,
};
use super::config::{
    chord_key, Bindings, JoystickConfig, PadConfig, CHORD_SEPARATOR, ENCODER_TICKS,
};
use super::latch::{DoubleTapMachine, LatchMachine, Signal, LATCH_WINDOW_SECONDS};
use super::pointer::PointerEngine;
use crate::device::{EVENT_JOYSTICK, EVENT_KEY};
use crate::runtime::lighting_config::{ActivityLight, DEFAULT_ACTIVITY_TIMEOUT};
use crate::state::agentkeys::AgentKeyOwnership;

/// How long a physical `hold` (or a layer) may stay down before the bridge
/// lets go on its own: the backstop for a lost key-up. Equal to the
/// activity-light timeout so both recover on one clock.
pub const DEFAULT_MAX_HOLD_SECONDS: f64 = DEFAULT_ACTIVITY_TIMEOUT;

/// One decoded pad input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    pub input_id: String,
    pub pressed: bool,
}

/// The outcome of one input firing, enough for a log line.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Dispatch {
    /// A chord id (`"AG00+AG01"`) when a chord fired.
    pub input_id: String,
    pub action: Option<Action>,
    pub ok: bool,
    pub error: String,
    pub suppressed_by: String,
    pub holding: String,
    pub chord: String,
    pub stuck_release: bool,
}

impl Dispatch {
    fn new(input_id: &str, action: Option<Action>) -> Dispatch {
        Dispatch {
            input_id: input_id.to_string(),
            action,
            ok: true,
            ..Dispatch::default()
        }
    }
    fn failed(input_id: &str, action: Action, error: String) -> Dispatch {
        Dispatch {
            ok: false,
            error,
            ..Dispatch::new(input_id, Some(action))
        }
    }
    fn stuck(input_id: &str, action: Action) -> Dispatch {
        Dispatch {
            stuck_release: true,
            ..Dispatch::new(input_id, Some(action))
        }
    }
    pub fn bound(&self) -> bool {
        self.action.is_some()
    }
    pub fn suppressed(&self) -> bool {
        !self.suppressed_by.is_empty()
    }
    /// One log line (Python `_print_dispatch`).
    pub fn describe(&self) -> String {
        if self.stuck_release {
            return format!(
                "released a stuck hold on {} - its key-up was lost",
                self.input_id
            );
        }
        let Some(action) = &self.action else {
            if !self.chord.is_empty() {
                return format!("chord key - held, ready for {}", self.chord);
            }
            return "unmapped".to_string();
        };
        let summary = format!("{}: {}", action.label, action.describe());
        if self.suppressed() {
            let held = if self.holding.is_empty() {
                " held down".to_string()
            } else {
                format!(" holding {}", self.holding)
            };
            return format!("{summary}  [NOT SENT - {} is{held}]", self.suppressed_by);
        }
        summary
    }
}

/// An activity-light change for the LED overlay: `Some(light)` on press,
/// `None` on release (Python `on_activity(input_id, light)`).
pub type ActivityNote = (String, Option<ActivityLight>);

/// Direction-mode joystick: which `JOY_*` is held.
#[derive(Debug, Clone)]
pub struct JoystickTracker {
    pub config: JoystickConfig,
    armed: bool,
}

impl JoystickTracker {
    /// Fraction of the deadzone the stick must fall under to re-arm.
    pub const REARM_RATIO: f64 = 0.75;

    pub fn new(config: JoystickConfig) -> JoystickTracker {
        JoystickTracker {
            config,
            armed: true,
        }
    }

    /// The direction newly entered, if any (rising edge, with hysteresis).
    pub fn update(&mut self, angle: f64, distance: f64) -> Option<String> {
        if distance < self.config.deadzone * Self::REARM_RATIO {
            self.armed = true;
            return None;
        }
        if distance >= self.config.deadzone && self.armed {
            self.armed = false;
            return Some(self.config.direction_for(angle));
        }
        None
    }
}

/// Insertion-ordered string map (Python dict order matters here: the
/// first hold blocks, the newest layer and undecided key win).
#[derive(Debug, Clone)]
struct Ordered<T>(Vec<(String, T)>);

impl<T> Default for Ordered<T> {
    fn default() -> Self {
        Ordered(Vec::new())
    }
}

impl<T> Ordered<T> {
    fn get(&self, key: &str) -> Option<&T> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn get_mut(&mut self, key: &str) -> Option<&mut T> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    fn insert(&mut self, key: &str, value: T) {
        match self.get_mut(key) {
            Some(slot) => *slot = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    fn remove(&mut self, key: &str) -> Option<T> {
        let at = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(at).1)
    }
    fn keys(&self) -> Vec<String> {
        self.0.iter().map(|(k, _)| k.clone()).collect()
    }
    fn values(&self) -> impl Iterator<Item = &T> {
        self.0.iter().map(|(_, v)| v)
    }
    fn clear(&mut self) {
        self.0.clear();
    }
}

/// One latching MIC key and the binding driving it.
#[derive(Debug, Clone)]
struct Latch {
    machine: LatchMachine,
    action: Action,
}

/// One held layer trigger.
#[derive(Debug, Clone)]
struct LayerState {
    name: String,
    action: Action,
    started_at: f64,
}

/// A chord-capable key that is down and undecided. No `deadline` = no solo
/// binding, so it waits for nothing.
#[derive(Debug, Clone)]
struct Unresolved {
    action: Option<Action>,
    deadline: Option<f64>,
}

fn chord_label(members: &[String]) -> String {
    chord_key(members).join(CHORD_SEPARATOR)
}

pub struct Bridge {
    config: PadConfig,
    backend: Box<dyn Backend>,
    env: ActionEnv,
    frontmost: Option<String>,
    joystick: JoystickTracker,
    pointer: PointerEngine,
    activity: Vec<ActivityNote>,
    /// The last `now` seen, for paths that take no time argument.
    now: f64,
    max_hold_seconds: f64,
    chord_keys: Vec<String>,
    /// input id -> the `hold` physically down, and when it went down.
    holding: Ordered<(Action, f64)>,
    doubletap: Ordered<DoubleTapMachine>,
    lit: Ordered<()>,
    /// Presses refused; their releases are refused to match.
    suppressed: Ordered<()>,
    unresolved: Ordered<Unresolved>,
    /// member -> the chord that consumed its press.
    chorded: Ordered<Vec<String>>,
    /// Chords fired and not yet released.
    open_chords: BTreeMap<Vec<String>, Action>,
    /// Dispatches produced off the return path (recoveries).
    deferred: Vec<Dispatch>,
    latch: Ordered<Latch>,
    latch_refresh_at: Option<f64>,
    overrides: Bindings,
    /// The action a release-sensitive press delivered, and when; its
    /// release replays it, so a profile hold let go in another app still
    /// sends its key-up.
    pressed: Ordered<(Action, f64)>,
    layers: Ordered<LayerState>,
}

impl Bridge {
    pub fn new(config: PadConfig, backend: Box<dyn Backend>, env: ActionEnv) -> Bridge {
        let joystick = JoystickTracker::new(config.joystick.clone());
        let pointer = PointerEngine::new(config.joystick.clone());
        let mut bridge = Bridge {
            chord_keys: Vec::new(),
            config,
            backend,
            env,
            frontmost: None,
            joystick,
            pointer,
            activity: Vec::new(),
            now: 0.0,
            max_hold_seconds: DEFAULT_MAX_HOLD_SECONDS,
            holding: Ordered::default(),
            doubletap: Ordered::default(),
            lit: Ordered::default(),
            suppressed: Ordered::default(),
            unresolved: Ordered::default(),
            chorded: Ordered::default(),
            open_chords: BTreeMap::new(),
            deferred: Vec::new(),
            latch: Ordered::default(),
            latch_refresh_at: None,
            overrides: Bindings::new(),
            pressed: Ordered::default(),
            layers: Ordered::default(),
        };
        bridge.index_chords();
        bridge
    }

    fn index_chords(&mut self) {
        let mut keys: Vec<String> = self.config.chords.keys().flatten().cloned().collect();
        keys.sort();
        keys.dedup();
        self.chord_keys = keys;
    }

    pub fn config(&self) -> &PadConfig {
        &self.config
    }

    /// Adopt a reloaded config (bindings, joystick, pointer tuning). Every
    /// half-made decision was made against the old file and is dropped; a
    /// latch gets its toggle-off and a hold its key-up first.
    pub fn set_config(&mut self, config: PadConfig) {
        self.stop_latches("config reloaded", self.now);
        self.config = config;
        self.index_chords();
        self.unresolved.clear();
        self.chorded.clear();
        self.open_chords.clear();
        self.overrides = self.config.resolve_profile(self.frontmost.as_deref());
        self.pressed.clear();
        self.doubletap.clear();
        self.layers.clear();
        self.joystick = JoystickTracker::new(self.config.joystick.clone());
        self.pointer.configure(self.config.joystick.clone());
        if !self.config.joystick.pointing() {
            self.pointer.reset();
        }
        self.release_held_keys();
    }

    pub fn backend(&mut self) -> &mut dyn Backend {
        self.backend.as_mut()
    }

    /// The cached frontmost app name profiles resolve against. True if changed.
    pub fn set_frontmost(&mut self, app: Option<String>) -> bool {
        if app == self.frontmost {
            return false;
        }
        self.overrides = self.config.resolve_profile(app.as_deref());
        self.frontmost = app;
        true
    }

    pub fn frontmost(&self) -> Option<&str> {
        self.frontmost.as_deref()
    }

    /// Layer > profile > base.
    fn resolve(&self, input_id: &str) -> Option<Action> {
        for state in self.layers.values().collect::<Vec<_>>().into_iter().rev() {
            if let Some(action) = self
                .config
                .layers
                .get(&state.name)
                .and_then(|l| l.get(input_id))
            {
                return Some(action.clone());
            }
        }
        self.overrides
            .get(input_id)
            .or_else(|| self.config.bindings.get(input_id))
            .cloned()
    }

    /// `v.oai.hid`, `v.oai.rad` -> inputs (`act` 0 = release, 1 = press;
    /// encoder ticks fire on any `act`). `kb.radial` is ignored: the firmware
    /// runs that layer's radial keycodes itself.
    pub fn decode(&mut self, message: &Value, now: f64) -> Vec<InputEvent> {
        let method = message
            .get("m")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .or_else(|| message.get("method").and_then(Value::as_str))
            .unwrap_or("");
        let params = match message.get("p") {
            Some(p) if !p.is_null() => Some(p),
            _ => message.get("params"),
        };
        let Some(params) = params.and_then(Value::as_object) else {
            return vec![];
        };
        let press = |id: &str, pressed: bool| {
            vec![InputEvent {
                input_id: id.to_string(),
                pressed,
            }]
        };
        if method == EVENT_KEY {
            let key = match params.get("k") {
                Some(Value::String(k)) if !k.is_empty() => k.as_str(),
                _ => return vec![],
            };
            let act = params.get("act").and_then(Value::as_f64);
            let joystick = &self.config.joystick;
            if joystick.pointing() && key == joystick.precision_key {
                // Consumed: it slows the cursor while held.
                self.pointer.set_precision(act == Some(1.0));
                return vec![];
            }
            if ENCODER_TICKS.contains(&key) {
                // Detents are momentary and arrive with act 1 or 2.
                return press(key, true);
            }
            return match act {
                Some(1.0) => press(key, true),
                Some(0.0) => press(key, false),
                _ => vec![],
            };
        }
        if method == EVENT_JOYSTICK {
            let number = |name| match params.get(name) {
                None => Some(0.0),
                Some(v) => py_float(v),
            };
            let (Some(angle), Some(distance)) = (number("a"), number("d")) else {
                return vec![];
            };
            if self.config.joystick.pointing() {
                // Motion happens on the pointer's tick (see `expire`).
                self.pointer.update(angle, distance, now);
                let button = self.config.joystick.tap_click_button.clone();
                for _ in 0..self.pointer.take_tap() {
                    let _ = self.backend.click_mouse(&button, 1);
                }
                return vec![];
            }
            return match self.joystick.update(angle, distance) {
                Some(direction) => press(&direction, true),
                None => vec![],
            };
        }
        vec![]
    }

    /// Prepend recoveries emitted meanwhile, as Python's `on_dispatch` would
    /// have printed them first.
    fn with_deferred(&mut self, own: Vec<Dispatch>) -> Vec<Dispatch> {
        let mut out = std::mem::take(&mut self.deferred);
        out.extend(own);
        out
    }

    /// Handle one pad message. Agent Keys `ownership` leaves to Codex are
    /// ignored.
    pub fn handle(
        &mut self,
        message: &Value,
        ownership: &AgentKeyOwnership,
        now: f64,
    ) -> Vec<Dispatch> {
        self.now = now;
        let mut out = std::mem::take(&mut self.deferred);
        for event in self.decode(message, now) {
            let own = if event.pressed {
                self.press_inner(&event.input_id, ownership)
            } else {
                self.release_inner(&event.input_id, ownership)
            };
            out.extend(self.with_deferred(own));
        }
        out
    }

    pub fn press(
        &mut self,
        input_id: &str,
        ownership: &AgentKeyOwnership,
        now: f64,
    ) -> Vec<Dispatch> {
        self.now = now;
        let own = self.press_inner(input_id, ownership);
        self.with_deferred(own)
    }

    pub fn release(
        &mut self,
        input_id: &str,
        ownership: &AgentKeyOwnership,
        now: f64,
    ) -> Vec<Dispatch> {
        self.now = now;
        let own = self.release_inner(input_id, ownership);
        self.with_deferred(own)
    }

    fn press_inner(&mut self, input_id: &str, ownership: &AgentKeyOwnership) -> Vec<Dispatch> {
        if ownership.leaves_to_codex(input_id) {
            // Codex's key: inert for chords, layers and profiles too.
            return vec![];
        }
        if !self.chord_keys.iter().any(|k| k == input_id) {
            let action = self.resolve(input_id);
            return self.run(input_id, action, true).into_iter().collect();
        }
        let action = self.resolve(input_id);
        // `none` declares a pure chord partner: nothing to hold back.
        let solo = action.clone().filter(|a| a.kind != ActionKind::None);
        let settle = self.config.chord_settle_ms / 1000.0;

        // A key going down ends whatever its last press resolved to (its
        // key-up may have been lost).
        self.chorded.remove(input_id);
        if let Some(partner) = self.find_partner(input_id) {
            let members = chord_key(&[partner.clone(), input_id.to_string()]);
            self.unresolved.remove(&partner);
            self.chorded.insert(&partner, members.clone());
            self.chorded.insert(input_id, members.clone());
            let chord_action = self.config.chords[&members].clone();
            self.open_chords
                .insert(members.clone(), chord_action.clone());
            return self
                .run(&chord_label(&members), Some(chord_action), true)
                .into_iter()
                .collect();
        }
        if solo.is_none() {
            self.unresolved.insert(
                input_id,
                Unresolved {
                    action: None,
                    deadline: None,
                },
            );
            if action.is_none() {
                let chords: Vec<String> = self
                    .config
                    .chord_partners(input_id)
                    .into_iter()
                    .map(|p| chord_label(&[input_id.to_string(), p]))
                    .collect();
                return vec![Dispatch {
                    chord: chords.join(", "),
                    ..Dispatch::new(input_id, None)
                }];
            }
            return self.run(input_id, action, true).into_iter().collect();
        }
        if settle <= 0.0 {
            // Deferring is off: this key can only be a chord's second half.
            return self.run(input_id, solo, true).into_iter().collect();
        }
        self.unresolved.insert(
            input_id,
            Unresolved {
                action: solo,
                deadline: Some(self.now + settle),
            },
        );
        vec![]
    }

    fn release_inner(&mut self, input_id: &str, ownership: &AgentKeyOwnership) -> Vec<Dispatch> {
        if ownership.leaves_to_codex(input_id) {
            return vec![];
        }
        let members = self.chorded.remove(input_id);
        let pending = self.unresolved.remove(input_id);
        if let Some(members) = members {
            // The other member's key-up finds no open chord: one release only.
            return match self.open_chords.remove(&members) {
                Some(action) => self
                    .run(&chord_label(&members), Some(action), false)
                    .into_iter()
                    .collect(),
                None => vec![],
            };
        }
        if let Some(action) = pending.and_then(|p| p.action) {
            // Tapped inside the settle window: the key meant itself.
            let mut out: Vec<Dispatch> = self
                .run(input_id, Some(action.clone()), true)
                .into_iter()
                .collect();
            out.extend(self.run(input_id, Some(action), false));
            return out;
        }
        let action = self.resolve(input_id);
        self.run(input_id, action, false).into_iter().collect()
    }

    /// An undecided key forming a bound chord with `input_id`, newest first.
    fn find_partner(&self, input_id: &str) -> Option<String> {
        self.unresolved.keys().into_iter().rev().find(|other| {
            self.config
                .chords
                .contains_key(&chord_key(&[other.clone(), input_id.to_string()]))
        })
    }

    fn light(&mut self, input_id: &str, light: Option<ActivityLight>) {
        self.activity.push((input_id.to_string(), light));
    }

    fn end_light(&mut self, input_id: &str) {
        if self.lit.remove(input_id).is_some() {
            self.light(input_id, None);
        }
    }

    fn release_lights(&mut self) -> usize {
        let lit = self.lit.keys();
        self.lit.clear();
        for input_id in &lit {
            self.light(input_id, None);
        }
        lit.len()
    }

    /// Deliver one resolved binding. `input_id` may be a chord id.
    fn run(&mut self, input_id: &str, action: Option<Action>, pressed: bool) -> Option<Dispatch> {
        // A layer trigger is a pure modal switch, routed before everything.
        if pressed {
            if let Some(layer) = action.as_ref().filter(|a| is_layer(a)) {
                return Some(self.activate_layer(input_id, layer.clone()));
            }
        } else if let Some(state) = self.layers.remove(input_id) {
            self.end_light(input_id);
            return Some(Dispatch::new(input_id, Some(state.action)));
        }
        let mut action = action;
        let mut held_for = None;
        if pressed {
            if let Some(a) = action.as_ref().filter(|a| cares_about_release(&a.kind)) {
                self.pressed.insert(input_id, (a.clone(), self.now));
            }
        } else if let Some((latched, since)) = self.pressed.remove(input_id) {
            held_for = Some(self.now - since);
            action = Some(latched);
        }
        if let Some(latching) = action.as_ref().filter(|a| is_latching(a)) {
            return self.run_latch(input_id, latching.clone(), pressed);
        }
        if !pressed {
            self.end_light(input_id);
            if self.suppressed.remove(input_id).is_some() {
                // Never sent, so its key-up is not sent either.
                return None;
            }
            self.holding.remove(input_id);
            let now = self.now;
            if let Some(machine) = self.doubletap.get_mut(input_id) {
                machine.release(now);
            }
            let action = action.filter(|a| cares_about_release(&a.kind))?;
            return Some(
                match release(&action, self.backend.as_mut(), &self.env, held_for) {
                    Ok(()) => Dispatch::new(input_id, Some(action)),
                    Err(e) => Dispatch::failed(input_id, action, e.0),
                },
            );
        }
        let Some(action) = action else {
            return Some(Dispatch::new(input_id, None));
        };
        // Two downs with no up between: the first key-up was lost.
        self.reconcile_stale_hold(input_id);
        if let Some((by, holding)) = self.blocking_hold(&action) {
            self.suppressed.insert(input_id, ());
            return Some(Dispatch {
                suppressed_by: by,
                holding,
                ..Dispatch::new(input_id, Some(action))
            });
        }
        if action.kind == ActionKind::Hold {
            // Registered before delivery, so a half-failed hold is released.
            self.holding.insert(input_id, (action.clone(), self.now));
        }
        // Before the hold goes down, so the second shortcut has clean modifiers.
        self.maybe_double_tap(input_id, &action);
        if let Some(light) = &action.light {
            self.lit.insert(input_id, ());
            self.light(input_id, Some(light.clone()));
        }
        match perform(&action, self.backend.as_mut(), &self.env) {
            Ok(()) => Some(Dispatch::new(input_id, Some(action))),
            Err(e) => {
                self.holding.remove(input_id);
                self.end_light(input_id);
                Some(Dispatch::failed(input_id, action, e.0))
            }
        }
    }

    /// The held binding that must stop `action`, as (input id, key).
    fn blocking_hold(&self, action: &Action) -> Option<(String, String)> {
        if action.kind.modifier_safe() {
            return None;
        }
        self.holding.0.first().map(|(id, (held, _))| {
            (
                id.clone(),
                held.params.get("key").map(py_str).unwrap_or_default(),
            )
        })
    }

    fn force_release_hold(&mut self, input_id: &str, action: &Action) {
        self.holding.remove(input_id);
        self.doubletap.remove(input_id);
        // This is the release: a later real key-up has nothing to replay.
        self.pressed.remove(input_id);
        let _ = release(action, self.backend.as_mut(), &self.env, None);
        self.end_light(input_id);
    }

    fn reconcile_stale_hold(&mut self, input_id: &str) {
        let Some((stale, _)) = self.holding.get(input_id).cloned() else {
            return;
        };
        self.force_release_hold(input_id, &stale);
        self.deferred.push(Dispatch::stuck(input_id, stale));
    }

    fn hold_deadline(&self) -> Option<f64> {
        self.holding
            .values()
            .map(|(_, started)| *started)
            .chain(self.layers.values().map(|s| s.started_at))
            .reduce(f64::min)
            .map(|earliest| earliest + self.max_hold_seconds)
    }

    /// Release every hold and layer past the cap; each recovery is logged.
    fn hold_expire(&mut self, now: f64) {
        let max = self.max_hold_seconds;
        let due: Vec<(String, Action)> = self
            .holding
            .0
            .iter()
            .filter(|(_, (_, started))| now - started >= max)
            .map(|(id, (action, _))| (id.clone(), action.clone()))
            .collect();
        for (input_id, action) in due {
            self.force_release_hold(&input_id, &action);
            self.deferred.push(Dispatch::stuck(&input_id, action));
        }
        let due_layers: Vec<String> = self
            .layers
            .0
            .iter()
            .filter(|(_, s)| now - s.started_at >= max)
            .map(|(id, _)| id.clone())
            .collect();
        for input_id in due_layers {
            if let Some(state) = self.layers.remove(&input_id) {
                self.end_light(&input_id);
                self.deferred.push(Dispatch::stuck(&input_id, state.action));
            }
        }
    }

    /// A layer is on while its trigger is held; a repeated key-down replaces
    /// the stale entry (its key-up was lost) and says so.
    fn activate_layer(&mut self, input_id: &str, action: Action) -> Dispatch {
        let stale = self.layers.remove(input_id);
        self.layers.insert(
            input_id,
            LayerState {
                name: layer_name(&action),
                action: action.clone(),
                started_at: self.now,
            },
        );
        if let Some(light) = &action.light {
            self.lit.insert(input_id, ());
            self.light(input_id, Some(light.clone()));
        }
        if let Some(stale) = stale {
            self.deferred.push(Dispatch::stuck(input_id, stale.action));
        }
        Dispatch::new(input_id, Some(action))
    }

    fn maybe_double_tap(&mut self, input_id: &str, action: &Action) {
        let Some(combo) = double_tap_combo(action) else {
            return;
        };
        if self.doubletap.get(input_id).is_none() {
            self.doubletap
                .insert(input_id, DoubleTapMachine::new(LATCH_WINDOW_SECONDS));
        }
        let now = self.now;
        let fired = self
            .doubletap
            .get_mut(input_id)
            .is_some_and(|m| m.press(now).contains(&Signal::Fire));
        if fired {
            // A missed second tap must not derail the hold that follows.
            let _ = self.backend.press_key(&combo);
        }
    }

    fn run_latch(&mut self, input_id: &str, action: Action, pressed: bool) -> Option<Dispatch> {
        match self.latch.get_mut(input_id) {
            Some(entry) => entry.action = action.clone(),
            None => self.latch.insert(
                input_id,
                Latch {
                    machine: LatchMachine::new(LATCH_WINDOW_SECONDS),
                    action: action.clone(),
                },
            ),
        }
        if pressed {
            if let Some((by, holding)) = self.blocking_hold(&action) {
                // A tap now would come out as a shortcut; the machine waits.
                self.suppressed.insert(input_id, ());
                return Some(Dispatch {
                    suppressed_by: by,
                    holding,
                    ..Dispatch::new(input_id, Some(action))
                });
            }
        } else if self.suppressed.remove(input_id).is_some() {
            return None;
        }
        let now = self.now;
        let machine = &mut self.latch.get_mut(input_id)?.machine;
        let emits = if pressed {
            machine.press(now)
        } else {
            machine.release(now)
        };
        let dispatch = self.apply_latch(input_id, &action, &emits);
        self.reschedule_latch(now);
        Some(dispatch)
    }

    /// Each Start/Stop is one tap of the toggle shortcut plus a light change.
    fn apply_latch(&mut self, input_id: &str, action: &Action, emits: &[Signal]) -> Dispatch {
        let mut dispatch = Dispatch::new(input_id, Some(action.clone()));
        let key = action.params.get("key").map(py_str).unwrap_or_default();
        for emit in emits {
            if *emit == Signal::Start {
                if let Some(light) = &action.light {
                    self.lit.insert(input_id, ());
                    self.light(input_id, Some(light.clone()));
                }
            }
            if let Err(e) = self.backend.press_key(&key) {
                dispatch = Dispatch::failed(input_id, action.clone(), e.0);
                if *emit == Signal::Start {
                    self.end_light(input_id);
                }
            }
            if *emit == Signal::Stop {
                self.end_light(input_id);
            }
        }
        dispatch
    }

    /// Half the light's timeout, so the overlay never times out a live latch.
    fn latch_refresh_interval(&self) -> Option<f64> {
        self.latch
            .values()
            .filter(|e| e.machine.recording())
            .filter_map(|e| e.action.light.as_ref())
            .map(|light| {
                if light.timeout_seconds > 0.0 {
                    (light.timeout_seconds * 0.5).max(1.0)
                } else {
                    30.0
                }
            })
            .reduce(f64::min)
    }

    fn reschedule_latch(&mut self, now: f64) {
        match self.latch_refresh_interval() {
            None => self.latch_refresh_at = None,
            Some(interval) if self.latch_refresh_at.is_none() => {
                self.latch_refresh_at = Some(now + interval);
            }
            Some(_) => {}
        }
    }

    fn latch_deadline(&self) -> Option<f64> {
        self.latch
            .values()
            .filter_map(|e| e.machine.deadline())
            .chain(self.latch_refresh_at)
            .reduce(f64::min)
    }

    fn latch_expire(&mut self, now: f64) -> Vec<Dispatch> {
        let refresh_due = self.latch_refresh_at.is_some_and(|at| now >= at);
        let mut produced = Vec::new();
        for input_id in self.latch.keys() {
            let Some(entry) = self.latch.get_mut(&input_id) else {
                continue;
            };
            if !entry.machine.deadline().is_some_and(|d| now >= d) {
                continue;
            }
            let emits = entry.machine.tick(now);
            let action = entry.action.clone();
            let dispatch = self.apply_latch(&input_id, &action, &emits);
            if !emits.is_empty() {
                produced.push(dispatch);
            }
        }
        if refresh_due {
            self.latch_refresh_at = None;
            let live: Vec<(String, ActivityLight)> = self
                .latch
                .0
                .iter()
                .filter(|(_, e)| e.machine.recording())
                .filter_map(|(id, e)| e.action.light.clone().map(|l| (id.clone(), l)))
                .collect();
            for (input_id, light) in live {
                self.lit.insert(&input_id, ());
                self.light(&input_id, Some(light));
            }
        }
        self.reschedule_latch(now);
        produced
    }

    /// Earliest monotonic time [`Bridge::expire`] has work: chord settle,
    /// hold cap, latch window, double-tap window, pointer tick.
    pub fn next_deadline(&self) -> Option<f64> {
        let pointer = if self.config.joystick.pointing() {
            self.pointer.deadline(self.now)
        } else {
            None
        };
        self.unresolved
            .values()
            .filter_map(|u| u.deadline)
            .chain(self.latch_deadline())
            .chain(self.hold_deadline())
            .chain(pointer)
            .reduce(f64::min)
    }

    /// Run whatever is due at `now`.
    pub fn expire(&mut self, _ownership: &AgentKeyOwnership, now: f64) -> Vec<Dispatch> {
        self.now = now;
        let mut own = Vec::new();
        // Settle windows that ran out: the solo binding fires.
        let due: Vec<String> = self
            .unresolved
            .0
            .iter()
            .filter(|(_, u)| u.deadline.is_some_and(|d| d <= now))
            .map(|(id, _)| id.clone())
            .collect();
        for input_id in due {
            if let Some(pending) = self.unresolved.remove(&input_id) {
                own.extend(self.run(&input_id, pending.action, true));
            }
        }
        own.extend(self.latch_expire(now));
        self.hold_expire(now);
        if self.config.joystick.pointing() && self.pointer.deadline(now).is_some_and(|d| d <= now) {
            let (dx, dy) = self.pointer.tick(now);
            if (dx != 0 || dy != 0)
                && self
                    .backend
                    .move_mouse(f64::from(dx), f64::from(dy), true)
                    .is_err()
            {
                // A backend that cannot move the mouse stops pointing.
                self.pointer.reset();
            }
        }
        self.with_deferred(own)
    }

    /// Activity-light changes since the last call, in order.
    pub fn drain_activity(&mut self) -> Vec<ActivityNote> {
        std::mem::take(&mut self.activity)
    }

    /// Send the toggle-off for every latched push-to-talk (pad dropped).
    pub fn stop_latches(&mut self, _reason: &str, now: f64) -> Vec<Dispatch> {
        let mut produced = Vec::new();
        for input_id in self.latch.keys() {
            let Some(entry) = self.latch.get_mut(&input_id) else {
                continue;
            };
            let emits = entry.machine.force_stop(now);
            let action = entry.action.clone();
            let dispatch = self.apply_latch(&input_id, &action, &emits);
            if !emits.is_empty() {
                produced.push(dispatch);
            }
        }
        self.latch.clear();
        self.latch_refresh_at = None;
        produced
    }

    /// Let go of every hold, layer, latch and light. Never fails.
    pub fn release_held_keys(&mut self) -> usize {
        self.stop_latches("released", self.now);
        self.release_lights();
        self.holding.clear();
        self.suppressed.clear();
        self.pressed.clear();
        self.layers.clear();
        self.backend.release_held_keys()
    }

    /// Release everything held; called on every exit path. A press still in
    /// its settle window is dropped, not typed on the way out.
    pub fn close(&mut self) {
        self.unresolved.clear();
        self.chorded.clear();
        self.open_chords.clear();
        self.doubletap.clear();
        self.pressed.clear();
        self.layers.clear();
        self.deferred.clear();
        self.release_held_keys();
        self.pointer.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::actions::{tests::env, ActionError, RecordingBackend};
    use crate::input::config::parse;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A recording backend the test keeps a handle to; `fail` makes key
    /// deliveries fail.
    #[derive(Default)]
    struct Probe {
        rec: RecordingBackend,
        fail: bool,
    }

    struct Shared(Rc<RefCell<Probe>>);

    impl Shared {
        fn check(&self) -> Result<(), ActionError> {
            if self.0.borrow().fail {
                Err(ActionError("not allowed".into()))
            } else {
                Ok(())
            }
        }
    }

    impl Backend for Shared {
        fn description(&self) -> &str {
            "shared"
        }
        fn type_text(&mut self, text: &str) -> Result<(), ActionError> {
            self.check()?;
            self.0.borrow_mut().rec.type_text(text)
        }
        fn press_key(&mut self, combo: &str) -> Result<(), ActionError> {
            self.check()?;
            self.0.borrow_mut().rec.press_key(combo)
        }
        fn hold_key(&mut self, combo: &str, down: bool) -> Result<(), ActionError> {
            self.check()?;
            self.0.borrow_mut().rec.hold_key(combo, down)
        }
        fn run_shell(&mut self, c: &str, cwd: Option<&str>, w: bool) -> Result<(), ActionError> {
            self.0.borrow_mut().rec.run_shell(c, cwd, w)
        }
        fn run_applescript(&mut self, s: &str) -> Result<(), ActionError> {
            self.0.borrow_mut().rec.run_applescript(s)
        }
        fn activate_app(&mut self, n: &str, c: bool) -> Result<(), ActionError> {
            self.0.borrow_mut().rec.activate_app(n, c)
        }
        fn move_mouse(&mut self, x: f64, y: f64, r: bool) -> Result<(), ActionError> {
            self.0.borrow_mut().rec.move_mouse(x, y, r)
        }
        fn click_mouse(&mut self, b: &str, c: u32) -> Result<(), ActionError> {
            self.0.borrow_mut().rec.click_mouse(b, c)
        }
        fn release_held_keys(&mut self) -> usize {
            self.0.borrow_mut().rec.release_held_keys()
        }
        fn settle(&mut self, s: f64) {
            self.0.borrow_mut().rec.settle(s)
        }
    }

    struct Rig {
        bridge: Bridge,
        probe: Rc<RefCell<Probe>>,
        owner: AgentKeyOwnership,
    }

    impl Rig {
        fn new(doc: Value) -> Rig {
            let config = parse(&doc, None).unwrap_or_else(|e| panic!("{e}"));
            let probe = Rc::new(RefCell::new(Probe::default()));
            let owner = AgentKeyOwnership::new(config.agent_keys.clone(), Box::new(|| true));
            let bridge = Bridge::new(config, Box::new(Shared(probe.clone())), env());
            Rig {
                bridge,
                probe,
                owner,
            }
        }
        fn bindings(bindings: Value) -> Rig {
            Rig::new(json!({"bindings": bindings}))
        }
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut self.probe.borrow_mut().rec.calls)
        }
        fn key(&mut self, k: &str, act: u8, now: f64) -> Vec<Dispatch> {
            let msg = json!({"m": "v.oai.hid", "p": {"k": k, "act": act}});
            self.bridge.handle(&msg, &self.owner, now)
        }
        fn expire(&mut self, now: f64) -> Vec<Dispatch> {
            self.bridge.expire(&self.owner, now)
        }
    }

    fn ids(dispatches: &[Dispatch]) -> Vec<String> {
        dispatches.iter().map(|d| d.input_id.clone()).collect()
    }

    const HOLD: &str = "ctrl+cmd+o";

    #[test]
    fn key_down_fires_and_key_up_does_nothing() {
        let mut rig = Rig::bindings(json!({"ACT06": {"action": "key", "key": "shift-tab"}}));
        let out = rig.key("ACT06", 1, 0.0);
        assert_eq!(out.len(), 1);
        assert!(out[0].ok && out[0].bound());
        assert_eq!(rig.key("ACT06", 0, 0.1), vec![]);
        assert_eq!(rig.calls(), vec!["press shift-tab"]);
        let unmapped = rig.key("ACT09", 1, 0.2);
        assert_eq!(unmapped[0].describe(), "unmapped");
    }

    #[test]
    fn decode_accepts_both_spellings_and_ignores_noise() {
        let mut rig = Rig::bindings(json!({"ACT06": "x"}));
        let cases = [
            (
                json!({"method": "v.oai.hid", "params": {"k": "ACT06", "act": 1}}),
                1,
            ),
            (json!({"m": "v.oai.hid", "p": {"k": "ACT06", "act": 2}}), 0),
            (json!({"m": "v.oai.hid", "p": {"k": "", "act": 1}}), 0),
            (json!({"m": "v.oai.hid", "p": "junk"}), 0),
            (
                json!({"result": {"ok": 1}, "id": null, "method": "v.oai.thstatus"}),
                0,
            ),
            (json!({"m": "v.oai.hid", "p": {"k": "ENC_CW", "act": 2}}), 1),
            (json!({"m": "v.oai.hid", "p": {"k": "ENC_CC", "act": 0}}), 1),
        ];
        for (msg, want) in cases {
            assert_eq!(rig.bridge.decode(&msg, 0.0).len(), want, "{msg}");
        }
    }

    #[test]
    fn a_failing_action_is_reported_not_raised() {
        let mut rig = Rig::bindings(json!({"ACT06": {"action": "key", "key": "escape"}}));
        rig.probe.borrow_mut().fail = true;
        let out = rig.key("ACT06", 1, 0.0);
        assert!(!out[0].ok);
        assert_eq!(out[0].error, "not allowed");
    }

    #[test]
    fn a_hold_suppresses_typing_kinds_until_released() {
        let mut rig = Rig::bindings(json!({
            "ACT10": {"action": "hold", "key": HOLD},
            "ACT06": "continue",
            "ACT07": {"action": "app", "name": "Mail"},
        }));
        rig.key("ACT10", 1, 0.0);
        let typed = rig.key("ACT06", 1, 0.1);
        assert_eq!(typed[0].suppressed_by, "ACT10");
        assert!(typed[0]
            .describe()
            .contains("[NOT SENT - ACT10 is holding ctrl+cmd+o]"));
        assert_eq!(
            rig.key("ACT06", 0, 0.2),
            vec![],
            "suppressed release swallowed"
        );
        assert!(
            !rig.key("ACT07", 1, 0.3)[0].suppressed(),
            "modifier-safe kind"
        );
        rig.key("ACT10", 0, 0.4);
        assert!(!rig.key("ACT06", 1, 0.5)[0].suppressed());
        assert_eq!(
            rig.calls(),
            vec![
                "hold ctrl+cmd+o down",
                "app Mail cycle=false",
                "hold ctrl+cmd+o up",
                "type continue"
            ]
        );
    }

    #[test]
    fn a_repeated_key_down_recovers_a_lost_hold_release() {
        let mut rig = Rig::bindings(json!({"ACT10": {"action": "hold", "key": HOLD}}));
        rig.key("ACT10", 1, 0.0);
        let out = rig.key("ACT10", 1, 5.0);
        assert!(out[0].stuck_release);
        assert!(out[0]
            .describe()
            .starts_with("released a stuck hold on ACT10"));
        assert!(!out[1].stuck_release);
        assert_eq!(
            rig.calls(),
            vec![
                "hold ctrl+cmd+o down",
                "hold ctrl+cmd+o up",
                "hold ctrl+cmd+o down"
            ]
        );
    }

    #[test]
    fn the_hold_cap_releases_a_hold_whose_key_up_never_came() {
        let mut rig = Rig::bindings(json!({"ACT10": {"action": "hold", "key": HOLD}}));
        rig.key("ACT10", 1, 10.0);
        assert_eq!(rig.bridge.next_deadline(), Some(130.0));
        assert_eq!(rig.expire(129.0), vec![]);
        let out = rig.expire(130.0);
        assert!(out[0].stuck_release);
        assert_eq!(rig.bridge.next_deadline(), None);
        assert!(rig.probe.borrow().rec.held.is_empty());
    }

    #[test]
    fn close_and_reload_let_go_of_a_held_key() {
        let doc = json!({"ACT10": {"action": "hold", "key": HOLD}});
        let mut rig = Rig::bindings(doc.clone());
        rig.key("ACT10", 1, 0.0);
        rig.bridge.close();
        assert!(rig.probe.borrow().rec.held.is_empty());
        let mut rig = Rig::bindings(doc.clone());
        rig.key("ACT10", 1, 0.0);
        let config = parse(&json!({"bindings": doc}), None).unwrap();
        rig.bridge.set_config(config);
        assert!(rig.probe.borrow().rec.held.is_empty());
    }

    #[test]
    fn a_chord_fires_instead_of_its_members() {
        let mut rig = Rig::bindings(json!({
            "AG00": {"action": "key", "key": "a"},
            "AG01": {"action": "key", "key": "b"},
            "AG00+AG01": {"action": "key", "key": "escape"},
        }));
        assert_eq!(rig.key("AG01", 1, 0.0), vec![], "held back");
        assert_eq!(rig.bridge.next_deadline(), Some(0.045));
        let out = rig.key("AG00", 1, 0.02);
        assert_eq!(ids(&out), vec!["AG00+AG01"]);
        assert_eq!(rig.key("AG00", 0, 0.1), vec![]);
        assert_eq!(rig.key("AG01", 0, 0.1), vec![]);
        assert_eq!(rig.calls(), vec!["press escape"]);
    }

    #[test]
    fn a_lone_chord_key_fires_on_expiry_or_on_a_quick_release() {
        let mut rig = Rig::bindings(json!({
            "AG00": {"action": "key", "key": "a"},
            "AG00+AG01": {"action": "key", "key": "escape"},
        }));
        rig.key("AG00", 1, 0.0);
        assert_eq!(rig.expire(0.04), vec![]);
        assert_eq!(ids(&rig.expire(0.05)), vec!["AG00"]);
        // AG01 after the solo fired is not a chord.
        let late = rig.key("AG01", 1, 0.06);
        assert!(late[0].chord.contains("AG00+AG01"));
        rig.key("AG01", 0, 0.07);
        rig.key("AG00", 0, 0.08);
        // A tap shorter than the window fires on release.
        rig.key("AG00", 1, 1.0);
        assert_eq!(ids(&rig.key("AG00", 0, 1.01)), vec!["AG00"]);
        assert_eq!(rig.calls(), vec!["press a", "press a"]);
    }

    #[test]
    fn an_unbound_chord_key_pays_no_latency_and_says_so() {
        let mut rig = Rig::bindings(json!({
            "AG00": {"action": "none"},
            "AG01": {"action": "key", "key": "b"},
            "AG00+AG01": {"action": "key", "key": "escape"},
        }));
        let out = rig.key("AG00", 1, 0.0);
        assert_eq!(out[0].describe(), "AG00: (unbound)");
        assert_eq!(rig.bridge.next_deadline(), None);
        assert_eq!(ids(&rig.key("AG01", 1, 1.0)), vec!["AG00+AG01"]);
    }

    #[test]
    fn settle_ms_zero_fires_at_once() {
        let mut rig = Rig::new(json!({
            "bindings": {"AG00": "a", "AG01": {"action": "none"}, "AG00+AG01": "chord"},
            "chords": {"settle_ms": 0},
        }));
        assert_eq!(ids(&rig.key("AG00", 1, 0.0)), vec!["AG00"]);
    }

    #[test]
    fn a_chord_hold_is_released_once_by_the_first_key_up() {
        let mut rig = Rig::bindings(json!({
            "AG00": {"action": "none"},
            "AG00+AG01": {"action": "hold", "key": HOLD},
        }));
        rig.key("AG00", 1, 0.0);
        rig.key("AG01", 1, 0.01);
        rig.key("AG01", 0, 0.5);
        rig.key("AG00", 0, 0.6);
        assert_eq!(
            rig.calls(),
            vec!["hold ctrl+cmd+o down", "hold ctrl+cmd+o up"]
        );
    }

    #[test]
    fn a_layer_overrides_while_held_and_reverts() {
        let mut rig = Rig::new(json!({
            "bindings": {"ACT12": {"action": "layer", "layer": "fn"}, "ACT06": "base"},
            "layers": {"fn": {"ACT06": "layered"}},
            "profiles": {"Mail": {"ACT06": "profile"}},
        }));
        rig.bridge.set_frontmost(Some("Mail".into()));
        rig.key("ACT06", 1, 0.0);
        rig.key("ACT12", 1, 0.1);
        rig.key("ACT06", 1, 0.2);
        rig.key("ACT12", 0, 0.3);
        rig.key("ACT06", 1, 0.4);
        assert_eq!(
            rig.calls(),
            vec!["type profile", "type layered", "type profile"]
        );
        rig.key("ACT12", 1, 1.0);
        assert_eq!(rig.bridge.next_deadline(), Some(121.0));
        assert!(rig.expire(121.0)[0].stuck_release);
        rig.key("ACT06", 1, 122.0);
        assert_eq!(rig.calls(), vec!["type profile"]);
    }

    #[test]
    fn a_profile_hold_survives_an_app_switch() {
        let mut rig = Rig::new(json!({
            "bindings": {"ACT10": {"action": "hold", "key": "f5"}},
            "profiles": {"Ghostty": {"ACT10": {"action": "hold", "key": HOLD}}},
        }));
        assert!(rig.bridge.set_frontmost(Some("Ghostty".into())));
        assert!(!rig.bridge.set_frontmost(Some("Ghostty".into())));
        rig.key("ACT10", 1, 0.0);
        rig.bridge.set_frontmost(Some("Mail".into()));
        rig.key("ACT10", 0, 0.5);
        assert_eq!(
            rig.calls(),
            vec!["hold ctrl+cmd+o down", "hold ctrl+cmd+o up"]
        );
    }

    fn latch_rig() -> Rig {
        Rig::bindings(json!({
            "ACT10": {"action": "hold", "key": HOLD, "latch": true,
                      "light": {"color": "#2E8B57"}},
            "ACT06": "x",
        }))
    }

    #[test]
    fn a_latch_taps_the_toggle_and_lights_while_recording() {
        let mut rig = latch_rig();
        rig.key("ACT10", 1, 0.0);
        rig.key("ACT10", 0, 0.1);
        rig.key("ACT10", 1, 0.2); // latched
        rig.key("ACT10", 0, 0.3);
        assert_eq!(rig.calls(), vec![format!("press {HOLD}")]);
        assert!(
            !rig.key("ACT06", 1, 0.4)[0].suppressed(),
            "a latch holds nothing"
        );
        rig.calls();
        rig.key("ACT10", 1, 5.0); // stop
        assert_eq!(rig.calls(), vec![format!("press {HOLD}")]);
        let notes: Vec<bool> = rig
            .bridge
            .drain_activity()
            .into_iter()
            .map(|(_, l)| l.is_some())
            .collect();
        assert_eq!(notes.first(), Some(&true));
        assert_eq!(notes.last(), Some(&false));
    }

    #[test]
    fn a_lapsed_latch_window_stops_through_expire() {
        let mut rig = latch_rig();
        rig.key("ACT10", 1, 0.0);
        rig.key("ACT10", 0, 0.1);
        let deadline = rig.bridge.next_deadline().unwrap();
        assert!((deadline - 0.45).abs() < 1e-9);
        let out = rig.expire(deadline);
        assert_eq!(ids(&out), vec!["ACT10"]);
        assert_eq!(
            rig.calls(),
            vec![format!("press {HOLD}"), format!("press {HOLD}")]
        );
    }

    #[test]
    fn stop_latches_sends_the_off_tap_and_is_quiet_when_idle() {
        let mut rig = latch_rig();
        assert_eq!(rig.bridge.stop_latches("dropped", 0.0), vec![]);
        rig.key("ACT10", 1, 0.0);
        assert_eq!(rig.bridge.stop_latches("dropped", 0.1).len(), 1);
        assert_eq!(rig.calls().len(), 2);
    }

    #[test]
    fn a_double_tap_fires_the_second_shortcut_before_the_hold() {
        let mut rig = Rig::bindings(json!({
            "ACT10": {"action": "hold", "key": HOLD, "double_tap": "ctrl+cmd+u"},
        }));
        rig.key("ACT10", 1, 0.0);
        rig.key("ACT10", 0, 0.1);
        rig.key("ACT10", 1, 0.2);
        rig.key("ACT10", 0, 0.3);
        assert_eq!(
            rig.calls(),
            vec![
                "hold ctrl+cmd+o down",
                "hold ctrl+cmd+o up",
                "press ctrl+cmd+u",
                "hold ctrl+cmd+o down",
                "hold ctrl+cmd+o up"
            ]
        );
    }

    #[test]
    fn un_owned_agent_keys_are_inert_only_while_codex_runs() {
        let mut rig = Rig::new(json!({
            "bindings": {"AG00": {"action": "key", "key": "a"}, "AG03": {"action": "key", "key": "d"}},
            "agent_keys": {"keys": [3, 4, 5]},
        }));
        assert_eq!(rig.key("AG00", 1, 0.0), vec![]);
        assert_eq!(rig.key("AG00", 0, 0.1), vec![]);
        rig.key("AG03", 1, 0.2);
        assert_eq!(rig.calls(), vec!["press d"]);
        let mut owner = AgentKeyOwnership::new(rig.owner.config.clone(), Box::new(|| false));
        owner.refresh(0.0);
        assert!(!rig.bridge.press("AG00", &owner, 1.0).is_empty());
        assert_eq!(rig.calls(), vec!["press a"]);
    }

    #[test]
    fn joystick_directions_fire_once_per_flick_and_kb_radial_is_ignored() {
        // Radial samples recorded in hw4.log (a stick pushed up, released).
        let mut rig = Rig::new(json!({
            "bindings": {"JOY_UP": "up", "JOY_DOWN": "down", "JOY_LEFT": "l", "JOY_RIGHT": "r"},
            "joystick": {"mode": "directions"},
        }));
        let samples = [
            (0.763265, 1.0),
            (0.783265, 0.283329),
            (0.007758, 0.0),
            (0.237044, 1.0),
            (0.231042, 0.626368),
            (0.009752, 0.0),
        ];
        let flick = |rig: &mut Rig, method: &str| {
            for (a, d) in samples {
                let msg =
                    json!({"m": method, "p": {"a": a, "d": d, "s": 1, "l": 2, "p": 0, "o": 2}});
                rig.bridge.handle(&msg, &rig.owner, 0.0);
            }
            rig.calls()
        };
        // Non-vendor layer: the firmware handles the stick, the host does not.
        assert!(flick(&mut rig, "kb.radial").is_empty());
        assert_eq!(flick(&mut rig, "v.oai.rad"), vec!["type up", "type down"]);
    }

    #[test]
    fn answer_permission_answers_on_press_or_by_hold_length_on_release() {
        use crate::state::store::SessionMeta;
        use crate::state::{AgentState, SessionSignals, StateStore, TerminalInfo};

        let home = crate::testutil::TempDir::new();
        let base = crate::clock::wall();
        // A fresh prompt record from this (live) process; each call is a new
        // record, so the answered-once latch lets it through.
        let ask = |n: f64| {
            let meta = SessionMeta {
                cwd: "/p/answer".into(),
                terminal: TerminalInfo {
                    tty: "/dev/ttys004".into(),
                    pid: std::process::id() as i32,
                    program: "Apple_Terminal".into(),
                    session: String::new(),
                },
                signals: SessionSignals {
                    event: "Notification".into(),
                    ..SessionSignals::default()
                },
                ..SessionMeta::default()
            };
            StateStore::for_home(home.path())
                .update("answer-rig", AgentState::Waiting, &meta, base + n)
                .unwrap();
        };
        let config = parse(
            &json!({"bindings": {
                "ACT07": {"action": "answer_permission", "long_press": "always"},
                "ACT08": {"action": "answer_permission", "answer": "reject"},
            }}),
            None,
        )
        .unwrap();
        let probe = Rc::new(RefCell::new(Probe::default()));
        let env = ActionEnv {
            home: home.path().to_path_buf(),
            ..env()
        };
        let mut rig = Rig {
            bridge: Bridge::new(config.clone(), Box::new(Shared(probe.clone())), env),
            probe,
            owner: AgentKeyOwnership::new(config.agent_keys, Box::new(|| true)),
        };
        let sent = |rig: &Rig| -> Vec<String> {
            let calls = rig.calls();
            calls
                .iter()
                .map(|c| c.rsplit(" then ").next().unwrap_or("").to_string())
                .collect()
        };
        let keystroke = |what: &str| format!("tell application \"System Events\" to {what}");

        ask(1.0);
        rig.key("ACT08", 1, 0.0);
        rig.key("ACT08", 0, 1.0);
        assert_eq!(sent(&rig), vec![keystroke("key code 53")]);
        ask(2.0);
        rig.key("ACT07", 1, 2.0);
        assert!(
            sent(&rig).is_empty(),
            "a long-press key waits for its release"
        );
        rig.key("ACT07", 0, 2.2);
        assert_eq!(sent(&rig), vec![keystroke("keystroke \"1\"")]);
        ask(3.0);
        rig.key("ACT07", 1, 3.0);
        rig.key("ACT07", 0, 3.6);
        assert_eq!(sent(&rig), vec![keystroke("keystroke \"2\"")]);
        // Answered already: a second press sends nothing.
        rig.key("ACT08", 1, 4.0);
        assert!(sent(&rig).is_empty());
    }

    #[test]
    fn pointer_mode_moves_on_expire_and_taps_click() {
        let mut rig = Rig::bindings(json!({}));
        let rad = |a: f64, d: f64| json!({"m": "v.oai.rad", "p": {"a": a, "d": d}});
        assert!(rig
            .bridge
            .handle(&rad(0.0, 1.0), &rig.owner, 0.0)
            .is_empty());
        assert_eq!(rig.bridge.next_deadline(), Some(0.0));
        rig.expire(0.0);
        let next = rig.bridge.next_deadline().unwrap();
        assert!((next - 1.0 / 90.0).abs() < 1e-9);
        rig.expire(0.01); // not due yet
        rig.expire(0.02);
        assert_eq!(rig.calls(), vec!["move 24 0 relative=true"]);
        rig.bridge.handle(&rad(0.0, 0.0), &rig.owner, 0.3);
        assert_eq!(rig.bridge.next_deadline(), None);
        // A quick push and return is a click.
        rig.bridge.handle(&rad(0.5, 0.9), &rig.owner, 1.0);
        rig.bridge.handle(&rad(0.5, 0.0), &rig.owner, 1.1);
        assert_eq!(rig.calls(), vec!["click left 1"]);
    }

    #[test]
    fn a_held_light_goes_up_on_press_and_down_on_release() {
        let mut rig = Rig::bindings(json!({
            "ACT10": {"action": "hold", "key": HOLD, "light": {"color": "#2E8B57"}},
        }));
        rig.key("ACT10", 1, 0.0);
        rig.key("ACT10", 0, 1.0);
        let notes = rig.bridge.drain_activity();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].1.is_some() && notes[1].1.is_none());
        assert!(rig.bridge.drain_activity().is_empty());
    }
}
