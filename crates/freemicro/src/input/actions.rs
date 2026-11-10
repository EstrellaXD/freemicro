//! Binding kinds and how they reach the OS. Port of `input/actions.py`.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::keys::parse_combo;
use crate::herdr::{self, Herdr};
use crate::permission_prompt;
use crate::runtime::lighting_config::ActivityLight;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError(pub String);

impl std::fmt::Display for ActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ActionError {}

/// The binding kinds. Parameters stay in [`Action::params`], validated at
/// parse time per kind (Python `REGISTRY` / `validate_params`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    Key,
    Text,
    Hold,
    Shell,
    App,
    AppleScript,
    Herdr,
    FocusSession,
    None,
    Layer,
    Mouse,
    AnswerPermission,
    /// A kind this port does not know: loads with a warning, fires as a
    /// logged no-op.
    Unsupported(String),
}

const KINDS: [(&str, ActionKind); 12] = [
    ("key", ActionKind::Key),
    ("text", ActionKind::Text),
    ("hold", ActionKind::Hold),
    ("shell", ActionKind::Shell),
    ("app", ActionKind::App),
    ("applescript", ActionKind::AppleScript),
    ("herdr", ActionKind::Herdr),
    ("focus_session", ActionKind::FocusSession),
    ("none", ActionKind::None),
    ("layer", ActionKind::Layer),
    ("mouse", ActionKind::Mouse),
    ("answer_permission", ActionKind::AnswerPermission),
];

impl ActionKind {
    /// keymap.json `action` value -> kind (`"applescript"`, `"focus_session"`, ...).
    pub fn parse(name: &str) -> ActionKind {
        KINDS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, k)| k.clone())
            .unwrap_or_else(|| ActionKind::Unsupported(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        match self {
            ActionKind::Unsupported(name) => name,
            kind => KINDS
                .iter()
                .find(|(_, k)| k == kind)
                .map(|(n, _)| *n)
                .unwrap_or(""),
        }
    }

    /// Still means what it says while a `hold` has real modifiers down.
    /// An allowlist: anything else (including an unknown kind) may type.
    pub fn modifier_safe(&self) -> bool {
        matches!(
            self,
            ActionKind::Shell
                | ActionKind::App
                | ActionKind::Herdr
                | ActionKind::FocusSession
                | ActionKind::None
                | ActionKind::Layer
                | ActionKind::Mouse
        )
    }
}

// -- Python value semantics, shared with `config` ---------------------------

/// Python `bool(value)`.
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `str(value)`.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        other => other.to_string(),
    }
}

/// Python `float(value)`; `None` where Python raises.
pub(crate) fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python `int(value)`; `None` where Python raises.
pub(crate) fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Python `repr(str)`, single-quoted.
pub(crate) fn repr(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A resolved binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub kind: ActionKind,
    pub params: Map<String, Value>,
    pub label: String,
    pub comment: String,
    /// What the pad shows while this binding is live.
    pub light: Option<ActivityLight>,
}

impl Action {
    fn param(&self, name: &str) -> Option<&Value> {
        self.params.get(name).filter(|v| !v.is_null())
    }
    fn flag(&self, name: &str) -> bool {
        self.params.get(name).is_some_and(truthy)
    }
    fn text(&self, name: &str) -> String {
        self.param(name).map(py_str).unwrap_or_default()
    }

    pub fn describe(&self) -> String {
        match &self.kind {
            ActionKind::Text => {
                let submit = if self.flag("submit") { " + Return" } else { "" };
                format!("type {}{submit}", repr(&self.text("text")))
            }
            ActionKind::Key => format!("press {}", self.text("key")),
            ActionKind::Shell => format!("run $ {}", self.text("command")),
            ActionKind::AppleScript => "run AppleScript".to_string(),
            ActionKind::App => {
                let cycle = if self.flag("cycle") {
                    " (cycle windows)"
                } else {
                    ""
                };
                format!("focus {}{cycle}", self.text("name"))
            }
            ActionKind::Herdr => {
                let field = ["preset", "keys", "text"]
                    .into_iter()
                    .find(|f| self.flag(f))
                    .unwrap_or("preset");
                format!("herdr {field} {}", self.text(field))
            }
            ActionKind::FocusSession => {
                // Static on purpose: Python resolved the live plan here, which
                // reads the session store on every log line.
                let target = match self.param("project") {
                    Some(p) if truthy(p) => format!("project {}", py_str(p)),
                    _ => match self.param("slot").and_then(py_int) {
                        Some(slot) => format!("Agent Key {}'s session", slot + 1),
                        None => "this key's session".to_string(),
                    },
                };
                let mut line = format!("focus {target}");
                let app = self.text("terminal");
                if self.params.get("new_terminal").is_none_or(truthy) && !app.is_empty() {
                    line += &format!("; opens a new {app} window if nothing is there");
                }
                line
            }
            ActionKind::Hold => {
                let key = self.text("key");
                if self.flag("latch") {
                    format!("tap {key} to record; tap-tap to keep recording, tap again to stop")
                } else if let Some(second) = double_tap_combo(self) {
                    format!("hold {key} while pressed; double-tap sends {second}")
                } else {
                    format!("hold {key} while pressed")
                }
            }
            ActionKind::None => "(unbound)".to_string(),
            ActionKind::Layer => {
                format!("layer: hold for the '{}' bindings", self.text("layer"))
            }
            ActionKind::Mouse => {
                if self.flag("click") {
                    let count = mouse_count(self);
                    let suffix = if count > 1 {
                        format!(" x{count}")
                    } else {
                        String::new()
                    };
                    format!("{} click{suffix}", self.text("click"))
                } else {
                    let coord = |name| self.param(name).map(py_str).unwrap_or("0".to_string());
                    format!("move mouse ({}, {})", coord("x"), coord("y"))
                }
            }
            ActionKind::AnswerPermission => {
                let plan = permission_prompt::plan_live(
                    &answer_of(self, "answer"),
                    &self.text("project"),
                    number_of(self, "max_age", permission_prompt::MAX_AGE_SECONDS),
                    &crate::config::config_home(),
                );
                let mut line = plan.describe();
                let long_answer = answer_of(self, "long_press");
                if !long_answer.is_empty() {
                    let held = number_of(self, "long_press_ms", DEFAULT_LONG_PRESS_MS);
                    line += &format!("; hold {held}ms for {long_answer}");
                }
                line
            }
            ActionKind::Unsupported(name) => {
                format!("{name} (not supported by freemicro-rs; does nothing)")
            }
        }
    }
}

fn mouse_count(act: &Action) -> u32 {
    match act.params.get("count") {
        Some(v) if truthy(v) => py_int(v).unwrap_or(1).clamp(1, i64::from(u32::MAX)) as u32,
        _ => 1,
    }
}

/// How an action reaches the outside world. Real: `quartz::CGEventBackend`;
/// dry runs and tests: [`RecordingBackend`]. Used only on the pad-owner
/// thread.
pub trait Backend {
    fn description(&self) -> &str;
    fn type_text(&mut self, text: &str) -> Result<(), ActionError>;
    fn press_key(&mut self, combo: &str) -> Result<(), ActionError>;
    fn hold_key(&mut self, combo: &str, down: bool) -> Result<(), ActionError>;
    fn run_shell(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        wait: bool,
    ) -> Result<(), ActionError>;
    fn run_applescript(&mut self, script: &str) -> Result<(), ActionError>;
    fn activate_app(&mut self, name: &str, cycle: bool) -> Result<(), ActionError>;
    fn move_mouse(&mut self, x: f64, y: f64, relative: bool) -> Result<(), ActionError>;
    fn click_mouse(&mut self, button: &str, count: u32) -> Result<(), ActionError>;
    /// Let go of anything a `hold` left down; count released.
    fn release_held_keys(&mut self) -> usize;
    /// Pause between deliveries; a recording backend only notes it.
    fn settle(&mut self, seconds: f64);
}

/// Records calls as strings (`"press cmd+c"`, `"type hello"`, ...).
#[derive(Debug, Default)]
pub struct RecordingBackend {
    pub calls: Vec<String>,
    pub held: Vec<String>,
}

impl Backend for RecordingBackend {
    fn description(&self) -> &str {
        "dry run (recording only)"
    }
    fn type_text(&mut self, text: &str) -> Result<(), ActionError> {
        self.calls.push(format!("type {text}"));
        Ok(())
    }
    fn press_key(&mut self, combo: &str) -> Result<(), ActionError> {
        self.calls.push(format!("press {combo}"));
        Ok(())
    }
    fn hold_key(&mut self, combo: &str, down: bool) -> Result<(), ActionError> {
        let half = if down { "down" } else { "up" };
        self.calls.push(format!("hold {combo} {half}"));
        if down {
            self.held.push(combo.to_string());
        } else if let Some(at) = self.held.iter().position(|c| c == combo) {
            self.held.remove(at);
        }
        Ok(())
    }
    fn run_shell(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        wait: bool,
    ) -> Result<(), ActionError> {
        self.calls.push(format!(
            "shell {command} cwd={} wait={wait}",
            cwd.unwrap_or("")
        ));
        Ok(())
    }
    fn run_applescript(&mut self, script: &str) -> Result<(), ActionError> {
        self.calls.push(format!("applescript {script}"));
        Ok(())
    }
    fn activate_app(&mut self, name: &str, cycle: bool) -> Result<(), ActionError> {
        self.calls.push(format!("app {name} cycle={cycle}"));
        Ok(())
    }
    fn move_mouse(&mut self, x: f64, y: f64, relative: bool) -> Result<(), ActionError> {
        self.calls.push(format!("move {x} {y} relative={relative}"));
        Ok(())
    }
    fn click_mouse(&mut self, button: &str, count: u32) -> Result<(), ActionError> {
        self.calls.push(format!("click {button} {count}"));
        Ok(())
    }
    fn release_held_keys(&mut self) -> usize {
        let outstanding = self.held.clone();
        for combo in outstanding.iter().rev() {
            let _ = self.hold_key(combo, false);
        }
        self.calls
            .push(format!("release_held_keys {}", outstanding.len()));
        outstanding.len()
    }
    fn settle(&mut self, seconds: f64) {
        self.calls
            .push(format!("settle {}", (seconds * 1000.0).round() / 1000.0));
    }
}

/// CGEvent when the process may post events, else the osascript backend.
pub fn best_backend() -> Box<dyn Backend> {
    use super::quartz::{AppleScriptBackend, CGEventBackend};
    let script = AppleScriptBackend {
        timeout_seconds: 5.0,
    };
    if super::quartz::is_available() {
        Box::new(CGEventBackend { script })
    } else {
        Box::new(script)
    }
}

/// What actions need beyond the backend.
#[derive(Debug, Clone)]
pub struct ActionEnv {
    /// `~/.freemicro`, for `focus_session` (store + slots cache).
    pub home: PathBuf,
    pub herdr: Herdr,
}

fn err(message: impl Into<String>) -> ActionError {
    ActionError(message.into())
}

/// (required, optional) fields per kind.
fn fields(kind: &ActionKind) -> (&'static [&'static str], &'static [&'static str]) {
    match kind {
        ActionKind::Text => (&["text"], &["submit", "submit_delay"]),
        ActionKind::Key => (&["key"], &[]),
        ActionKind::Shell => (&["command"], &["cwd", "wait"]),
        ActionKind::AppleScript => (&["script"], &[]),
        ActionKind::App => (&["name"], &["cycle"]),
        ActionKind::Herdr => (&[], &["preset", "keys", "text"]),
        ActionKind::FocusSession => (
            &[],
            &["slot", "project", "fallback", "new_terminal", "terminal"],
        ),
        ActionKind::Hold => (&["key"], &["latch", "double_tap"]),
        ActionKind::None => (&[], &[]),
        ActionKind::Layer => (&["layer"], &[]),
        ActionKind::Mouse => (&[], &["x", "y", "absolute", "click", "count"]),
        ActionKind::AnswerPermission => (
            &[],
            &[
                "answer",
                "long_press",
                "long_press_ms",
                "project",
                "max_age",
            ],
        ),
        ActionKind::Unsupported(_) => (&[], &[]),
    }
}

/// Check one binding's params at parse time.
pub fn validate_params(kind: &ActionKind, params: &Map<String, Value>) -> Result<(), ActionError> {
    if matches!(kind, ActionKind::Unsupported(_)) {
        // Python-only kind: its params are Python's business.
        return Ok(());
    }
    let name = kind.as_str();
    let (required, optional) = fields(kind);
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|f| !params.contains_key(*f))
        .collect();
    if !missing.is_empty() {
        return Err(err(format!(
            "action '{name}' is missing required field(s): {}",
            missing.join(", ")
        )));
    }
    let mut unknown: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|k| !required.contains(k) && !optional.contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        let all: Vec<&str> = required.iter().chain(optional).copied().collect();
        let allowed = if all.is_empty() {
            "(none)".to_string()
        } else {
            all.join(", ")
        };
        return Err(err(format!(
            "action '{name}' does not take {}; it takes: {allowed}",
            unknown.join(", ")
        )));
    }
    let combo = |field: &str| -> Result<(), ActionError> {
        parse_combo(&py_str(&params[field]))
            .map(|_| ())
            .map_err(|e| err(e.0))
    };
    match kind {
        ActionKind::Key => combo("key"),
        ActionKind::Herdr => {
            let given: Vec<&str> = ["preset", "keys", "text"]
                .into_iter()
                .filter(|f| params.get(*f).is_some_and(truthy))
                .collect();
            if given.len() != 1 {
                return Err(err(
                    "a herdr binding needs exactly one of preset, keys or text",
                ));
            }
            if given == ["preset"] {
                let preset = py_str(&params["preset"]);
                if !herdr::PRESET_NAMES.contains(&preset.as_str()) {
                    let mut names = herdr::PRESET_NAMES.to_vec();
                    names.sort();
                    return Err(err(format!(
                        "unknown herdr preset {}; expected one of {}",
                        repr(&preset),
                        names.join(", ")
                    )));
                }
            }
            Ok(())
        }
        ActionKind::FocusSession => {
            if let Some(v) = params.get("new_terminal").filter(|v| !v.is_null()) {
                if !v.is_boolean() {
                    return Err(err(format!("'new_terminal' must be true or false, got {v}")));
                }
            }
            if let Some(v) = params.get("terminal").filter(|v| !v.is_null()) {
                if !v.is_string() {
                    return Err(err(format!("'terminal' must be an app name, got {v}")));
                }
            }
            Ok(())
        }
        ActionKind::Hold => {
            combo("key")?;
            if let Some(v) = params.get("latch").filter(|v| !v.is_null()) {
                if !v.is_boolean() {
                    return Err(err(format!("'latch' must be true or false, got {v}")));
                }
            }
            if double_tap_of(params).is_some() {
                if params.get("latch").is_some_and(truthy) {
                    return Err(err(
                        "'double_tap' and 'latch' cannot be combined on one binding: they are \
                         two different models of the same key. 'latch' taps the same shortcut \
                         for a toggle app; 'double_tap' keeps a real hold for push-to-talk and \
                         fires a *second* shortcut on a double-tap. Pick one.",
                    ));
                }
                combo("double_tap")?;
            }
            Ok(())
        }
        ActionKind::AnswerPermission => {
            for name in ["answer", "long_press"] {
                match params.get(name) {
                    None | Some(Value::Null) => {}
                    Some(Value::String(s)) if s.is_empty() => {}
                    Some(v) if permission_prompt::normalise_answer(&py_str(v)).is_empty() => {
                        let shown = match v {
                            Value::String(s) => repr(s),
                            other => py_str(other),
                        };
                        return Err(err(format!(
                            "'{name}' must be one of {}, got {shown}",
                            permission_prompt::ANSWERS.join(", ")
                        )));
                    }
                    Some(_) => {}
                }
            }
            for name in ["long_press_ms", "max_age"] {
                if let Some(v) = params.get(name) {
                    let number =
                        py_float(v).ok_or_else(|| err(format!("'{name}' must be a number")))?;
                    if number < 0.0 {
                        return Err(err(format!("'{name}' must be 0 or more")));
                    }
                }
            }
            Ok(())
        }
        ActionKind::Layer => match params.get("layer") {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(()),
            _ => Err(err(
                "a layer trigger needs a layer name, e.g. {\"action\": \"layer\", \"layer\": \"fn\"}",
            )),
        },
        _ => Ok(()),
    }
}

pub const SUBMIT_SETTLE_BASE: f64 = 0.12;
pub const SUBMIT_SETTLE_PER_CHAR: f64 = 0.003;
pub const SUBMIT_SETTLE_MAX: f64 = 0.6;

/// Fire `act` (press half).
pub fn perform(
    act: &Action,
    backend: &mut dyn Backend,
    env: &ActionEnv,
) -> Result<(), ActionError> {
    match &act.kind {
        ActionKind::Text => {
            let text = act.text("text");
            backend.type_text(&text)?;
            if act.flag("submit") {
                // A long burst reads as a paste; a Return during it is a newline.
                let wait = match act.param("submit_delay") {
                    Some(v) => py_float(v)
                        .ok_or_else(|| err(format!("submit_delay must be a number, got {v}")))?
                        .max(0.0),
                    None => (SUBMIT_SETTLE_BASE
                        + text.chars().count() as f64 * SUBMIT_SETTLE_PER_CHAR)
                        .min(SUBMIT_SETTLE_MAX),
                };
                backend.settle(wait);
                backend.press_key("return")?;
            }
            Ok(())
        }
        ActionKind::Key => backend.press_key(&act.text("key")),
        ActionKind::Shell => {
            let cwd = act.params.get("cwd").filter(|v| truthy(v)).map(py_str);
            backend.run_shell(&act.text("command"), cwd.as_deref(), act.flag("wait"))
        }
        ActionKind::AppleScript => backend.run_applescript(&act.text("script")),
        ActionKind::App => backend.activate_app(&act.text("name"), act.flag("cycle")),
        ActionKind::Herdr => {
            let herdr = env.herdr.clone();
            if act.flag("preset") {
                let preset = act.text("preset");
                let now = crate::clock::mono();
                herdr::run_in_background(move || herdr.run_preset(&preset, now));
            } else if act.flag("keys") {
                let keys = act.text("keys");
                herdr::run_in_background(move || {
                    herdr.send_to_focused_pane("pane.send_keys", json!({"keys": [keys]}))
                });
            } else {
                let text = act.text("text");
                herdr::run_in_background(move || {
                    herdr.send_to_focused_pane("pane.send_text", json!({"text": text}))
                });
            }
            Ok(())
        }
        ActionKind::FocusSession => {
            focus_session(act, backend, env);
            Ok(())
        }
        ActionKind::Hold => backend.hold_key(&act.text("key"), true),
        // With a long press, the release decides (see `release`).
        ActionKind::AnswerPermission if !answer_of(act, "long_press").is_empty() => Ok(()),
        ActionKind::AnswerPermission => {
            deliver_answer(act, &answer_of(act, "answer"), backend, env)
        }
        ActionKind::None | ActionKind::Layer | ActionKind::Unsupported(_) => Ok(()),
        ActionKind::Mouse => {
            let coord = |name| act.params.get(name).and_then(py_float).unwrap_or(0.0);
            let (x, y) = (coord("x"), coord("y"));
            let absolute = act.flag("absolute");
            if x != 0.0 || y != 0.0 || absolute {
                backend.move_mouse(x, y, !absolute)?;
            }
            if act.flag("click") {
                backend.click_mouse(&act.text("click"), mouse_count(act))?;
            }
            Ok(())
        }
    }
}

/// Raise the project's terminal; on an empty slot open a new terminal
/// unless `new_terminal` is false or no terminal app is set.
fn focus_session(act: &Action, backend: &mut dyn Backend, env: &ActionEnv) {
    use crate::focus::{self, FocusPlan};
    use crate::state::agentkeys::normalise_project;

    let agent_keys = super::config::resolve_path(&env.home, None)
        .and_then(|p| super::config::load(&p))
        .map(|c| c.agent_keys)
        .unwrap_or_default();
    let now = crate::clock::wall();
    let mut store = crate::state::StateStore::for_home(&env.home);
    let slots = focus::current_slots(&env.home, &agent_keys, &mut store, now, &env.herdr);
    let fallback = act.params.get("fallback").is_none_or(truthy);
    let mut tty = |pid: i32| crate::state::process::tty_for_pid(pid).0;
    let wanted = normalise_project(&act.text("project"));
    let slot = act.param("slot").and_then(py_int);
    let plan = if !wanted.is_empty() {
        match slots
            .iter()
            .find(|s| s.path == wanted && s.project.is_some())
        {
            Some(s) => {
                focus::plan_for_session(s.session(), &s.path, &s.label(), fallback, &mut tty, now)
            }
            None => {
                let session = slots
                    .iter()
                    .find(|s| s.project.as_ref().is_some_and(|p| p.path == wanted))
                    .and_then(|s| s.session());
                focus::plan_for_session(session, &wanted, "", fallback, &mut tty, now)
            }
        }
    } else {
        match slot.filter(|i| (0..6).contains(i)) {
            Some(index) => focus::plan_for_slot(index as usize, &slots, fallback, &mut tty, now),
            None => FocusPlan {
                reason: format!(
                    "slot {} is not one of the six Agent Keys",
                    slot.unwrap_or(-1)
                ),
                ..FocusPlan::default()
            },
        }
    };
    if focus::perform(&plan, backend, &env.herdr) || !plan.slot_empty() {
        return;
    }
    if !act.params.get("new_terminal").is_none_or(truthy) {
        return;
    }
    let app = act.text("terminal");
    if !app.is_empty() {
        focus::open_new_terminal(&app, backend);
    }
}

/// The release half (hold up, latch logic is in the bridge). `held_for`
/// is how long the key was down, if the bridge saw its press.
pub fn release(
    act: &Action,
    backend: &mut dyn Backend,
    env: &ActionEnv,
    held_for: Option<f64>,
) -> Result<(), ActionError> {
    match act.kind {
        ActionKind::Hold => backend.hold_key(&act.text("key"), false),
        ActionKind::AnswerPermission => {
            let long_answer = answer_of(act, "long_press");
            let Some(held_for) = held_for.filter(|_| !long_answer.is_empty()) else {
                return Ok(()); // no long press, so the press already answered
            };
            let threshold = number_of(act, "long_press_ms", DEFAULT_LONG_PRESS_MS);
            let answer = if held_for.max(0.0) * 1000.0 >= threshold {
                long_answer
            } else {
                answer_of(act, "answer")
            };
            deliver_answer(act, &answer, backend, env)
        }
        _ => Ok(()),
    }
}

/// Kinds that act on the key-up as well (Python `HOLD_KINDS`).
pub fn cares_about_release(kind: &ActionKind) -> bool {
    matches!(kind, ActionKind::Hold | ActionKind::AnswerPermission)
}

/// The vendor firmware's own long-press threshold.
pub const DEFAULT_LONG_PRESS_MS: f64 = 500.0;

/// `answer` defaults to approve; `long_press` to none.
fn answer_of(act: &Action, name: &str) -> String {
    let raw = act.text(name);
    if name == "answer" && raw.is_empty() {
        return permission_prompt::APPROVE.to_string();
    }
    permission_prompt::normalise_answer(&raw)
}

/// A number >= 0, else `fallback`.
fn number_of(act: &Action, name: &str, fallback: f64) -> f64 {
    act.params
        .get(name)
        .and_then(py_float)
        .filter(|v| *v >= 0.0)
        .unwrap_or(fallback)
}

/// Plan one answer and maybe send it. Silence is a correct outcome.
fn deliver_answer(
    act: &Action,
    answer: &str,
    backend: &mut dyn Backend,
    env: &ActionEnv,
) -> Result<(), ActionError> {
    if answer.is_empty() {
        return Ok(());
    }
    let plan = permission_prompt::plan_live(
        answer,
        &act.text("project"),
        number_of(act, "max_age", permission_prompt::MAX_AGE_SECONDS),
        &env.home,
    );
    permission_prompt::perform(&plan, backend).map(|_| ())
}

pub fn is_latching(act: &Action) -> bool {
    act.kind == ActionKind::Hold && act.flag("latch")
}

fn double_tap_of(params: &Map<String, Value>) -> Option<String> {
    match params.get("double_tap") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(v) => Some(py_str(v)),
    }
}

pub fn double_tap_combo(act: &Action) -> Option<String> {
    if act.kind != ActionKind::Hold {
        return None;
    }
    double_tap_of(&act.params)
}

pub fn is_layer(act: &Action) -> bool {
    act.kind == ActionKind::Layer
}

pub fn layer_name(act: &Action) -> String {
    if is_layer(act) {
        act.params.get("layer").map(py_str).unwrap_or_default()
    } else {
        String::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn action(kind: &str, params: Value) -> Action {
        Action {
            kind: ActionKind::parse(kind),
            params: params.as_object().cloned().unwrap_or_default(),
            label: kind.to_string(),
            comment: String::new(),
            light: None,
        }
    }

    pub(crate) fn env() -> ActionEnv {
        ActionEnv {
            home: std::env::temp_dir().join("freemicro-rs-no-home"),
            herdr: Herdr {
                socket: std::env::temp_dir().join("freemicro-rs-no-herdr.sock"),
            },
        }
    }

    fn run(kind: &str, params: Value) -> Vec<String> {
        let mut backend = RecordingBackend::default();
        perform(&action(kind, params), &mut backend, &env()).unwrap();
        backend.calls
    }

    #[test]
    fn kind_names_round_trip_and_unknown_is_unsupported() {
        for (name, kind) in KINDS.iter() {
            assert_eq!(&ActionKind::parse(name), kind);
            assert_eq!(kind.as_str(), *name);
        }
        let other = ActionKind::parse("answer_telepathy");
        assert_eq!(
            other,
            ActionKind::Unsupported("answer_telepathy".to_string())
        );
        assert_eq!(other.as_str(), "answer_telepathy");
        assert!(!other.modifier_safe());
    }

    #[test]
    fn perform_delivers_each_kind_through_the_backend() {
        let cases: Vec<(&str, Value, Vec<&str>)> = vec![
            ("key", json!({"key": "escape"}), vec!["press escape"]),
            ("text", json!({"text": "hi"}), vec!["type hi"]),
            (
                "text",
                json!({"text": "continue", "submit": true}),
                vec!["type continue", "settle 0.144", "press return"],
            ),
            (
                "text",
                json!({"text": "x", "submit": true, "submit_delay": -1}),
                vec!["type x", "settle 0", "press return"],
            ),
            (
                "shell",
                json!({"command": "ls", "cwd": "/tmp", "wait": true}),
                vec!["shell ls cwd=/tmp wait=true"],
            ),
            (
                "app",
                json!({"name": "Ghostty"}),
                vec!["app Ghostty cycle=false"],
            ),
            (
                "applescript",
                json!({"script": "beep"}),
                vec!["applescript beep"],
            ),
            (
                "hold",
                json!({"key": "ctrl+cmd+o"}),
                vec!["hold ctrl+cmd+o down"],
            ),
            ("mouse", json!({"y": -40}), vec!["move 0 -40 relative=true"]),
            (
                "mouse",
                json!({"click": "left", "count": 2}),
                vec!["click left 2"],
            ),
            (
                "mouse",
                json!({"absolute": true}),
                vec!["move 0 0 relative=false"],
            ),
            ("none", json!({}), vec![]),
            ("layer", json!({"layer": "fn"}), vec![]),
            ("answer_permission", json!({"answer": "approve"}), vec![]),
        ];
        for (kind, params, want) in cases {
            assert_eq!(run(kind, params.clone()), want, "{kind} {params}");
        }
    }

    #[test]
    fn release_lets_go_of_a_hold_only() {
        let mut backend = RecordingBackend::default();
        release(
            &action("hold", json!({"key": "f5"})),
            &mut backend,
            &env(),
            None,
        )
        .unwrap();
        release(
            &action("key", json!({"key": "f5"})),
            &mut backend,
            &env(),
            None,
        )
        .unwrap();
        assert_eq!(backend.calls, vec!["hold f5 up"]);
    }

    #[test]
    fn validate_params_rejects_what_python_rejects() {
        let bad: Vec<(&str, Value)> = vec![
            ("key", json!({})),
            ("key", json!({"key": "escape", "x": 1})),
            ("key", json!({"key": "nope+q"})),
            ("herdr", json!({})),
            ("herdr", json!({"preset": "tab-next", "text": "x"})),
            ("herdr", json!({"preset": "warp-drive"})),
            ("focus_session", json!({"new_terminal": "yes"})),
            ("focus_session", json!({"terminal": 3})),
            ("hold", json!({"key": "f5", "latch": "true"})),
            (
                "hold",
                json!({"key": "f5", "latch": true, "double_tap": "f6"}),
            ),
            ("hold", json!({"key": "f5", "double_tap": "bogus+x"})),
            ("layer", json!({"layer": "  "})),
            ("answer_permission", json!({"answer": "maybe"})),
            ("answer_permission", json!({"long_press_ms": "soon"})),
            ("answer_permission", json!({"max_age": -1})),
            ("answer_permission", json!({"slot": 1})),
        ];
        for (kind, params) in bad {
            let p = params.as_object().unwrap();
            assert!(
                validate_params(&ActionKind::parse(kind), p).is_err(),
                "{kind} {params}"
            );
        }
        let good: Vec<(&str, Value)> = vec![
            ("herdr", json!({"preset": "tab-next"})),
            ("hold", json!({"key": "f5", "double_tap": "f6"})),
            ("answer_telepathy", json!({"anything": 1})),
            (
                "answer_permission",
                json!({"answer": "Reject", "long_press": "", "long_press_ms": "750", "max_age": 0}),
            ),
            ("mouse", json!({})),
        ];
        for (kind, params) in good {
            let p = params.as_object().unwrap();
            assert!(
                validate_params(&ActionKind::parse(kind), p).is_ok(),
                "{kind} {params}"
            );
        }
    }

    #[test]
    fn describe_matches_python() {
        let cases: Vec<(&str, Value, &str)> = vec![
            (
                "text",
                json!({"text": "/resume", "submit": true}),
                "type '/resume' + Return",
            ),
            ("key", json!({"key": "escape"}), "press escape"),
            (
                "app",
                json!({"name": "Mail", "cycle": true}),
                "focus Mail (cycle windows)",
            ),
            ("herdr", json!({"keys": "C-c"}), "herdr keys C-c"),
            ("hold", json!({"key": "f5"}), "hold f5 while pressed"),
            (
                "hold",
                json!({"key": "f5", "double_tap": "f6"}),
                "hold f5 while pressed; double-tap sends f6",
            ),
            (
                "hold",
                json!({"key": "f5", "latch": true}),
                "tap f5 to record; tap-tap to keep recording, tap again to stop",
            ),
            (
                "mouse",
                json!({"click": "right", "count": 2}),
                "right click x2",
            ),
            ("mouse", json!({"x": 40}), "move mouse (40, 0)"),
            ("none", json!({}), "(unbound)"),
            (
                "focus_session",
                json!({"slot": 2, "terminal": "Terminal"}),
                "focus Agent Key 3's session; opens a new Terminal window if nothing is there",
            ),
        ];
        for (kind, params, want) in cases {
            assert_eq!(action(kind, params).describe(), want, "{kind}");
        }
    }

    #[test]
    fn latching_double_tap_and_layer_predicates() {
        assert!(is_latching(&action(
            "hold",
            json!({"key": "f5", "latch": true})
        )));
        assert!(!is_latching(&action(
            "key",
            json!({"key": "f5", "latch": true})
        )));
        assert_eq!(
            double_tap_combo(&action("hold", json!({"key": "f5", "double_tap": "f6"}))),
            Some("f6".to_string())
        );
        assert_eq!(
            double_tap_combo(&action("hold", json!({"key": "f5", "double_tap": ""}))),
            None
        );
        assert_eq!(layer_name(&action("layer", json!({"layer": "fn"}))), "fn");
        assert_eq!(layer_name(&action("key", json!({"layer": "fn"}))), "");
    }
}
