//! Raise the terminal of an Agent Key's session: Terminal/iTerm2 tab by tty,
//! a herdr pane, or just the app; open a new terminal for an empty slot.
//! Port of `freemicro/focus.py`.
//!
//! Owner: herdr+focus. Depends on `state`, `herdr` and `input::actions::Backend`
//! (scripts and app activation go through the backend so dry runs record
//! them).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::herdr::Herdr;
use crate::input::actions::Backend;
use crate::input::keys::escape_applescript;
use crate::state::agentkeys::{
    normalise_project, resolve_slots, AgentKeysConfig, AgentSlot, SLOT_COUNT,
};
use crate::state::{slots, SessionState, StateStore};

/// Apps scriptable by tty.
pub const SCRIPTABLE_APPS: [&str; 2] = ["Terminal", "iTerm2"];
pub const TTY_CACHE_SECONDS: f64 = 15.0;
/// Backstop cap: past it, expired entries are swept.
const TTY_CACHE_MAX: usize = 512;

/// `TERM_PROGRAM` (lower case) -> macOS app name.
const TERMINAL_APPS: [(&str, &str); 14] = [
    ("apple_terminal", "Terminal"),
    ("iterm.app", "iTerm2"),
    ("iterm2", "iTerm2"),
    ("vscode", "Code"),
    ("cursor", "Cursor"),
    ("ghostty", "Ghostty"),
    ("warpterminal", "Warp"),
    ("warp", "Warp"),
    ("wezterm", "WezTerm"),
    ("hyper", "Hyper"),
    ("kitty", "kitty"),
    ("alacritty", "Alacritty"),
    ("tabby", "Tabby"),
    ("rio", "Rio"),
];

/// Terminals with no "new window" verb that honour Cmd-N.
const CMDN_NEW_WINDOW_APPS: [&str; 10] = [
    "Ghostty",
    "Warp",
    "WezTerm",
    "Hyper",
    "kitty",
    "Alacritty",
    "Tabby",
    "Rio",
    "Code",
    "Cursor",
];

/// Derived ttys: `pid -> (expires_at, tty)`. Short-lived, because a reused
/// pid must never send a press to the wrong tab.
static TTY_CACHE: Mutex<Option<HashMap<i32, (f64, String)>>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusMethod {
    Tab,
    Herdr,
    App,
    #[default]
    None,
}

/// What pressing a key would do, built without side effects.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FocusPlan {
    pub method: FocusMethod,
    pub app: String,
    pub tty: String,
    pub script: String,
    pub project: String,
    pub label: String,
    pub session: Option<SessionState>,
    pub reason: String,
}

impl FocusPlan {
    pub fn actionable(&self) -> bool {
        self.method != FocusMethod::None
    }
    pub fn slot_empty(&self) -> bool {
        self.method == FocusMethod::None && self.session.is_none()
    }
    pub fn describe(&self) -> String {
        let place = if self.label.is_empty() {
            &self.project
        } else {
            &self.label
        };
        match self.method {
            FocusMethod::Tab => format!("focus {place} - {} tab {}", self.app, self.tty),
            FocusMethod::Herdr => format!("focus {place} - its herdr pane"),
            FocusMethod::App => format!("focus {place} - {} (tab unknown)", self.app),
            FocusMethod::None if !place.is_empty() => {
                format!("focus {place} - does nothing: {}", self.reason)
            }
            FocusMethod::None => format!("nothing on this key ({})", self.reason),
        }
    }
}

/// `TERM_PROGRAM` value -> app name (`"Apple_Terminal"` -> `"Terminal"`).
pub fn app_name_for(program: &str) -> String {
    let key = program.trim().to_lowercase();
    TERMINAL_APPS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, app)| app.to_string())
        .unwrap_or_default()
}

pub fn is_scriptable(app: &str) -> bool {
    SCRIPTABLE_APPS.contains(&app)
}

/// `/dev/<name>` with a safe charset; guards AppleScript interpolation.
pub fn valid_tty(tty: &str) -> bool {
    tty.strip_prefix("/dev/").is_some_and(|name| {
        (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
    })
}

/// The session's tty: recorded, else looked up from its pid.
pub fn session_tty(
    session: &SessionState,
    tty_lookup: &mut dyn FnMut(i32) -> String,
    now: f64,
) -> String {
    if valid_tty(&session.tty) {
        return session.tty.clone();
    }
    tty_from_pid(session.pid, tty_lookup, now)
}

/// The tty owned by `pid`, cached for [`TTY_CACHE_SECONDS`]; `""` if none.
fn tty_from_pid(pid: i32, lookup: &mut dyn FnMut(i32) -> String, now: f64) -> String {
    if pid <= 0 {
        return String::new();
    }
    let mut guard = TTY_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((expires, tty)) = cache.get(&pid) {
        if *expires > now {
            return tty.clone();
        }
    }
    let found = lookup(pid);
    let found = if valid_tty(&found) {
        found
    } else {
        String::new()
    };
    if cache.len() >= TTY_CACHE_MAX {
        cache.retain(|_, (expires, _)| *expires > now);
    }
    cache.insert(pid, (now + TTY_CACHE_SECONDS, found.clone()));
    found
}

pub fn plan_for_session(
    session: Option<&SessionState>,
    project: &str,
    label: &str,
    fallback: bool,
    tty_lookup: &mut dyn FnMut(i32) -> String,
    now: f64,
) -> FocusPlan {
    let Some(session) = session else {
        return FocusPlan {
            project: normalise_project(project),
            label: label.to_string(),
            reason: "no live session for this key".into(),
            ..FocusPlan::default()
        };
    };
    let project = normalise_project(if project.is_empty() {
        &session.cwd
    } else {
        project
    });
    let label = if label.is_empty() {
        project.clone()
    } else {
        label.to_string()
    };
    let base = FocusPlan {
        project,
        label,
        session: Some(session.clone()),
        ..FocusPlan::default()
    };
    if session.term_program.trim().eq_ignore_ascii_case("herdr") {
        return FocusPlan {
            method: FocusMethod::Herdr,
            app: "herdr".into(),
            ..base
        };
    }
    let app = app_name_for(&session.term_program);
    let tty = session_tty(session, tty_lookup, now);
    if !tty.is_empty() && is_scriptable(&app) {
        return FocusPlan {
            method: FocusMethod::Tab,
            script: tab_script(&app, &tty),
            app,
            tty,
            ..base
        };
    }
    if !app.is_empty() && fallback {
        let reason = if is_scriptable(&app) {
            "neither this session nor its process has a controlling terminal".to_string()
        } else {
            format!("{app} does not expose its tabs to AppleScript")
        };
        return FocusPlan {
            method: FocusMethod::App,
            app,
            tty,
            reason,
            ..base
        };
    }
    let reason = if app.is_empty() {
        "its terminal is not one FreeMicro can identify"
    } else {
        "tab targeting is off for this key"
    };
    FocusPlan {
        tty,
        reason: reason.into(),
        ..base
    }
}

/// AppleScript selecting the tab whose tty is `tty` in `app`.
///
/// It never launches an app (`is running` first), and only activates on a
/// match.
pub fn tab_script(app: &str, tty: &str) -> String {
    if !valid_tty(tty) || !is_scriptable(app) {
        return String::new();
    }
    let body = if app == "Terminal" {
        format!(
            "    repeat with w in windows\n      repeat with t in tabs of w\n        \
             if tty of t is \"{tty}\" then\n          set selected of t to true\n          \
             set frontmost of w to true\n          set matched to true\n          \
             exit repeat\n        end if\n      end repeat\n      \
             if matched then exit repeat\n    end repeat\n"
        )
    } else {
        format!(
            "    repeat with w in windows\n      repeat with t in tabs of w\n        \
             repeat with s in sessions of t\n          if tty of s is \"{tty}\" then\n            \
             select w\n            select t\n            select s\n            \
             set matched to true\n            exit repeat\n          end if\n        \
             end repeat\n        if matched then exit repeat\n      end repeat\n      \
             if matched then exit repeat\n    end repeat\n"
        )
    };
    format!(
        "if application \"{app}\" is running then\n  tell application \"{app}\"\n    \
         set matched to false\n{body}    if matched then activate\n  end tell\nend if"
    )
}

pub fn new_terminal_script(app: &str) -> String {
    let safe = escape_applescript(app);
    match app {
        "Terminal" => format!("tell application \"{safe}\"\n  activate\n  do script \"\"\nend tell"),
        "iTerm2" => format!(
            "tell application \"{safe}\"\n  activate\n  create window with default profile\nend tell"
        ),
        _ if CMDN_NEW_WINDOW_APPS.contains(&app) => format!(
            "tell application \"{safe}\" to activate\ndelay 0.15\n\
             tell application \"System Events\" to keystroke \"n\" using command down"
        ),
        _ => format!("tell application \"{safe}\" to activate"),
    }
}

pub fn open_new_terminal(app: &str, backend: &mut dyn Backend) -> bool {
    let name = app.trim();
    !name.is_empty() && backend.run_applescript(&new_terminal_script(name)).is_ok()
}

/// The slots as the pad shows them, seeded from `<home>/slots.json`.
pub fn current_slots(
    home: &Path,
    config: &AgentKeysConfig,
    store: &mut StateStore,
    now: f64,
    herdr: &Herdr,
) -> Vec<AgentSlot> {
    let previous = slots::load(&slots::slots_path(home));
    let sessions = herdr.agent_sessions(&store.sessions(now, false, true), now);
    resolve_slots(config, &sessions, &previous, now, &store.decay, None)
}

/// The plan for Agent Key `index`.
pub fn plan_for_slot(
    index: usize,
    slots: &[AgentSlot],
    fallback: bool,
    tty_lookup: &mut dyn FnMut(i32) -> String,
    now: f64,
) -> FocusPlan {
    if index >= SLOT_COUNT {
        return FocusPlan {
            reason: format!("slot {index} is not one of the six Agent Keys"),
            ..FocusPlan::default()
        };
    }
    let Some(slot) = slots.get(index) else {
        return FocusPlan {
            reason: "no live sessions".into(),
            ..FocusPlan::default()
        };
    };
    plan_for_session(
        slot.session(),
        &slot.path,
        &slot.label(),
        fallback,
        tty_lookup,
        now,
    )
}

/// Carry out `plan`. False if it did nothing.
pub fn perform(plan: &FocusPlan, backend: &mut dyn Backend, herdr: &Herdr) -> bool {
    match (plan.method, &plan.session) {
        (FocusMethod::Herdr, Some(session)) => herdr.focus_session(&session.session_id),
        (FocusMethod::Tab, _) if !plan.script.is_empty() => {
            backend.run_applescript(&plan.script).is_ok()
        }
        (FocusMethod::App, _) if !plan.app.is_empty() => {
            backend.activate_app(&plan.app, false).is_ok()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::tests::serve;
    use crate::input::actions::ActionError;
    use crate::state::process::{ProcessLiveness, ProcessProbe};
    use crate::state::store::SessionMeta;
    use crate::state::{AgentState, DecayPolicy, TerminalInfo};
    use crate::testutil::TempDir;
    use serde_json::json;

    const NOW: f64 = 1_000_000.0;

    /// Records `run_applescript` / `activate_app`; nothing else is expected.
    #[derive(Default)]
    struct Calls(Vec<String>);

    impl Backend for Calls {
        fn description(&self) -> &str {
            "test"
        }
        fn type_text(&mut self, _: &str) -> Result<(), ActionError> {
            unreachable!()
        }
        fn press_key(&mut self, _: &str) -> Result<(), ActionError> {
            unreachable!()
        }
        fn hold_key(&mut self, _: &str, _: bool) -> Result<(), ActionError> {
            unreachable!()
        }
        fn run_shell(&mut self, _: &str, _: Option<&str>, _: bool) -> Result<(), ActionError> {
            unreachable!()
        }
        fn run_applescript(&mut self, script: &str) -> Result<(), ActionError> {
            self.0.push(format!("applescript {script}"));
            Ok(())
        }
        fn activate_app(&mut self, name: &str, cycle: bool) -> Result<(), ActionError> {
            self.0.push(format!("activate {name} {cycle}"));
            Ok(())
        }
        fn move_mouse(&mut self, _: f64, _: f64, _: bool) -> Result<(), ActionError> {
            unreachable!()
        }
        fn click_mouse(&mut self, _: &str, _: u32) -> Result<(), ActionError> {
            unreachable!()
        }
        fn release_held_keys(&mut self) -> usize {
            0
        }
        fn settle(&mut self, _: f64) {}
    }

    fn session(tty: &str, pid: i32, program: &str) -> SessionState {
        let mut s = SessionState::new("a", AgentState::Working, NOW);
        s.cwd = "/code/api".into();
        s.tty = tty.into();
        s.pid = pid;
        s.term_program = program.into();
        s
    }

    fn never(_: i32) -> String {
        panic!("no lookup expected")
    }

    fn down() -> Herdr {
        Herdr {
            socket: "/nonexistent/herdr.sock".into(),
        }
    }

    const EVIL: &str = "/dev/ttys004\" \n do shell script \"rm -rf ~\"";

    #[test]
    fn valid_tty_accepts_only_device_paths() {
        for (tty, ok) in [
            ("/dev/ttys004", true),
            ("/dev/pts/3", true),
            ("/dev/", false),
            ("ttys004", false),
            ("/dev/ttys004\n", false),
            (EVIL, false),
        ] {
            assert_eq!(valid_tty(tty), ok, "{tty:?}");
        }
        assert_eq!(tab_script("Terminal", EVIL), "");
        assert_eq!(tab_script("Ghostty", "/dev/ttys004"), "");
    }

    #[test]
    fn app_name_for_maps_term_program() {
        for (program, app) in [
            ("Apple_Terminal", "Terminal"),
            (" iTerm.app ", "iTerm2"),
            ("ghostty", "Ghostty"),
            ("", ""),
            ("xterm", ""),
        ] {
            assert_eq!(app_name_for(program), app);
        }
    }

    #[test]
    fn plan_for_session_picks_tab_app_or_nothing() {
        // (tty, pid, program, fallback, method, app, tty, describe fragment)
        for (stored, pid, program, fallback, method, app, tty, says) in [
            (
                "/dev/ttys004",
                0,
                "Apple_Terminal",
                true,
                FocusMethod::Tab,
                "Terminal",
                "/dev/ttys004",
                "Terminal tab /dev/ttys004",
            ),
            (
                "/dev/ttys004",
                0,
                "ghostty",
                true,
                FocusMethod::App,
                "Ghostty",
                "/dev/ttys004",
                "tab unknown",
            ),
            (
                "",
                0,
                "ghostty",
                false,
                FocusMethod::None,
                "",
                "",
                "tab targeting is off",
            ),
            (
                "",
                0,
                "",
                true,
                FocusMethod::None,
                "",
                "",
                "not one FreeMicro can identify",
            ),
            // A hostile stored tty never reaches a script: app level only.
            (
                EVIL,
                0,
                "Apple_Terminal",
                true,
                FocusMethod::App,
                "Terminal",
                "",
                "tab unknown",
            ),
        ] {
            let s = session(stored, pid, program);
            let plan = plan_for_session(Some(&s), "", "", fallback, &mut never, 0.0);
            assert_eq!((plan.method, plan.app.as_str()), (method, app), "{program}");
            assert_eq!(plan.tty, tty);
            assert_eq!(plan.project, "/code/api");
            assert!(plan.describe().contains(says), "{}", plan.describe());
            assert_eq!(plan.script.is_empty(), method != FocusMethod::Tab);
        }
    }

    #[test]
    fn plan_for_session_tab_scripts_never_launch_and_match_by_tty() {
        let s = session("/dev/ttys004", 0, "Apple_Terminal");
        let plan = plan_for_session(Some(&s), "", "", true, &mut never, 0.0);
        assert!(plan
            .script
            .starts_with("if application \"Terminal\" is running then"));
        assert!(plan.script.contains("if tty of t is \"/dev/ttys004\" then"));
        assert!(plan.script.contains("if matched then activate"));
        let s = session("/dev/ttys009", 0, "iTerm.app");
        let plan = plan_for_session(Some(&s), "", "", true, &mut never, 0.0);
        assert_eq!(plan.app, "iTerm2");
        assert!(plan.script.contains("sessions of t") && plan.script.contains("select s"));
    }

    #[test]
    fn plan_for_session_derives_a_missing_tty_from_the_pid() {
        // Distinct pids: the tty cache is process-wide.
        for (pid, found, method) in [
            (4201, "/dev/ttys003", FocusMethod::Tab),
            (4202, "", FocusMethod::App), // dead pid: tab unknown, no guess
            (4203, EVIL, FocusMethod::App), // a lookup is gated like a stored tty
        ] {
            let s = session("", pid, "Apple_Terminal");
            let plan = plan_for_session(
                Some(&s),
                "",
                "",
                true,
                &mut |p| {
                    assert_eq!(p, pid);
                    found.to_string()
                },
                0.0,
            );
            assert_eq!(plan.method, method, "{pid}");
        }
        // No pid at all: nothing to look up.
        let s = session("", 0, "Apple_Terminal");
        assert_eq!(
            plan_for_session(Some(&s), "", "", true, &mut never, 0.0).method,
            FocusMethod::App
        );
    }

    #[test]
    fn tty_from_pid_caches_until_expiry() {
        let mut calls = 0;
        let mut lookup = |_| {
            calls += 1;
            "/dev/ttys005".to_string()
        };
        for now in [10.0, 11.0, 24.9] {
            assert_eq!(tty_from_pid(4300, &mut lookup, now), "/dev/ttys005");
        }
        tty_from_pid(4300, &mut lookup, 25.0);
        assert_eq!(calls, 2);
    }

    #[test]
    fn plan_for_session_without_a_session_is_an_empty_slot() {
        let plan = plan_for_session(None, "/code/api", "", true, &mut never, 0.0);
        assert!(plan.slot_empty() && !plan.actionable());
        assert!(plan.describe().contains("no live session"));
        assert!(!perform(&plan, &mut Calls::default(), &down()));
    }

    #[test]
    fn plan_for_slot_rejects_bad_and_unfilled_indices() {
        for (index, says) in [(6, "not one of the six"), (0, "no live sessions")] {
            let plan = plan_for_slot(index, &[], true, &mut never, 0.0);
            assert!(plan.slot_empty());
            assert!(plan.reason.contains(says));
        }
    }

    #[test]
    fn perform_runs_the_script_or_activates_the_app() {
        for (program, call) in [
            ("Apple_Terminal", "applescript if application"),
            ("warp", "activate Warp false"),
        ] {
            let s = session("/dev/ttys004", 0, program);
            let plan = plan_for_session(Some(&s), "", "", true, &mut never, 0.0);
            let mut backend = Calls::default();
            assert!(perform(&plan, &mut backend, &down()));
            assert!(backend.0[0].starts_with(call), "{:?}", backend.0);
        }
    }

    #[test]
    fn perform_focuses_a_herdr_pane_by_session_id() {
        let dir = TempDir::new();
        let (herdr, calls) = serve(&dir, |_, _| {
            json!({"result": {"agents": [
                {"pane_id": "w3:p7", "agent_session": {"value": "a"}}]}})
        });
        let s = session("/dev/ttys038", 0, "herdr");
        let plan = plan_for_session(Some(&s), "", "", true, &mut never, 0.0);
        assert_eq!(plan.method, FocusMethod::Herdr);
        let mut backend = Calls::default();
        assert!(perform(&plan, &mut backend, &herdr));
        assert!(backend.0.is_empty());
        let last = calls.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last, ("agent.focus".into(), json!({"target": "w3:p7"})));
    }

    #[test]
    fn new_terminal_script_by_app_tier() {
        for (app, script) in [
            (
                "Terminal",
                "tell application \"Terminal\"\n  activate\n  do script \"\"\nend tell",
            ),
            (
                "iTerm2",
                "tell application \"iTerm2\"\n  activate\n  create window with default profile\nend tell",
            ),
            (
                "Ghostty",
                "tell application \"Ghostty\" to activate\ndelay 0.15\n\
                 tell application \"System Events\" to keystroke \"n\" using command down",
            ),
            (
                "SomeFutureTerm",
                "tell application \"SomeFutureTerm\" to activate",
            ),
        ] {
            assert_eq!(new_terminal_script(app), script);
        }
        let mut backend = Calls::default();
        assert!(!open_new_terminal("  ", &mut backend));
        assert!(backend.0.is_empty());
        assert!(open_new_terminal(" Terminal ", &mut backend));
        assert_eq!(backend.0.len(), 1);
    }

    struct Alive;

    impl ProcessProbe for Alive {
        fn alive(&self, _: i32) -> bool {
            true
        }
        fn started(&self, _: i32, _: f64) -> Option<f64> {
            None
        }
    }

    #[test]
    fn current_slots_resolves_the_key_to_its_project() {
        let home = TempDir::new();
        let mut store = StateStore::new(
            home.path().join("state"),
            DecayPolicy::default(),
            ProcessLiveness::new(Box::new(Alive)),
        );
        for (id, state, cwd, at, tty, program) in [
            ("a", AgentState::Working, "/code/api", NOW, "", ""),
            (
                "b",
                AgentState::Waiting,
                "/code/web",
                NOW - 1.0,
                "/dev/ttys007",
                "Apple_Terminal",
            ),
        ] {
            let meta = SessionMeta {
                cwd: cwd.into(),
                terminal: TerminalInfo {
                    tty: tty.into(),
                    program: program.into(),
                    ..TerminalInfo::default()
                },
                ..SessionMeta::default()
            };
            store.update(id, state, &meta, at).unwrap();
        }
        let herdr = Herdr {
            socket: home.path().join("no-herdr.sock"),
        };
        let slots = current_slots(
            home.path(),
            &AgentKeysConfig::default(),
            &mut store,
            NOW,
            &herdr,
        );
        let plan = plan_for_slot(1, &slots, true, &mut never, 0.0);
        assert_eq!(plan.project, "/code/web");
        assert_eq!(plan.tty, "/dev/ttys007");
    }
}
