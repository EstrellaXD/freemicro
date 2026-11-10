//! Agent Key slot resolution and Codex coexistence. Port of
//! `freemicro/agentkeys.py`.

use serde_json::Value;

use super::store::effective_state;
use super::{AgentState, DecayPolicy, SessionState};

pub const SLOT_COUNT: usize = 6;
pub const DEFAULT_CODEX_POLL_SECONDS: f64 = 3.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    #[default]
    Recent,
    Pinned,
    Manual,
    Mirror,
    /// Not sticky: urgent states take the top keys (waiting first).
    Status,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Recent => "recent",
            Policy::Pinned => "pinned",
            Policy::Manual => "manual",
            Policy::Mirror => "mirror",
            Policy::Status => "status",
        }
    }
}

/// The parsed `agent_keys` block of keymap.json.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentKeysConfig {
    pub policy: Policy,
    /// One normalised project path per key, AG00..AG05; `""` = none.
    pub slots: [String; SLOT_COUNT],
    /// Owned key indices (0..6), sorted, deduplicated; empty input = all six.
    pub keys: Vec<usize>,
}

impl Default for AgentKeysConfig {
    fn default() -> Self {
        AgentKeysConfig {
            policy: Policy::Recent,
            slots: Default::default(),
            keys: (0..SLOT_COUNT).collect(),
        }
    }
}

impl AgentKeysConfig {
    pub fn owns_all(&self) -> bool {
        self.keys.len() == SLOT_COUNT
    }
    pub fn subset(&self) -> bool {
        !self.owns_all()
    }
    pub fn owns(&self, index: usize) -> bool {
        self.keys.contains(&index)
    }
    /// True for an `AGnn` input id whose key this config leaves to Codex.
    pub fn leaves_to_codex(&self, input_id: &str) -> bool {
        !self.owns_all() && agent_key_index(input_id).is_some_and(|i| !self.owns(i))
    }
    /// Pinned paths in slot order; only `pinned` / `manual` honour pins.
    pub fn pins(&self) -> Vec<String> {
        match self.policy {
            Policy::Pinned | Policy::Manual => self.slots.to_vec(),
            _ => vec![String::new(); SLOT_COUNT],
        }
    }
    pub fn mirrors(&self) -> bool {
        self.policy == Policy::Mirror
    }
}

/// Python `type(x).__name__` for a JSON value, for error messages.
fn py_type(value: &Value) -> &'static str {
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

/// Parse the `agent_keys` block (absent = default). `Err` carries the
/// user-facing message Python raises as `AgentKeysError`.
pub fn parse_agent_keys(raw: &Value) -> Result<AgentKeysConfig, String> {
    let map = match raw {
        Value::Null => return Ok(AgentKeysConfig::default()),
        Value::Object(map) => map,
        _ => return Err("\"agent_keys\" must be an object".into()),
    };
    let mut unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !["policy", "slots", "keys", "comment", "_comment"].contains(k))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(format!(
            "agent_keys has unknown field(s): {}",
            unknown.join(", ")
        ));
    }

    let policy_raw = map.get("policy");
    let policy_text = match policy_raw {
        None => "recent".to_string(),
        Some(Value::String(s)) => s.trim().to_lowercase(),
        Some(other) => other.to_string(),
    };
    let policy = match policy_text.as_str() {
        "recent" => Policy::Recent,
        "pinned" => Policy::Pinned,
        "manual" => Policy::Manual,
        "mirror" => Policy::Mirror,
        "status" => Policy::Status,
        _ => {
            return Err(format!(
                "agent_keys.policy must be one of recent, pinned, manual, mirror, status; got {}",
                policy_raw.map_or("None".into(), Value::to_string)
            ))
        }
    };

    let slots_raw: &[Value] = match map.get("slots") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(list)) => list,
        Some(_) => return Err("\"agent_keys.slots\" must be a list of six entries".into()),
    };
    if slots_raw.len() > SLOT_COUNT {
        return Err(format!(
            "agent_keys.slots has {} entries; the pad has {SLOT_COUNT} Agent Keys",
            slots_raw.len()
        ));
    }
    let mut slots: [String; SLOT_COUNT] = Default::default();
    for (index, entry) in slots_raw.iter().enumerate() {
        slots[index] = match entry {
            Value::Null => String::new(),
            Value::String(s) => normalise_project(s),
            other => {
                return Err(format!(
                    "agent_keys.slots[{index}] must be a project path or null, got {}",
                    py_type(other)
                ))
            }
        };
    }

    let keys = match map.get("keys") {
        None | Some(Value::Null) => (0..SLOT_COUNT).collect(),
        Some(Value::Array(list)) => clean_owned_keys(list)?,
        Some(_) => {
            return Err(
                "\"agent_keys.keys\" must be a list of Agent Key indices (0-5); \
                 an empty list or all six means drive every key"
                    .into(),
            )
        }
    };
    Ok(AgentKeysConfig {
        policy,
        slots,
        keys,
    })
}

/// Sorted owned indices; empty = all six; non-integers, out-of-range and
/// repeats are hard errors (Python `_clean_owned_keys`).
fn clean_owned_keys(raw: &[Value]) -> Result<Vec<usize>, String> {
    let last = SLOT_COUNT - 1;
    let mut seen: Vec<usize> = Vec::new();
    for entry in raw {
        let Some(n) = entry.as_i64() else {
            return Err(format!(
                "agent_keys.keys entries must be whole numbers 0-{last}, got {entry}"
            ));
        };
        let index = usize::try_from(n)
            .ok()
            .filter(|&i| i < SLOT_COUNT)
            .ok_or_else(|| format!("agent_keys.keys has {n}; Agent Key indices are 0-{last}"))?;
        if seen.contains(&index) {
            return Err(format!(
                "agent_keys.keys lists key {index} twice; each Agent Key is owned once or not at all"
            ));
        }
        seen.push(index);
    }
    if seen.is_empty() {
        return Ok((0..SLOT_COUNT).collect());
    }
    seen.sort_unstable();
    Ok(seen)
}

/// Expand `~` and normalise textually (Python `expanduser` + `normpath`).
/// No `realpath`: the resolver stays pure and symlinks are not resolved.
pub fn normalise_project(path: &str) -> String {
    let text = path.trim();
    if text.is_empty() {
        return String::new();
    }
    let expanded;
    let text = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match std::env::var("HOME") {
            Ok(home) => {
                let home = if home == "/" {
                    ""
                } else {
                    home.trim_end_matches('/')
                };
                expanded = format!("{home}{rest}");
                if expanded.is_empty() {
                    "/"
                } else {
                    &expanded
                }
            }
            Err(_) => text,
        },
        _ => text,
    };
    normpath(text)
}

/// Python `posixpath.normpath`.
fn normpath(path: &str) -> String {
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if !initial.is_empty() => {}
            _ => parts.push(comp),
        }
    }
    let out = format!("{initial}{}", parts.join("/"));
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

/// `"AG03"` -> `Some(3)`; anything else `None`.
pub fn agent_key_index(input_id: &str) -> Option<usize> {
    let digits = input_id.strip_prefix("AG")?;
    if digits.len() != 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|&i| i < SLOT_COUNT)
}

/// The folder basename shown for a project.
pub fn project_label(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    match path.trim_end_matches('/').rsplit('/').next() {
        Some(base) if !base.is_empty() => base.to_string(),
        _ => path.to_string(),
    }
}

/// The last `depth` components of a path, for display only.
fn tail(path: &str, depth: usize) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return path.to_string();
    }
    parts[parts.len().saturating_sub(depth)..].join("/")
}

/// One project directory and the live sessions in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub path: String,
    pub state: AgentState,
    pub last_active: f64,
    pub sessions: Vec<SessionState>,
    pub label: String,
}

impl Project {
    /// The session that speaks for the project (highest priority, newest).
    pub fn lead(&self) -> Option<&SessionState> {
        self.sessions.first()
    }
}

/// Group sessions by normalised `cwd`, disambiguating equal labels.
pub fn group_projects(sessions: &[SessionState], now: f64, decay: &DecayPolicy) -> Vec<Project> {
    // First-seen order, as Python's dict, so stable sorts tie-break the same.
    let mut grouped: Vec<(String, Vec<(AgentState, &SessionState)>)> = Vec::new();
    for session in sessions {
        let path = normalise_project(&session.cwd);
        if path.is_empty() {
            continue;
        }
        let entry = (effective_state(session, now, decay), session);
        match grouped.iter_mut().find(|(p, _)| *p == path) {
            Some((_, list)) => list.push(entry),
            None => grouped.push((path, vec![entry])),
        }
    }
    let mut projects: Vec<Project> = grouped
        .into_iter()
        .map(|(path, mut entries)| {
            entries.sort_by(|a, b| {
                (b.0, b.1.updated_at)
                    .partial_cmp(&(a.0, a.1.updated_at))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Project {
                state: entries[0].0,
                last_active: entries
                    .iter()
                    .map(|(_, s)| s.updated_at)
                    .fold(f64::NEG_INFINITY, f64::max),
                sessions: entries.into_iter().map(|(_, s)| s.clone()).collect(),
                label: String::new(),
                path,
            }
        })
        .collect();
    projects.sort_by(|a, b| b.last_active.total_cmp(&a.last_active));

    // Add parent directories until labels differ (`api/web` vs `site/web`).
    let mut labels: Vec<String> = projects.iter().map(|p| tail(&p.path, 1)).collect();
    for depth in 2..7 {
        let clashing: Vec<String> = labels
            .iter()
            .filter(|l| labels.iter().filter(|m| m == l).count() > 1)
            .cloned()
            .collect();
        if clashing.is_empty() {
            break;
        }
        for (label, project) in labels.iter_mut().zip(&projects) {
            if clashing.contains(label) {
                *label = tail(&project.path, depth);
            }
        }
    }
    for (project, label) in projects.iter_mut().zip(labels) {
        project.label = if label.is_empty() {
            project_label(&project.path)
        } else {
            label
        };
    }
    projects
}

/// What one Agent Key stands for right now.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSlot {
    pub index: usize,
    /// Normalised project path; non-empty with `project == None` = reserved.
    pub path: String,
    pub project: Option<Project>,
    pub pinned: bool,
    /// False for a key left to Codex: never allocated, omitted from thstatus.
    pub owned: bool,
}

impl AgentSlot {
    pub fn empty(&self) -> bool {
        self.project.is_none()
    }
    pub fn reserved(&self) -> bool {
        self.project.is_none() && !self.path.is_empty()
    }
    pub fn state(&self) -> Option<AgentState> {
        self.project.as_ref().map(|p| p.state)
    }
    pub fn session(&self) -> Option<&SessionState> {
        self.project.as_ref().and_then(|p| p.lead())
    }
    /// `"AG00"`..`"AG05"`.
    pub fn key_id(&self) -> String {
        format!("AG{:02}", self.index)
    }
    pub fn label(&self) -> String {
        match &self.project {
            Some(project) => project.label.clone(),
            None => project_label(&self.path),
        }
    }
}

/// Assign the six keys. Pure. `previous` = last paths per slot (sticky);
/// `owned` overrides `config.keys` for this call (coexistence).
pub fn resolve_slots(
    config: &AgentKeysConfig,
    sessions: &[SessionState],
    previous: &[String],
    now: f64,
    decay: &DecayPolicy,
    owned: Option<&[usize]>,
) -> Vec<AgentSlot> {
    let projects = group_projects(sessions, now, decay);
    let owned = owned.unwrap_or(&config.keys);
    let mut assigned: Vec<String> = vec![String::new(); SLOT_COUNT];
    let mut pinned = [false; SLOT_COUNT];

    // 1. Pins are absolute: never lent out, dark when nothing runs there.
    for (index, pin) in config.pins().into_iter().enumerate() {
        if !pin.is_empty() && owned.contains(&index) {
            assigned[index] = pin;
            pinned[index] = true;
        }
    }

    if config.policy == Policy::Status {
        // ponytail: a state change between the LED frame and a press can move
        // the pressed key's project; sticky slots if that bites.
        let mut ranked: Vec<&Project> = projects
            .iter()
            .filter(|p| !assigned.contains(&p.path))
            .collect();
        ranked.sort_by(|a, b| {
            (b.state, b.last_active)
                .partial_cmp(&(a.state, a.last_active))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut ranked = ranked.into_iter();
        for (index, slot) in assigned.iter_mut().enumerate() {
            if owned.contains(&index) && slot.is_empty() {
                match ranked.next() {
                    Some(project) => *slot = project.path.clone(),
                    None => break,
                }
            }
        }
    } else if config.policy != Policy::Manual {
        let mut taken: Vec<String> = assigned.iter().filter(|p| !p.is_empty()).cloned().collect();
        // 2. Incumbents keep their key while their project is live.
        for (index, previous_path) in previous.iter().take(SLOT_COUNT).enumerate() {
            if !owned.contains(&index) || !assigned[index].is_empty() {
                continue;
            }
            let path = normalise_project(previous_path);
            if !path.is_empty() && projects.iter().any(|p| p.path == path) && !taken.contains(&path)
            {
                taken.push(path.clone());
                assigned[index] = path;
            }
        }
        // 3. Newcomers fill the lowest free owned key, newest first.
        let mut waiting = projects
            .iter()
            .map(|p| &p.path)
            .filter(|p| !taken.contains(p));
        for (index, slot) in assigned.iter_mut().enumerate() {
            if owned.contains(&index) && slot.is_empty() {
                match waiting.next() {
                    Some(path) => *slot = path.clone(),
                    None => break,
                }
            }
        }
    }

    assigned
        .into_iter()
        .enumerate()
        .map(|(index, path)| AgentSlot {
            index,
            project: projects.iter().find(|p| p.path == path).cloned(),
            path,
            pinned: pinned[index],
            owned: owned.contains(&index),
        })
        .collect()
}

/// [`resolve_slots`] that remembers the previous assignment.
#[derive(Debug, Clone)]
pub struct SlotResolver {
    pub config: AgentKeysConfig,
    pub decay: DecayPolicy,
    previous: Vec<String>,
}

impl SlotResolver {
    pub fn new(config: AgentKeysConfig, decay: DecayPolicy) -> SlotResolver {
        SlotResolver {
            config,
            decay,
            previous: vec![String::new(); SLOT_COUNT],
        }
    }
    pub fn previous(&self) -> &[String] {
        &self.previous
    }
    pub fn resolve(
        &mut self,
        sessions: &[SessionState],
        now: f64,
        owned: Option<&[usize]>,
    ) -> Vec<AgentSlot> {
        let slots = resolve_slots(
            &self.config,
            sessions,
            &self.previous,
            now,
            &self.decay,
            owned,
        );
        // An emptied key keeps its memory until another project takes it;
        // an un-owned key remembers nothing.
        self.previous = slots
            .iter()
            .map(|slot| match (slot.owned, slot.path.is_empty()) {
                (false, _) => String::new(),
                (true, false) => slot.path.clone(),
                (true, true) => self.previous.get(slot.index).cloned().unwrap_or_default(),
            })
            .collect();
        slots
    }
    /// Seed from the shared `slots.json` cache.
    pub fn seed(&mut self, previous: &[String]) {
        let mut entries: Vec<String> = previous
            .iter()
            .take(SLOT_COUNT)
            .map(|p| normalise_project(p))
            .collect();
        entries.resize(SLOT_COUNT, String::new());
        self.previous = entries;
    }
    pub fn forget(&mut self) {
        self.previous = vec![String::new(); SLOT_COUNT];
    }
}

pub fn slot_for_project<'a>(slots: &'a [AgentSlot], path: &str) -> Option<&'a AgentSlot> {
    let wanted = normalise_project(path);
    slots.iter().find(|slot| slot.path == wanted)
}

/// `pgrep -x ChatGPT` (Python `permissions.chatgpt_running`): a timeout
/// after 5 s, like a failure, means "not running".
pub fn default_codex_probe() -> bool {
    let mut pgrep = std::process::Command::new("pgrep");
    pgrep.args(["-x", "ChatGPT"]);
    crate::input::quartz::run_bounded(pgrep, std::time::Duration::from_secs(5)).is_ok()
}

/// Which Agent Keys FreeMicro drives right now. A configured subset holds
/// only while the ChatGPT app runs; otherwise FreeMicro owns all six. One
/// instance lives in the run loop and is passed by `&` to the bridge (press
/// gating) and the LED renderer (lighting), so they never disagree.
pub struct AgentKeyOwnership {
    pub config: AgentKeysConfig,
    probe: Box<dyn FnMut() -> bool>,
    pub poll_seconds: f64,
    codex_here: bool,
    checked_at: Option<f64>,
}

impl AgentKeyOwnership {
    pub fn new(config: AgentKeysConfig, probe: Box<dyn FnMut() -> bool>) -> AgentKeyOwnership {
        AgentKeyOwnership {
            config,
            probe,
            poll_seconds: DEFAULT_CODEX_POLL_SECONDS,
            codex_here: true,
            checked_at: None,
        }
    }
    pub fn codex_here(&self) -> bool {
        self.codex_here
    }
    /// A subset is configured and Codex is here to share with.
    pub fn coexisting(&self) -> bool {
        self.config.subset() && self.codex_here
    }
    /// The effective owned key indices.
    pub fn keys(&self) -> Vec<usize> {
        if self.coexisting() {
            self.config.keys.clone()
        } else {
            (0..SLOT_COUNT).collect()
        }
    }
    pub fn owns(&self, index: usize) -> bool {
        !self.coexisting() || self.config.owns(index)
    }
    pub fn leaves_to_codex(&self, input_id: &str) -> bool {
        self.coexisting() && self.config.leaves_to_codex(input_id)
    }
    /// Re-probe at most every `poll_seconds` (monotonic `now`). True when the
    /// effective owned set changed (caller invalidates the LED frame).
    pub fn refresh(&mut self, now: f64) -> bool {
        if self.config.owns_all() {
            return false;
        }
        if self
            .checked_at
            .is_some_and(|at| now - at < self.poll_seconds)
        {
            return false;
        }
        self.checked_at = Some(now);
        let was = self.codex_here;
        self.codex_here = (self.probe)();
        self.codex_here != was
    }
    /// Adopt a reloaded config; the next `refresh` probes at once.
    pub fn set_config(&mut self, config: AgentKeysConfig) {
        self.config = config;
        self.codex_here = true;
        self.checked_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;

    const NOW: f64 = 1_784_000_000.0;

    fn session(id: &str, cwd: &str, state: AgentState, at: f64) -> SessionState {
        let mut s = SessionState::new(id, state, at);
        s.cwd = cwd.into();
        s
    }

    fn paths(slots: &[AgentSlot]) -> Vec<&str> {
        slots.iter().map(|s| s.path.as_str()).collect()
    }

    fn config(raw: Value) -> AgentKeysConfig {
        parse_agent_keys(&raw).unwrap()
    }

    #[test]
    fn parse_accepts_good_sections() {
        assert_eq!(config(Value::Null), AgentKeysConfig::default());
        let c = config(
            json!({"policy": " Pinned ", "slots": ["/a/b/", null], "keys": [5, 3], "comment": "x"}),
        );
        assert_eq!(c.policy, Policy::Pinned);
        assert_eq!(c.slots[0], "/a/b");
        assert_eq!(c.slots[1], "");
        assert_eq!(c.keys, [3, 5]);
        assert!(c.subset());
        for all in [json!([]), json!([0, 1, 2, 3, 4, 5])] {
            assert!(config(json!({ "keys": all })).owns_all());
        }
        // Pins are kept but ignored by recent/mirror.
        let recent = config(json!({"slots": ["/a"]}));
        assert_eq!(recent.slots[0], "/a");
        assert_eq!(recent.pins()[0], "");
    }

    #[test]
    fn parse_rejects_bad_sections_with_a_useful_message() {
        for (raw, message) in [
            (json!({"policy": "nonsense"}), "policy must be one of"),
            (json!({"slots": "not-a-list"}), "must be a list"),
            (json!({"slots": [1, 2]}), "must be a project path"),
            (json!({"slots": vec![""; 7]}), "the pad has 6 Agent Keys"),
            (json!({"nope": 1}), "unknown field"),
            (json!("not an object"), "must be an object"),
            (json!({"keys": [6]}), "indices are 0-5"),
            (json!({"keys": [-1]}), "indices are 0-5"),
            (json!({"keys": [3, 3]}), "twice"),
            (json!({"keys": ["3"]}), "whole numbers"),
            (json!({"keys": [true]}), "whole numbers"),
            (json!({"keys": [1.5]}), "whole numbers"),
            (json!({"keys": "345"}), "must be a list"),
        ] {
            let err = parse_agent_keys(&raw).unwrap_err();
            assert!(err.contains(message), "{raw}: {err}");
        }
    }

    #[test]
    fn normalise_project_is_pure_text() {
        let home = std::env::var("HOME").unwrap();
        for (raw, want) in [
            ("", String::new()),
            ("  ", String::new()),
            ("/a/b/", "/a/b".into()),
            ("/a/./b//c/../d", "/a/b/d".into()),
            ("/..", "/".into()),
            ("rel/../..", "..".into()),
            ("~/code", format!("{}/code", home.trim_end_matches('/'))),
            ("~user/x", "~user/x".into()),
        ] {
            assert_eq!(normalise_project(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn agent_key_index_and_leaves_to_codex() {
        for (id, want) in [
            ("AG00", Some(0)),
            ("AG05", Some(5)),
            ("AG06", None),
            ("AG3", None),
            ("ACT06", None),
            ("AG00+AG03", None),
        ] {
            assert_eq!(agent_key_index(id), want, "{id}");
        }
        let c = config(json!({"keys": [3, 4, 5]}));
        assert!(c.leaves_to_codex("AG00"));
        assert!(!c.leaves_to_codex("AG03"));
        assert!(!c.leaves_to_codex("ACT06"));
        assert!(!AgentKeysConfig::default().leaves_to_codex("AG00"));
    }

    #[test]
    fn group_projects_collapses_orders_decays_and_disambiguates() {
        let d = DecayPolicy::default();
        let sessions = [
            session("1", "/w/api", AgentState::Working, NOW - 5.0),
            session("2", "/w/api/", AgentState::Waiting, NOW - 50.0),
            session("3", "/site/web", AgentState::Done, NOW - 1.0),
            session("4", "/apps/web", AgentState::Done, NOW - 500.0),
            session("5", "", AgentState::Waiting, NOW),
        ];
        let projects = group_projects(&sessions, NOW, &d);
        let summary: Vec<_> = projects
            .iter()
            .map(|p| (p.path.as_str(), p.state, p.label.as_str(), p.sessions.len()))
            .collect();
        assert_eq!(
            summary,
            [
                ("/site/web", AgentState::Done, "site/web", 1),
                ("/w/api", AgentState::Waiting, "api", 2),
                // Done decays to idle per project.
                ("/apps/web", AgentState::Idle, "apps/web", 1),
            ]
        );
        assert_eq!(projects[1].lead().unwrap().session_id, "2");
        assert_eq!(projects[1].last_active, NOW - 5.0);
    }

    #[test]
    fn recent_policy_is_sticky_and_waits_rather_than_evicts() {
        let mut r = SlotResolver::new(AgentKeysConfig::default(), DecayPolicy::default());
        let mut live: Vec<SessionState> = ["a", "b"]
            .iter()
            .enumerate()
            .map(|(i, p)| session(p, &format!("/{p}"), AgentState::Working, NOW + i as f64))
            .collect();
        assert_eq!(paths(&r.resolve(&live, NOW + 2.0, None))[..2], ["/b", "/a"]);
        // Order changes, incumbents stay put; a newcomer takes the lowest free key.
        live[0].updated_at = NOW + 3.0;
        live.push(session("c", "/c", AgentState::Working, NOW + 4.0));
        assert_eq!(
            paths(&r.resolve(&live, NOW + 5.0, None))[..3],
            ["/b", "/a", "/c"]
        );
        // A key that empties remembers its project and gets it back.
        let without_a: Vec<_> = live.iter().filter(|s| s.cwd != "/a").cloned().collect();
        let slots = r.resolve(&without_a, NOW + 5.0, None);
        assert!(slots[1].empty() && !slots[1].reserved());
        assert_eq!(r.previous()[1], "/a");
        assert_eq!(paths(&r.resolve(&live, NOW + 5.0, None))[1], "/a");
        // Seven projects: the seventh waits.
        let seven: Vec<_> = (0..7)
            .map(|i| {
                session(
                    &i.to_string(),
                    &format!("/p{i}"),
                    AgentState::Working,
                    NOW + i as f64,
                )
            })
            .collect();
        let slots = resolve_slots(
            &AgentKeysConfig::default(),
            &seven,
            &[],
            NOW + 7.0,
            &DecayPolicy::default(),
            None,
        );
        assert_eq!(paths(&slots), ["/p6", "/p5", "/p4", "/p3", "/p2", "/p1"]);
        r.forget();
        assert_eq!(r.previous(), vec![String::new(); SLOT_COUNT]);
        r.seed(&["/x/".into()]);
        assert_eq!(r.previous()[0], "/x");
        assert_eq!(r.previous().len(), SLOT_COUNT);
    }

    #[test]
    fn pinned_and_manual_policies_honour_pins() {
        let d = DecayPolicy::default();
        let live = [
            session("1", "/new", AgentState::Working, NOW),
            session("2", "/api", AgentState::Waiting, NOW - 1.0),
        ];
        let pinned = config(json!({"policy": "pinned", "slots": [null, "/api", "/docs"]}));
        let slots = resolve_slots(&pinned, &live, &[], NOW, &d, None);
        assert_eq!(paths(&slots)[..3], ["/new", "/api", "/docs"]);
        assert!(slots[2].reserved() && slots[2].pinned);
        assert_eq!(slots[2].label(), "docs");
        assert_eq!(slot_for_project(&slots, "/api/").unwrap().index, 1);

        let manual = config(json!({"policy": "manual", "slots": [null, "/api"]}));
        let slots = resolve_slots(&manual, &live, &[], NOW, &d, None);
        assert_eq!(paths(&slots)[..2], ["", "/api"]);
        assert_eq!(slots[1].state(), Some(AgentState::Waiting));

        // status: urgent first, then newest; a stale assignment is ignored.
        let status = config(json!({"policy": "status"}));
        let mut many: Vec<_> = (0..6)
            .map(|i| {
                session(
                    &i.to_string(),
                    &format!("/idle{i}"),
                    AgentState::Idle,
                    NOW + i as f64,
                )
            })
            .collect();
        many.push(session("w", "/wait", AgentState::Waiting, NOW - 9.0));
        many.push(session("k", "/work", AgentState::Working, NOW - 8.0));
        let slots = resolve_slots(&status, &many, &["/idle0".into()], NOW + 9.0, &d, None);
        assert_eq!(
            paths(&slots),
            ["/wait", "/work", "/idle5", "/idle4", "/idle3", "/idle2"]
        );
    }

    #[test]
    fn projects_land_only_on_owned_keys() {
        let d = DecayPolicy::default();
        let c = config(json!({"policy": "pinned", "slots": ["/pin"], "keys": [3, 4, 5]}));
        let live = [
            session("1", "/a", AgentState::Working, NOW),
            session("2", "/pin", AgentState::Working, NOW - 1.0),
        ];
        let slots = resolve_slots(&c, &live, &["/a".into()], NOW, &d, None);
        assert_eq!(paths(&slots), ["", "", "", "/a", "/pin", ""]);
        assert!(!slots[0].owned && slots[3].owned && !slots[0].pinned);
        // The effective owned set overrides the config.
        let all: Vec<usize> = (0..SLOT_COUNT).collect();
        let slots = resolve_slots(&c, &live, &[], NOW, &d, Some(&all));
        assert_eq!(paths(&slots)[..2], ["/pin", "/a"]);
        // An un-owned key remembers nothing.
        let mut r = SlotResolver::new(c, d);
        r.seed(&["/a".into()]);
        r.resolve(&live, NOW, None);
        assert_eq!(r.previous()[0], "");
    }

    #[test]
    fn ownership_follows_codex_and_rate_limits_the_probe() {
        let here = Rc::new(Cell::new(true));
        let calls = Rc::new(Cell::new(0));
        let probe = {
            let (here, calls) = (here.clone(), calls.clone());
            Box::new(move || {
                calls.set(calls.get() + 1);
                here.get()
            })
        };
        let subset = config(json!({"keys": [3, 4, 5]}));
        let mut own = AgentKeyOwnership::new(subset.clone(), probe);
        // Before the first probe the subset is assumed active.
        assert!(own.coexisting());
        assert_eq!(own.keys(), [3, 4, 5]);
        assert!(own.leaves_to_codex("AG00"));
        assert!(!own.refresh(0.0));
        assert_eq!(calls.get(), 1);
        here.set(false);
        assert!(!own.refresh(1.0), "inside the poll window: no probe");
        assert_eq!(calls.get(), 1);
        assert!(own.refresh(3.0));
        assert_eq!(own.keys(), (0..SLOT_COUNT).collect::<Vec<_>>());
        assert!(own.owns(0) && !own.leaves_to_codex("AG00"));
        assert!(!own.refresh(6.0));
        // A reload re-probes on the next refresh.
        here.set(true);
        own.set_config(subset);
        assert!(!own.refresh(6.5));
        assert_eq!(calls.get(), 4);
        // All six owned: the probe is never called.
        own.set_config(AgentKeysConfig::default());
        for t in [10.0, 20.0, 30.0] {
            assert!(!own.refresh(t));
        }
        assert_eq!(calls.get(), 4);
        assert!(!own.coexisting() && own.owns(0));
    }
}
