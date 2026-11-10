//! Per-session state files, decay and resolution. Port of
//! `state/engine.py` (`StateStore`, `claim_ttl`, `effective_state`,
//! `as_read`, `ProcessLiveness`).
//!
//! File format is shared with Python: `<dir>/<safe id>.json`, written by
//! tempfile + rename, holding [`SessionState::to_json`].

use std::io;
use std::path::{Path, PathBuf};

use super::process::ProcessLiveness;
use super::{AgentState, DecayPolicy, SessionSignals, SessionState, TerminalInfo};

/// What a hook event says about a session besides its state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub title: String,
    pub cwd: String,
    pub terminal: TerminalInfo,
    pub signals: SessionSignals,
}

/// Reads and writes per-session state files and resolves the winner.
pub struct StateStore {
    pub dir: PathBuf,
    pub decay: DecayPolicy,
    pub liveness: ProcessLiveness,
}

impl StateStore {
    /// Creates `dir` if missing. `liveness` decides whether a quiet record's
    /// process still runs (inject a fake probe in tests).
    pub fn new(dir: PathBuf, decay: DecayPolicy, liveness: ProcessLiveness) -> StateStore {
        let _ = std::fs::create_dir_all(&dir);
        StateStore {
            dir,
            decay,
            liveness,
        }
    }

    /// The default store for `home`: `<home>/state`, decay from
    /// `<home>/config.json`, the system process probe.
    pub fn for_home(home: &Path) -> StateStore {
        StateStore::new(
            crate::config::state_dir(home),
            crate::config::Config::load(home).decay,
            ProcessLiveness::system(),
        )
    }

    /// `<dir>/<id with non-alphanumeric (except `-`, `_`) replaced by `_`,
    /// or "default">.json`. Alphanumeric is Unicode-aware, as Python `isalnum`.
    pub fn path_for(&self, session_id: &str) -> PathBuf {
        let safe: String = session_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let name = if safe.is_empty() { "default" } else { &safe };
        self.dir.join(format!("{name}.json"))
    }

    /// Record `state` for `session_id` atomically and return the snapshot.
    /// Turn tracking (`prompt_id`, `turn_open`, `interrupted`) follows
    /// Python `_turn_after`.
    pub fn update(
        &self,
        session_id: &str,
        state: AgentState,
        meta: &SessionMeta,
        now: f64,
    ) -> io::Result<SessionState> {
        let signals = &meta.signals;
        let (prompt_id, turn_open, interrupted) = self.turn_after(session_id, signals);
        let mut record = SessionState::new(session_id, state, now);
        record.title = meta.title.clone();
        record.cwd = meta.cwd.clone();
        record.tty = meta.terminal.tty.clone();
        record.pid = meta.terminal.pid;
        record.term_program = meta.terminal.program.clone();
        record.term_session = meta.terminal.session.clone();
        record.last_event = signals.event.clone();
        record.prompt_id = prompt_id;
        record.turn_open = turn_open;
        record.interrupted = interrupted;
        record.permission_mode = signals.permission_mode.clone();
        record.effort = signals.effort.clone();
        record.background_tasks = signals.background_tasks;

        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(session_id);
        // Same dir, `.tmp` suffix (never matched by the `*.json` scan), pid in
        // the name so two hooks for one session never share a temp file.
        let tmp = self.dir.join(format!(
            ".{}.{}.tmp",
            path.file_stem().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let body = serde_json::to_vec(&record.to_json()).map_err(io::Error::other)?;
        std::fs::write(&tmp, body)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })?;
        Ok(record)
    }

    /// `(prompt_id, turn_open, interrupted)` once `signals` is applied. A new
    /// `prompt_id` over a still-open turn can only mean an interrupt.
    fn turn_after(&self, session_id: &str, signals: &SessionSignals) -> (String, bool, bool) {
        let previous = self.read(session_id);
        let prompt_id = if signals.prompt_id.is_empty() {
            previous
                .as_ref()
                .map(|p| p.prompt_id.clone())
                .unwrap_or_default()
        } else {
            signals.prompt_id.clone()
        };
        if signals.closes_turn() {
            return (prompt_id, false, false);
        }
        let Some(previous) = previous else {
            let open = !prompt_id.is_empty();
            return (prompt_id, open, false);
        };
        let same_turn = !prompt_id.is_empty() && prompt_id == previous.prompt_id;
        let abandoned = previous.turn_open
            && !signals.prompt_id.is_empty()
            && !previous.prompt_id.is_empty()
            && signals.prompt_id != previous.prompt_id;
        let open = !prompt_id.is_empty() || previous.turn_open;
        (
            prompt_id,
            open,
            abandoned || (same_turn && previous.interrupted),
        )
    }

    /// Remove a session's record. Idempotent.
    pub fn clear(&self, session_id: &str) {
        let _ = std::fs::remove_file(self.path_for(session_id));
    }

    /// The raw record on disk, or `None`. Never fails.
    pub fn read(&self, session_id: &str) -> Option<SessionState> {
        read_file(&self.path_for(session_id))
    }

    /// All live sessions, freshest first, with expired claims retired
    /// (`decay`) and dead quiet records deleted (unless `include_stale`).
    pub fn sessions(&mut self, now: f64, include_stale: bool, decay: bool) -> Vec<SessionState> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut live = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            // A partial or corrupt file is skipped; the next update rewrites it.
            let Some(mut record) = read_file(&path) else {
                continue;
            };
            let quiet = record.age(now) > self.decay.ttl_seconds;
            // The `ps` half is only paid once a pid has had time to be reused.
            let alive = self.liveness.verdict(&record, quiet, now);
            if !include_stale && quiet && alive != Some(true) {
                // Nothing vouches for this one: opportunistic cleanup.
                let _ = std::fs::remove_file(&path);
                continue;
            }
            record.process_alive = alive;
            record.kept_by_process = quiet && alive == Some(true);
            live.push(if decay {
                as_read(&record, now, &self.decay)
            } else {
                record
            });
        }
        live.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
        live
    }

    /// One directory scan: the winning state (priority, then recency) and
    /// the session list it was drawn from.
    pub fn resolved_and_sessions(&mut self, now: f64) -> (AgentState, Vec<SessionState>) {
        let sessions = self.sessions(now, false, true);
        let winner = sessions
            .iter()
            .max_by(|a, b| {
                a.state
                    .cmp(&b.state)
                    .then(a.updated_at.total_cmp(&b.updated_at))
            })
            .map_or(AgentState::Idle, |s| s.state);
        (winner, sessions)
    }
}

fn read_file(path: &Path) -> Option<SessionState> {
    let bytes = std::fs::read(path).ok()?;
    SessionState::from_json(&serde_json::from_slice(&bytes).ok()?)
}

/// How long `session`'s claim is believed, in seconds. `0` = forever.
pub fn claim_ttl(session: &SessionState, decay: &DecayPolicy) -> f64 {
    if session.busy_elsewhere() {
        return 0.0;
    }
    match session.state {
        AgentState::Done => decay.done_ttl_seconds,
        AgentState::Working if session.tool_running() => decay.tool_ttl_seconds,
        AgentState::Working => decay.working_ttl_seconds,
        _ => 0.0,
    }
}

/// The state `session` should be read as at `now`.
pub fn effective_state(session: &SessionState, now: f64, decay: &DecayPolicy) -> AgentState {
    let ttl = claim_ttl(session, decay);
    if ttl > 0.0 && now - session.updated_at > ttl {
        AgentState::Idle
    } else {
        session.state
    }
}

/// `session` with expiry applied: `state` possibly retired to idle and
/// `claimed_state` set to what it claimed.
pub fn as_read(session: &SessionState, now: f64, decay: &DecayPolicy) -> SessionState {
    let mut out = session.clone();
    let state = effective_state(session, now, decay);
    if state != session.state {
        out.state = state;
        out.claimed_state = Some(session.state);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::process::tests::{Table, EPOCH};
    use crate::testutil::TempDir;

    fn store_with(home: &TempDir, pids: Vec<(i32, Option<f64>)>) -> StateStore {
        StateStore::new(
            home.path().join("state"),
            DecayPolicy::default(),
            ProcessLiveness::new(Box::new(Table {
                pids,
                ..Table::default()
            })),
        )
    }

    fn meta(cwd: &str, event: &str, prompt: &str, pid: i32) -> SessionMeta {
        SessionMeta {
            cwd: cwd.into(),
            terminal: TerminalInfo {
                pid,
                ..TerminalInfo::default()
            },
            signals: SessionSignals {
                event: event.into(),
                prompt_id: prompt.into(),
                ..SessionSignals::default()
            },
            ..SessionMeta::default()
        }
    }

    fn plain() -> SessionMeta {
        SessionMeta::default()
    }

    #[test]
    fn update_writes_python_keys_in_order_and_round_trips() {
        let home = TempDir::new();
        let mut store = store_with(&home, vec![]);
        let rec = store
            .update(
                "s1",
                AgentState::Working,
                &meta("/p", "PreToolUse", "t1", 7),
                EPOCH,
            )
            .unwrap();
        let text = std::fs::read_to_string(home.path().join("state/s1.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(
            keys,
            [
                "session_id",
                "state",
                "updated_at",
                "title",
                "cwd",
                "tty",
                "pid",
                "term_program",
                "term_session",
                "last_event",
                "prompt_id",
                "turn_open",
                "interrupted",
                "permission_mode",
                "effort",
                "background_tasks"
            ]
        );
        assert_eq!(store.read("s1"), Some(rec.clone()));
        // A liveness reading is never persisted, only shown.
        let listed = store.sessions(EPOCH, false, true);
        assert_eq!(listed[0].process_alive, Some(false));
        let mut shown = rec;
        shown.process_alive = Some(true);
        shown.kept_by_process = true;
        let shown = shown.to_json();
        assert_eq!(shown["process_alive"], true);
        assert_eq!(shown["kept_by_process"], true);
        assert!(!text.contains("process_alive"));
    }

    #[test]
    fn from_json_is_lenient_like_python() {
        let v = serde_json::json!({"session_id": "a", "state": "done", "updated_at": 5,
            "pid": "bad", "turn_open": 1, "background_tasks": 2.9});
        let s = SessionState::from_json(&v).unwrap();
        assert_eq!((s.pid, s.turn_open, s.background_tasks), (0, true, 2));
        for bad in [
            serde_json::json!({"state": "done", "updated_at": 5}),
            serde_json::json!({"session_id": "a", "state": "nope", "updated_at": 5}),
            serde_json::json!({"session_id": "a", "state": "done", "updated_at": "x"}),
            serde_json::json!([1]),
        ] {
            assert_eq!(SessionState::from_json(&bad), None, "{bad}");
        }
    }

    #[test]
    fn resolution_priority_recency_and_empty() {
        let home = TempDir::new();
        let mut store = store_with(&home, vec![]);
        assert_eq!(store.resolved_and_sessions(EPOCH).0, AgentState::Idle);
        for (i, (id, state)) in [
            ("a", AgentState::Idle),
            ("b", AgentState::Working),
            ("c", AgentState::Waiting),
            ("d", AgentState::Error),
        ]
        .into_iter()
        .enumerate()
        {
            store.update(id, state, &plain(), EPOCH + i as f64).unwrap();
        }
        let (winner, sessions) = store.resolved_and_sessions(EPOCH + 5.0);
        assert_eq!(winner, AgentState::Waiting);
        let ids: Vec<_> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["d", "c", "b", "a"]);
        store.clear("c");
        store.clear("c");
        assert_eq!(
            store.resolved_and_sessions(EPOCH + 5.0).0,
            AgentState::Error
        );
        // Equal priority: the newer one wins.
        store
            .update("e", AgentState::Error, &plain(), EPOCH + 9.0)
            .unwrap();
        assert_eq!(
            store.resolved_and_sessions(EPOCH + 10.0).1[0].session_id,
            "e"
        );
    }

    #[test]
    fn path_for_sanitises_and_corrupt_files_are_skipped() {
        let home = TempDir::new();
        let mut store = store_with(&home, vec![]);
        assert_eq!(store.path_for("../x y"), store.dir.join("___x_y.json"));
        assert_eq!(store.path_for(""), store.dir.join("default.json"));
        std::fs::write(store.dir.join("junk.json"), "{nope").unwrap();
        store
            .update("ok", AgentState::Done, &plain(), EPOCH)
            .unwrap();
        assert_eq!(store.sessions(EPOCH, false, true).len(), 1);
    }

    #[test]
    fn liveness_decides_what_survives_the_ttl() {
        let home = TempDir::new();
        // 1: alive and original. 2: dead. 3: recycled. 4: alive, start unknown.
        let mut store = store_with(
            &home,
            vec![
                (1001, Some(EPOCH - 10.0)),
                (1003, Some(EPOCH + 100.0)),
                (1004, None),
            ],
        );
        for (id, pid) in [
            ("alive", 1001),
            ("dead", 1002),
            ("recycled", 1003),
            ("unknown", 1004),
            ("nopid", 0),
        ] {
            store
                .update(id, AgentState::Waiting, &meta("/p", "", "", pid), EPOCH)
                .unwrap();
        }
        // Inside the TTL a dead pid removes nothing.
        assert_eq!(store.sessions(EPOCH + 60.0, false, true).len(), 5);
        let late = EPOCH + 1801.0;
        let stale = store.sessions(late, true, true);
        assert_eq!(stale.len(), 5);
        let kept = store.sessions(late, false, true);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session_id, "alive");
        assert!(kept[0].kept_by_process);
        assert_eq!(kept[0].describe_claim(), "waiting - terminal still open");
        assert_eq!(std::fs::read_dir(&store.dir).unwrap().count(), 1);
    }

    #[test]
    fn claims_decay_by_state_and_signals() {
        let d = DecayPolicy::default();
        let at = |state, event: &str, bg, age: f64| {
            let mut s = SessionState::new("s", state, EPOCH);
            s.last_event = event.into();
            s.background_tasks = bg;
            effective_state(&s, EPOCH + age, &d)
        };
        use AgentState::*;
        for (state, event, bg, age, want) in [
            (Done, "", 0, 179.0, Done),
            (Done, "", 0, 181.0, Idle),
            (Working, "PostToolUse", 0, 121.0, Idle),
            (Working, "PreToolUse", 0, 121.0, Working),
            (Working, "PreToolUse", 0, 601.0, Idle),
            (Working, "", 2, 5000.0, Working),
            (Waiting, "", 0, 5000.0, Waiting),
            (Error, "", 0, 5000.0, Error),
        ] {
            assert_eq!(
                at(state, event, bg, age),
                want,
                "{state:?} {event} {bg} {age}"
            );
        }
        let off = DecayPolicy {
            done_ttl_seconds: 0.0,
            working_ttl_seconds: 0.0,
            ..d
        };
        let s = SessionState::new("s", Working, EPOCH);
        assert_eq!(effective_state(&s, EPOCH + 1e6, &off), Working);
        let read = as_read(&s, EPOCH + 121.0, &d);
        assert_eq!((read.state, read.claimed_state), (Idle, Some(Working)));
        assert_eq!(read.describe_claim(), "idle (was working, and went quiet)");
        let mut bg = SessionState::new("s", Working, EPOCH);
        bg.background_tasks = 1;
        assert_eq!(bg.describe_claim(), "working (1 background task)");
    }

    #[test]
    fn turn_tracking_detects_interrupts() {
        let home = TempDir::new();
        let store = store_with(&home, vec![]);
        let step = |event: &str, prompt: &str| {
            let r = store
                .update("s", AgentState::Working, &meta("", event, prompt, 0), EPOCH)
                .unwrap();
            (r.prompt_id, r.turn_open, r.interrupted)
        };
        assert_eq!(step("UserPromptSubmit", "t1"), ("t1".into(), true, false));
        // A new prompt over the open turn: the old one was abandoned.
        assert_eq!(step("UserPromptSubmit", "t2"), ("t2".into(), true, true));
        // Remembered through the same turn, even without a prompt id.
        assert_eq!(step("PreToolUse", ""), ("t2".into(), true, true));
        assert_eq!(step("Stop", ""), ("t2".into(), false, false));
        // After a clean stop a new prompt is no interrupt.
        assert_eq!(step("UserPromptSubmit", "t3"), ("t3".into(), true, false));
        let mut gone = store.read("s").unwrap();
        gone.interrupted = true;
        let read = as_read(&gone, EPOCH + 200.0, &DecayPolicy::default());
        assert_eq!(
            read.describe_claim(),
            "idle (was working; last turn was interrupted)"
        );
    }
}
