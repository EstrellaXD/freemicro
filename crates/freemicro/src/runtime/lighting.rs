//! LED wire builders: effects, colours, `v.oai.thstatus`, `v.oai.rgbcfg`,
//! `lights.preview`. Pure. Port of `device/lighting.py`.
//!
//! Key order inside each object is the Python order, so the bytes on the wire
//! (and the renderer's dedupe key) match the Python build exactly.

use serde_json::{json, Map, Value};

pub const METHOD_PREVIEW: &str = "lights.preview";
pub const METHOD_THREAD_STATUS: &str = "v.oai.thstatus";
pub const METHOD_RGBCFG: &str = "v.oai.rgbcfg";
pub const AGENT_KEY_COUNT: usize = 6;
/// Firmware effect ids.
pub const EFFECTS: [(&str, u8); 7] = [
    ("off", 0),
    ("solid", 1),
    ("snake", 2),
    ("rainbow", 3),
    ("breath", 4),
    ("gradient", 5),
    ("shallow-breath", 6),
];
/// Software effect driven by the render loop; never a firmware id.
pub const SOFT_BLINK: &str = "blink";

const EFFECT_ALIASES: [(&str, &str); 7] = [
    ("static", "solid"),
    ("on", "solid"),
    ("breathe", "breath"),
    ("breathing", "breath"),
    ("shallowbreath", "shallow-breath"),
    ("shallow_breath", "shallow-breath"),
    ("pulse", "shallow-breath"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Zone {
    Backlight,
    Underglow,
    AgentKeys,
}

impl Zone {
    pub const ALL: [Zone; 3] = [Zone::Backlight, Zone::Underglow, Zone::AgentKeys];

    /// `"backlight"`, `"underglow"`, `"agent_keys"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Zone::Backlight => "backlight",
            Zone::Underglow => "underglow",
            Zone::AgentKeys => "agent_keys",
        }
    }

    /// Accepts the Python aliases.
    pub fn parse(text: &str) -> Result<Zone, String> {
        let zone = text.trim().to_lowercase().replace('-', "_");
        match zone.as_str() {
            "backlight" | "keys" | "keycaps" => Ok(Zone::Backlight),
            "underglow" | "ambient" | "base" | "glow" => Ok(Zone::Underglow),
            "agent_keys" | "agentkeys" | "agent" => Ok(Zone::AgentKeys),
            _ => Err(format!(
                "unknown lighting zone {text:?}; expected one of backlight, underglow, agent_keys"
            )),
        }
    }
}

fn effect_names() -> String {
    let mut names: Vec<&str> = EFFECTS.iter().map(|(n, _)| *n).collect();
    names.sort();
    names.join(", ")
}

/// Name, alias or integer -> firmware id. Rejects `blink`.
pub fn parse_effect(value: &Value) -> Result<u8, String> {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            let id = n.as_i64().unwrap_or(-1);
            EFFECTS
                .iter()
                .find(|(_, v)| i64::from(*v) == id)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    format!("unknown effect id {id}; valid ids are [0, 1, 2, 3, 4, 5, 6]")
                })
        }
        Value::String(text) => {
            let key = text.trim().to_lowercase().replace(' ', "-");
            let alias = |k: &str| {
                EFFECT_ALIASES
                    .iter()
                    .find(|(a, _)| *a == k)
                    .map(|(_, c)| c.to_string())
            };
            let key = alias(&key)
                .or_else(|| alias(&key.replace('-', "_")))
                .unwrap_or(key);
            EFFECTS
                .iter()
                .find(|(n, _)| *n == key)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    format!(
                        "unknown effect {text:?}; expected one of {}",
                        effect_names()
                    )
                })
        }
        Value::Bool(_) => Err(format!("effect must be a name or number, got {value}")),
        other => Err(format!(
            "unknown effect {other}; expected one of {}",
            effect_names()
        )),
    }
}

/// -> (firmware id, software blink). `blink` = (solid, true).
pub fn parse_effect_spec(value: &Value) -> Result<(u8, bool), String> {
    if let Value::String(text) = value {
        let key = text.trim().to_lowercase().replace([' ', '_'], "-");
        if key == SOFT_BLINK {
            return Ok((1, true));
        }
    }
    Ok((parse_effect(value)?, false))
}

pub fn effect_name(effect: u8) -> String {
    EFFECTS
        .iter()
        .find(|(_, v)| *v == effect)
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| effect.to_string())
}

/// `"#RRGGBB"`, `"RRGGBB"`, `[r,g,b]`, or an int -> packed 0xRRGGBB.
pub fn parse_color(value: &Value) -> Result<u32, String> {
    match value {
        Value::Bool(_) => Err(format!(
            "colour must be a hex string, [r,g,b] or int: {value}"
        )),
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            let c = n.as_i64().unwrap_or(-1);
            if (0..=0xFF_FFFF).contains(&c) {
                Ok(c as u32)
            } else {
                Err(format!("colour {n} is outside 0x000000-0xFFFFFF"))
            }
        }
        Value::String(raw) => {
            let mut text = raw.trim().trim_start_matches('#').to_string();
            if text.to_lowercase().starts_with("0x") {
                text = text[2..].to_string();
            }
            if text.chars().count() == 3 {
                text = text.chars().flat_map(|c| [c, c]).collect();
            }
            if text.len() == 6 {
                if let Ok(c) = u32::from_str_radix(&text, 16) {
                    return Ok(c);
                }
            }
            Err(format!("colour {raw:?} is not a #RRGGBB hex string"))
        }
        Value::Array(parts) if parts.len() == 3 => {
            let mut rgb = [0i64; 3];
            for (slot, part) in rgb.iter_mut().zip(parts) {
                *slot = match part {
                    Value::Number(n) => n.as_f64().map(|f| f.trunc() as i64),
                    Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                }
                .ok_or_else(|| format!("colour {value} must be three integers"))?;
            }
            if !rgb.iter().all(|c| (0..=255).contains(c)) {
                return Err(format!("colour {value} components must be 0-255"));
            }
            Ok(((rgb[0] << 16) | (rgb[1] << 8) | rgb[2]) as u32)
        }
        other => Err(format!("cannot read colour {other}")),
    }
}

pub fn color_to_hex(color: u32) -> String {
    format!("#{:06X}", color & 0xFF_FFFF)
}

/// One `thstatus` entry for Agent Key `index`.
pub fn thread_entry(index: usize, color: u32, effect: u8, brightness: f64, speed: f64) -> Value {
    json!({ "id": index, "c": color, "b": brightness, "e": effect, "s": speed })
}

/// `{"m":"v.oai.thstatus","p":[entries]}`.
pub fn thstatus_message(entries: Vec<Value>) -> Value {
    json!({ "m": METHOD_THREAD_STATUS, "p": entries })
}

pub fn all_agent_keys(color: u32, effect: u8, brightness: f64, speed: f64) -> Value {
    thstatus_message(
        (0..AGENT_KEY_COUNT)
            .map(|i| thread_entry(i, color, effect, brightness, speed))
            .collect(),
    )
}

/// `{e,b,s,c}` short-key side.
pub fn rgbcfg_side(color: u32, effect: u8, brightness: f64, speed: f64) -> Value {
    json!({ "e": effect, "b": brightness, "s": speed, "c": color })
}

/// `{"m":"v.oai.rgbcfg","p":{"ambient":..,"keys":..}}`.
pub fn rgbcfg_message(ambient: Option<Value>, keys: Option<Value>) -> Value {
    let mut params = Map::new();
    if let Some(ambient) = ambient {
        params.insert("ambient".into(), ambient);
    }
    if let Some(keys) = keys {
        params.insert("keys".into(), keys);
    }
    json!({ "m": METHOD_RGBCFG, "p": params })
}

/// `{effect,brightness,speed,color,magic?}` long-key zone.
pub fn preview_zone(
    color: u32,
    effect: u8,
    brightness: f64,
    speed: f64,
    magic: Option<f64>,
) -> Value {
    let mut zone = json!({
        "effect": effect, "brightness": brightness, "speed": speed, "color": color,
    });
    if let Some(magic) = magic {
        zone["magic"] = json!(magic);
    }
    zone
}

pub fn preview_message(
    backlight: Option<Value>,
    underglow: Option<Value>,
    request_id: u32,
) -> Value {
    let mut params = Map::new();
    if let Some(backlight) = backlight {
        params.insert("backlight".into(), backlight);
    }
    if let Some(underglow) = underglow {
        params.insert("underglow".into(), underglow);
    }
    json!({ "m": METHOD_PREVIEW, "p": params, "id": request_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_effect_accepts_names_aliases_and_ids() {
        for (raw, id) in [
            (json!("solid"), 1),
            (json!("Breathing"), 4),
            (json!("shallow breath"), 6),
            (json!("pulse"), 6),
            (json!(3), 3),
        ] {
            assert_eq!(parse_effect(&raw), Ok(id), "{raw}");
        }
    }

    #[test]
    fn parse_effect_rejects_unknowns_bools_and_blink() {
        for raw in [json!("blink"), json!(true), json!(9), json!("sparkle")] {
            assert!(parse_effect(&raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn parse_effect_spec_splits_blink() {
        assert_eq!(parse_effect_spec(&json!("Blink")), Ok((1, true)));
        assert_eq!(parse_effect_spec(&json!("snake")), Ok((2, false)));
    }

    #[test]
    fn parse_color_reads_every_spelling() {
        for (raw, c) in [
            (json!("#FF6D00"), 0xFF6D00),
            (json!("0x00ff4c"), 0x00FF4C),
            (json!("#f0a"), 0xFF00AA),
            (json!([255, 0, 51]), 0xFF0033),
            (json!(16711680), 0xFF0000),
        ] {
            assert_eq!(parse_color(&raw), Ok(c), "{raw}");
        }
    }

    #[test]
    fn parse_color_rejects_bad_values() {
        for raw in [
            json!("#GGGGGG"),
            json!(0x1000000),
            json!([256, 0, 0]),
            json!(false),
            json!([1, 2]),
        ] {
            assert!(parse_color(&raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn zone_parse_accepts_aliases() {
        assert_eq!(Zone::parse("Keys"), Ok(Zone::Backlight));
        assert_eq!(Zone::parse("ambient"), Ok(Zone::Underglow));
        assert_eq!(Zone::parse("agent-keys"), Ok(Zone::AgentKeys));
        assert!(Zone::parse("lid").is_err());
    }

    #[test]
    fn builders_keep_the_python_key_order() {
        let text = |v: &Value| serde_json::to_string(v).unwrap();
        assert_eq!(
            text(&thstatus_message(vec![thread_entry(
                2, 0x304FFE, 1, 1.0, 0.0
            )])),
            r#"{"m":"v.oai.thstatus","p":[{"id":2,"c":3166206,"b":1.0,"e":1,"s":0.0}]}"#
        );
        assert_eq!(
            text(&rgbcfg_message(
                Some(rgbcfg_side(0, 0, 0.0, 0.0)),
                Some(rgbcfg_side(1, 1, 1.0, 0.5))
            )),
            r#"{"m":"v.oai.rgbcfg","p":{"ambient":{"e":0,"b":0.0,"s":0.0,"c":0},"keys":{"e":1,"b":1.0,"s":0.5,"c":1}}}"#
        );
        assert_eq!(
            text(&preview_message(
                Some(preview_zone(1, 1, 1.0, 0.0, Some(0.5))),
                None,
                7
            )),
            r#"{"m":"lights.preview","p":{"backlight":{"effect":1,"brightness":1.0,"speed":0.0,"color":1,"magic":0.5}},"id":7}"#
        );
    }
}
