//! Agent state: the session store, decay rules, hook classification, Agent
//! Key slot resolution and Codex coexistence. Port of `freemicro/state/*`
//! and `freemicro/agentkeys.py`.
//!
//! Owner: state. Leaf module: depends on nothing else in the crate except
//! `crate::config` and `crate::clock`.

pub mod agentkeys;
pub mod hooks;
pub mod process;
pub mod slots;
pub mod store;

use serde_json::Value;

pub use store::StateStore;

/// The normalised states every agent maps onto. JSON value is the
/// lower-case name, as in Python. Variants are declared in priority order,
/// so the derived `Ord` equals [`AgentState::priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgentState {
    Idle,
    Working,
    Done,
    Error,
    Waiting,
}

impl AgentState {
    pub const ALL: [AgentState; 5] = [
        AgentState::Idle,
        AgentState::Working,
        AgentState::Waiting,
        AgentState::Done,
        AgentState::Error,
    ];

    /// `"idle"`, `"working"`, ... (the on-disk value).
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Waiting => "waiting",
            AgentState::Done => "done",
            AgentState::Error => "error",
        }
    }

    pub fn parse(text: &str) -> Option<AgentState> {
        AgentState::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// Higher wins: waiting > error > done > working > idle.
    pub fn priority(self) -> u8 {
        match self {
            AgentState::Idle => 0,
            AgentState::Working => 1,
            AgentState::Done => 2,
            AgentState::Error => 3,
            AgentState::Waiting => 4,
        }
    }

    /// True where the human is the blocker (waiting, error, done).
    pub fn needs_you(self) -> bool {
        matches!(
            self,
            AgentState::Waiting | AgentState::Error | AgentState::Done
        )
    }
}

pub const DEFAULT_TTL_SECONDS: f64 = 30.0 * 60.0;
pub const DEFAULT_DONE_TTL_SECONDS: f64 = 180.0;
pub const DEFAULT_WORKING_TTL_SECONDS: f64 = 120.0;
pub const DEFAULT_TOOL_TTL_SECONDS: f64 = 600.0;
/// `last_event` values that mean a tool call is running.
pub const TOOL_START_EVENTS: [&str; 2] = ["PreToolUse", "PreCompact"];
/// Events that close a turn.
pub const TURN_CLOSING_EVENTS: [&str; 3] = ["Stop", "StopFailure", "SessionEnd"];

/// Every timer that governs how a claim expires, as one value. `0` = never.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayPolicy {
    pub ttl_seconds: f64,
    pub done_ttl_seconds: f64,
    pub working_ttl_seconds: f64,
    pub tool_ttl_seconds: f64,
}

impl Default for DecayPolicy {
    fn default() -> Self {
        DecayPolicy {
            ttl_seconds: DEFAULT_TTL_SECONDS,
            done_ttl_seconds: DEFAULT_DONE_TTL_SECONDS,
            working_ttl_seconds: DEFAULT_WORKING_TTL_SECONDS,
            tool_ttl_seconds: DEFAULT_TOOL_TTL_SECONDS,
        }
    }
}

impl DecayPolicy {
    /// From `config.json`'s `state` block; bad or missing values take the
    /// default, negatives clamp to 0 (Python `_as_seconds`).
    pub fn from_state_block(state: &Value) -> DecayPolicy {
        let d = DecayPolicy::default();
        let get = |name: &str, default: f64| as_seconds(state.get(name), default);
        DecayPolicy {
            ttl_seconds: get("ttl_seconds", d.ttl_seconds),
            done_ttl_seconds: get("done_ttl_seconds", d.done_ttl_seconds),
            working_ttl_seconds: get("working_ttl_seconds", d.working_ttl_seconds),
            tool_ttl_seconds: get("tool_ttl_seconds", d.tool_ttl_seconds),
        }
    }
}

/// Which terminal a session runs in. Every field is a best guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalInfo {
    pub tty: String,
    pub pid: i32,
    pub program: String,
    pub session: String,
}

impl TerminalInfo {
    pub fn is_empty(&self) -> bool {
        self.tty.is_empty() && self.program.is_empty() && self.session.is_empty()
    }
}

/// What else a hook event said, beyond its state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSignals {
    pub event: String,
    pub prompt_id: String,
    pub permission_mode: String,
    pub effort: String,
    pub background_tasks: u32,
    pub end_reason: String,
}

impl SessionSignals {
    pub fn closes_turn(&self) -> bool {
        TURN_CLOSING_EVENTS.contains(&self.event.as_str())
    }
}

/// One session's snapshot. Port of `engine.SessionState`.
///
/// On disk (`<home>/state/<safe id>.json`) it is exactly the 16 keys of
/// [`SessionState::to_json`]; `claimed_state`, `process_alive` and
/// `kept_by_process` are read-time annotations and never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    pub session_id: String,
    pub state: AgentState,
    /// Wall seconds (Python `time.time()` float).
    pub updated_at: f64,
    pub title: String,
    pub cwd: String,
    pub tty: String,
    pub pid: i32,
    pub term_program: String,
    pub term_session: String,
    pub last_event: String,
    pub prompt_id: String,
    pub turn_open: bool,
    pub interrupted: bool,
    pub permission_mode: String,
    pub effort: String,
    pub background_tasks: u32,
    /// What the session claimed before decay retired it, if it did.
    pub claimed_state: Option<AgentState>,
    pub process_alive: Option<bool>,
    pub kept_by_process: bool,
}

impl SessionState {
    /// A record with every optional field empty.
    pub fn new(session_id: &str, state: AgentState, updated_at: f64) -> SessionState {
        SessionState {
            session_id: session_id.to_string(),
            state,
            updated_at,
            title: String::new(),
            cwd: String::new(),
            tty: String::new(),
            pid: 0,
            term_program: String::new(),
            term_session: String::new(),
            last_event: String::new(),
            prompt_id: String::new(),
            turn_open: false,
            interrupted: false,
            permission_mode: String::new(),
            effort: String::new(),
            background_tasks: 0,
            claimed_state: None,
            process_alive: None,
            kept_by_process: false,
        }
    }

    pub fn claim(&self) -> AgentState {
        self.claimed_state.unwrap_or(self.state)
    }

    pub fn stale(&self) -> bool {
        self.claimed_state.is_some_and(|c| c != self.state)
    }

    pub fn tool_running(&self) -> bool {
        TOOL_START_EVENTS.contains(&self.last_event.as_str())
    }

    pub fn busy_elsewhere(&self) -> bool {
        self.background_tasks > 0
    }

    pub fn prompts_for_permission(&self) -> bool {
        self.permission_mode != "bypassPermissions"
    }

    pub fn terminal(&self) -> TerminalInfo {
        TerminalInfo {
            tty: self.tty.clone(),
            pid: self.pid,
            program: self.term_program.clone(),
            session: self.term_session.clone(),
        }
    }

    pub fn age(&self, now: f64) -> f64 {
        now - self.updated_at
    }

    /// The on-disk form: the 16 keys Python writes, in Python's order, plus
    /// `process_alive` / `kept_by_process` only when set (status output).
    pub fn to_json(&self) -> Value {
        let mut data = serde_json::json!({
            "session_id": self.session_id,
            "state": self.state.as_str(),
            "updated_at": self.updated_at,
            "title": self.title,
            "cwd": self.cwd,
            "tty": self.tty,
            "pid": self.pid,
            "term_program": self.term_program,
            "term_session": self.term_session,
            "last_event": self.last_event,
            "prompt_id": self.prompt_id,
            "turn_open": self.turn_open,
            "interrupted": self.interrupted,
            "permission_mode": self.permission_mode,
            "effort": self.effort,
            "background_tasks": self.background_tasks,
        });
        let map = data.as_object_mut().expect("object literal");
        if let Some(alive) = self.process_alive {
            map.insert("process_alive".into(), Value::Bool(alive));
        }
        if self.kept_by_process {
            map.insert("kept_by_process".into(), Value::Bool(true));
        }
        data
    }

    /// Lenient read, as Python `from_dict`: `None` without `session_id`,
    /// a valid `state` or a numeric `updated_at`; bad optional fields default.
    pub fn from_json(data: &Value) -> Option<SessionState> {
        let session_id = match data.get("session_id")? {
            Value::String(s) => s.clone(),
            Value::Null => return None,
            other => other.to_string(),
        };
        let state = AgentState::parse(data.get("state")?.as_str()?)?;
        let updated_at = as_float(data.get("updated_at")?)?;
        let text = |key: &str| text(data.get(key));
        let int = |key: &str| data.get(key).and_then(as_int).unwrap_or(0);
        let flag = |key: &str| data.get(key).is_some_and(truthy);
        let mut record = SessionState::new(&session_id, state, updated_at);
        record.title = text("title");
        record.cwd = text("cwd");
        record.tty = text("tty");
        record.pid = i32::try_from(int("pid")).unwrap_or(0);
        record.term_program = text("term_program");
        record.term_session = text("term_session");
        record.last_event = text("last_event");
        record.prompt_id = text("prompt_id");
        record.turn_open = flag("turn_open");
        record.interrupted = flag("interrupted");
        record.permission_mode = text("permission_mode");
        record.effort = text("effort");
        record.background_tasks = u32::try_from(int("background_tasks").max(0)).unwrap_or(u32::MAX);
        Some(record)
    }

    /// One phrase for `status` output (Python `describe_claim`).
    pub fn describe_claim(&self) -> String {
        let text = if self.stale() {
            let claimed = self.claim().as_str();
            if self.interrupted {
                format!("idle (was {claimed}; last turn was interrupted)")
            } else {
                format!("idle (was {claimed}, and went quiet)")
            }
        } else if self.state == AgentState::Working && self.busy_elsewhere() {
            let count = self.background_tasks;
            let plural = if count == 1 { "" } else { "s" };
            format!("working ({count} background task{plural})")
        } else {
            self.state.as_str().to_string()
        };
        if self.kept_by_process {
            format!("{text} - terminal still open")
        } else {
            text
        }
    }
}

/// Python `_as_seconds`: bad or non-finite -> `default`, negative -> 0.
fn as_seconds(value: Option<&Value>, default: f64) -> f64 {
    match value.and_then(as_float) {
        Some(seconds) if seconds.is_finite() => seconds.max(0.0),
        _ => default,
    }
}

/// Python `float(value)` for JSON: numbers, numeric strings, bools.
fn as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// Python `int(value)` for JSON (floats truncate); `None` if it is not one.
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// Python truthiness of a JSON value.
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

/// A string field: strings as-is, numbers and bools as JSON text, else `""`.
fn text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
        _ => String::new(),
    }
}
