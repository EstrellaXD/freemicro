//! The Claude Code hook writer: what `freemicro-rs hook` does with one event
//! on stdin. Port of `state/hooks.py` and `cli.py` (`cmd_hook`,
//! `_apply_hook_event`, `_log_raw_event`). Writes the same state files as
//! Python, so either build's hook can feed either build's daemon.

use std::io::{self, Read, Write};
use std::path::Path;

use serde_json::Value;

use super::store::{SessionMeta, StateStore};
use super::{text, truthy, AgentState, SessionSignals, SessionState, TerminalInfo};

pub const PERMISSION_PROMPT: &str = "permission_prompt";
pub const IDLE_PROMPT: &str = "idle_prompt";
pub const PROMPT_EVENT: &str = "Notification";
pub const ENV_HOOK_LOG: &str = "FREEMICRO_HOOK_LOG";
/// Same cap as Python `cli.HOOK_LOG_MAX_BYTES`: both builds may append to one log.
pub const HOOK_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

const WORKING_EVENTS: [&str; 4] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
];
const PERMISSION_HINTS: [&str; 4] = ["permission", "approve", "waiting for your", "needs your"];

/// Python `event.get(key) or ""` as a string.
fn field(event: &Value, key: &str) -> String {
    text(event.get(key).filter(|v| truthy(v)))
}

/// `hook_event_name`, else `event`.
fn event_name(event: &Value) -> String {
    let name = field(event, "hook_event_name");
    if name.is_empty() {
        field(event, "event")
    } else {
        name
    }
}

fn looks_like_permission_prompt(event: &Value) -> bool {
    // `notification_type` is trusted outright when present.
    let kind = field(event, "notification_type").trim().to_lowercase();
    if !kind.is_empty() {
        return kind == PERMISSION_PROMPT;
    }
    let haystack =
        format!("{} {}", field(event, "matcher"), field(event, "message")).to_lowercase();
    PERMISSION_HINTS.iter().any(|hint| haystack.contains(hint))
}

fn stop_was_error(event: &Value) -> bool {
    if matches!(
        event.get("hook_event_name").and_then(Value::as_str),
        Some("StopFailure" | "Error")
    ) {
        return true;
    }
    if ["error", "is_error"]
        .iter()
        .any(|k| event.get(*k).is_some_and(truthy))
    {
        return true;
    }
    matches!(
        field(event, "status").to_lowercase().as_str(),
        "error" | "failed" | "failure"
    )
}

/// The state a hook event means, or `None` to ignore it.
pub fn classify(event: &Value) -> Option<AgentState> {
    let name = event_name(event);
    match name.as_str() {
        "" => None,
        // A fresh window is idle, not absent: it claims an Agent Key at once.
        "SessionStart" | "SessionEnd" => Some(AgentState::Idle),
        n if WORKING_EVENTS.contains(&n) => Some(AgentState::Working),
        "Notification" => looks_like_permission_prompt(event).then_some(AgentState::Waiting),
        "Stop" | "StopFailure" => Some(if stop_was_error(event) {
            AgentState::Error
        } else {
            AgentState::Done
        }),
        _ => None,
    }
}

/// Everything the engine can use out of a payload. Never fails.
pub fn read_signals(event: &Value) -> SessionSignals {
    if !event.is_object() {
        return SessionSignals::default();
    }
    let effort = match event.get("effort") {
        Some(Value::Object(map)) => text(map.get("level").filter(|v| truthy(v))),
        _ => field(event, "effort"),
    };
    let running = event
        .get("background_tasks")
        .and_then(Value::as_array)
        .map_or(0, |tasks| {
            tasks
                .iter()
                .filter(|t| t.is_object() && field(t, "status").trim().to_lowercase() == "running")
                .count() as u32
        });
    SessionSignals {
        event: event_name(event),
        prompt_id: field(event, "prompt_id"),
        permission_mode: field(event, "permission_mode"),
        effort,
        background_tasks: running,
        end_reason: field(event, "reason"),
    }
}

/// `session_id` / `sessionId` / `session`, else `"default"`.
pub fn session_id_of(event: &Value) -> String {
    ["session_id", "sessionId", "session"]
        .iter()
        .map(|key| field(event, key))
        .find(|id| !id.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Is a permission prompt on screen in `session`'s terminal now?
pub fn prompt_is_pending(session: &SessionState) -> bool {
    session.state == AgentState::Waiting
        && !session.stale()
        && session.last_event == PROMPT_EVENT
        && session.prompts_for_permission()
}

/// Classify `event` and fold it into `<home>/state`: `SessionEnd` (not
/// `/clear`) clears the record, anything classified updates it.
pub fn apply_event(
    home: &Path,
    event: &Value,
    terminal: &TerminalInfo,
    now: f64,
) -> io::Result<()> {
    let Some(state) = classify(event) else {
        return Ok(());
    };
    let store = StateStore::for_home(home);
    let session = session_id_of(event);
    let signals = read_signals(event);
    // Of the three idle events only a real SessionEnd means "gone"; a
    // `/clear` keeps the project's slot.
    let ends_session =
        state == AgentState::Idle && signals.event == "SessionEnd" && signals.end_reason != "clear";
    if ends_session {
        store.clear(&session);
        return Ok(());
    }
    let meta = SessionMeta {
        title: text(event.get("title")),
        cwd: text(event.get("cwd")),
        terminal: terminal.clone(),
        signals,
    };
    store.update(&session, state, &meta, now).map(|_| ())
}

/// Append the raw event to `$FREEMICRO_HOOK_LOG` (size-capped, one
/// rollover). Never fails.
pub fn log_raw_event(event: &Value, state: Option<AgentState>, now: f64) {
    if let Some(path) = std::env::var_os(ENV_HOOK_LOG).filter(|p| !p.is_empty()) {
        log_raw_event_to(Path::new(&path), event, state, now, HOOK_LOG_MAX_BYTES);
    }
}

fn log_raw_event_to(path: &Path, event: &Value, state: Option<AgentState>, now: f64, cap: u64) {
    // One generation: at the cap the file becomes `<path>.1`.
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= cap) {
        let mut rolled = path.as_os_str().to_owned();
        rolled.push(".1");
        let _ = std::fs::rename(path, rolled);
    }
    let mut keys: Vec<&String> = event
        .as_object()
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    keys.sort();
    keys.truncate(20);
    let record = serde_json::json!({
        "at": now,
        "event": event.get("hook_event_name"),
        "classified_as": state.map(AgentState::as_str),
        "cwd": event.get("cwd").cloned().unwrap_or_else(|| Value::from("")),
        "payload_keys": keys,
        "payload": event,
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{record}");
    }
}

/// Entry point for `freemicro-rs hook`: read stdin, apply, print at most one
/// line on stderr. Always returns 0 so Claude Code never sees a failure.
pub fn run_hook_stdin(home: &Path) -> i32 {
    let mut input = Vec::new();
    if io::stdin().read_to_end(&mut input).is_err() {
        return 0;
    }
    let Ok(event) = serde_json::from_slice::<Value>(&input) else {
        return 0;
    };
    let now = crate::clock::wall();
    log_raw_event(&event, classify(&event), now);
    if let Err(err) = apply_event(home, &event, &super::process::current_terminal(), now) {
        eprintln!("freemicro hook: {err}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn classify_maps_hook_events() {
        use AgentState::*;
        for (event, want) in [
            (
                json!({"hook_event_name": "UserPromptSubmit"}),
                Some(Working),
            ),
            (json!({"hook_event_name": "PreToolUse"}), Some(Working)),
            (json!({"hook_event_name": "PostToolUse"}), Some(Working)),
            (json!({"hook_event_name": "PreCompact"}), Some(Working)),
            (json!({"event": "PreToolUse"}), Some(Working)),
            (json!({"hook_event_name": "SessionStart"}), Some(Idle)),
            (json!({"hook_event_name": "SessionEnd"}), Some(Idle)),
            (
                json!({"hook_event_name": "Notification", "message": "Claude needs your permission to use Bash"}),
                Some(Waiting),
            ),
            (
                json!({"hook_event_name": "Notification", "matcher": "permission_prompt"}),
                Some(Waiting),
            ),
            (
                json!({"hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "hi"}),
                Some(Waiting),
            ),
            (
                json!({"hook_event_name": "Notification", "notification_type": "idle_prompt", "message": "waiting for your input"}),
                None,
            ),
            (
                json!({"hook_event_name": "Notification", "message": "Build finished"}),
                None,
            ),
            (json!({"hook_event_name": "Stop"}), Some(Done)),
            (
                json!({"hook_event_name": "Stop", "error": "boom"}),
                Some(Error),
            ),
            (
                json!({"hook_event_name": "Stop", "is_error": true}),
                Some(Error),
            ),
            (
                json!({"hook_event_name": "Stop", "status": "Failed"}),
                Some(Error),
            ),
            (json!({"hook_event_name": "StopFailure"}), Some(Error)),
            (json!({"hook_event_name": "SubagentStop"}), None),
            (json!({"hook_event_name": "Whatever"}), None),
            (json!({}), None),
            (json!([1, 2]), None),
        ] {
            assert_eq!(classify(&event), want, "{event}");
        }
    }

    #[test]
    fn read_signals_lifts_payload_fields() {
        let event = json!({
            "hook_event_name": "Stop", "prompt_id": "p1", "permission_mode": "bypassPermissions",
            "effort": {"level": "high"}, "reason": "clear",
            "background_tasks": [{"status": "running"}, {"status": "Running "}, {"status": "done"}, "x"],
        });
        assert_eq!(
            read_signals(&event),
            SessionSignals {
                event: "Stop".into(),
                prompt_id: "p1".into(),
                permission_mode: "bypassPermissions".into(),
                effort: "high".into(),
                background_tasks: 2,
                end_reason: "clear".into(),
            }
        );
        assert_eq!(read_signals(&json!({"effort": "low"})).effort, "low");
        for hostile in [
            json!(null),
            json!("x"),
            json!({"background_tasks": "lots", "prompt_id": null}),
        ] {
            assert_eq!(
                read_signals(&hostile),
                SessionSignals::default(),
                "{hostile}"
            );
        }
    }

    #[test]
    fn session_id_of_falls_back() {
        for (event, want) in [
            (json!({"session_id": "a", "sessionId": "b"}), "a"),
            (json!({"session_id": "", "sessionId": "b"}), "b"),
            (json!({"session": "c"}), "c"),
            (json!({}), "default"),
        ] {
            assert_eq!(session_id_of(&event), want);
        }
    }

    #[test]
    fn apply_event_updates_clears_and_keeps_on_clear() {
        let home = TempDir::new();
        let term = TerminalInfo {
            tty: "/dev/ttys004".into(),
            pid: 4242,
            ..TerminalInfo::default()
        };
        let store = StateStore::for_home(home.path());
        let notify = json!({"hook_event_name": "Notification", "session_id": "s",
            "notification_type": "permission_prompt", "cwd": "/p"});
        apply_event(home.path(), &notify, &term, 10.0).unwrap();
        let rec = store.read("s").unwrap();
        assert_eq!(
            (rec.state, rec.cwd.as_str(), rec.tty.as_str(), rec.pid),
            (AgentState::Waiting, "/p", "/dev/ttys004", 4242)
        );
        assert!(prompt_is_pending(&rec));
        // An ignored event writes nothing.
        apply_event(
            home.path(),
            &json!({"hook_event_name": "SubagentStop", "session_id": "s"}),
            &term,
            11.0,
        )
        .unwrap();
        assert_eq!(store.read("s").unwrap().updated_at, 10.0);
        // `/clear` keeps the record, idle; a real end removes it.
        apply_event(
            home.path(),
            &json!({"hook_event_name": "SessionEnd", "session_id": "s", "reason": "clear"}),
            &term,
            12.0,
        )
        .unwrap();
        let rec = store.read("s").unwrap();
        assert_eq!(rec.state, AgentState::Idle);
        assert!(!prompt_is_pending(&rec));
        let end = json!({"hook_event_name": "SessionEnd", "session_id": "s", "reason": "exit"});
        apply_event(home.path(), &end, &term, 13.0).unwrap();
        apply_event(home.path(), &end, &term, 14.0).unwrap();
        assert_eq!(store.read("s"), None);
    }

    #[test]
    fn prompt_is_pending_needs_every_clause() {
        let mut s = SessionState::new("s", AgentState::Waiting, 0.0);
        assert!(!prompt_is_pending(&s), "emit waiting has no event name");
        s.last_event = PROMPT_EVENT.into();
        assert!(prompt_is_pending(&s));
        s.permission_mode = "bypassPermissions".into();
        assert!(!prompt_is_pending(&s));
    }

    #[test]
    fn hook_log_appends_and_rotates_at_the_cap() {
        let dir = TempDir::new();
        let path = dir.path().join("hook.log");
        let event = json!({"hook_event_name": "Stop", "cwd": "/p", "b": 1, "a": 2});
        log_raw_event_to(&path, &event, Some(AgentState::Done), 1.0, 10_000);
        let line: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(line["classified_as"], "done");
        assert_eq!(
            line["payload_keys"],
            json!(["a", "b", "cwd", "hook_event_name"])
        );
        // Under the cap: no rotation. Past it: the old file rolls to `.1`.
        log_raw_event_to(&path, &event, None, 2.0, 10_000);
        assert!(!dir.path().join("hook.log.1").exists());
        log_raw_event_to(&path, &event, None, 3.0, 1);
        assert!(dir.path().join("hook.log.1").exists());
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert_eq!(fresh.lines().count(), 1);
        assert!(fresh.contains("\"at\":3.0"));
    }
}
