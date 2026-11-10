//! Process and terminal probes used by the hook writer and the store.
//! Port of `state/engine.py` (`current_terminal`, `tty_for_pid`,
//! `pid_alive`, `pid_started`, `parse_elapsed`, `ProcessLiveness`).

use std::collections::HashMap;
use std::process::{Command, Stdio};

use super::{SessionState, TerminalInfo};

pub const PS_TIMEOUT_SECONDS: f64 = 2.0;
pub const MAX_TTY_HOPS: usize = 8;
pub const DEFAULT_LIVENESS_CACHE_SECONDS: f64 = 1.0;
pub const DEFAULT_IDENTITY_CACHE_SECONDS: f64 = 60.0;
pub const PID_START_TOLERANCE_SECONDS: f64 = 5.0;
/// Backstop cap on each liveness cache before stale entries are swept.
const LIVENESS_CACHE_MAX: usize = 512;

/// The OS questions liveness needs. Real: [`SystemProbe`]; tests: a fake.
pub trait ProcessProbe {
    fn alive(&self, pid: i32) -> bool;
    /// Wall-clock start time of `pid`, if it can be read.
    fn started(&self, pid: i32, now: f64) -> Option<f64>;
}

/// `kill(pid, 0)` and `ps -o etime=`.
pub struct SystemProbe;

impl ProcessProbe for SystemProbe {
    fn alive(&self, pid: i32) -> bool {
        // pid 0 / negatives name process groups, 1 is launchd: never a session.
        if pid <= 1 {
            return false;
        }
        // SAFETY: signal 0 only checks existence and permission.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        // EPERM: it exists, it just is not ours to signal.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn started(&self, pid: i32, now: f64) -> Option<f64> {
        if pid <= 1 {
            return None;
        }
        let out = ps(&["-o", "etime=", "-p", &pid.to_string()])?;
        parse_elapsed(&out).map(|elapsed| now - elapsed)
    }
}

/// `ps <args>` stdout, or `None` when it fails or exits non-zero.
// ponytail: no PS_TIMEOUT_SECONDS; std has no wait-with-timeout and a local
// `ps` answers at once. Add kill-after-timeout if it ever hangs a hook.
fn ps(args: &[&str]) -> Option<String> {
    let out = Command::new("ps")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Cached liveness verdicts, so a 1 s poll costs one probe per window.
pub struct ProcessLiveness {
    probe: Box<dyn ProcessProbe>,
    alive_cache: HashMap<i32, (f64, bool)>,
    started_cache: HashMap<i32, (f64, Option<f64>)>,
}

impl ProcessLiveness {
    pub fn new(probe: Box<dyn ProcessProbe>) -> ProcessLiveness {
        ProcessLiveness {
            probe,
            alive_cache: HashMap::new(),
            started_cache: HashMap::new(),
        }
    }

    pub fn system() -> ProcessLiveness {
        ProcessLiveness::new(Box::new(SystemProbe))
    }

    /// `Some(true)` = provably the original process still runs; `Some(false)`
    /// = gone or recycled; `None` = no pid or not asked (`verify == false`).
    pub fn verdict(&mut self, session: &SessionState, verify: bool, now: f64) -> Option<bool> {
        let pid = session.pid;
        if pid <= 1 {
            return None;
        }
        if !self.alive(pid, now) {
            return Some(false);
        }
        if !verify {
            return Some(true);
        }
        // A process that started after the record was written cannot have
        // written it: it is a later tenant of a recycled pid.
        let started = self.started(pid, now)?;
        Some(started <= session.updated_at + PID_START_TOLERANCE_SECONDS)
    }

    fn alive(&mut self, pid: i32, now: f64) -> bool {
        if let Some(&(at, answer)) = self.alive_cache.get(&pid) {
            if now - at < DEFAULT_LIVENESS_CACHE_SECONDS {
                return answer;
            }
        }
        if !self.probe.alive(pid) {
            // Whoever holds this pid next is a different process.
            self.alive_cache.remove(&pid);
            self.started_cache.remove(&pid);
            return false;
        }
        if self.alive_cache.len() >= LIVENESS_CACHE_MAX {
            self.prune(now);
        }
        self.alive_cache.insert(pid, (now, true));
        true
    }

    fn started(&mut self, pid: i32, now: f64) -> Option<f64> {
        if let Some(&(at, answer)) = self.started_cache.get(&pid) {
            if now - at < DEFAULT_IDENTITY_CACHE_SECONDS {
                return answer;
            }
        }
        let answer = self.probe.started(pid, now);
        if self.started_cache.len() >= LIVENESS_CACHE_MAX {
            self.prune(now);
        }
        self.started_cache.insert(pid, (now, answer));
        answer
    }

    fn prune(&mut self, now: f64) {
        self.alive_cache
            .retain(|_, (at, _)| now - *at < DEFAULT_LIVENESS_CACHE_SECONDS);
        self.started_cache
            .retain(|_, (at, _)| now - *at < DEFAULT_IDENTITY_CACHE_SECONDS);
    }
}

/// `"/dev/ttys003"` from `ttys003`, `s003`, `/dev/ttys003`; `""` for `??`.
pub fn normalise_tty(value: &str) -> String {
    let name = value.trim();
    if name.is_empty() || name.starts_with('?') {
        return String::new();
    }
    let name = name.strip_prefix("/dev/").unwrap_or(name);
    // Segments may not start with a dot, which keeps `..` out.
    let segment_ok = |seg: &str| {
        let mut chars = seg.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    if name.is_empty() || name.len() > 59 || !name.split('/').all(segment_ok) {
        return String::new();
    }
    format!("/dev/{name}")
}

/// `ps -o etime=` text (`[[dd-]hh:]mm:ss`) to seconds.
pub fn parse_elapsed(text: &str) -> Option<f64> {
    let mut text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut days = 0.0;
    if let Some((head, rest)) = text.split_once('-') {
        days = head.trim().parse::<i64>().ok()? as f64;
        text = rest;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut seconds = 0.0;
    for part in parts {
        seconds = seconds * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(seconds + days * 86400.0)
}

/// Walk up from `pid` to the first ancestor with a tty. Returns the tty and
/// the pid that owns it, or `("", 0)` when nothing in the chain has one.
pub fn tty_for_pid(pid: i32) -> (String, i32) {
    walk_tty(pid, ps_tty_and_parent, MAX_TTY_HOPS)
}

/// `(tty, ppid)` of `pid` according to `ps`; `("", 0)` when it is gone.
fn ps_tty_and_parent(pid: i32) -> (String, i32) {
    if pid <= 0 {
        return (String::new(), 0);
    }
    let Some(out) = ps(&["-o", "tty=,ppid=", "-p", &pid.to_string()]) else {
        return (String::new(), 0);
    };
    let fields: Vec<&str> = out.split_whitespace().collect();
    match fields.as_slice() {
        [] => (String::new(), 0),
        // One column: a number can only be the ppid.
        [only] if only.chars().all(|c| c.is_ascii_digit()) => {
            (String::new(), only.parse().unwrap_or(0))
        }
        [only] => (only.to_string(), 0),
        [tty, ppid, ..] => (tty.to_string(), ppid.parse().unwrap_or(0)),
    }
}

fn walk_tty(
    pid: i32,
    mut probe: impl FnMut(i32) -> (String, i32),
    max_hops: usize,
) -> (String, i32) {
    let mut seen = Vec::new();
    let mut current = pid;
    for _ in 0..max_hops {
        // pid 1 is launchd: never a terminal, and its own ceiling.
        if current <= 1 || seen.contains(&current) {
            break;
        }
        seen.push(current);
        let (raw, parent) = probe(current);
        let tty = normalise_tty(&raw);
        if !tty.is_empty() {
            return (tty, current);
        }
        current = parent;
    }
    (String::new(), 0)
}

/// The terminal the calling process (a Claude Code hook) runs in: tty, the
/// `claude` pid, `TERM_PROGRAM`, `TERM_SESSION_ID` / `ITERM_SESSION_ID`.
pub fn current_terminal() -> TerminalInfo {
    let mut tty = controlling_tty();
    if tty.is_empty() {
        tty = tty_for_pid(std::process::id() as i32).0;
    }
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    TerminalInfo {
        tty: normalise_tty(&tty),
        // SAFETY: getppid has no preconditions.
        pid: unsafe { libc::getppid() },
        program: env("TERM_PROGRAM").unwrap_or_default(),
        session: env("ITERM_SESSION_ID")
            .or_else(|| env("TERM_SESSION_ID"))
            .unwrap_or_default(),
    }
}

/// The tty of `/dev/tty`, if this process has a controlling terminal. Empty
/// inside a Claude Code hook; exact when a command is run by hand.
fn controlling_tty() -> String {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty")
    else {
        return String::new();
    };
    // SAFETY: the fd stays open for the lifetime of `file`; ttyname returns
    // static storage or null.
    let name = unsafe { libc::ttyname(file.as_raw_fd()) };
    if name.is_null() {
        return String::new();
    }
    // SAFETY: a non-null ttyname result is a NUL-terminated C string.
    let name = unsafe { std::ffi::CStr::from_ptr(name) };
    normalise_tty(&name.to_string_lossy())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::AgentState;
    use std::cell::Cell;
    use std::rc::Rc;

    pub const EPOCH: f64 = 1_784_000_000.0;

    /// pid -> start time (`None` = alive, start unknown); counts questions.
    #[derive(Default)]
    pub struct Table {
        pub pids: Vec<(i32, Option<f64>)>,
        pub alive_calls: Rc<Cell<u32>>,
        pub started_calls: Rc<Cell<u32>>,
    }

    impl ProcessProbe for Table {
        fn alive(&self, pid: i32) -> bool {
            self.alive_calls.set(self.alive_calls.get() + 1);
            self.pids.iter().any(|(p, _)| *p == pid)
        }
        fn started(&self, pid: i32, _now: f64) -> Option<f64> {
            self.started_calls.set(self.started_calls.get() + 1);
            self.pids
                .iter()
                .find(|(p, _)| *p == pid)
                .and_then(|(_, s)| *s)
        }
    }

    #[test]
    fn normalise_tty_spellings() {
        for (raw, want) in [
            ("ttys003", "/dev/ttys003"),
            ("/dev/ttys003", "/dev/ttys003"),
            (" pts/3 ", "/dev/pts/3"),
            ("??", ""),
            ("", ""),
            ("/dev/", ""),
            ("../etc/passwd", ""),
            ("ttys0; rm -rf", ""),
            ("a/.hidden", ""),
        ] {
            assert_eq!(normalise_tty(raw), want, "{raw:?}");
        }
        assert_eq!(normalise_tty(&"t".repeat(60)), "");
    }

    #[test]
    fn parse_elapsed_shapes() {
        for (raw, want) in [
            ("05", Some(5.0)),
            ("01:05", Some(65.0)),
            ("  02:01:05\n", Some(7265.0)),
            ("3-02:01:05", Some(3.0 * 86400.0 + 7265.0)),
            ("", None),
            ("x-01:00", None),
            ("1:2:3:4", None),
            ("ab:cd", None),
        ] {
            assert_eq!(parse_elapsed(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn walk_tty_finds_nearest_ancestor_and_stops() {
        // hook 300 (no tty) -> claude 200 (ttys004) -> shell 100 (ttys001)
        let table = |pid: i32| match pid {
            300 => ("??".to_string(), 200),
            200 => ("ttys004".to_string(), 100),
            100 => ("ttys001".to_string(), 1),
            _ => (String::new(), 0),
        };
        assert_eq!(walk_tty(300, table, 8), ("/dev/ttys004".to_string(), 200));
        assert_eq!(walk_tty(300, table, 1), (String::new(), 0));
        assert_eq!(walk_tty(999, table, 8), (String::new(), 0));
        assert_eq!(walk_tty(1, table, 8), (String::new(), 0));
        let calls = Cell::new(0);
        let cycle = |pid: i32| {
            calls.set(calls.get() + 1);
            ("??".to_string(), if pid == 10 { 11 } else { 10 })
        };
        assert_eq!(walk_tty(10, cycle, 8), (String::new(), 0));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn verdict_is_three_valued_and_cached() {
        let table = Table {
            pids: vec![
                (10, Some(EPOCH - 100.0)),
                (11, None),
                (12, Some(EPOCH + 50.0)),
            ],
            ..Table::default()
        };
        let (alive_calls, started_calls) = (table.alive_calls.clone(), table.started_calls.clone());
        let mut live = ProcessLiveness::new(Box::new(table));
        let rec = |pid| {
            let mut s = SessionState::new("s", AgentState::Idle, EPOCH);
            s.pid = pid;
            s
        };
        assert_eq!(live.verdict(&rec(0), true, EPOCH), None);
        assert_eq!(live.verdict(&rec(1), true, EPOCH), None);
        assert_eq!(live.verdict(&rec(99), true, EPOCH), Some(false));
        assert_eq!(live.verdict(&rec(10), false, EPOCH), Some(true));
        assert_eq!(live.verdict(&rec(10), true, EPOCH), Some(true));
        assert_eq!(live.verdict(&rec(11), true, EPOCH), None);
        // Started after the record was written: a recycled pid.
        assert_eq!(live.verdict(&rec(12), true, EPOCH), Some(false));
        // Within the 5 s tolerance it is still the writer.
        let mut tol = rec(12);
        tol.updated_at = EPOCH + 46.0;
        assert_eq!(live.verdict(&tol, true, EPOCH + 0.5), Some(true));

        let (a, s) = (alive_calls.get(), started_calls.get());
        live.verdict(&rec(10), true, EPOCH + 0.5);
        assert_eq!((alive_calls.get(), started_calls.get()), (a, s));
        live.verdict(&rec(10), true, EPOCH + 2.0);
        assert_eq!((alive_calls.get(), started_calls.get()), (a + 1, s));
        live.verdict(&rec(10), true, EPOCH + 61.0);
        assert_eq!(started_calls.get(), s + 1);
    }
}
