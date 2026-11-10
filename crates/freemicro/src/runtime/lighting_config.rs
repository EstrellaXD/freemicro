//! The `lighting` block of `~/.freemicro/keymap.json` and per-binding
//! activity lights. Port of the lighting part of `padconfig.py`
//! (`StateLight`, `ActivityLight`, `ReassertConfig`, `BatteryConfig`,
//! `LightingConfig`, themes, `_parse_lighting`, `_parse_activity_light`).
//! `input::config::parse` calls [`parse_lighting`] and
//! [`parse_activity_light`].

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::lighting::{parse_color, parse_effect_spec, Zone};
use crate::input::config::{CHORD_SEPARATOR, ENCODER_TICKS, JOYSTICK_INPUTS};
use crate::state::AgentState;

pub const DEFAULT_ACTIVITY_TIMEOUT: f64 = 120.0;
pub const ACTIVITY_TIMEOUT_MAX: f64 = 600.0;
pub const DEFAULT_BATTERY_THRESHOLD: u8 = 15;
pub const DEFAULT_BATTERY_POLL_SECONDS: f64 = 60.0;
pub const THEME_NAMES: [&str; 4] = ["factory", "nord", "solarized", "high-contrast"];
pub const EXIT_MODES: [&str; 3] = ["leave", "off", "breath"];
pub const LIGHTING_METHODS: [&str; 2] = ["rgbcfg", "preview"];
/// The two states that exist to fetch you flash on entry by default.
pub const DEFAULT_FLASH_STATES: [AgentState; 2] = [AgentState::Waiting, AgentState::Error];

const ACTIVITY_LIGHT_FIELDS: [&str; 8] = [
    "color",
    "effect",
    "brightness",
    "speed",
    "magic",
    "zones",
    "timeout_seconds",
    "comment",
];
const BATTERY_FIELDS: [&str; 9] = [
    "enabled",
    "threshold",
    "zone",
    "color",
    "effect",
    "brightness",
    "speed",
    "poll_seconds",
    "comment",
];
const STATE_LIGHT_FIELDS: [&str; 6] =
    ["color", "effect", "brightness", "speed", "magic", "comment"];
const REASSERT_FIELDS: [&str; 4] = ["enabled", "heartbeat_seconds", "poll_seconds", "comment"];

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateLight {
    pub color: u32,
    pub effect: u8,
    pub brightness: f64,
    pub speed: f64,
    pub magic: Option<f64>,
    /// Software blink (on phase uses `effect`).
    pub blink: bool,
}

impl StateLight {
    /// Solid, full brightness, still: the factory look for a colour.
    pub const fn solid(color: u32) -> StateLight {
        StateLight {
            color,
            effect: 1,
            brightness: 1.0,
            speed: 0.0,
            magic: None,
            blink: false,
        }
    }

    /// The all-off look (auto-dim, `on_exit: off`, empty slots).
    pub const DARK: StateLight = StateLight {
        color: 0,
        effect: 0,
        brightness: 0.0,
        speed: 0.0,
        magic: None,
        blink: false,
    };
}

/// A light shown while a binding is live, composed over the state frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityLight {
    pub light: StateLight,
    pub zones: Vec<Zone>,
    pub timeout_seconds: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReassertConfig {
    pub enabled: bool,
    /// 0 = off (the default).
    pub heartbeat_seconds: f64,
    pub poll_seconds: f64,
}

impl Default for ReassertConfig {
    fn default() -> Self {
        ReassertConfig {
            enabled: true,
            heartbeat_seconds: 0.0,
            poll_seconds: 3.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatteryConfig {
    pub enabled: bool,
    pub threshold: u8,
    pub zone: Zone,
    pub light: StateLight,
    pub poll_seconds: f64,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        BatteryConfig {
            enabled: false,
            threshold: DEFAULT_BATTERY_THRESHOLD,
            zone: Zone::Underglow,
            light: StateLight {
                color: 0xFF6D00,
                effect: 4,
                brightness: 1.0,
                speed: 0.3,
                magic: None,
                blink: false,
            },
            poll_seconds: DEFAULT_BATTERY_POLL_SECONDS,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LightingConfig {
    pub enabled: bool,
    pub zones: Vec<Zone>,
    /// `"leave"`, `"off"` or `"breath"`.
    pub on_exit: String,
    /// `"rgbcfg"` or `"preview"`.
    pub method: String,
    /// Explicit per-state overrides.
    pub states: BTreeMap<AgentState, StateLight>,
    pub theme: String,
    /// A fixed light for the underglow, whatever the agent state. On a
    /// Creator Micro 2 it also takes AG05, which shares that LED channel.
    pub underglow: Option<StateLight>,
    pub flash_on: Vec<AgentState>,
    pub battery: BatteryConfig,
    pub reassert: ReassertConfig,
    pub auto_dim_seconds: f64,
    pub auto_dim_alerts: bool,
}

impl LightingConfig {
    /// Override, else theme, else factory palette.
    pub fn light_for(&self, state: AgentState) -> StateLight {
        if let Some(explicit) = self.states.get(&state) {
            return *explicit;
        }
        theme_light(&self.theme, state).unwrap_or_else(|| factory_light(state))
    }
    pub fn drives(&self, zone: Zone) -> bool {
        self.zones.contains(&zone)
    }
}

impl Default for LightingConfig {
    fn default() -> Self {
        LightingConfig {
            enabled: false,
            zones: vec![Zone::AgentKeys],
            on_exit: "off".into(),
            method: "rgbcfg".into(),
            states: BTreeMap::new(),
            theme: String::new(),
            underglow: None,
            flash_on: DEFAULT_FLASH_STATES.to_vec(),
            battery: BatteryConfig::default(),
            reassert: ReassertConfig::default(),
            auto_dim_seconds: 180.0,
            auto_dim_alerts: false,
        }
    }
}

pub fn factory_light(state: AgentState) -> StateLight {
    StateLight::solid(match state {
        AgentState::Idle => 0xFFFFFF,
        AgentState::Working => 0x304FFE,
        AgentState::Waiting => 0xFF6D00,
        AgentState::Done => 0x00FF4C,
        AgentState::Error => 0xFF0033,
    })
}

pub fn theme_light(theme: &str, state: AgentState) -> Option<StateLight> {
    use AgentState::*;
    let themed = |color: u32, effect: u8, speed: f64, blink: bool| StateLight {
        color,
        effect,
        brightness: 1.0,
        speed,
        magic: None,
        blink,
    };
    let light = match (theme, state) {
        ("factory", s) => factory_light(s),
        ("nord", Idle) => StateLight::solid(0xD8DEE9),
        ("nord", Working) => StateLight::solid(0x5E81AC),
        ("nord", Waiting) => StateLight::solid(0xEBCB8B),
        ("nord", Done) => StateLight::solid(0xA3BE8C),
        ("nord", Error) => StateLight::solid(0xBF616A),
        ("solarized", Idle) => StateLight::solid(0x839496),
        ("solarized", Working) => StateLight::solid(0x268BD2),
        ("solarized", Waiting) => StateLight::solid(0xB58900),
        ("solarized", Done) => StateLight::solid(0x859900),
        ("solarized", Error) => StateLight::solid(0xDC322F),
        ("high-contrast", Idle) => StateLight::solid(0xFFFFFF),
        ("high-contrast", Working) => StateLight::solid(0x0072B2),
        ("high-contrast", Waiting) => themed(0xE69F00, 4, 0.3, false),
        ("high-contrast", Done) => StateLight::solid(0x009E73),
        ("high-contrast", Error) => themed(0xD55E00, 1, 0.5, true),
        _ => return None,
    };
    Some(light)
}

// -- Python coercions -------------------------------------------------------

/// Python `bool(x)`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `float(x)`.
fn float(value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(n) => n.as_f64().ok_or_else(|| format!("bad number {n}")),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => s
            .trim()
            .parse()
            .map_err(|_| format!("could not convert string to float: {s:?}")),
        other => Err(format!("float() argument must be a number, not {other}")),
    }
}

/// Python `int(x)`.
fn int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f.trunc() as i64))
            .ok_or_else(|| format!("bad number {n}")),
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::String(s) => s
            .trim()
            .parse()
            .map_err(|_| format!("invalid literal for int(): {s:?}")),
        other => Err(format!("int() argument must be a number, not {other}")),
    }
}

/// Python `str(x).lower()`.
fn lower(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_lowercase(),
        other => other.to_string().to_lowercase(),
    }
}

fn get<'a>(raw: &'a Map<String, Value>, key: &str, default: &'a Value) -> &'a Value {
    raw.get(key).unwrap_or(default)
}

fn opt_float(raw: &Map<String, Value>, key: &str, default: f64) -> Result<f64, String> {
    raw.get(key).map_or(Ok(default), float)
}

fn unknown_fields(raw: &Map<String, Value>, allowed: &[&str], skip_private: bool) -> Vec<String> {
    let mut unknown: Vec<String> = raw
        .keys()
        .filter(|k| !(skip_private && k.starts_with('_')) && !allowed.contains(&k.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown
}

fn parse_zones(raw: Option<&Value>, default: &[Zone]) -> Result<Option<Vec<Zone>>, String> {
    let list: Vec<Value> = match raw {
        None => return Ok(Some(default.to_vec())),
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(items)) if !items.is_empty() => items.clone(),
        Some(_) => return Ok(None),
    };
    let mut zones = Vec::new();
    for item in &list {
        let text = match item {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let zone = Zone::parse(&text)?;
        if !zones.contains(&zone) {
            zones.push(zone);
        }
    }
    Ok(Some(zones))
}

fn check_units(values: &[(&str, Option<f64>)]) -> Option<String> {
    values
        .iter()
        .find(|(_, v)| v.is_some_and(|v| !(0.0..=1.0).contains(&v)))
        .map(|(name, _)| name.to_string())
}

/// color/effect/brightness/speed/magic shared by state and activity lights.
fn parse_look(raw: &Map<String, Value>, default_effect: &str) -> Result<StateLight, String> {
    let color = parse_color(&raw["color"])?;
    let effect_default = Value::String(default_effect.into());
    let (effect, blink) = parse_effect_spec(get(raw, "effect", &effect_default))?;
    let brightness = opt_float(raw, "brightness", 1.0)?;
    let speed = opt_float(raw, "speed", 0.0)?;
    let magic = match raw.get("magic") {
        None | Some(Value::Null) => None,
        Some(v) => Some(float(v)?),
    };
    Ok(StateLight {
        color,
        effect,
        brightness,
        speed,
        magic,
        blink,
    })
}

fn parse_state_light(state: &str, raw: &Value) -> Result<StateLight, String> {
    let Value::Object(raw) = raw else {
        return Err(format!("lighting.states.{state} must be an object"));
    };
    if !raw.contains_key("color") {
        return Err(format!("lighting.states.{state} needs a \"color\""));
    }
    let unknown = unknown_fields(raw, &STATE_LIGHT_FIELDS, false);
    if !unknown.is_empty() {
        return Err(format!(
            "lighting.states.{state} has unknown field(s): {}",
            unknown.join(", ")
        ));
    }
    let light = parse_look(raw, "solid").map_err(|e| format!("lighting.states.{state}: {e}"))?;
    if let Some(name) = check_units(&[
        ("brightness", Some(light.brightness)),
        ("speed", Some(light.speed)),
        ("magic", light.magic),
    ]) {
        return Err(format!(
            "lighting.states.{state}.{name} must be between 0 and 1"
        ));
    }
    Ok(light)
}

fn parse_reassert(raw: Option<&Value>) -> Result<ReassertConfig, String> {
    let defaults = ReassertConfig::default();
    let raw = match raw {
        None | Some(Value::Null) => return Ok(defaults),
        Some(Value::Object(raw)) => raw,
        Some(_) => return Err("\"lighting.reassert\" must be an object".into()),
    };
    let unknown = unknown_fields(raw, &REASSERT_FIELDS, false);
    if !unknown.is_empty() {
        return Err(format!(
            "lighting.reassert has unknown field(s): {}",
            unknown.join(", ")
        ));
    }
    let heartbeat = opt_float(raw, "heartbeat_seconds", defaults.heartbeat_seconds)
        .map_err(|e| format!("lighting.reassert: {e}"))?;
    let poll = opt_float(raw, "poll_seconds", defaults.poll_seconds)
        .map_err(|e| format!("lighting.reassert: {e}"))?;
    if heartbeat < 0.0 {
        return Err("lighting.reassert.heartbeat_seconds must be 0 (off) or more".into());
    }
    if poll <= 0.0 {
        return Err("lighting.reassert.poll_seconds must be more than 0".into());
    }
    Ok(ReassertConfig {
        enabled: raw.get("enabled").map_or(defaults.enabled, truthy),
        heartbeat_seconds: heartbeat,
        poll_seconds: poll,
    })
}

fn parse_auto_dim(raw: &Value) -> Result<f64, String> {
    match raw {
        Value::Null | Value::Bool(false) => return Ok(0.0),
        Value::String(s)
            if ["off", "never", "none", ""].contains(&s.trim().to_lowercase().as_str()) =>
        {
            return Ok(0.0)
        }
        _ => {}
    }
    let seconds = float(raw).map_err(|_| {
        format!(
            "\"lighting.auto_dim_seconds\" must be a number of seconds (0 or 'off' never dims), got {raw}"
        )
    })?;
    if seconds < 0.0 {
        return Err("\"lighting.auto_dim_seconds\" must be 0 (never dim) or more".into());
    }
    Ok(seconds)
}

fn parse_flash_on(raw: Option<&Value>) -> Result<Vec<AgentState>, String> {
    let names: Vec<Value> = match raw {
        None | Some(Value::Null) => return Ok(DEFAULT_FLASH_STATES.to_vec()),
        Some(Value::Bool(false)) => return Ok(Vec::new()),
        Some(Value::String(s))
            if ["off", "none", "never", ""].contains(&s.trim().to_lowercase().as_str()) =>
        {
            return Ok(Vec::new())
        }
        Some(Value::String(s)) => vec![Value::String(s.clone())],
        Some(Value::Array(items)) => items.clone(),
        Some(_) => {
            return Err("\"lighting.flash_on\" must be a list of state names, or false to turn the entry flash off".into())
        }
    };
    let mut states = Vec::new();
    for name in &names {
        let state = name.as_str().and_then(AgentState::parse).ok_or_else(|| {
            format!(
                "lighting.flash_on has unknown state {name}; expected done, error, idle, waiting, working"
            )
        })?;
        if !states.contains(&state) {
            states.push(state);
        }
    }
    Ok(states)
}

fn parse_battery(raw: Option<&Value>) -> Result<BatteryConfig, String> {
    let defaults = BatteryConfig::default();
    let raw = match raw {
        None | Some(Value::Null) => return Ok(defaults),
        Some(Value::Object(raw)) => raw,
        Some(_) => return Err("\"lighting.battery\" must be an object".into()),
    };
    let unknown = unknown_fields(raw, &BATTERY_FIELDS, true);
    if !unknown.is_empty() {
        return Err(format!(
            "lighting.battery has unknown field(s): {}; it takes: {}",
            unknown.join(", "),
            BATTERY_FIELDS.join(", ")
        ));
    }
    let parsed = (|| -> Result<_, String> {
        let threshold = raw
            .get("threshold")
            .map_or(Ok(i64::from(defaults.threshold)), int)?;
        let zone = match raw.get("zone") {
            None => Zone::Underglow,
            Some(Value::String(s)) => Zone::parse(s)?,
            Some(other) => Zone::parse(&other.to_string())?,
        };
        let color = raw
            .get("color")
            .map_or(Ok(defaults.light.color), parse_color)?;
        let breath = Value::String("breath".into());
        let (effect, blink) = parse_effect_spec(get(raw, "effect", &breath))?;
        let brightness = opt_float(raw, "brightness", defaults.light.brightness)?;
        let speed = opt_float(raw, "speed", defaults.light.speed)?;
        let poll = opt_float(raw, "poll_seconds", defaults.poll_seconds)?;
        Ok((
            threshold, zone, color, effect, blink, brightness, speed, poll,
        ))
    })()
    .map_err(|e| format!("lighting.battery: {e}"))?;
    let (threshold, zone, color, effect, blink, brightness, speed, poll_seconds) = parsed;
    if !(0..=100).contains(&threshold) {
        return Err("\"lighting.battery.threshold\" is a percent, so it must be 0-100".into());
    }
    if let Some(name) = check_units(&[("brightness", Some(brightness)), ("speed", Some(speed))]) {
        return Err(format!(
            "\"lighting.battery.{name}\" must be between 0 and 1"
        ));
    }
    if poll_seconds <= 0.0 {
        return Err("\"lighting.battery.poll_seconds\" must be more than 0".into());
    }
    Ok(BatteryConfig {
        enabled: raw.get("enabled").map_or(defaults.enabled, truthy),
        threshold: threshold as u8,
        zone,
        light: StateLight {
            color,
            effect,
            brightness,
            speed,
            magic: None,
            blink,
        },
        poll_seconds,
    })
}

/// Parse the `lighting` block (absent = default). Non-fatal issues go to
/// `warnings` (Python has none for this block today).
pub fn parse_lighting(raw: &Value, _warnings: &mut Vec<String>) -> Result<LightingConfig, String> {
    let defaults = LightingConfig::default();
    let raw = match raw {
        Value::Null => return Ok(defaults),
        Value::Object(raw) => raw,
        _ => return Err("\"lighting\" must be an object".into()),
    };
    let zones = parse_zones(raw.get("zones"), &defaults.zones)
        .map_err(|e| format!("lighting.zones: {e}"))?
        .ok_or("\"lighting.zones\" must be a non-empty list")?;
    let on_exit = raw.get("on_exit").map_or(defaults.on_exit.clone(), lower);
    if !EXIT_MODES.contains(&on_exit.as_str()) {
        return Err(format!(
            "lighting.on_exit must be one of {}",
            EXIT_MODES.join(", ")
        ));
    }
    let method = raw.get("method").map_or(defaults.method.clone(), lower);
    if !LIGHTING_METHODS.contains(&method.as_str()) {
        return Err(format!(
            "lighting.method must be one of {}",
            LIGHTING_METHODS.join(", ")
        ));
    }
    let auto_dim = match raw.get("auto_dim_seconds") {
        None => defaults.auto_dim_seconds,
        Some(v) => parse_auto_dim(v)?,
    };
    let theme = match raw.get("theme") {
        Some(v) if truthy(v) => lower(v).trim().to_string(),
        _ => String::new(),
    };
    if !theme.is_empty() && !THEME_NAMES.contains(&theme.as_str()) {
        return Err(format!(
            "lighting.theme must be one of {}, or omitted",
            THEME_NAMES.join(", ")
        ));
    }
    let flash_on = parse_flash_on(raw.get("flash_on"))?;
    let battery = parse_battery(raw.get("battery"))?;
    let mut states = BTreeMap::new();
    match raw.get("states") {
        Some(v) if truthy(v) => {
            let Value::Object(entries) = v else {
                return Err("\"lighting.states\" must be an object".into());
            };
            for (name, value) in entries {
                let state = AgentState::parse(name).ok_or_else(|| {
                    format!(
                        "lighting.states has unknown state {name:?}; expected done, error, idle, waiting, working"
                    )
                })?;
                states.insert(state, parse_state_light(name, value)?);
            }
        }
        _ => {}
    }
    Ok(LightingConfig {
        enabled: raw.get("enabled").map_or(defaults.enabled, truthy),
        zones,
        on_exit,
        method,
        states,
        theme,
        underglow: match raw.get("underglow") {
            Some(v) if truthy(v) => Some(parse_state_light("underglow", v)?),
            _ => None,
        },
        flash_on,
        battery,
        reassert: parse_reassert(raw.get("reassert"))?,
        auto_dim_seconds: auto_dim,
        auto_dim_alerts: raw
            .get("auto_dim_alerts")
            .map_or(defaults.auto_dim_alerts, truthy),
    })
}

/// Parse a binding's `light` field.
pub fn parse_activity_light(input_id: &str, raw: &Value) -> Result<ActivityLight, String> {
    let momentary: Vec<&str> = input_id
        .split(CHORD_SEPARATOR)
        .map(str::trim)
        .filter(|m| ENCODER_TICKS.contains(m) || JOYSTICK_INPUTS.contains(m))
        .collect();
    if !momentary.is_empty() {
        return Err(format!(
            "binding {input_id:?} cannot take a \"light\": {} report one event and no release, so nothing would ever turn the light off again. A light belongs on an input you can hold.",
            momentary.join(", ")
        ));
    }
    let Value::Object(raw) = raw else {
        return Err(format!(
            "binding {input_id:?}: \"light\" must be an object, e.g. {{\"color\": \"#2E8B57\", \"effect\": \"snake\"}}"
        ));
    };
    if !raw.contains_key("color") {
        return Err(format!("binding {input_id:?}: \"light\" needs a \"color\""));
    }
    let unknown = unknown_fields(raw, &ACTIVITY_LIGHT_FIELDS, true);
    if !unknown.is_empty() {
        return Err(format!(
            "binding {input_id:?}: \"light\" has unknown field(s): {}; it takes: {}",
            unknown.join(", "),
            ACTIVITY_LIGHT_FIELDS.join(", ")
        ));
    }
    let parsed = (|| -> Result<_, String> {
        let zones = parse_zones(raw.get("zones"), &[Zone::Underglow])?;
        let light = parse_look(raw, "solid")?;
        let timeout = opt_float(raw, "timeout_seconds", DEFAULT_ACTIVITY_TIMEOUT)?;
        Ok((zones, light, timeout))
    })();
    let (zones, light, timeout) = match parsed {
        Ok((Some(zones), light, timeout)) => (zones, light, timeout),
        Ok((None, ..)) => {
            return Err(format!(
                "binding {input_id:?}: \"light.zones\" must be a non-empty list"
            ))
        }
        Err(e) => return Err(format!("binding {input_id:?}: light: {e}")),
    };
    if let Some(name) = check_units(&[
        ("brightness", Some(light.brightness)),
        ("speed", Some(light.speed)),
        ("magic", light.magic),
    ]) {
        return Err(format!(
            "binding {input_id:?}: light.{name} must be between 0 and 1"
        ));
    }
    if !(timeout > 0.0 && timeout <= ACTIVITY_TIMEOUT_MAX) {
        return Err(format!(
            "binding {input_id:?}: \"light.timeout_seconds\" must be more than 0 and at most {ACTIVITY_TIMEOUT_MAX}. There is no 'never' here on purpose: a Bluetooth drop mid-hold loses the key-up, and a light with nothing to end it would then claim the key was still down until you restarted FreeMicro."
        ));
    }
    Ok(ActivityLight {
        light,
        zones,
        timeout_seconds: timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lighting(raw: Value) -> Result<LightingConfig, String> {
        parse_lighting(&raw, &mut Vec::new())
    }

    #[test]
    fn absent_lighting_is_the_default() {
        let config = lighting(Value::Null).unwrap();
        assert_eq!(config, LightingConfig::default());
        assert!(!config.enabled);
        assert_eq!(config.zones, vec![Zone::AgentKeys]);
        assert_eq!(config.flash_on, DEFAULT_FLASH_STATES.to_vec());
    }

    #[test]
    fn light_for_prefers_explicit_then_theme_then_factory() {
        let config = lighting(json!({
            "theme": "Nord",
            "states": {"error": {"color": "#123456", "effect": "blink"}},
        }))
        .unwrap();
        let error = config.light_for(AgentState::Error);
        assert_eq!(
            (error.color, error.effect, error.blink),
            (0x123456, 1, true)
        );
        assert_eq!(config.light_for(AgentState::Idle).color, 0xD8DEE9);
        let plain = LightingConfig::default();
        assert_eq!(plain.light_for(AgentState::Waiting).color, 0xFF6D00);
    }

    #[test]
    fn the_factory_palette_is_the_documented_one() {
        let colors: Vec<u32> = AgentState::ALL
            .iter()
            .map(|s| factory_light(*s).color)
            .collect();
        assert_eq!(colors, [0xFFFFFF, 0x304FFE, 0xFF6D00, 0x00FF4C, 0xFF0033]);
        assert!(theme_light("", AgentState::Idle).is_none());
        assert!(
            theme_light("high-contrast", AgentState::Error)
                .unwrap()
                .blink
        );
    }

    #[test]
    fn parse_lighting_reads_the_full_block() {
        let config = lighting(json!({
            "enabled": true,
            "zones": ["keys", "underglow", "keys"],
            "on_exit": "Breath",
            "method": "preview",
            "auto_dim_seconds": "off",
            "flash_on": "done",
            "battery": {"enabled": true, "threshold": 20, "zone": "agent_keys", "_note": 1},
            "reassert": {"heartbeat_seconds": 5},
            "auto_dim_alerts": 1,
        }))
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.zones, vec![Zone::Backlight, Zone::Underglow]);
        assert_eq!(config.on_exit, "breath");
        assert_eq!(config.method, "preview");
        assert_eq!(config.auto_dim_seconds, 0.0);
        assert_eq!(config.flash_on, vec![AgentState::Done]);
        assert_eq!(config.battery.threshold, 20);
        assert_eq!(config.battery.zone, Zone::AgentKeys);
        assert_eq!(config.reassert.heartbeat_seconds, 5.0);
        assert!(config.auto_dim_alerts);
    }

    #[test]
    fn flash_on_false_turns_the_flash_off() {
        assert!(lighting(json!({"flash_on": false}))
            .unwrap()
            .flash_on
            .is_empty());
    }

    #[test]
    fn parse_lighting_rejects_bad_values() {
        for raw in [
            json!([]),
            json!({"zones": []}),
            json!({"zones": ["lid"]}),
            json!({"on_exit": "explode"}),
            json!({"method": "magic"}),
            json!({"theme": "neon"}),
            json!({"auto_dim_seconds": -1}),
            json!({"flash_on": ["sleepy"]}),
            json!({"flash_on": 3}),
            json!({"states": {"busy": {"color": "#fff"}}}),
            json!({"states": {"idle": {"effect": "solid"}}}),
            json!({"states": {"idle": {"color": "#fff", "hue": 3}}}),
            json!({"states": {"idle": {"color": "#fff", "brightness": 2}}}),
            json!({"reassert": {"poll_seconds": 0}}),
            json!({"reassert": {"heartbeat_seconds": -1}}),
            json!({"reassert": {"often": true}}),
            json!({"battery": {"threshold": 120}}),
            json!({"battery": {"speed": 3}}),
            json!({"battery": {"poll_seconds": 0}}),
            json!({"battery": {"volts": 3}}),
        ] {
            assert!(lighting(raw.clone()).is_err(), "{raw}");
        }
    }

    #[test]
    fn parse_activity_light_defaults_to_the_underglow() {
        let light = parse_activity_light(
            "ACT06",
            &json!({"color": "#2E8B57", "effect": "snake", "speed": 0.4}),
        )
        .unwrap();
        assert_eq!(light.zones, vec![Zone::Underglow]);
        assert_eq!(light.timeout_seconds, DEFAULT_ACTIVITY_TIMEOUT);
        assert_eq!((light.light.color, light.light.effect), (0x2E8B57, 2));
    }

    #[test]
    fn parse_activity_light_rejects_bad_lights() {
        for (input, raw) in [
            ("ENC_CW", json!({"color": "#fff"})),
            ("AG00+JOY_UP", json!({"color": "#fff"})),
            ("ACT06", json!("red")),
            ("ACT06", json!({"effect": "snake"})),
            ("ACT06", json!({"color": "#fff", "glow": 1})),
            ("ACT06", json!({"color": "#fff", "zones": []})),
            ("ACT06", json!({"color": "#fff", "magic": 1.5})),
            ("ACT06", json!({"color": "#fff", "timeout_seconds": 0})),
            ("ACT06", json!({"color": "#fff", "timeout_seconds": 601})),
        ] {
            assert!(parse_activity_light(input, &raw).is_err(), "{input} {raw}");
        }
    }
}
