//! `~/.freemicro/keymap.json`: the host-side keymap (bindings, chords,
//! per-app profiles, layers, joystick, lighting, agent_keys). Port of
//! `freemicro/padconfig.py`. Not the pad's own keymap.json (see
//! `runtime::keymap`).
//!
//! Compatibility rule: any file Python accepts must load here. Binding kinds
//! this port does not know parse to `ActionKind::Unsupported` plus a
//! warning, never an error.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::actions::{py_float, py_str, repr, truthy, validate_params, Action, ActionKind};
use crate::runtime::lighting_config::{parse_activity_light, parse_lighting, LightingConfig};
use crate::state::agentkeys::{parse_agent_keys, AgentKeysConfig, Policy};

pub const ENV_PATH: &str = "FREEMICRO_KEYMAP";
pub const FILENAME: &str = "keymap.json";
/// Shipped default, a copy of `src/freemicro/default_keymap.json`.
pub const DEFAULT_KEYMAP: &str = include_str!("../../assets/default_keymap.json");

pub const AGENT_KEYS: [&str; 6] = ["AG00", "AG01", "AG02", "AG03", "AG04", "AG05"];
pub const ACTION_KEYS: [&str; 7] = [
    "ACT06", "ACT07", "ACT08", "ACT09", "ACT10", "ACT11", "ACT12",
];
pub const JOYSTICK_INPUTS: [&str; 4] = ["JOY_RIGHT", "JOY_DOWN", "JOY_LEFT", "JOY_UP"];
pub const ENCODER_INPUTS: [&str; 3] = ["ENC_CLK", "ENC_CW", "ENC_CC"];
pub const ENCODER_TICKS: [&str; 2] = ["ENC_CW", "ENC_CC"];
pub const CHORD_SEPARATOR: &str = "+";
pub const CHORD_MAX_KEYS: usize = 2;
pub const DEFAULT_CHORD_SETTLE_MS: f64 = 45.0;
pub const CHORD_SETTLE_MS_MAX: f64 = 500.0;
pub const DEFAULT_PROFILE_POLL_MS: f64 = 300.0;
pub const PROFILE_POLL_MS_MAX: f64 = 10000.0;
pub const DEFAULT_TERMINAL_APP: &str = "Terminal";
pub const TICK_HZ_MIN: f64 = 20.0;
pub const TICK_HZ_MAX: f64 = 480.0;

/// The order shipped before the up/down fix; still parses, with a warning.
const LEGACY_JOYSTICK_INPUTS: [&str; 4] = ["JOY_RIGHT", "JOY_UP", "JOY_LEFT", "JOY_DOWN"];
const META_FIELDS: [&str; 4] = ["action", "label", "comment", "light"];
const CHORD_FIELDS: [&str; 2] = ["settle_ms", "comment"];
const JOYSTICK_FIELDS: [&str; 14] = [
    "mode",
    "deadzone",
    "origin",
    "directions",
    "pointer_deadzone",
    "max_speed",
    "gamma",
    "tick_hz",
    "precision_key",
    "precision_scale",
    "invert_y",
    "tap_click",
    "tap_click_button",
    "comment",
];
const TAP_CLICK_BUTTONS: [&str; 3] = ["left", "right", "middle"];

fn known_input(input_id: &str) -> bool {
    AGENT_KEYS
        .iter()
        .chain(&ACTION_KEYS)
        .chain(&ENCODER_INPUTS)
        .chain(&JOYSTICK_INPUTS)
        .any(|k| *k == input_id)
}

pub fn known_inputs() -> impl Iterator<Item = &'static str> {
    AGENT_KEYS
        .into_iter()
        .chain(ACTION_KEYS)
        .chain(ENCODER_INPUTS)
        .chain(JOYSTICK_INPUTS)
}

/// One event and no release: cannot chord, latch or double-tap.
fn momentary(input_id: &str) -> bool {
    ENCODER_TICKS.contains(&input_id) || JOYSTICK_INPUTS.contains(&input_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadConfigError(pub String);

impl fmt::Display for PadConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PadConfigError {}

fn fail<T>(message: impl Into<String>) -> Result<T, PadConfigError> {
    Err(PadConfigError(message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JoystickMode {
    #[default]
    Pointer,
    Directions,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoystickConfig {
    /// Action deadzone (a direction fires past it).
    pub deadzone: f64,
    pub origin: f64,
    pub directions: Vec<String>,
    pub mode: JoystickMode,
    /// Motion deadzone (pointer mode).
    pub pointer_deadzone: f64,
    pub max_speed: f64,
    pub gamma: f64,
    pub tick_hz: f64,
    pub precision_key: String,
    pub precision_scale: f64,
    pub invert_y: bool,
    pub tap_click: bool,
    pub tap_click_button: String,
}

impl Default for JoystickConfig {
    fn default() -> Self {
        JoystickConfig {
            deadzone: 0.6,
            origin: 0.0,
            directions: JOYSTICK_INPUTS.iter().map(|s| s.to_string()).collect(),
            mode: JoystickMode::Pointer,
            pointer_deadzone: 0.1,
            max_speed: 1200.0,
            gamma: 2.0,
            tick_hz: 90.0,
            precision_key: String::new(),
            precision_scale: 0.25,
            invert_y: false,
            tap_click: true,
            tap_click_button: "left".to_string(),
        }
    }
}

impl JoystickConfig {
    pub fn pointing(&self) -> bool {
        self.mode == JoystickMode::Pointer
    }

    /// The `JOY_*` direction for `angle` (0..1 clockwise from +x).
    pub fn direction_for(&self, angle: f64) -> String {
        let steps = self.directions.len();
        let shifted = (angle - self.origin).rem_euclid(1.0);
        // Python's round() is round-half-even.
        let index = (shifted * steps as f64).round_ties_even() as usize % steps;
        self.directions[index].clone()
    }
}

pub type Bindings = BTreeMap<String, Action>;

/// The whole parsed host keymap.
#[derive(Debug, Clone, PartialEq)]
pub struct PadConfig {
    pub bindings: Bindings,
    pub joystick: JoystickConfig,
    pub lighting: LightingConfig,
    pub agent_keys: AgentKeysConfig,
    /// Sorted member ids -> action.
    pub chords: BTreeMap<Vec<String>, Action>,
    pub chord_settle_ms: f64,
    /// App name -> override bindings.
    pub profiles: BTreeMap<String, Bindings>,
    pub profile_poll_ms: f64,
    /// Layer name -> override bindings.
    pub layers: BTreeMap<String, Bindings>,
    pub terminal_app: String,
    /// Safari sites with their own pad layer (`linkedApps` process
    /// `com.apple.Safari/<site>`).
    pub sites: Vec<String>,
    pub source: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl PadConfig {
    /// The profile in force for `app`: exact case-insensitive name, else the
    /// longest profile name the app's name contains (ties: first in order).
    fn profile(&self, app: Option<&str>) -> Option<&Bindings> {
        let lowered = app.filter(|a| !a.is_empty())?.to_lowercase();
        if let Some((_, overrides)) = self
            .profiles
            .iter()
            .find(|(key, _)| key.to_lowercase() == lowered)
        {
            return Some(overrides);
        }
        let mut best: Option<(usize, &Bindings)> = None;
        for (key, overrides) in &self.profiles {
            let keyl = key.to_lowercase();
            if !keyl.is_empty()
                && lowered.contains(&keyl)
                && best.is_none_or(|(len, _)| keyl.len() > len)
            {
                best = Some((keyl.len(), overrides));
            }
        }
        best.map(|(_, b)| b)
    }

    /// Bindings for `app` (profile over base), Python `resolve_profile`.
    pub fn resolve_profile(&self, app: Option<&str>) -> Bindings {
        self.profile(app).cloned().unwrap_or_default()
    }

    pub fn action_for(&self, input_id: &str, app: Option<&str>) -> Option<&Action> {
        self.profile(app)
            .and_then(|p| p.get(input_id))
            .or_else(|| self.bindings.get(input_id))
    }

    pub fn chord_for(&self, members: &[String]) -> Option<&Action> {
        self.chords.get(&chord_key(members))
    }

    /// Keys that can complete a chord with `input_id`.
    pub fn chord_partners(&self, input_id: &str) -> Vec<String> {
        let mut partners: Vec<String> = Vec::new();
        for members in self.chords.keys() {
            if members.iter().any(|m| m == input_id) {
                for m in members {
                    if m != input_id && !partners.contains(m) {
                        partners.push(m.clone());
                    }
                }
            }
        }
        partners
    }

    /// Bindings that carry an activity light, by input id: known inputs in
    /// physical order, then unknown ones, then chords.
    pub fn activity_lights(&self) -> Vec<(String, Action)> {
        let mut ordered: Vec<&str> = known_inputs()
            .filter(|i| self.bindings.contains_key(*i))
            .collect();
        ordered.extend(
            self.bindings
                .keys()
                .map(String::as_str)
                .filter(|i| !known_input(i)),
        );
        let mut found: Vec<(String, Action)> = ordered
            .into_iter()
            .map(|i| (i.to_string(), &self.bindings[i]))
            .filter(|(_, a)| a.light.is_some())
            .map(|(i, a)| (i, a.clone()))
            .collect();
        for (members, action) in &self.chords {
            if action.light.is_some() {
                found.push((members.join(CHORD_SEPARATOR), action.clone()));
            }
        }
        found
    }

    /// Where this config came from, for the startup banner.
    pub fn origin(&self) -> String {
        match &self.source {
            None => "built-in default".to_string(),
            Some(path) => path.display().to_string(),
        }
    }
}

pub fn is_chord_id(input_id: &str) -> bool {
    input_id.contains(CHORD_SEPARATOR)
}

/// Sorted, deduplicated chord members.
pub fn chord_key(members: &[String]) -> Vec<String> {
    let mut key = members.to_vec();
    key.sort();
    key.dedup();
    key
}

pub fn parse_chord_id(input_id: &str) -> Result<Vec<String>, PadConfigError> {
    let members: Vec<String> = input_id
        .split(CHORD_SEPARATOR)
        .map(|p| p.trim().to_string())
        .collect();
    let id = repr(input_id);
    if members.iter().any(String::is_empty) {
        return fail(format!(
            "binding {id} names an empty key; write a chord as \"AG00+AG01\""
        ));
    }
    if members.len() > CHORD_MAX_KEYS {
        return fail(format!(
            "binding {id} names {} keys; FreeMicro binds chords of {CHORD_MAX_KEYS} keys \
             only, so that a two-key chord never has to wait to find out whether a third \
             is coming",
            members.len()
        ));
    }
    if chord_key(&members).len() != members.len() {
        return fail(format!(
            "binding {id} names the same key twice; a chord needs two different keys"
        ));
    }
    let moment: Vec<&str> = members
        .iter()
        .map(String::as_str)
        .filter(|m| momentary(m))
        .collect();
    if !moment.is_empty() {
        return fail(format!(
            "binding {id} cannot use {}: dial detents and joystick flicks are momentary - \
             they report one event and no release - so they can never be held alongside \
             another key",
            moment.join(", ")
        ));
    }
    Ok(chord_key(&members))
}

fn join_comment(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => String::new(),
        Some(Value::Array(lines)) => lines
            .iter()
            .map(py_str)
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Some(other) => py_str(other),
    }
}

fn parse_binding(
    input_id: &str,
    raw: &Value,
    terminal_app: &str,
    warnings: &mut Vec<String>,
) -> Result<Action, PadConfigError> {
    let id = repr(input_id);
    let shorthand;
    let raw = match raw {
        // "AG00": "/resume" == type that text.
        Value::String(text) => {
            let mut obj = Map::new();
            obj.insert("action".into(), Value::from("text"));
            obj.insert("text".into(), Value::from(text.as_str()));
            shorthand = Value::Object(obj);
            &shorthand
        }
        other => other,
    };
    let Some(obj) = raw.as_object() else {
        return fail(format!(
            "binding for {id} must be an object or a string, got {}",
            type_name(raw)
        ));
    };
    let kind_name = match obj.get("action") {
        Some(Value::String(k)) if !k.is_empty() => k.as_str(),
        _ => return fail(format!("binding for {id} is missing its \"action\" field")),
    };
    let kind = ActionKind::parse(kind_name);
    let mut params: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !META_FIELDS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if kind == ActionKind::FocusSession {
        if !params.contains_key("slot") && !params.contains_key("project") {
            // On an Agent Key, the key's own slot.
            match AGENT_KEYS.iter().position(|k| *k == input_id) {
                Some(index) => {
                    params.insert("slot".into(), Value::from(index));
                }
                None => {
                    return fail(format!(
                        "binding for {id}: 'focus_session' needs a \"slot\" (0-5) or a \
                         \"project\" path unless it is on an Agent Key"
                    ))
                }
            }
        }
        if !terminal_app.is_empty() && !params.contains_key("terminal") {
            params.insert("terminal".into(), Value::from(terminal_app));
        }
    }
    if let ActionKind::Unsupported(name) = &kind {
        warnings.push(format!(
            "binding for {id} uses the action '{name}', which freemicro-rs does not run - \
             the key does nothing here (the Python build still runs it)."
        ));
    }
    validate_params(&kind, &params)
        .map_err(|e| PadConfigError(format!("binding for {id}: {e}")))?;
    let latch = params.get("latch").is_some_and(truthy);
    if kind == ActionKind::Hold && (latch || params.get("double_tap").is_some_and(truthy)) {
        let members = if is_chord_id(input_id) {
            parse_chord_id(input_id)?
        } else {
            vec![input_id.to_string()]
        };
        let moment: Vec<&str> = members
            .iter()
            .map(String::as_str)
            .filter(|m| momentary(m))
            .collect();
        if !moment.is_empty() {
            let (what, cap) = if latch {
                ("latch", "Latch")
            } else {
                ("double-tap", "Double-tap")
            };
            return fail(format!(
                "binding {id} cannot {what}: {} report one event and no release, so the \
                 window could never resolve. {cap} belongs on an input you can hold.",
                moment.join(", ")
            ));
        }
    }
    let light = match obj.get("light") {
        Some(raw_light) => Some(parse_activity_light(input_id, raw_light).map_err(PadConfigError)?),
        None => None,
    };
    let label = match obj.get("label") {
        Some(v) if truthy(v) => py_str(v),
        _ => input_id.to_string(),
    };
    Ok(Action {
        kind,
        params,
        label,
        comment: join_comment(obj.get("comment")),
        light,
    })
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// A number of milliseconds; absent / null = `default`.
fn number_ms(raw: Option<&Value>, name: &str, default: f64) -> Result<f64, PadConfigError> {
    match raw {
        None | Some(Value::Null) => Ok(default),
        Some(v) => py_float(v).ok_or_else(|| {
            PadConfigError(format!(
                "\"{name}\" must be a number of milliseconds, got {v}"
            ))
        }),
    }
}

fn parse_chord_settle(raw: Option<&Value>) -> Result<f64, PadConfigError> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(DEFAULT_CHORD_SETTLE_MS),
        Some(Value::Object(obj)) => obj,
        Some(_) => return fail("\"chords\" must be an object"),
    };
    let mut unknown: Vec<&str> = raw
        .keys()
        .map(String::as_str)
        .filter(|k| !k.starts_with('_') && !CHORD_FIELDS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return fail(format!(
            "\"chords\" has unknown field(s): {}; it takes: {}",
            unknown.join(", "),
            CHORD_FIELDS.join(", ")
        ));
    }
    let settle = number_ms(
        raw.get("settle_ms"),
        "chords.settle_ms",
        DEFAULT_CHORD_SETTLE_MS,
    )?;
    if !(0.0..=CHORD_SETTLE_MS_MAX).contains(&settle) {
        return fail(format!(
            "\"chords.settle_ms\" must be between 0 (never wait) and {CHORD_SETTLE_MS_MAX}; \
             anything longer stops feeling like a key press"
        ));
    }
    Ok(settle)
}

fn parse_profile_poll_ms(raw: Option<&Value>) -> Result<f64, PadConfigError> {
    let value = number_ms(raw, "profile_poll_ms", DEFAULT_PROFILE_POLL_MS)?;
    if !(0.0..=PROFILE_POLL_MS_MAX).contains(&value) {
        return fail(format!(
            "\"profile_poll_ms\" must be between 0 (re-read every tick) and \
             {PROFILE_POLL_MS_MAX}; anything longer keeps acting on an app you have \
             already switched away from"
        ));
    }
    Ok(value)
}

fn parse_sites(raw: Option<&Value>) -> Result<Vec<String>, PadConfigError> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let sites: Option<Vec<String>> = raw.as_array().and_then(|list| {
        list.iter()
            .map(|v| v.as_str().map(|s| s.trim().to_lowercase()))
            .collect()
    });
    match sites {
        Some(sites) if sites.iter().all(|s| !s.is_empty()) => Ok(sites),
        _ => fail("\"sites\" must be a list of host names, e.g. [\"github.com\"]"),
    }
}

fn parse_terminal_app(raw: Option<&Value>) -> Result<String, PadConfigError> {
    match raw {
        None | Some(Value::Null) => Ok(DEFAULT_TERMINAL_APP.to_string()),
        Some(Value::Bool(false)) => Ok(String::new()),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if ["off", "none", ""].contains(&trimmed.to_lowercase().as_str()) {
                Ok(String::new())
            } else {
                Ok(trimmed.to_string())
            }
        }
        Some(other) => fail(format!(
            "\"terminal_app\" must be a terminal app name, or false to leave an empty Agent \
             Key inert; got {other}"
        )),
    }
}

/// The `profiles` or `layers` section: name -> partial bindings, no chords.
fn parse_overrides(
    raw: Option<&Value>,
    what: &str,
    section: &str,
    warnings: &mut Vec<String>,
    terminal_app: &str,
) -> Result<BTreeMap<String, Bindings>, PadConfigError> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(Value::Object(obj)) => obj,
        Some(_) => {
            return fail(format!(
                "\"{section}\" must be an object of {what} name -> overrides"
            ))
        }
    };
    let mut out = BTreeMap::new();
    for (name, overrides_raw) in raw {
        if name.starts_with('_') {
            continue;
        }
        let label = format!("{what} {}", repr(name));
        let Some(overrides_raw) = overrides_raw.as_object() else {
            return fail(format!("{label} must be an object of input id -> binding"));
        };
        let mut overrides = Bindings::new();
        for (input_id, binding_raw) in overrides_raw {
            if input_id.starts_with('_') {
                continue;
            }
            if is_chord_id(input_id) {
                return fail(format!(
                    "{label} binds the chord {}, but chords are global: they stay in the \
                     top-level \"bindings\" and fire the same everywhere. A {what} overrides \
                     single keys only.",
                    repr(input_id)
                ));
            }
            let action = parse_binding(input_id, binding_raw, terminal_app, warnings)
                .map_err(|e| PadConfigError(format!("{label}: {e}")))?;
            overrides.insert(input_id.clone(), action);
            if !known_input(input_id) {
                warnings.push(format!(
                    "{label} binds {}, which is not an input this build knows about - it \
                     will only fire if your firmware reports that id.",
                    repr(input_id)
                ));
            }
        }
        out.insert(name.clone(), overrides);
    }
    Ok(out)
}

fn joystick_number(
    raw: &Map<String, Value>,
    name: &str,
    fallback: f64,
) -> Result<f64, PadConfigError> {
    match raw.get(name) {
        None => Ok(fallback),
        Some(v) => py_float(v).ok_or_else(|| {
            PadConfigError(format!("\"joystick.{name}\" must be a number, got {v}"))
        }),
    }
}

fn parse_joystick(raw: Option<&Value>) -> Result<JoystickConfig, PadConfigError> {
    let defaults = JoystickConfig::default();
    let raw = match raw {
        None | Some(Value::Null) => return Ok(defaults),
        Some(Value::Object(obj)) => obj,
        Some(_) => return fail("\"joystick\" must be an object"),
    };
    let mut unknown: Vec<&str> = raw
        .keys()
        .map(String::as_str)
        .filter(|k| !k.starts_with('_') && !JOYSTICK_FIELDS.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return fail(format!(
            "\"joystick\" has unknown field(s): {}; it takes: {}",
            unknown.join(", "),
            JOYSTICK_FIELDS.join(", ")
        ));
    }
    let mode = match raw.get("mode") {
        None => JoystickMode::Pointer,
        Some(v) => match py_str(v).to_lowercase().as_str() {
            "pointer" => JoystickMode::Pointer,
            "directions" => JoystickMode::Directions,
            _ => {
                return fail(format!(
                    "\"joystick.mode\" must be one of pointer, directions, got {v}"
                ))
            }
        },
    };
    let directions = match raw.get("directions") {
        None => defaults.directions.clone(),
        Some(Value::Array(items)) if items.len() >= 2 => {
            let mut out = Vec::new();
            for item in items {
                match item {
                    Value::String(s) if !s.is_empty() => out.push(s.clone()),
                    _ => return fail("\"joystick.directions\" entries must be input ids"),
                }
            }
            out
        }
        Some(_) => return fail("\"joystick.directions\" must list at least two input ids"),
    };
    let deadzone = joystick_number(raw, "deadzone", defaults.deadzone)?;
    let origin = joystick_number(raw, "origin", defaults.origin)?;
    let pointer_deadzone = joystick_number(raw, "pointer_deadzone", defaults.pointer_deadzone)?;
    let max_speed = joystick_number(raw, "max_speed", defaults.max_speed)?;
    let gamma = joystick_number(raw, "gamma", defaults.gamma)?;
    let tick_hz = joystick_number(raw, "tick_hz", defaults.tick_hz)?;
    let precision_scale = joystick_number(raw, "precision_scale", defaults.precision_scale)?;

    if !(0.0..=1.0).contains(&deadzone) {
        return fail("\"joystick.deadzone\" must be between 0 and 1");
    }
    if !(0.0..1.0).contains(&pointer_deadzone) {
        return fail(
            "\"joystick.pointer_deadzone\" must be at least 0 and less than 1 (1 would leave \
             no travel to point with)",
        );
    }
    if max_speed <= 0.0 {
        return fail("\"joystick.max_speed\" must be more than 0 pixels per second");
    }
    if gamma <= 0.0 {
        return fail(
            "\"joystick.gamma\" must be more than 0 (1 is linear, ~2 is a TrackPoint-like curve)",
        );
    }
    if !(TICK_HZ_MIN..=TICK_HZ_MAX).contains(&tick_hz) {
        return fail(format!(
            "\"joystick.tick_hz\" must be between {TICK_HZ_MIN} and {TICK_HZ_MAX}"
        ));
    }
    if !(precision_scale > 0.0 && precision_scale <= 1.0) {
        return fail("\"joystick.precision_scale\" must be more than 0 and at most 1");
    }
    let precision_key = match raw.get("precision_key") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return fail("\"joystick.precision_key\" must be an input id"),
    };
    if ENCODER_TICKS.contains(&precision_key.as_str()) {
        return fail(format!(
            "\"joystick.precision_key\" cannot be {}: dial detents are momentary and have no \
             release, so precision mode would latch on forever",
            repr(&precision_key)
        ));
    }
    let tap_click_button = match raw.get("tap_click_button") {
        None => defaults.tap_click_button.clone(),
        Some(v) => py_str(v).to_lowercase(),
    };
    if !TAP_CLICK_BUTTONS.contains(&tap_click_button.as_str()) {
        return fail(format!(
            "\"joystick.tap_click_button\" must be one of {}, got {}",
            TAP_CLICK_BUTTONS.join(", "),
            raw.get("tap_click_button").unwrap_or(&Value::Null)
        ));
    }
    Ok(JoystickConfig {
        deadzone,
        origin,
        directions,
        mode,
        pointer_deadzone,
        max_speed,
        gamma,
        tick_hz,
        precision_key,
        precision_scale,
        invert_y: raw.get("invert_y").map_or(defaults.invert_y, truthy),
        tap_click: raw.get("tap_click").map_or(defaults.tap_click, truthy),
        tap_click_button,
    })
}

fn parse_agent_keys_section(
    raw: Option<&Value>,
    warnings: &mut Vec<String>,
) -> Result<AgentKeysConfig, PadConfigError> {
    let config = parse_agent_keys(raw.unwrap_or(&Value::Null)).map_err(PadConfigError)?;
    if matches!(config.policy, Policy::Pinned | Policy::Manual) {
        for (index, pin) in config.slots.iter().enumerate() {
            if pin.is_empty() {
                continue;
            }
            if !config.owns(index) {
                warnings.push(format!(
                    "agent_keys.slots[{index}] pins AG{index:02} to {}, but that key is not in \
                     agent_keys.keys - FreeMicro leaves it to the vendor app (Codex), so the \
                     pin is ignored.",
                    repr(pin)
                ));
            } else if !Path::new(pin).is_dir() {
                warnings.push(format!(
                    "agent_keys.slots[{index}] pins AG{index:02} to {}, which is not a \
                     directory right now - that key will stay dark until it is.",
                    repr(pin)
                ));
            }
        }
    }
    if config.policy == Policy::Manual && config.slots.iter().all(String::is_empty) {
        warnings.push(
            "agent_keys.policy is 'manual' but no slot names a project, so all six Agent \
             Keys will stay dark. Use 'recent' to fill them automatically."
                .to_string(),
        );
    }
    Ok(config)
}

fn check_layer_references(config: &PadConfig, warnings: &mut Vec<String>) {
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut scan = |place: String, source: &Bindings| {
        for (input_id, action) in source {
            if action.kind != ActionKind::Layer {
                continue;
            }
            let name = super::actions::layer_name(action);
            if !name.is_empty()
                && !config.layers.contains_key(&name)
                && !missing.iter().any(|(n, _)| *n == name)
            {
                missing.push((name, format!("{place}{}", repr(input_id))));
            }
        }
    };
    scan(String::new(), &config.bindings);
    for (app, overrides) in &config.profiles {
        scan(format!("profile {} ", repr(app)), overrides);
    }
    for (layer, overrides) in &config.layers {
        scan(format!("layer {} ", repr(layer)), overrides);
    }
    for (name, place) in missing {
        let n = repr(&name);
        warnings.push(format!(
            "{place} triggers the layer {n}, which is not defined in \"layers\" - holding it \
             switches nothing on. Add a \"layers\": {{{n}: {{...}}}} block, or fix the trigger."
        ));
    }
}

fn check_unowned_agent_keys(config: &PadConfig, warnings: &mut Vec<String>) {
    let keys = &config.agent_keys;
    if keys.owns_all() {
        return;
    }
    let note = |place: String, input_id: &str| {
        format!(
            "{place}{} is bound, but {input_id} is not in agent_keys.keys - FreeMicro leaves \
             that Agent Key to the vendor app (Codex) and ignores its presses, so this binding \
             never fires. Add the key to agent_keys.keys to drive it, or move the binding to \
             an owned key.",
            repr(input_id)
        )
    };
    for input_id in config.bindings.keys() {
        if keys.leaves_to_codex(input_id) {
            warnings.push(note(String::new(), input_id));
        }
    }
    for members in config.chords.keys() {
        let unowned: Vec<&str> = members
            .iter()
            .map(String::as_str)
            .filter(|m| keys.leaves_to_codex(m))
            .collect();
        if !unowned.is_empty() {
            warnings.push(format!(
                "chord {} includes {}, which is not in agent_keys.keys - FreeMicro leaves that \
                 Agent Key to the vendor app (Codex) and ignores its presses, so the chord can \
                 never form.",
                repr(&members.join(CHORD_SEPARATOR)),
                unowned.join(", ")
            ));
        }
    }
    for (what, map) in [("profile", &config.profiles), ("layer", &config.layers)] {
        for (name, overrides) in map {
            for input_id in overrides.keys() {
                if keys.leaves_to_codex(input_id) {
                    warnings.push(note(format!("{what} {} ", repr(name)), input_id));
                }
            }
        }
    }
}

/// Parse a whole keymap.json document.
pub fn parse(data: &Value, source: Option<&Path>) -> Result<PadConfig, PadConfigError> {
    let Some(doc) = data.as_object() else {
        return fail("the config file must contain a JSON object");
    };
    // Python compares `version != 1`, so 1, 1.0 and true all pass.
    match doc.get("version") {
        None => {}
        Some(v) if v.as_f64() == Some(1.0) || *v == Value::Bool(true) => {}
        Some(v) => {
            return fail(format!(
                "unsupported config version {v}; this build understands 1"
            ))
        }
    }
    let bindings_raw = match doc.get("bindings") {
        None | Some(Value::Null) => return fail("the config file needs a \"bindings\" object"),
        Some(Value::Object(obj)) => obj,
        Some(_) => return fail("\"bindings\" must be an object of input id -> binding"),
    };

    let mut warnings = Vec::new();
    let terminal_app = parse_terminal_app(doc.get("terminal_app"))?;
    let mut bindings = Bindings::new();
    let mut chords: BTreeMap<Vec<String>, Action> = BTreeMap::new();
    let mut chord_written_as: BTreeMap<Vec<String>, String> = BTreeMap::new();
    for (input_id, raw) in bindings_raw {
        if input_id.starts_with('_') {
            continue;
        }
        if is_chord_id(input_id) {
            let members = parse_chord_id(input_id)?;
            if let Some(first) = chord_written_as.get(&members) {
                return fail(format!(
                    "binding {} is the same chord as {} - two keys pressed together have no \
                     order, so only one of them can be bound",
                    repr(input_id),
                    repr(first)
                ));
            }
            chord_written_as.insert(members.clone(), input_id.clone());
            let mut action = parse_binding(input_id, raw, &terminal_app, &mut warnings)?;
            if action.label == *input_id {
                action.label = members.join(CHORD_SEPARATOR);
            }
            chords.insert(members.clone(), action);
            let unknown: Vec<String> = members
                .iter()
                .filter(|m| !known_input(m))
                .map(|m| repr(m))
                .collect();
            if !unknown.is_empty() {
                warnings.push(format!(
                    "chord {} uses {}, which this build does not know about - it will only \
                     fire if your firmware reports that id.",
                    repr(input_id),
                    unknown.join(", ")
                ));
            }
            continue;
        }
        bindings.insert(
            input_id.clone(),
            parse_binding(input_id, raw, &terminal_app, &mut warnings)?,
        );
        if !known_input(input_id) {
            warnings.push(format!(
                "{} is not an input this build knows about - it will only fire if your \
                 firmware reports that id.",
                repr(input_id)
            ));
        }
    }

    let chord_settle_ms = parse_chord_settle(doc.get("chords"))?;
    if !chords.is_empty() && chord_settle_ms <= 0.0 {
        let mut dead: Vec<&str> = chord_written_as
            .iter()
            .filter(|(members, _)| {
                members
                    .iter()
                    .all(|m| bindings.get(m).is_some_and(|a| a.kind != ActionKind::None))
            })
            .map(|(_, written)| written.as_str())
            .collect();
        if !dead.is_empty() {
            dead.sort();
            warnings.push(format!(
                "chords.settle_ms is 0, so no press is ever held back and {} can never fire: \
                 every key in them already runs its own binding the instant it goes down. Give \
                 one key of each chord {{\"action\": \"none\"}}, or raise chords.settle_ms.",
                dead.join(", ")
            ));
        }
    }

    let joystick = parse_joystick(doc.get("joystick"))?;
    let lighting = parse_lighting(doc.get("lighting").unwrap_or(&Value::Null), &mut warnings)
        .map_err(PadConfigError)?;
    let agent_keys = parse_agent_keys_section(doc.get("agent_keys"), &mut warnings)?;
    let profiles = parse_overrides(
        doc.get("profiles"),
        "profile",
        "profiles",
        &mut warnings,
        &terminal_app,
    )?;
    let profile_poll_ms = parse_profile_poll_ms(doc.get("profile_poll_ms"))?;
    let layers = parse_overrides(
        doc.get("layers"),
        "layer",
        "layers",
        &mut warnings,
        &terminal_app,
    )?;

    let mut config = PadConfig {
        bindings,
        joystick,
        lighting,
        agent_keys,
        chords,
        chord_settle_ms,
        profiles,
        profile_poll_ms,
        layers,
        terminal_app,
        sites: parse_sites(doc.get("sites"))?,
        source: source.map(Path::to_path_buf),
        warnings: Vec::new(),
    };
    check_layer_references(&config, &mut warnings);
    check_unowned_agent_keys(&config, &mut warnings);

    let lit: Vec<(String, &Action)> = config
        .bindings
        .iter()
        .map(|(i, a)| (i.clone(), a))
        .chain(
            config
                .chords
                .iter()
                .map(|(m, a)| (m.join(CHORD_SEPARATOR), a)),
        )
        .filter(|(_, a)| a.light.is_some())
        .collect();
    for (input_id, action) in &lit {
        // A hold or a layer is lit exactly while held; anything else only for
        // the length of a tap. answer_permission is a hold kind in Python.
        let held = matches!(
            action.kind,
            ActionKind::Hold | ActionKind::Layer | ActionKind::AnswerPermission
        );
        if !held {
            warnings.push(format!(
                "{} has a \"light\" but its action is '{}', so the light lasts exactly as long \
                 as your finger is on the key. If this is dictation, FreeMicro cannot see a \
                 toggle stop - use {{\"action\": \"hold\"}} and set your dictation app's \
                 push-to-talk (hold) shortcut to match.",
                repr(input_id),
                action.kind.as_str()
            ));
        }
    }
    if !lit.is_empty() && !config.lighting.enabled {
        let names: Vec<String> = lit.iter().map(|(i, _)| repr(i)).collect();
        warnings.push(format!(
            "lighting.enabled is false, so the \"light\" on {} will not show. Turn the LEDs on \
             once with: freemicro lights --enable",
            names.join(", ")
        ));
    }

    let joystick = &config.joystick;
    if joystick.pointing() {
        let shadowed: Vec<&str> = joystick
            .directions
            .iter()
            .filter(|d| {
                config
                    .bindings
                    .get(*d)
                    .is_some_and(|a| !matches!(a.kind, ActionKind::None | ActionKind::Mouse))
            })
            .map(String::as_str)
            .collect();
        if !shadowed.is_empty() {
            warnings.push(format!(
                "joystick.mode is 'pointer', so the stick moves the cursor and {} will not \
                 fire. Set \"joystick\": {{\"mode\": \"directions\"}} to get the flicks back.",
                shadowed.join(", ")
            ));
        }
        if !joystick.precision_key.is_empty()
            && config.bindings.contains_key(&joystick.precision_key)
        {
            warnings.push(format!(
                "joystick.precision_key is {}, which also has a binding - while pointing, \
                 holding it slows the cursor instead of running that binding.",
                repr(&joystick.precision_key)
            ));
        }
    } else {
        for direction in &joystick.directions {
            if !config.bindings.contains_key(direction) {
                warnings.push(format!(
                    "joystick direction {} has no binding.",
                    repr(direction)
                ));
            }
        }
    }
    if joystick.directions == LEGACY_JOYSTICK_INPUTS {
        warnings.push(
            "joystick.directions is in the old right/up/left/down order, which puts JOY_UP \
             where the hardware reports DOWN. The correct order is [\"JOY_RIGHT\", \
             \"JOY_DOWN\", \"JOY_LEFT\", \"JOY_UP\"] - swap the two, or delete the line to \
             take the default."
                .to_string(),
        );
    }
    config.warnings = warnings;
    Ok(config)
}

/// `<home>/keymap.json`.
pub fn user_path(home: &Path) -> PathBuf {
    home.join(FILENAME)
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand_user(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => user_home().join(rest),
        None if path == "~" => user_home(),
        None => PathBuf::from(path),
    }
}

/// `$FREEMICRO_KEYMAP`, `<home>/keymap.json`, `$XDG_CONFIG_HOME/freemicro/keymap.json`.
pub fn search_paths(home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(env) = std::env::var_os(ENV_PATH).filter(|v| !v.is_empty()) {
        paths.push(expand_user(&env.to_string_lossy()));
    }
    paths.push(user_path(home));
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| user_home().join(".config"));
    paths.push(xdg.join("freemicro").join(FILENAME));
    paths
}

/// `explicit` (which must exist), else the first existing search path,
/// else the user path.
pub fn resolve_path(home: &Path, explicit: Option<&Path>) -> Result<PathBuf, PadConfigError> {
    if let Some(path) = explicit {
        let path = expand_user(&path.to_string_lossy());
        if !path.exists() {
            return Err(PadConfigError(format!(
                "no such config file: {}",
                path.display()
            )));
        }
        return Ok(path);
    }
    Ok(search_paths(home)
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| user_path(home)))
}

/// Load and parse `path`; a missing file loads the default. A broken file
/// is an error: running on defaults while edits vanish is worse.
pub fn load(path: &Path) -> Result<PadConfig, PadConfigError> {
    if !path.exists() {
        return Ok(load_default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| PadConfigError(format!("could not read {}: {e}", path.display())))?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|e| PadConfigError(format!("{} is not valid JSON: {e}", path.display())))?;
    parse(&data, Some(path))
}

pub fn load_default() -> PadConfig {
    let data: Value = serde_json::from_str(DEFAULT_KEYMAP).expect("shipped keymap is JSON");
    parse(&data, None).expect("shipped keymap parses")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(doc: Value) -> PadConfig {
        parse(&doc, None).unwrap_or_else(|e| panic!("{e}"))
    }

    fn error(doc: Value) -> String {
        parse(&doc, None).expect_err("should not parse").0
    }

    #[test]
    fn the_default_keymap_parses_and_binds_every_known_input() {
        let config = load_default();
        for input in known_inputs() {
            assert!(config.bindings.contains_key(input), "{input}");
        }
        assert_eq!(config.bindings["AG03"].params["slot"], json!(3));
        assert_eq!(
            config.bindings["AG03"].params["terminal"],
            json!("Terminal")
        );
        assert_eq!(config.bindings["ACT07"].kind, ActionKind::AnswerPermission);
        assert!(!config
            .warnings
            .iter()
            .any(|w| w.contains("answer_permission")));
        assert_eq!(config.origin(), "built-in default");
    }

    #[test]
    fn string_shorthand_types_text_and_label_defaults_to_the_id() {
        let config = parsed(json!({"bindings": {"ACT06": "/resume"}}));
        let act = &config.bindings["ACT06"];
        assert_eq!(act.kind, ActionKind::Text);
        assert_eq!(act.params["text"], json!("/resume"));
        assert_eq!(act.label, "ACT06");
    }

    #[test]
    fn parse_rejects_broken_documents() {
        let cases = [
            json!([]),
            json!({"version": 2, "bindings": {}}),
            json!({}),
            json!({"bindings": []}),
            json!({"bindings": {"ACT06": 5}}),
            json!({"bindings": {"ACT06": {"label": "x"}}}),
            json!({"bindings": {"ACT06": {"action": "key"}}}),
            json!({"bindings": {"ACT06": {"action": "focus_session"}}}),
            json!({"bindings": {"AG00+AG01+AG02": "x"}}),
            json!({"bindings": {"AG00+AG00": "x"}}),
            json!({"bindings": {"AG00+ENC_CW": "x"}}),
            json!({"bindings": {"AG00+AG01": "x", "AG01+AG00": "y"}}),
            json!({"bindings": {"ENC_CW": {"action": "hold", "key": "f5", "latch": true}}}),
            json!({"bindings": {}, "chords": {"settle_ms": 900}}),
            json!({"bindings": {}, "chords": {"wait": 1}}),
            json!({"bindings": {}, "profile_poll_ms": -1}),
            json!({"bindings": {}, "terminal_app": 3}),
            json!({"bindings": {}, "profiles": {"Chrome": {"AG00+AG01": "x"}}}),
            json!({"bindings": {}, "layers": {"fn": []}}),
            json!({"bindings": {}, "joystick": {"speed": 3}}),
            json!({"bindings": {}, "joystick": {"mode": "warp"}}),
            json!({"bindings": {}, "joystick": {"deadzone": 2}}),
            json!({"bindings": {}, "joystick": {"tick_hz": 5}}),
            json!({"bindings": {}, "joystick": {"gamma": "steep"}}),
            json!({"bindings": {}, "joystick": {"directions": ["JOY_UP"]}}),
            json!({"bindings": {}, "joystick": {"precision_key": "ENC_CW"}}),
            json!({"bindings": {}, "joystick": {"tap_click_button": "thumb"}}),
        ];
        for doc in cases {
            assert!(parse(&doc, None).is_err(), "{doc}");
        }
        assert!(error(
            json!({"bindings": {}, "profiles": {"Chrome": {"AG00": {"action": "key"}}}})
        )
        .starts_with("profile 'Chrome': binding for 'AG00'"));
    }

    #[test]
    fn an_unknown_input_id_is_a_warning_not_an_error() {
        let config = parsed(json!({"bindings": {"KNOB9": "x"}}));
        assert!(config.bindings.contains_key("KNOB9"));
        assert!(config.warnings.iter().any(|w| w.starts_with("'KNOB9'")));
    }

    #[test]
    fn chords_are_canonical_and_relabelled() {
        let config = parsed(json!({"bindings": {
            "AG01+AG00": {"action": "key", "key": "escape"},
            "AG00": {"action": "none"},
        }}));
        let key = vec!["AG00".to_string(), "AG01".to_string()];
        assert_eq!(config.chords[&key].label, "AG00+AG01");
        assert!(config.chord_for(&["AG01".into(), "AG00".into()]).is_some());
        assert_eq!(config.chord_partners("AG00"), vec!["AG01".to_string()]);
        assert!(config.chord_partners("AG02").is_empty());
        assert_eq!(config.chord_settle_ms, DEFAULT_CHORD_SETTLE_MS);
    }

    #[test]
    fn settle_ms_zero_warns_about_dead_chords() {
        let config = parsed(json!({
            "bindings": {"AG00+AG01": "x", "AG00": "a", "AG01": "b"},
            "chords": {"settle_ms": 0}
        }));
        assert!(config.warnings.iter().any(|w| w.contains("can never fire")));
    }

    #[test]
    fn resolve_profile_prefers_exact_then_longest_substring() {
        let config = parsed(json!({"bindings": {"ACT06": "base"}, "profiles": {
            "Chrome": {"ACT06": "chrome"},
            "Google Chrome": {"ACT06": "google"},
            "terminal": {"ACT06": "term"},
        }}));
        let text = |app: Option<&str>| {
            config
                .action_for("ACT06", app)
                .map(|a| py_str(&a.params["text"]))
        };
        assert_eq!(text(None).as_deref(), Some("base"));
        assert_eq!(text(Some("Terminal")).as_deref(), Some("term"));
        assert_eq!(
            text(Some("Google Chrome Canary")).as_deref(),
            Some("google")
        );
        assert_eq!(text(Some("Chrome")).as_deref(), Some("chrome"));
        assert_eq!(text(Some("Mail")).as_deref(), Some("base"));
        assert!(config.resolve_profile(Some("Mail")).is_empty());
        assert_eq!(
            config.resolve_profile(Some("Google Chrome Canary")).len(),
            1
        );
    }

    #[test]
    fn terminal_app_is_injected_into_focus_session_unless_off() {
        let cases = [
            (json!(null), Some("Terminal")),
            (json!(" iTerm2 "), Some("iTerm2")),
            (json!(false), None),
            (json!("off"), None),
        ];
        for (raw, want) in cases {
            let mut doc = json!({"bindings": {"AG00": {"action": "focus_session"}}});
            if !raw.is_null() {
                doc["terminal_app"] = raw.clone();
            }
            let config = parsed(doc);
            let got = config.bindings["AG00"].params.get("terminal").map(py_str);
            assert_eq!(got.as_deref(), want, "{raw}");
        }
    }

    #[test]
    fn layer_and_joystick_warnings() {
        let config = parsed(json!({
            "bindings": {"ACT12": {"action": "layer", "layer": "fn"}, "JOY_UP": "x"},
            "joystick": {"directions": ["JOY_RIGHT", "JOY_UP", "JOY_LEFT", "JOY_DOWN"]}
        }));
        let has = |s: &str| config.warnings.iter().any(|w| w.contains(s));
        assert!(has("triggers the layer 'fn'"));
        assert!(has("will not fire"));
        assert!(has("old right/up/left/down order"));
        let directions = parsed(json!({"bindings": {}, "joystick": {"mode": "directions"}}));
        assert_eq!(directions.joystick.mode, JoystickMode::Directions);
        assert!(directions
            .warnings
            .iter()
            .any(|w| w == "joystick direction 'JOY_RIGHT' has no binding."));
    }

    #[test]
    fn direction_for_follows_the_hardware_wheel() {
        let joystick = JoystickConfig::default();
        let cases = [
            (0.0, "JOY_RIGHT"),
            (0.25, "JOY_DOWN"),
            (0.5, "JOY_LEFT"),
            (0.75, "JOY_UP"),
            (0.97, "JOY_RIGHT"),
            (0.125, "JOY_RIGHT"), // round half to even
        ];
        for (angle, want) in cases {
            assert_eq!(joystick.direction_for(angle), want, "{angle}");
        }
    }

    #[test]
    fn load_reads_a_file_and_falls_back_on_a_missing_one() {
        let dir = crate::testutil::TempDir::new();
        let path = dir.path().join(FILENAME);
        assert_eq!(load(&path).unwrap().origin(), "built-in default");
        std::fs::write(&path, r#"{"bindings": {"ACT06": "x"}}"#).unwrap();
        let config = load(&path).unwrap();
        assert_eq!(config.origin(), path.display().to_string());
        std::fs::write(&path, "{").unwrap();
        assert!(load(&path).unwrap_err().0.contains("is not valid JSON"));
        assert_eq!(resolve_path(dir.path(), Some(&path)).unwrap(), path);
        let missing = dir.path().join("typo.json");
        let err = resolve_path(dir.path(), Some(&missing)).unwrap_err();
        assert!(err.0.starts_with("no such config file: "));
    }
}
