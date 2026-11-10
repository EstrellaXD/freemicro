//! herdr client: unix socket, one connection per request, one JSON line each
//! way `{id, method, params}`. Presets for the pad keys, and Agent Keys
//! from herdr's agents. Port of `freemicro/herdr.py`.
//!
//! Owner: herdr+focus. Depends on `state` only. herdr calls never touch the
//! pad, so presets may run on background threads.

use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use crate::state::{AgentState, SessionState};

pub const ENV_SOCKET: &str = "HERDR_SOCKET_PATH";
pub const TIMEOUT: Duration = Duration::from_secs(2);
pub const DIAL_MIN_INTERVAL: f64 = 0.12;
pub const DIAL_MODES: [&str; 3] = ["workspaces", "agents", "effort"];
/// Claude Code's `/effort` levels, low to high.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// The effort command goes out this long after the last dial step, so a fast
/// turn types one command, not five.
const EFFORT_SETTLE: Duration = Duration::from_millis(400);
/// Preset names accepted by a `herdr` binding.
pub const PRESET_NAMES: [&str; 17] = [
    "tab-next",
    "tab-prev",
    "tab-new",
    "workspace-next",
    "workspace-prev",
    "agent-next",
    "agent-prev",
    "zoom",
    "pane-split-right",
    "pane-split-down",
    "pane-left",
    "pane-right",
    "pane-up",
    "pane-down",
    "dial-next",
    "dial-prev",
    "dial-mode",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError(pub String);

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HerdrError {}

impl From<std::io::Error> for HerdrError {
    fn from(e: std::io::Error) -> Self {
        HerdrError(format!("herdr: {e}"))
    }
}

/// The dial: its mode index into [`DIAL_MODES`] and the last step (mono s).
struct Dial {
    mode: usize,
    last: Option<f64>,
    /// Index into [`EFFORT_LEVELS`]; starts at medium.
    // ponytail: not read back from the session; a session that starts elsewhere
    // drifts until the first step clamps it. Read the hook's `effort` if that bites.
    effort: usize,
    /// Bumped on every effort step; only the newest step sends.
    effort_step: u64,
}

static DIAL: Mutex<Dial> = Mutex::new(Dial {
    mode: 0,
    last: None,
    effort: 1,
    effort_step: 0,
});

/// The dial's mode and effort level, for `runtime.json`.
pub fn dial_state() -> (&'static str, &'static str) {
    let dial = DIAL.lock().unwrap_or_else(|e| e.into_inner());
    (DIAL_MODES[dial.mode], EFFORT_LEVELS[dial.effort])
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str(value)` for the ids herdr sends (strings, sometimes numbers).
fn text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}

/// `result[key]` as a list, `[]` if absent or not a list.
fn list(result: &Value, key: &str) -> Vec<Value> {
    result
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The item `step` away from the focused one, wrapping.
fn cycle(items: &[Value], step: i32) -> Option<&Value> {
    let current = items.iter().position(|i| truthy(i.get("focused")))?;
    if items.len() < 2 {
        return None;
    }
    let n = items.len() as i64;
    Some(&items[(current as i64 + step as i64).rem_euclid(n) as usize])
}

/// herdr's tab attention priority (`slots.ts: attention`).
fn attention(status: Option<&Value>) -> i64 {
    match status.and_then(Value::as_str) {
        Some("blocked") => 4,
        Some("done") => 3,
        Some("working") => 2,
        Some("idle") => 1,
        _ => 0,
    }
}

fn herdr_state(status: Option<&Value>) -> AgentState {
    match status.and_then(Value::as_str) {
        Some("working") => AgentState::Working,
        Some("blocked") => AgentState::Waiting,
        Some("done") => AgentState::Done,
        _ => AgentState::Idle,
    }
}

fn seq(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// A herdr endpoint. Cheap to clone; tests point it at a `UnixListener`
/// in a temp dir instead of setting the env var.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Herdr {
    pub socket: PathBuf,
}

impl Herdr {
    /// `$HERDR_SOCKET_PATH`, else `~/.config/herdr/herdr.sock`.
    pub fn from_env() -> Herdr {
        let socket = match std::env::var_os(ENV_SOCKET) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".config/herdr/herdr.sock"),
        };
        Herdr { socket }
    }

    /// One call; its `result` (`{}` when absent). Error on connect, timeout,
    /// bad JSON or an `error` reply.
    pub fn request(&self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let params = if params.is_null() { json!({}) } else { params };
        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let mut line = json!({"id": "freemicro", "method": method, "params": params}).to_string();
        line.push('\n');
        stream.write_all(line.as_bytes())?;
        let mut reply = Vec::new();
        BufReader::new(stream).read_until(b'\n', &mut reply)?;
        let parsed: Value = serde_json::from_slice(&reply)
            .map_err(|e| HerdrError(format!("herdr {method}: bad reply: {e}")))?;
        if truthy(parsed.get("error")) {
            let error = text(parsed.get("error"));
            return Err(HerdrError(format!("herdr {method}: {error}")));
        }
        Ok(match parsed.get("result") {
            Some(r) if truthy(Some(r)) => r.clone(),
            _ => json!({}),
        })
    }

    pub fn pane_for_session(&self, session_id: &str) -> Result<Option<String>, HerdrError> {
        let agents = list(&self.request("agent.list", json!({}))?, "agents");
        for agent in &agents {
            let sid = agent.get("agent_session").and_then(|s| s.get("value"));
            if sid.and_then(Value::as_str) == Some(session_id)
                || agent.get("pane_id").and_then(Value::as_str) == Some(session_id)
            {
                let pane = text(agent.get("pane_id"));
                return Ok((!pane.is_empty()).then_some(pane));
            }
        }
        Ok(None)
    }

    /// Focus the pane running `session_id`. False if not found or herdr down.
    pub fn focus_session(&self, session_id: &str) -> bool {
        match self.pane_for_session(session_id) {
            Ok(Some(pane)) => self.request("agent.focus", json!({"target": pane})).is_ok(),
            _ => false,
        }
    }

    /// The sessions the Agent Keys show while herdr runs: one per herdr agent
    /// (`<cwd>#2` for a second agent in one folder); unchanged when herdr is
    /// down.
    pub fn agent_sessions(&self, sessions: &[SessionState], now: f64) -> Vec<SessionState> {
        let Ok(result) = self.request("agent.list", json!({})) else {
            return sessions.to_vec();
        };
        let mut agents = list(&result, "agents");
        agents.sort_by_cached_key(|a| text(a.get("pane_id")));
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        for agent in &agents {
            let cwd = if truthy(agent.get("foreground_cwd")) {
                text(agent.get("foreground_cwd"))
            } else {
                text(agent.get("cwd"))
            };
            if cwd.is_empty() {
                continue;
            }
            let n = seen.entry(cwd.clone()).or_insert(0);
            *n += 1;
            let path = if *n == 1 { cwd } else { format!("{cwd}#{n}") };
            let value = agent.get("agent_session").and_then(|s| s.get("value"));
            let sid = if truthy(value) {
                text(value)
            } else {
                text(agent.get("pane_id"))
            };
            // herdr's status, not the hook's: the pad must show what herdr
            // shows (herdr clears "done" once the pane is seen; a hook does not).
            let state = herdr_state(agent.get("agent_status"));
            let mut s = match sessions.iter().find(|s| s.session_id == sid) {
                Some(known) => SessionState {
                    state,
                    ..known.clone()
                },
                None => SessionState::new(&sid, state, now),
            };
            s.cwd = path;
            s.term_program = "herdr".to_string();
            out.push(s);
        }
        out
    }

    pub fn step_workspace(&self, step: i32) -> Result<(), HerdrError> {
        let workspaces = list(&self.request("workspace.list", json!({}))?, "workspaces");
        if let Some(target) = cycle(&workspaces, step) {
            self.request(
                "workspace.focus",
                json!({"workspace_id": target.get("workspace_id")}),
            )?;
        }
        Ok(())
    }

    pub fn step_tab(&self, step: i32) -> Result<(), HerdrError> {
        let workspaces = list(&self.request("workspace.list", json!({}))?, "workspaces");
        let Some(workspace) = workspaces.iter().find(|w| truthy(w.get("focused"))) else {
            return Ok(());
        };
        let tabs = list(
            &self.request(
                "tab.list",
                json!({"workspace_id": workspace.get("workspace_id")}),
            )?,
            "tabs",
        );
        if let Some(target) = cycle(&tabs, step) {
            self.request("tab.focus", json!({"tab_id": target.get("tab_id")}))?;
        }
        Ok(())
    }

    /// Neediest first, then most recently changed.
    pub fn step_agent(&self, step: i32) -> Result<(), HerdrError> {
        let mut agents = list(&self.request("agent.list", json!({}))?, "agents");
        agents.sort_by_key(|a| {
            (
                -attention(a.get("agent_status")),
                -seq(a.get("state_change_seq")),
            )
        });
        if agents.is_empty() {
            return Ok(());
        }
        // The focused pane is often not an agent: start at the neediest then.
        let target = if agents.iter().any(|a| truthy(a.get("focused"))) {
            cycle(&agents, step)
        } else {
            agents.first()
        };
        if let Some(target) = target {
            self.request("agent.focus", json!({"target": target.get("pane_id")}))?;
        }
        Ok(())
    }

    pub fn send_to_focused_pane(&self, method: &str, params: Value) -> Result<(), HerdrError> {
        let current = self.request("pane.current", json!({}))?;
        let pane = current.get("pane").and_then(|p| p.get("pane_id"));
        if truthy(pane) {
            let mut merged = json!({"pane_id": pane});
            if let (Some(m), Value::Object(extra)) = (merged.as_object_mut(), params) {
                m.extend(extra);
            }
            self.request(method, merged)?;
        }
        Ok(())
    }

    /// Run preset `name` (see [`PRESET_NAMES`]). Dial state is process-wide
    /// (a `static Mutex`), as in Python.
    pub fn run_preset(&self, name: &str, now: f64) -> Result<(), HerdrError> {
        let call = |method: &str, params: Value| self.request(method, params).map(|_| ());
        match name {
            "tab-next" => self.step_tab(1),
            "tab-prev" => self.step_tab(-1),
            "tab-new" => call("tab.create", json!({"focus": true})),
            "workspace-next" => self.step_workspace(1),
            "workspace-prev" => self.step_workspace(-1),
            "agent-next" => self.step_agent(1),
            "agent-prev" => self.step_agent(-1),
            "zoom" => call("pane.zoom", json!({})),
            "pane-split-right" => call("pane.split", json!({"direction": "right", "focus": true})),
            "pane-split-down" => call("pane.split", json!({"direction": "down", "focus": true})),
            "pane-left" | "pane-right" | "pane-up" | "pane-down" => call(
                "pane.focus_direction",
                json!({"direction": &name["pane-".len()..]}),
            ),
            "dial-next" => self.dial(1, now),
            "dial-prev" => self.dial(-1, now),
            "dial-mode" => {
                let mut dial = DIAL.lock().unwrap_or_else(|e| e.into_inner());
                dial.mode = (dial.mode + 1) % DIAL_MODES.len();
                Ok(())
            }
            _ => Err(HerdrError(format!("unknown herdr preset {name:?}"))),
        }
    }

    /// One dial step, dropped if the last one was under
    /// [`DIAL_MIN_INTERVAL`] ago (a fast spin must not queue calls).
    fn dial(&self, step: i32, now: f64) -> Result<(), HerdrError> {
        let mode = {
            let mut dial = DIAL.lock().unwrap_or_else(|e| e.into_inner());
            if dial.last.is_some_and(|last| now - last < DIAL_MIN_INTERVAL) {
                return Ok(());
            }
            dial.last = Some(now);
            dial.mode
        };
        match DIAL_MODES[mode] {
            "workspaces" => self.step_workspace(step),
            "agents" => self.step_agent(step),
            _ => self.step_effort(step),
        }
    }

    /// Move the effort level one step, then (after [`EFFORT_SETTLE`] with no
    /// newer step) type `/effort <level>` + Enter into the focused pane.
    /// Runs on the preset's background thread, so the wait blocks nothing.
    fn step_effort(&self, step: i32) -> Result<(), HerdrError> {
        let (level, mine) = {
            let mut dial = DIAL.lock().unwrap_or_else(|e| e.into_inner());
            let top = EFFORT_LEVELS.len() as i32 - 1;
            dial.effort = (dial.effort as i32 + step).clamp(0, top) as usize;
            dial.effort_step += 1;
            (EFFORT_LEVELS[dial.effort], dial.effort_step)
        };
        std::thread::sleep(EFFORT_SETTLE);
        if DIAL.lock().unwrap_or_else(|e| e.into_inner()).effort_step != mine {
            return Ok(());
        }
        self.send_to_focused_pane(
            "pane.send_text",
            json!({"text": format!("/effort {level}")}),
        )?;
        self.send_to_focused_pane("pane.send_keys", json!({"keys": ["enter"]}))
    }
}

/// Run `work` on a detached thread; errors (herdr down) are dropped.
pub fn run_in_background(work: impl FnOnce() -> Result<(), HerdrError> + Send + 'static) {
    std::thread::spawn(move || {
        let _ = work();
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    pub type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    /// A herdr server in `dir` answering each request with `reply(method,
    /// params)` (the whole reply line). Returns the client and the calls seen.
    pub fn serve(
        dir: &TempDir,
        reply: impl Fn(&str, &Value) -> Value + Send + 'static,
    ) -> (Herdr, Calls) {
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).expect("bind herdr socket");
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).expect("request JSON");
                assert_eq!(req["id"], "freemicro");
                let method = req["method"].as_str().unwrap_or_default().to_string();
                let answer = reply(&method, &req["params"]);
                seen.lock().unwrap().push((method, req["params"].clone()));
                let _ = stream.write_all(format!("{answer}\n").as_bytes());
            }
        });
        (Herdr { socket }, calls)
    }

    fn last(calls: &Calls) -> (String, Value) {
        calls.lock().unwrap().last().cloned().expect("a call")
    }

    fn agents(focused: Option<&'static str>) -> impl Fn(&str, &Value) -> Value {
        move |method, _| match method {
            "agent.list" => {
                let agents: Vec<Value> = [
                    ("idle", "idle", json!(9)),
                    ("blocked", "blocked", json!(1)),
                    ("done-old", "done", json!(2)),
                    ("done-new", "done", json!("5")),
                ]
                .into_iter()
                .map(|(pane, status, seq)| {
                    json!({"pane_id": pane, "agent_status": status,
                           "state_change_seq": seq, "focused": Some(pane) == focused})
                })
                .collect();
                json!({"result": {"agents": agents}})
            }
            "workspace.list" => json!({"result": {"workspaces": [
                {"workspace_id": "w1", "focused": true}]}}),
            "tab.list" => json!({"result": {"tabs": [
                {"tab_id": "t1"}, {"tab_id": "t2", "focused": true}]}}),
            "pane.current" => json!({"result": {"pane": {"pane_id": "p9"}}}),
            _ => json!({"result": null}),
        }
    }

    #[test]
    fn step_agent_follows_herdr_attention_order() {
        for (focused, step, expected) in [
            (None, 1, "blocked"),             // no agent focused: start at the neediest
            (Some("blocked"), 1, "done-new"), // then by attention, newest first
            (Some("done-new"), 1, "done-old"),
            (Some("idle"), 1, "blocked"), // and wrap
            (Some("blocked"), -1, "idle"),
        ] {
            let dir = TempDir::new();
            let (herdr, calls) = serve(&dir, agents(focused));
            herdr.step_agent(step).unwrap();
            assert_eq!(
                last(&calls),
                ("agent.focus".into(), json!({"target": expected})),
                "{focused:?} {step}"
            );
        }
    }

    #[test]
    fn run_preset_tab_next_wraps_within_the_focused_workspace() {
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, agents(None));
        herdr.run_preset("tab-next", 0.0).unwrap();
        assert_eq!(last(&calls), ("tab.focus".into(), json!({"tab_id": "t1"})));
        assert_eq!(calls.lock().unwrap()[1].1, json!({"workspace_id": "w1"}));
    }

    #[test]
    fn run_preset_dial_mode_switches_the_dial_and_debounces() {
        // The only test that touches the process-wide dial.
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, agents(None));
        herdr.run_preset("dial-next", 100.0).unwrap();
        assert_eq!(last(&calls).0, "workspace.list");
        herdr.run_preset("dial-mode", 100.0).unwrap();
        herdr.run_preset("dial-next", 101.0).unwrap();
        assert_eq!(
            last(&calls),
            ("agent.focus".into(), json!({"target": "blocked"}))
        );
        let before = calls.lock().unwrap().len();
        herdr.run_preset("dial-next", 101.05).unwrap();
        assert_eq!(calls.lock().unwrap().len(), before);
        // Third mode: effort. One step up from medium types /effort high.
        herdr.run_preset("dial-mode", 102.0).unwrap();
        herdr.run_preset("dial-next", 103.0).unwrap();
        let sent: Vec<_> = calls.lock().unwrap()[before..]
            .iter()
            .filter(|(m, _)| m.starts_with("pane.send"))
            .map(|(m, p)| (m.clone(), p.get("text").or(p.get("keys")).cloned()))
            .collect();
        assert_eq!(
            sent,
            [
                ("pane.send_text".to_string(), Some(json!("/effort high"))),
                ("pane.send_keys".to_string(), Some(json!(["enter"]))),
            ]
        );
        herdr.run_preset("dial-mode", 104.0).unwrap();
    }

    #[test]
    fn run_preset_fixed_calls_and_unknown_name() {
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, agents(None));
        for (name, method, params) in [
            ("tab-new", "tab.create", json!({"focus": true})),
            ("zoom", "pane.zoom", json!({})),
            (
                "pane-split-down",
                "pane.split",
                json!({"direction": "down", "focus": true}),
            ),
            (
                "pane-up",
                "pane.focus_direction",
                json!({"direction": "up"}),
            ),
        ] {
            herdr.run_preset(name, 0.0).unwrap();
            assert_eq!(last(&calls), (method.into(), params));
        }
        assert!(herdr.run_preset("bogus", 0.0).is_err());
    }

    #[test]
    fn send_to_focused_pane_adds_the_pane_id() {
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, agents(None));
        herdr
            .send_to_focused_pane("pane.send_keys", json!({"keys": ["C-c"]}))
            .unwrap();
        assert_eq!(
            last(&calls),
            (
                "pane.send_keys".into(),
                json!({"pane_id": "p9", "keys": ["C-c"]})
            )
        );
    }

    #[test]
    fn request_maps_error_and_missing_result() {
        let dir = TempDir::new();
        let (herdr, _) = serve(&dir, |method, _| match method {
            "bad" => json!({"id": "freemicro", "error": "no such method"}),
            _ => json!({"id": "freemicro"}),
        });
        assert_eq!(
            herdr.request("bad", Value::Null),
            Err(HerdrError("herdr bad: no such method".into()))
        );
        assert_eq!(herdr.request("ok", Value::Null), Ok(json!({})));
    }

    #[test]
    fn focus_session_focuses_the_pane_by_session_id() {
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, |_, _| {
            json!({"result": {"agents": [
                {"pane_id": "w1:p1", "agent_session": {"value": "other"}},
                {"pane_id": "w3:p7", "agent_session": {"value": "a"}},
            ]}})
        });
        assert!(herdr.focus_session("a"));
        assert_eq!(
            last(&calls),
            ("agent.focus".into(), json!({"target": "w3:p7"}))
        );
        assert!(!herdr.focus_session("missing"));
    }

    #[test]
    fn agent_sessions_gives_one_key_per_herdr_agent() {
        let dir = TempDir::new();
        let (herdr, _) = serve(&dir, |_, _| {
            json!({"result": {"agents": [
                {"pane_id": "w1:p2", "cwd": "/code/api", "agent_status": "working",
                 "agent_session": {"value": "codex-1"}},
                {"pane_id": "w1:p1", "foreground_cwd": "/code/api", "agent_status": "idle",
                 "agent_session": {"value": "s1"}},
                {"pane_id": "w1:p3", "agent_status": "done"},
            ]}})
        });
        let mut hooked = SessionState::new("s1", AgentState::Waiting, 1.0);
        hooked.cwd = "/code/old-subdir".into();
        let gone = SessionState::new("s9", AgentState::Done, 1.0);
        let out = herdr.agent_sessions(&[hooked, gone], 50.0);
        let got: Vec<_> = out
            .iter()
            .map(|s| (s.session_id.as_str(), s.cwd.as_str(), s.state, s.updated_at))
            .collect();
        assert_eq!(
            got,
            [
                // herdr's status wins, as the pad must match what herdr shows;
                // a hooked agent keeps its record otherwise.
                ("s1", "/code/api", AgentState::Idle, 1.0),
                ("codex-1", "/code/api#2", AgentState::Working, 50.0),
            ]
        );
        assert!(out.iter().all(|s| s.term_program == "herdr"));
    }

    #[test]
    fn agent_sessions_is_a_no_op_without_herdr() {
        let dir = TempDir::new();
        let herdr = Herdr {
            socket: dir.path().join("absent.sock"),
        };
        let mut s = SessionState::new("s1", AgentState::Idle, 1.0);
        s.cwd = "/a".into();
        assert_eq!(herdr.agent_sessions(&[s.clone()], 2.0), vec![s]);
    }
}
