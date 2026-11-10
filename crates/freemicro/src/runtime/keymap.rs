//! `keymap pull | push`: edit a Creator Micro 2's own keymap.json. Port of
//! `padkeymap.py`. Push gates, in order: refuse while Input or ChatGPT run;
//! pause the daemon and take the pad lock; back up the current file; write
//! once, verify SHA-1, then `device.status`. `push --test` writes the pad's
//! own bytes back.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::daemon::{self, Launchctl, PadLock};
use super::run::{PadSource, SystemPads};
use crate::device::padfs::{PadFs, KEYMAP};
use crate::device::{self, frame::to_ascii_json};

/// Work run while holding the pad: (pad files, log sink) -> exit code.
pub type PadWork<'a> = dyn FnMut(&mut PadFs<'_>, &mut dyn FnMut(&str)) -> i32 + 'a;

pub const SETTLE: Duration = Duration::from_millis(2500);
pub const LOCK_WAIT: Duration = Duration::from_secs(10);
const STATUS_ID: u32 = 900;
const LAYER_PROBE_ID: u32 = 904;

/// Process → 1-based pad layer, in layer order.
pub type LayerMap = Vec<(String, u64)>;

/// `(process, layer)` for each layer of the active profile linked to an app
/// (layer 1-based, as `kb.pl.set` and `layer_index` count). A process linked
/// to two layers keeps the first, as the firmware's own match does.
pub fn layer_map(keymap: &Value) -> LayerMap {
    let active = keymap["activeProfileId"].as_u64().unwrap_or(0) as usize;
    let apps = keymap["linkedApps"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let layers = keymap["profiles"][active]["layers"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut map = LayerMap::new();
    for (index, layer) in layers.iter().enumerate() {
        let process = apps
            .iter()
            .find(|app| app["id"].as_u64().is_some() && app["id"] == layer["linkedAppId"])
            .and_then(|app| app["process"].as_str());
        if let Some(process) = process.filter(|p| !map.iter().any(|(q, _)| q == p)) {
            map.push((process.to_string(), index as u64 + 1));
        }
    }
    map
}

/// The pad's app → layer map when it takes `kb.pl.set` (firmware 0.7.0+);
/// `None` on older firmware (404) or an unreadable keymap, where the caller
/// keeps `host.focused_app`. An empty `kb.pl.set` only fails validation, so
/// the probe never moves the pad. Key presses during the ~1 s read are lost.
/// The keymap read is also saved to `cache` (the status window's copy).
pub fn read_layer_map(io: &mut dyn device::PadIo, cache: Option<&Path>) -> Option<LayerMap> {
    let probe = device::Request {
        method: crate::input::frontmost::METHOD_SET_LAYER,
        params: Some(serde_json::json!({})),
        id: LAYER_PROBE_ID,
        timeout: Duration::from_millis(1500),
        match_id: true,
        standard: false,
    };
    let reply = device::request(io, &probe, &mut |_| {}).ok()??;
    if reply["error"]["code"] == 404 {
        return None;
    }
    let bytes = PadFs::new(io).read(KEYMAP).ok()?;
    if let Some(cache) = cache {
        let _ = std::fs::write(cache, &bytes);
    }
    Some(layer_map(&serde_json::from_slice(&bytes).ok()?))
}

/// `<home>/pad-keymap.json`: the keymap last pushed to or pulled from the pad,
/// so the status window can name layers without opening the pad.
pub fn cache_path(home: &Path) -> PathBuf {
    home.join("pad-keymap.json")
}

/// `<home>/keymap-backups`.
pub fn backup_dir(home: &Path) -> PathBuf {
    home.join("keymap-backups")
}

fn running(pattern: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", pattern])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Running apps that talk to the pad (`"Work Louder Input"`, `"ChatGPT"`).
pub fn other_hosts() -> Vec<String> {
    let mut found = Vec::new();
    if running("input.app/Contents/MacOS/input") {
        found.push("Work Louder Input".to_string());
    }
    if running("ChatGPT.app/Contents/MacOS/ChatGPT") {
        found.push("ChatGPT".to_string());
    }
    found
}

/// The refusal line when another host is running, else `None`.
pub fn host_refusal(hosts: &[String]) -> Option<String> {
    (!hosts.is_empty()).then(|| {
        format!(
            "Refusing to write: {} is running and talks to the pad.\n  Quit it first. Two hosts during a write can brick the pad.",
            hosts.join(" and ")
        )
    })
}

/// Compact, ASCII-escaped JSON in the document's key order (what Input writes).
pub fn encode(keymap: &Value) -> Vec<u8> {
    to_ascii_json(keymap).into_bytes()
}

/// Read an edited keymap; refuse anything without a non-empty `profiles` list.
pub fn load_edit(path: &Path) -> Result<Vec<u8>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let keymap: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let ok = keymap
        .get("profiles")
        .and_then(Value::as_array)
        .is_some_and(|p| !p.is_empty());
    if !ok {
        return Err(format!(
            "{} is not a keymap: it needs a non-empty \"profiles\" list",
            path.display()
        ));
    }
    Ok(encode(&keymap))
}

/// Local `YYYYmmdd-HHMMSS`, as Python's `time.strftime`.
fn local_stamp() -> String {
    let mut buf = [0u8; 32];
    // SAFETY: time/localtime_r/strftime write into stack values we own; the
    // format is NUL-terminated and buf is large enough for 15 chars.
    let len = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y%m%d-%H%M%S".as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// The push sequence on an open pad (testable with `FakePad`): read, back up
/// to `backups`, skip if unchanged, write_verified, settle, status. `new` =
/// `None` for `--test`. Returns the exit code.
pub fn push_work(
    fs: &mut PadFs<'_>,
    new: Option<&[u8]>,
    backups: &Path,
    settle: Duration,
    say: &mut dyn FnMut(&str),
) -> i32 {
    let current = match fs.read(KEYMAP) {
        Ok(bytes) => bytes,
        Err(err) => {
            say(&format!("Could not read the pad's keymap: {err}"));
            return 1;
        }
    };
    let backup = backups.join(format!("{}-{KEYMAP}", local_stamp()));
    if let Err(err) =
        std::fs::create_dir_all(backups).and_then(|_| std::fs::write(&backup, &current))
    {
        say(&format!(
            "Could not back up the pad's keymap: {err}. Nothing written."
        ));
        return 1;
    }
    say(&format!(
        "Backed up the pad's keymap to {}",
        backup.display()
    ));
    let data = match new {
        Some(new) if new == current.as_slice() => {
            say("The pad already has this keymap. Nothing written.");
            return 0;
        }
        Some(new) => new,
        None => current.as_slice(),
    };
    let kind = if new.is_none() {
        "unchanged - test write"
    } else {
        "new keymap"
    };
    say(&format!("Writing {} bytes ({kind})…", data.len()));
    if let Err(err) = fs.write_verified(KEYMAP, data) {
        say(&format!(
            "Write FAILED: {err}\n  Restore with:  freemicro-rs keymap push {}",
            backup.display()
        ));
        return 1;
    }
    // The backups dir sits in `<home>`; a failed cache write only costs the GUI names.
    if let Some(home) = backups.parent() {
        let _ = std::fs::write(cache_path(home), data);
    }
    std::thread::sleep(settle);
    let status = device::status(fs.io, STATUS_ID, Duration::from_secs(3), &mut |_| {});
    match status {
        Ok(Some(status)) => {
            say(&format!(
                "Written and verified (SHA-1 matches). Pad on layer {}, firmware {}.",
                status.get("layer_index").unwrap_or(&Value::Null),
                status.get("version").and_then(Value::as_str).unwrap_or("?"),
            ));
            0
        }
        _ => {
            say(&format!(
                "Written and verified, but the pad did not answer a status check.\n  If keys misbehave, restore with:  freemicro-rs keymap push {}",
                backup.display()
            ));
            1
        }
    }
}

/// Pull: read the pad's keymap.json and save it pretty-printed to `out`.
pub fn pull_work(fs: &mut PadFs<'_>, out: &Path, say: &mut dyn FnMut(&str)) -> i32 {
    let keymap = fs
        .read(KEYMAP)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()));
    let keymap = match keymap {
        Ok(keymap) => keymap,
        Err(err) => {
            say(&format!("Could not read the pad's keymap: {err}"));
            return 1;
        }
    };
    let text = serde_json::to_string_pretty(&keymap).unwrap_or_default() + "\n";
    if let Err(err) = std::fs::write(out, text) {
        say(&format!("Could not write {}: {err}", out.display()));
        return 1;
    }
    say(&format!("Saved the pad's keymap to {}", out.display()));
    0
}

/// Run `work` as the pad's only host: pause the daemon, take the lock (10 s),
/// open the pad, require a Creator Micro 2, always resume.
pub fn with_pad(
    home: &Path,
    user_home: &Path,
    lc: &mut dyn Launchctl,
    pads: &mut dyn PadSource,
    lock_wait: Duration,
    say: &mut dyn FnMut(&str),
    work: &mut PadWork<'_>,
) -> i32 {
    let paused = daemon::pause(lc, user_home);
    if paused {
        say("Paused the FreeMicro daemon.");
    }
    let code = (|| {
        let mut lock = PadLock::new(daemon::lock_path(home), "keymap");
        let deadline = Instant::now() + lock_wait;
        while !lock.acquire(crate::clock::wall()) {
            if Instant::now() > deadline {
                let holder = daemon::lock_holder(&lock.path).unwrap_or_default();
                say(&format!(
                    "{} has the pad. Stop it and try again.",
                    daemon::describe_holder(&holder, crate::clock::wall())
                ));
                return 1;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let mut io = match pads.open() {
            Ok(Some(io)) => io,
            _ => {
                say("The pad is not connected (or Input Monitoring is not granted).");
                return 1;
            }
        };
        if !io.info().is_creator_micro_2() {
            say("This pad is not a Creator Micro 2; its keymap is not editable here.");
            return 1;
        }
        let mut fs = PadFs::new(io.as_mut());
        work(&mut fs, say)
    })();
    if paused {
        daemon::resume(lc, user_home);
        say("Resumed the FreeMicro daemon.");
    }
    code
}

pub fn pull(home: &Path, user_home: &Path, out: &Path, say: &mut dyn FnMut(&str)) -> i32 {
    let out = out.to_path_buf();
    let code = with_pad(
        home,
        user_home,
        &mut daemon::SystemLaunchctl,
        &mut SystemPads,
        LOCK_WAIT,
        say,
        &mut |fs, say| pull_work(fs, &out, say),
    );
    if code == 0 {
        let _ = std::fs::copy(&out, cache_path(home));
    }
    code
}

pub fn push(
    home: &Path,
    user_home: &Path,
    source: Option<&Path>,
    test: bool,
    say: &mut dyn FnMut(&str),
) -> i32 {
    if let Some(refusal) = host_refusal(&other_hosts()) {
        say(&refusal);
        return 1;
    }
    let new = match (test, source) {
        (true, _) => None,
        (false, Some(source)) => match load_edit(source) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                say(&format!("freemicro-rs keymap push: {err}"));
                return 2;
            }
        },
        (false, None) => {
            say("freemicro-rs keymap push: give the edited file, or --test");
            return 2;
        }
    };
    let backups = backup_dir(home);
    with_pad(
        home,
        user_home,
        &mut daemon::SystemLaunchctl,
        &mut SystemPads,
        LOCK_WAIT,
        say,
        &mut |fs, say| push_work(fs, new.as_deref(), &backups, SETTLE, say),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::fake::FakePad;
    use crate::device::{DeviceError, PadInfo, PadIo, Transport};
    use crate::runtime::daemon::tests::FakeLaunchctl;
    use crate::testutil::TempDir;
    use base64::Engine;
    use serde_json::json;
    use sha1::{Digest, Sha1};
    use std::cell::RefCell;
    use std::rc::Rc;

    const PAD_KEYMAP: &[u8] = br#"{"profiles":[{"name":"Default"}]}"#;

    /// A pad with one file system: answers fs.readbin / fs.writebin /
    /// fs.list {checksum} / device.status like firmware 0.6.3 did in
    /// fsprobe.log, echoing each request's id.
    fn pad_with(file: Rc<RefCell<Vec<u8>>>) -> FakePad {
        let b64 = base64::engine::general_purpose::STANDARD;
        FakePad::creator_micro_2().respond(move |msg| {
            let id = msg["id"].clone();
            let method = msg
                .get("method")
                .or_else(|| msg.get("m"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let params = msg.get("params").or_else(|| msg.get("p")).cloned().unwrap_or(json!({}));
            let result = match method {
                "fs.readbin" => {
                    let data = file.borrow();
                    let offset = params["offset"].as_u64().unwrap_or(0) as usize;
                    let end = (offset + 3072).min(data.len());
                    json!({"data": b64.encode(&data[offset.min(end)..end]), "total_size": data.len(), "offset": offset})
                }
                "fs.writebin" => {
                    let chunk = b64.decode(params["data"].as_str().unwrap_or("")).unwrap();
                    let mut data = file.borrow_mut();
                    if params["offset"].as_u64() == Some(0) {
                        data.clear();
                    }
                    data.extend(chunk);
                    json!({"ok": 1})
                }
                "fs.list" => {
                    let sum = format!("{:x}", Sha1::digest(&*file.borrow()));
                    json!([{"name": "keymap.json", "size": file.borrow().len(), "dir": false, "checksum": sum}])
                }
                "device.status" => json!({"version": "0.6.3", "profile_index": 0, "layer_index": 1, "battery": 99, "is_charging": true}),
                _ => return vec![json!({"error": {"code": 404, "message": "Method not found"}, "id": id})],
            };
            vec![json!({"result": result, "id": id, "method": method})]
        })
    }

    fn run_push(new: Option<&[u8]>) -> (i32, Vec<String>, Vec<u8>, TempDir) {
        let home = TempDir::new();
        let file = Rc::new(RefCell::new(PAD_KEYMAP.to_vec()));
        let mut pad = pad_with(file.clone());
        let mut fs = PadFs::new(&mut pad);
        fs.gap = Duration::ZERO;
        let mut lines = Vec::new();
        let code = push_work(
            &mut fs,
            new,
            &backup_dir(home.path()),
            Duration::ZERO,
            &mut |l| lines.push(l.to_string()),
        );
        let data = file.borrow().clone();
        (code, lines, data, home)
    }

    #[test]
    fn push_refuses_while_another_host_talks_to_the_pad() {
        assert!(host_refusal(&[]).is_none());
        let line = host_refusal(&["ChatGPT".to_string()]).unwrap();
        assert!(line.starts_with("Refusing to write: ChatGPT is running"));
    }

    #[test]
    fn load_edit_rejects_files_that_are_not_keymaps() {
        let home = TempDir::new();
        let path = home.path().join("keymap.json");
        for content in ["[]", r#"{"profiles": []}"#, r#"{"version": 1}"#, "nope"] {
            std::fs::write(&path, content).unwrap();
            assert!(load_edit(&path).is_err(), "{content}");
        }
    }

    #[test]
    fn load_edit_encodes_like_input() {
        let home = TempDir::new();
        let path = home.path().join("keymap.json");
        std::fs::write(&path, "{\n  \"profiles\": [{\"name\": \"Caf\u{e9}\"}]\n}").unwrap();
        assert_eq!(
            load_edit(&path).unwrap(),
            b"{\"profiles\":[{\"name\":\"Caf\\u00e9\"}]}"
        );
    }

    #[test]
    fn push_writes_verifies_and_backs_up() {
        let new = br#"{"profiles":[{"name":"Edited"}]}"#;
        let (code, lines, data, home) = run_push(Some(new));
        assert_eq!(code, 0, "{lines:?}");
        assert_eq!(data, new);
        assert!(lines
            .last()
            .unwrap()
            .contains("Pad on layer 1, firmware 0.6.3."));
        let backups: Vec<_> = std::fs::read_dir(backup_dir(home.path()))
            .unwrap()
            .collect();
        assert_eq!(backups.len(), 1);
        let backup = backups[0].as_ref().unwrap().path();
        assert!(backup.to_string_lossy().ends_with("-keymap.json"));
        assert_eq!(std::fs::read(backup).unwrap(), PAD_KEYMAP);
        assert_eq!(std::fs::read(cache_path(home.path())).unwrap(), new);
    }

    #[test]
    fn push_skips_an_unchanged_keymap_and_test_writes_it_back() {
        let (code, lines, _, _home) = run_push(Some(PAD_KEYMAP));
        assert_eq!(code, 0);
        assert_eq!(
            lines.last().unwrap(),
            "The pad already has this keymap. Nothing written."
        );
        let (code, lines, data, _home) = run_push(None);
        assert_eq!(code, 0, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("unchanged - test write")));
        assert_eq!(data, PAD_KEYMAP);
    }

    #[test]
    fn the_layer_map_comes_from_linked_apps_only_where_kb_pl_set_exists() {
        let keymap = json!({
            "activeProfileId": 0,
            "linkedApps": [
                {"id": 2, "process": "com.mitchellh.ghostty"},
                {"id": 4, "process": "com.apple.Safari/example.org"},
            ],
            "profiles": [{"layers": [
                {"name": "ChatGPT", "linkedAppId": 2},
                {"name": "Spare", "linkedAppId": null},
                {"name": "MINT", "linkedAppId": 4},
                {"name": "Again", "linkedAppId": 2},
            ]}],
        });
        let bytes = keymap.to_string().into_bytes();
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut pad = FakePad::creator_micro_2().respond(move |msg| {
            let id = msg["id"].clone();
            match msg.get("m").or_else(|| msg.get("method")).and_then(Value::as_str) {
                Some("kb.pl.set") => vec![json!({"error": {"code": 400, "message": "missing p and l"}, "id": id})],
                Some("fs.readbin") => vec![json!({"result": {"data": b64.encode(&bytes), "total_size": bytes.len()}, "id": id})],
                _ => Vec::new(),
            }
        });
        // A process linked twice keeps its first layer; an unlinked layer is skipped.
        let home = TempDir::new();
        let cache = cache_path(home.path());
        assert_eq!(
            read_layer_map(&mut pad, Some(&cache)),
            Some(vec![
                ("com.mitchellh.ghostty".to_string(), 1),
                ("com.apple.Safari/example.org".to_string(), 3),
            ])
        );
        assert_eq!(
            std::fs::read(&cache).unwrap(),
            keymap.to_string().into_bytes()
        );
        // Firmware 0.6.3 answers 404: keep host.focused_app.
        let mut old = pad_with(Rc::new(RefCell::new(PAD_KEYMAP.to_vec())));
        assert_eq!(read_layer_map(&mut old, None), None);
    }

    #[test]
    fn pull_saves_the_pad_keymap_pretty_printed() {
        let home = TempDir::new();
        let mut pad = pad_with(Rc::new(RefCell::new(PAD_KEYMAP.to_vec())));
        let mut fs = PadFs::new(&mut pad);
        fs.gap = Duration::ZERO;
        let out = home.path().join("out.json");
        assert_eq!(pull_work(&mut fs, &out, &mut |_| {}), 0);
        assert_eq!(
            std::fs::read_to_string(out).unwrap(),
            "{\n  \"profiles\": [\n    {\n      \"name\": \"Default\"\n    }\n  ]\n}\n"
        );
    }

    struct OnePad(Option<PadInfo>);

    impl PadSource for OnePad {
        fn open(&mut self) -> Result<Option<Box<dyn PadIo>>, DeviceError> {
            Ok(self
                .0
                .take()
                .map(|info| Box::new(FakePad::with_info(info)) as Box<dyn PadIo>))
        }
        fn present(&mut self) -> Option<u64> {
            self.0.as_ref().map(|_| 0)
        }
    }

    fn with_installed_daemon(pads: &mut dyn PadSource) -> (i32, Vec<String>, Vec<String>) {
        let home = TempDir::new();
        let user = TempDir::new();
        std::fs::create_dir_all(daemon::agents_dir(user.path())).unwrap();
        std::fs::write(daemon::plist_path(user.path(), daemon::LABEL), "").unwrap();
        let mut lc = FakeLaunchctl::default();
        let mut lines = Vec::new();
        let code = with_pad(
            home.path(),
            user.path(),
            &mut lc,
            pads,
            Duration::ZERO,
            &mut |l| lines.push(l.to_string()),
            &mut |_, _| 7,
        );
        (code, lines, lc.calls)
    }

    #[test]
    fn with_pad_pauses_the_daemon_and_always_resumes_it() {
        let cm2 = PadInfo {
            transport: Transport::Ble,
            product_id: 0x8297,
        };
        let (code, lines, calls) = with_installed_daemon(&mut OnePad(Some(cm2)));
        assert_eq!(code, 7);
        assert_eq!(lines.first().unwrap(), "Paused the FreeMicro daemon.");
        assert_eq!(lines.last().unwrap(), "Resumed the FreeMicro daemon.");
        assert!(calls[0].starts_with("bootout"));

        let codex = PadInfo {
            transport: Transport::Usb,
            product_id: 0x8360,
        };
        let (code, lines, calls) = with_installed_daemon(&mut OnePad(Some(codex)));
        assert_eq!(code, 1);
        assert!(lines[1].contains("not a Creator Micro 2"));
        assert!(calls.iter().any(|c| c.starts_with("kickstart")));

        let (code, lines, _) = with_installed_daemon(&mut OnePad(None));
        assert_eq!(code, 1);
        assert!(lines[1].starts_with("The pad is not connected"));
    }

    #[test]
    fn with_pad_gives_up_when_another_process_holds_the_lock() {
        let home = TempDir::new();
        let user = TempDir::new();
        let mut held = PadLock::new(daemon::lock_path(home.path()), "daemon");
        assert!(held.acquire(0.0));
        let mut lines = Vec::new();
        let code = with_pad(
            home.path(),
            user.path(),
            &mut FakeLaunchctl::default(),
            &mut OnePad(None),
            Duration::ZERO,
            &mut |l| lines.push(l.to_string()),
            &mut |_, _| 0,
        );
        assert_eq!(code, 1);
        assert!(lines[0].starts_with("the background daemon (pid "));
    }
}
