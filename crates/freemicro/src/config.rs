//! `~/.freemicro` location and `config.json`. Port of `freemicro/config.py`.
//!
//! Owner: state. Library code takes `home: &Path` explicitly; only the CLI,
//! the run loop and the hook entry point call [`config_home`].

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::state::DecayPolicy;

pub const ENV_HOME: &str = "FREEMICRO_HOME";
pub const CONFIG_FILENAME: &str = "config.json";

/// `$FREEMICRO_HOME`, else `~/.freemicro`. Same rule as Python.
pub fn config_home() -> PathBuf {
    match std::env::var_os(ENV_HOME) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".freemicro"),
    }
}

/// `<home>/state`: one JSON file per session.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join("state")
}

/// The parts of `config.json` the runtime reads. A broken or missing file
/// yields the defaults; it never stops the daemon.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The `state` block's four timers.
    pub decay: DecayPolicy,
    /// The whole merged document (defaults deep-merged with the file).
    pub raw: Value,
}

impl Config {
    /// Load `<home>/config.json`, deep-merged over the Python defaults.
    pub fn load(home: &Path) -> Config {
        let d = DecayPolicy::default();
        let mut raw = serde_json::json!({
            "renderers": {"prefer": null, "chime": true},
            "state": {
                "ttl_seconds": d.ttl_seconds,
                "done_ttl_seconds": d.done_ttl_seconds,
                "working_ttl_seconds": d.working_ttl_seconds,
                "tool_ttl_seconds": d.tool_ttl_seconds,
            },
            "palette": {},
        });
        // A broken config must never stop the light: anything unreadable or
        // not an object leaves the defaults.
        if let Some(loaded @ Value::Object(_)) = std::fs::read(home.join(CONFIG_FILENAME))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            deep_merge(&mut raw, loaded);
        }
        Config {
            decay: DecayPolicy::from_state_block(&raw["state"]),
            raw,
        }
    }
}

/// Python `_deep_merge`: objects merge key by key, anything else replaces.
fn deep_merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot @ Value::Object(_)) if value.is_object() => deep_merge(slot, value),
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn load_merges_file_over_defaults_and_survives_garbage() {
        let home = TempDir::new();
        let defaults = Config::load(home.path());
        assert_eq!(defaults.decay, DecayPolicy::default());
        assert_eq!(defaults.raw["renderers"]["chime"], true);

        std::fs::write(
            home.path().join(CONFIG_FILENAME),
            r#"{"state": {"done_ttl_seconds": 5, "working_ttl_seconds": -3, "tool_ttl_seconds": "x"}, "extra": 1}"#,
        )
        .unwrap();
        let cfg = Config::load(home.path());
        assert_eq!(cfg.decay.done_ttl_seconds, 5.0);
        assert_eq!(cfg.decay.working_ttl_seconds, 0.0);
        assert_eq!(cfg.decay.tool_ttl_seconds, 600.0);
        assert_eq!(cfg.decay.ttl_seconds, 1800.0);
        assert_eq!(cfg.raw["extra"], 1);
        assert_eq!(cfg.raw["renderers"]["chime"], true);

        std::fs::write(home.path().join(CONFIG_FILENAME), "{not json").unwrap();
        assert_eq!(Config::load(home.path()).decay, DecayPolicy::default());
    }
}
