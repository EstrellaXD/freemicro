//! Answer Claude Code's permission prompt in the session that is asking.
//! Port of `freemicro/permission_prompt.py`; its docstring has the evidence
//! for the keys and the six gates.
//!
//! The gates: only a pending prompt record (`hooks::prompt_is_pending`),
//! only a provably live process, only a record younger than `max_age`,
//! only an exact Terminal/iTerm2 tab, and the keystroke sits inside one
//! AppleScript behind `if matched and ready`. [`perform`] answers each
//! record at most once.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use crate::focus::{self, FocusMethod, FocusPlan};
use crate::input::actions::{ActionError, Backend};
use crate::input::keys::applescript_for;
use crate::state::agentkeys::normalise_project;
use crate::state::hooks::prompt_is_pending;
use crate::state::{SessionState, StateStore};

pub const APPROVE: &str = "approve";
pub const REJECT: &str = "reject";
pub const ALWAYS: &str = "always";
pub const ANSWERS: [&str; 3] = [APPROVE, REJECT, ALWAYS];

/// How stale a `waiting` claim may be before it is not answered.
pub const MAX_AGE_SECONDS: f64 = 900.0;
pub const SETTLE_TRIES: u32 = 8;
pub const SETTLE_DELAY: f64 = 0.05;
const LATCH_SIZE: usize = 64;

/// Answered records: (session id, updated_at bits, prompt id).
static LATCH: Mutex<VecDeque<(String, u64, String)>> = Mutex::new(VecDeque::new());

/// The keystroke for each answer: option 1 is "Yes", option 2 the broader
/// yes, Escape cancels (denies).
fn answer_key(answer: &str) -> &'static str {
    match answer {
        APPROVE => "1",
        ALWAYS => "2",
        _ => "escape",
    }
}

/// `value` as one of [`ANSWERS`], or `""`.
pub fn normalise_answer(value: &str) -> String {
    let text = value.trim().to_lowercase();
    if ANSWERS.contains(&text.as_str()) {
        text
    } else {
        String::new()
    }
}

/// Every session believed to have a prompt on screen, newest first.
pub fn pending(
    sessions: &[SessionState],
    now: f64,
    max_age: f64,
    project: &str,
) -> Vec<SessionState> {
    let wanted = normalise_project(project);
    let limit = max_age.max(0.0);
    let mut candidates: Vec<SessionState> = sessions
        .iter()
        .filter(|s| prompt_is_pending(s))
        // "We could not tell" is a refusal.
        .filter(|s| s.process_alive == Some(true))
        .filter(|s| limit == 0.0 || s.age(now) <= limit)
        .filter(|s| wanted.is_empty() || normalise_project(&s.cwd) == wanted)
        .cloned()
        .collect();
    candidates.sort_by(|a, b| {
        b.updated_at
            .total_cmp(&a.updated_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    candidates
}

/// What pressing the key would answer, where, and how - or why not.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnswerPlan {
    pub answer: String,
    pub key: String,
    pub session: Option<SessionState>,
    pub focus_plan: Option<FocusPlan>,
    /// Verifies the tab, then sends `key`.
    pub script: String,
    pub label: String,
    pub waiting: usize,
    pub reason: String,
}

impl AnswerPlan {
    pub fn actionable(&self) -> bool {
        !self.answer.is_empty()
            && !self.script.is_empty()
            && self.session.is_some()
            && self
                .focus_plan
                .as_ref()
                .is_some_and(|p| p.method == FocusMethod::Tab)
    }

    pub fn describe(&self) -> String {
        let answer = if self.answer.is_empty() {
            "answer"
        } else {
            &self.answer
        };
        let target = match &self.focus_plan {
            Some(target) if self.actionable() => target,
            _ if self.label.is_empty() => {
                return format!("{answer} - does nothing: {}", self.reason)
            }
            _ => return format!("{answer} {} - does nothing: {}", self.label, self.reason),
        };
        let mut line = format!(
            "{answer} {} - {} tab {}",
            self.label, target.app, target.tty
        );
        if self.waiting > 1 {
            line += &format!(" (newest of {} waiting)", self.waiting);
        }
        line
    }
}

/// One AppleScript that raises the tab on `tty` and only then sends `combo`.
pub fn confirm_script(app: &str, tty: &str, combo: &str) -> String {
    let raise_tab = focus::tab_script(app, tty);
    if raise_tab.is_empty() {
        return String::new();
    }
    let Ok(keystroke) = applescript_for(combo) else {
        return String::new();
    };
    format!(
        "set matched to false\n\
         set ready to false\n\
         {raise_tab}\n\
         if matched and application \"{app}\" is running then\n  \
         repeat {SETTLE_TRIES} times\n    \
         tell application \"{app}\" to set ready to frontmost\n    \
         if ready then exit repeat\n    \
         delay {SETTLE_DELAY}\n  \
         end repeat\n\
         end if\n\
         if matched and ready then {keystroke}"
    )
}

/// Work out what answering `answer` would do now. Pure over `sessions`.
pub fn plan(
    answer: &str,
    project: &str,
    max_age: f64,
    now: f64,
    sessions: &[SessionState],
    tty_lookup: &mut dyn FnMut(i32) -> String,
) -> AnswerPlan {
    let chosen = normalise_answer(answer);
    if chosen.is_empty() {
        return AnswerPlan {
            reason: format!(
                "{} is not one of {}",
                crate::input::actions::repr(answer),
                ANSWERS.join(", ")
            ),
            ..AnswerPlan::default()
        };
    }
    let key = answer_key(&chosen).to_string();
    let candidates = pending(sessions, now, max_age, project);
    let Some(session) = candidates.first() else {
        let place = if project.is_empty() {
            String::new()
        } else {
            format!(" in {}", normalise_project(project))
        };
        return AnswerPlan {
            answer: chosen,
            key,
            reason: format!("nothing is waiting on a permission prompt{place}"),
            ..AnswerPlan::default()
        };
    };
    let target = focus::plan_for_session(Some(session), &session.cwd, "", true, tty_lookup, now);
    let common = AnswerPlan {
        answer: chosen,
        key: key.clone(),
        session: Some(session.clone()),
        label: if target.label.is_empty() {
            target.project.clone()
        } else {
            target.label.clone()
        },
        waiting: candidates.len(),
        ..AnswerPlan::default()
    };
    if target.method != FocusMethod::Tab {
        // Right app, unknown tab: the answer would land in some other tab.
        let reason = if target.reason.is_empty() {
            "its terminal tab cannot be identified, so the answer would go somewhere else"
                .to_string()
        } else {
            target.reason.clone()
        };
        return AnswerPlan {
            reason,
            focus_plan: Some(target),
            ..common
        };
    }
    let script = confirm_script(&target.app, &target.tty, &key);
    if script.is_empty() {
        return AnswerPlan {
            reason: "its tab cannot be addressed safely".into(),
            focus_plan: Some(target),
            ..common
        };
    }
    AnswerPlan {
        script,
        focus_plan: Some(target),
        ..common
    }
}

/// [`plan`] against the session store in `home`.
pub fn plan_live(answer: &str, project: &str, max_age: f64, home: &Path) -> AnswerPlan {
    let now = crate::clock::wall();
    let sessions = StateStore::for_home(home).sessions(now, false, true);
    let mut tty = |pid: i32| crate::state::process::tty_for_pid(pid).0;
    plan(answer, project, max_age, now, &sessions, &mut tty)
}

/// Take the one answer this record is allowed; `false` if already taken.
fn claim(session: &SessionState) -> bool {
    let key = (
        session.session_id.clone(),
        session.updated_at.to_bits(),
        session.prompt_id.clone(),
    );
    let mut latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
    if latch.contains(&key) {
        return false;
    }
    latch.push_back(key);
    while latch.len() > LATCH_SIZE {
        latch.pop_front();
    }
    true
}

/// Carry out `plan`; `Ok(false)` if nothing was sent (often the case).
pub fn perform(plan: &AnswerPlan, backend: &mut dyn Backend) -> Result<bool, ActionError> {
    let Some(session) = plan.session.as_ref().filter(|_| plan.actionable()) else {
        return Ok(false);
    };
    if !claim(session) {
        return Ok(false);
    }
    backend.run_applescript(&plan.script)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::actions::RecordingBackend;
    use crate::state::hooks::PROMPT_EVENT;
    use crate::state::AgentState;

    const NOW: f64 = 1_000_000.0;

    fn asking(id: &str, cwd: &str, age: f64) -> SessionState {
        let mut s = SessionState::new(id, AgentState::Waiting, NOW - age);
        s.last_event = PROMPT_EVENT.to_string();
        s.cwd = cwd.to_string();
        s.tty = "/dev/ttys004".to_string();
        s.term_program = "Apple_Terminal".to_string();
        s.process_alive = Some(true);
        s
    }

    fn plan_of(answer: &str, project: &str, sessions: &[SessionState]) -> AnswerPlan {
        plan(answer, project, MAX_AGE_SECONDS, NOW, sessions, &mut |_| {
            String::new()
        })
    }

    #[test]
    fn pending_keeps_only_live_fresh_matching_prompts_newest_first() {
        let mut unknown = asking("unknown", "/p/a", 1.0);
        unknown.process_alive = None;
        let mut emitted = asking("emitted", "/p/a", 1.0);
        emitted.last_event = String::new();
        let sessions = vec![
            asking("old", "/p/a", 30.0),
            asking("new", "/p/a", 5.0),
            asking("other", "/p/b", 1.0),
            asking("stale", "/p/a", MAX_AGE_SECONDS + 1.0),
            unknown,
            emitted,
        ];
        let ids = |found: Vec<SessionState>| -> Vec<String> {
            found.into_iter().map(|s| s.session_id).collect()
        };
        assert_eq!(
            ids(pending(&sessions, NOW, MAX_AGE_SECONDS, "/p/a")),
            ["new", "old"]
        );
        assert_eq!(
            ids(pending(&sessions, NOW, 0.0, "/p/a")),
            ["new", "old", "stale"]
        );
        assert_eq!(
            ids(pending(&sessions, NOW, MAX_AGE_SECONDS, "")),
            ["other", "new", "old"]
        );
    }

    #[test]
    fn plan_targets_the_exact_tab_or_says_why_not() {
        let mut ghostty = asking("g", "/p/a", 1.0);
        ghostty.term_program = "ghostty".to_string();
        let cases: Vec<(&str, Vec<SessionState>, &str)> = vec![
            (
                "bogus",
                vec![],
                "answer - does nothing: 'bogus' is not one of approve, reject, always",
            ),
            (
                "approve",
                vec![],
                "approve - does nothing: nothing is waiting on a permission prompt",
            ),
            (
                "reject",
                vec![ghostty],
                "reject /p/a - does nothing: Ghostty does not expose its tabs to AppleScript",
            ),
            (
                "approve",
                vec![asking("a", "/p/a", 1.0), asking("b", "/p/b", 2.0)],
                "approve /p/a - Terminal tab /dev/ttys004 (newest of 2 waiting)",
            ),
        ];
        for (answer, sessions, want) in cases {
            assert_eq!(plan_of(answer, "", &sessions).describe(), want, "{answer}");
        }
        let none = plan_of("approve", "/p/x", &[asking("a", "/p/a", 1.0)]);
        assert!(none.reason.ends_with(" in /p/x"), "{}", none.reason);
    }

    #[test]
    fn confirm_script_types_only_after_the_tab_is_matched_and_frontmost() {
        let script = confirm_script("Terminal", "/dev/ttys004", "escape");
        assert!(script.starts_with("set matched to false\nset ready to false\n"));
        assert!(script.ends_with(
            "if matched and ready then tell application \"System Events\" to key code 53"
        ));
        assert!(script.contains(
            "repeat 8 times\n    tell application \"Terminal\" to set ready to frontmost\n"
        ));
        assert!(script.contains("delay 0.05\n"));
        assert_eq!(confirm_script("Ghostty", "/dev/ttys004", "1"), "");
    }

    #[test]
    fn perform_answers_each_record_once() {
        let mut session = asking("perform-once", "/p/a", 1.0);
        session.prompt_id = "p1".to_string();
        let ready = plan_of("approve", "", std::slice::from_ref(&session));
        let mut backend = RecordingBackend::default();
        assert!(perform(&ready, &mut backend).unwrap());
        assert!(!perform(&ready, &mut backend).unwrap());
        assert!(!perform(&plan_of("approve", "", &[]), &mut backend).unwrap());
        assert_eq!(backend.calls.len(), 1);
        assert!(
            backend.calls[0].contains("keystroke \"1\""),
            "{}",
            backend.calls[0]
        );
    }
}
