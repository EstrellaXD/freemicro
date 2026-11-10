//! The shared slot-assignment cache, `<home>/slots.json` =
//! `{"slots": [6 paths]}`. Port of `state/slots.py`. Python and Rust share it,
//! so a key press resolves to the project its LED shows.

use std::path::{Path, PathBuf};

use super::agentkeys::SLOT_COUNT;

pub const FILENAME: &str = "slots.json";

pub fn slots_path(home: &Path) -> PathBuf {
    home.join(FILENAME)
}

/// Always `SLOT_COUNT` entries; `""` for unknown. Never fails.
pub fn load(path: &Path) -> [String; SLOT_COUNT] {
    let mut out: [String; SLOT_COUNT] = Default::default();
    let data = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    if let Some(raw) = data.as_ref().and_then(|d| d.get("slots")?.as_array()) {
        for (slot, entry) in out.iter_mut().zip(raw) {
            *slot = entry.as_str().unwrap_or_default().to_string();
        }
    }
    out
}

/// Atomic write. `false` if it could not be written.
pub fn save(path: &Path, slots: &[String]) -> bool {
    let payload = serde_json::json!({ "slots": &slots[..slots.len().min(SLOT_COUNT)] });
    let Some(dir) = path.parent() else {
        return false;
    };
    let tmp = dir.join(format!(".{FILENAME}.{}.tmp", std::process::id()));
    let ok = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, payload.to_string()))
        .and_then(|()| std::fs::rename(&tmp, path))
        .is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
    }
    ok
}

pub fn clear(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn cache_round_trips_and_survives_corruption() {
        let home = TempDir::new();
        let path = slots_path(home.path());
        assert_eq!(load(&path), <[String; SLOT_COUNT]>::default());
        let slots: Vec<String> = ["/a", "", "/b"].map(String::from).to_vec();
        assert!(save(&path, &slots));
        let loaded = load(&path);
        assert_eq!(&loaded[..3], &slots[..]);
        assert_eq!(loaded[5], "");
        for junk in [
            "{nope",
            r#"{"slots": "x"}"#,
            r#"[1]"#,
            r#"{"slots": [1, "/c"]}"#,
        ] {
            std::fs::write(&path, junk).unwrap();
            let got = load(&path);
            assert_eq!(got[0], "", "{junk}");
        }
        assert_eq!(load(&path)[1], "/c");
        clear(&path);
        clear(&path);
        assert!(!path.exists());
    }
}
