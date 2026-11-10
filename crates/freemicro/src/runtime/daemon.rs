//! The pad lock and launchd. Port of `daemon.py`. Labels, plist paths, the
//! lock file and the log file are the Python ones, so either build's
//! `pause` / lock check sees the other.
//!
//! Code only during the port: nothing here may be run against the real
//! launchd. `launchctl` is reached only through [`Launchctl`], which tests
//! replace.

use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::device::{CREATOR_MICRO_2_PRODUCT_IDS, PRODUCT_ID_CODEX_MICRO, VENDOR_ID};

pub const LABEL: &str = "com.freemicro.daemon";
pub const ONCONNECT_LABEL: &str = "com.freemicro.onconnect";
pub const LOG_CAP_BYTES: u64 = 1024 * 1024;
pub const THROTTLE_SECONDS: u32 = 10;
pub const PLIST_PATH_ENV: &str = "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// `~/Library/LaunchAgents`.
pub fn agents_dir(user_home: &Path) -> PathBuf {
    user_home.join("Library").join("LaunchAgents")
}

pub fn plist_path(user_home: &Path, label: &str) -> PathBuf {
    agents_dir(user_home).join(format!("{label}.plist"))
}

/// `<home>/logs/daemon.log`.
pub fn log_path(home: &Path) -> PathBuf {
    home.join("logs").join("daemon.log")
}

/// `<home>/pad.lock`.
pub fn lock_path(home: &Path) -> PathBuf {
    home.join("pad.lock")
}

fn open_fd(path: &Path, create: bool) -> Option<i32> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let flags = libc::O_RDWR | if create { libc::O_CREAT } else { 0 };
    // SAFETY: c_path is a valid NUL-terminated string for the call.
    let fd = unsafe { libc::open(c_path.as_ptr(), flags, 0o644 as libc::c_uint) };
    (fd >= 0).then_some(fd)
}

fn try_flock(fd: i32) -> bool {
    // SAFETY: fd is an open descriptor owned by the caller.
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Exclusive ownership of the pad via `flock(LOCK_EX | LOCK_NB)`, with
/// `{"pid","role","started"}` written for "who has it". Released on drop.
pub struct PadLock {
    pub path: PathBuf,
    pub role: String,
    fd: Option<i32>,
}

impl PadLock {
    pub fn new(path: PathBuf, role: &str) -> PadLock {
        PadLock {
            path,
            role: role.to_string(),
            fd: None,
        }
    }
    /// Non-blocking. False = someone else holds it.
    pub fn acquire(&mut self, now: f64) -> bool {
        if self.fd.is_some() {
            return true;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Some(fd) = open_fd(&self.path, true) else {
            return false;
        };
        if !try_flock(fd) {
            // SAFETY: fd was opened above and is not used afterwards.
            unsafe { libc::close(fd) };
            return false;
        }
        let payload =
            json!({"pid": std::process::id(), "role": self.role, "started": now}).to_string();
        // SAFETY: fd is open and locked by us; the buffer outlives the call.
        unsafe {
            libc::ftruncate(fd, 0);
            libc::write(fd, payload.as_ptr().cast(), payload.len());
            libc::fsync(fd);
        }
        self.fd = Some(fd);
        true
    }
    pub fn release(&mut self) {
        if let Some(fd) = self.fd.take() {
            // SAFETY: fd is ours; truncate before unlocking hides the stale payload.
            unsafe {
                libc::ftruncate(fd, 0);
                libc::flock(fd, libc::LOCK_UN);
                libc::close(fd);
            }
        }
    }
}

impl Drop for PadLock {
    fn drop(&mut self) {
        if self.fd.is_some() {
            self.release();
        }
    }
}

/// Who holds `path`, by trying the lock. `None` if free.
pub fn lock_holder(path: &Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let fd = open_fd(path, false)?;
    if try_flock(fd) {
        // SAFETY: fd is ours; we got the lock, so it was free.
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
            libc::close(fd);
        }
        return None;
    }
    // SAFETY: fd is ours and not used afterwards.
    unsafe { libc::close(fd) };
    let data = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(if t.is_empty() { "{}" } else { &t }).ok());
    Some(match data {
        Some(v @ Value::Object(_)) => v,
        _ => json!({}),
    })
}

pub fn describe_holder(holder: &Value, now: f64) -> String {
    let role = holder
        .get("role")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .unwrap_or("another FreeMicro process");
    let label = match role {
        "daemon" => "the background daemon",
        "run" => "`freemicro run`",
        "keys" => "`freemicro keys`",
        other => other,
    };
    let pid = holder
        .get("pid")
        .filter(|p| !p.is_null() && *p != &json!(0));
    let age = holder
        .get("started")
        .and_then(Value::as_f64)
        .map(|s| format!(", up {}s", (now - s).max(0.0) as i64))
        .unwrap_or_default();
    match pid {
        Some(pid) => format!("{label} (pid {pid}{age})"),
        None => label.to_string(),
    }
}

/// Truncate-rotate `path` past `cap` (launchd keeps it open). True if rotated.
pub fn rotate_log(path: &Path, cap: u64) -> bool {
    let Ok(size) = std::fs::metadata(path).map(|m| m.len()) else {
        return false;
    };
    if size <= cap {
        return false;
    }
    let keep = cap / 2;
    let rotate = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        file.seek(SeekFrom::Start(size - keep))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        // Don't start mid-line.
        let start = tail
            .iter()
            .position(|b| *b == b'\n')
            .map_or(tail.len(), |i| i + 1);
        file.seek(SeekFrom::Start(0))?;
        writeln!(
            file,
            "--- log trimmed to the most recent {}KB ---",
            keep / 1024
        )?;
        file.write_all(&tail[start..])?;
        let end = file.stream_position()?;
        file.set_len(end)
    };
    rotate().is_ok()
}

/// A plist value; rendered as XML by hand (no plist crate).
#[derive(Debug, Clone, PartialEq)]
pub enum Plist {
    String(String),
    Integer(i64),
    Bool(bool),
    Array(Vec<Plist>),
    /// Ordered keys.
    Dict(Vec<(String, Plist)>),
}

impl Plist {
    fn s(text: impl Into<String>) -> Plist {
        Plist::String(text.into())
    }
    fn dict(entries: Vec<(&str, Plist)>) -> Plist {
        Plist::Dict(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
    /// The value at `key` of a dict.
    pub fn get(&self, key: &str) -> Option<&Plist> {
        match self {
            Plist::Dict(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// `[<exe>, "daemon", "run"]`.
pub fn daemon_argv(exe: &Path) -> Vec<String> {
    vec![exe.display().to_string(), "daemon".into(), "run".into()]
}

fn common_entries(
    argv: &[String],
    home: &Path,
    user_home: &Path,
    freemicro_home_env: Option<&str>,
) -> Vec<(&'static str, Plist)> {
    let mut env = vec![("PATH", Plist::s(PLIST_PATH_ENV))];
    if let Some(dir) = freemicro_home_env.filter(|d| !d.is_empty()) {
        env.push(("FREEMICRO_HOME", Plist::s(dir)));
    }
    let log = log_path(home).display().to_string();
    vec![
        (
            "ProgramArguments",
            Plist::Array(argv.iter().map(Plist::s).collect()),
        ),
        ("ProcessType", Plist::s("Interactive")),
        ("EnvironmentVariables", Plist::dict(env)),
        ("StandardOutPath", Plist::s(log.clone())),
        ("StandardErrorPath", Plist::s(log)),
        (
            "WorkingDirectory",
            Plist::s(user_home.display().to_string()),
        ),
    ]
}

/// KeepAlive login agent.
pub fn build_plist(
    argv: &[String],
    home: &Path,
    user_home: &Path,
    freemicro_home_env: Option<&str>,
) -> Plist {
    let mut entries = vec![
        ("Label", Plist::s(LABEL)),
        ("RunAtLoad", Plist::Bool(true)),
        ("KeepAlive", Plist::Bool(true)),
        ("ThrottleInterval", Plist::Integer(THROTTLE_SECONDS.into())),
    ];
    entries.extend(common_entries(argv, home, user_home, freemicro_home_env));
    Plist::dict(entries)
}

/// Launch-on-connect agent: `LaunchEvents` `com.apple.iokit.matching` for
/// IOHIDDevice (VendorID/ProductID) and IOUSBHostDevice (idVendor/idProduct),
/// for 0x8360, 0x8297 and 0x8298.
pub fn build_onconnect_plist(
    argv: &[String],
    home: &Path,
    user_home: &Path,
    freemicro_home_env: Option<&str>,
) -> Plist {
    let vid = i64::from(VENDOR_ID);
    let matcher = |hid: bool, pid: u16| {
        let pid = i64::from(pid);
        if hid {
            Plist::dict(vec![
                ("IOProviderClass", Plist::s("IOHIDDevice")),
                ("VendorID", Plist::Integer(vid)),
                ("ProductID", Plist::Integer(pid)),
            ])
        } else {
            Plist::dict(vec![
                ("IOProviderClass", Plist::s("IOUSBHostDevice")),
                ("idVendor", Plist::Integer(vid)),
                ("idProduct", Plist::Integer(pid)),
            ])
        }
    };
    let mut matches = vec![
        (
            "com.freemicro.codexmicro-hid".to_string(),
            matcher(true, PRODUCT_ID_CODEX_MICRO),
        ),
        (
            "com.freemicro.codexmicro-usb".to_string(),
            matcher(false, PRODUCT_ID_CODEX_MICRO),
        ),
    ];
    for pid in CREATOR_MICRO_2_PRODUCT_IDS {
        for (kind, hid) in [("hid", true), ("usb", false)] {
            matches.push((
                format!("com.freemicro.creatormicro2-{kind}-{pid:04x}"),
                matcher(hid, pid),
            ));
        }
    }
    let mut entries = vec![
        ("Label", Plist::s(ONCONNECT_LABEL)),
        (
            "LaunchEvents",
            Plist::dict(vec![("com.apple.iokit.matching", Plist::Dict(matches))]),
        ),
        ("RunAtLoad", Plist::Bool(false)),
        ("KeepAlive", Plist::Bool(false)),
    ];
    entries.extend(common_entries(argv, home, user_home, freemicro_home_env));
    Plist::dict(entries)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_value(value: &Plist, depth: usize, out: &mut String) {
    let pad = "\t".repeat(depth);
    match value {
        Plist::String(s) => out.push_str(&format!("{pad}<string>{}</string>\n", escape(s))),
        Plist::Integer(i) => out.push_str(&format!("{pad}<integer>{i}</integer>\n")),
        Plist::Bool(b) => out.push_str(&format!("{pad}<{b}/>\n")),
        Plist::Array(items) if items.is_empty() => out.push_str(&format!("{pad}<array/>\n")),
        Plist::Array(items) => {
            out.push_str(&format!("{pad}<array>\n"));
            for item in items {
                render_value(item, depth + 1, out);
            }
            out.push_str(&format!("{pad}</array>\n"));
        }
        Plist::Dict(entries) if entries.is_empty() => out.push_str(&format!("{pad}<dict/>\n")),
        Plist::Dict(entries) => {
            // Sorted keys, as Python's plistlib writes them.
            let mut sorted: Vec<&(String, Plist)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push_str(&format!("{pad}<dict>\n"));
            for (key, item) in sorted {
                out.push_str(&format!("{pad}\t<key>{}</key>\n", escape(key)));
                render_value(item, depth + 1, out);
            }
            out.push_str(&format!("{pad}</dict>\n"));
        }
    }
}

/// XML plist 1.0 document.
pub fn render_plist(plist: &Plist) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    render_value(plist, 0, &mut out);
    out.push_str("</plist>\n");
    out
}

/// The `launchctl` calls, injectable.
pub trait Launchctl {
    /// Run `launchctl <args>`; (exit code, combined output).
    fn run(&mut self, args: &[&str]) -> (i32, String);
}

/// The real `/bin/launchctl`. Never used by tests.
pub struct SystemLaunchctl;

impl Launchctl for SystemLaunchctl {
    fn run(&mut self, args: &[&str]) -> (i32, String) {
        match std::process::Command::new("launchctl").args(args).output() {
            Ok(out) => {
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                (out.status.code().unwrap_or(-1), text.trim().to_string())
            }
            Err(err) => (-1, err.to_string()),
        }
    }
}

fn gui_domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// `gui/<uid>/<label>`.
pub fn service_target(label: &str) -> String {
    format!("{}/{label}", gui_domain())
}

pub fn is_installed(user_home: &Path, label: &str) -> bool {
    plist_path(user_home, label).exists()
}

fn launcher_name(label: &str) -> &'static str {
    if label == ONCONNECT_LABEL {
        "on-connect agent"
    } else {
        "login daemon"
    }
}

fn bootout(lc: &mut dyn Launchctl, user_home: &Path, label: &str) -> (i32, String) {
    let (code, out) = lc.run(&["bootout", &service_target(label)]);
    if code == 0 || out.contains("No such process") || out.to_lowercase().contains("not find") {
        return (0, out);
    }
    let plist = plist_path(user_home, label).display().to_string();
    let (legacy, legacy_out) = lc.run(&["unload", "-w", &plist]);
    if legacy == 0 {
        return (0, legacy_out);
    }
    (code, out)
}

/// Write the plist and bootstrap it. Refuses when the other label is
/// installed. Only the login daemon is kickstarted: the on-connect agent
/// waits for its device event.
pub fn install(
    lc: &mut dyn Launchctl,
    user_home: &Path,
    label: &str,
    plist: &Plist,
) -> Result<(), String> {
    let other = if label == ONCONNECT_LABEL {
        LABEL
    } else {
        ONCONNECT_LABEL
    };
    if is_installed(user_home, other) {
        let (this_name, other_name) = (launcher_name(label), launcher_name(other));
        return Err(format!(
            "the {other_name} is already installed, and it and the {this_name}\n  would both try to drive the pad. Pick one - remove the other first:\n    freemicro-rs daemon uninstall\n  then install the {this_name} again."
        ));
    }
    let path = plist_path(user_home, label);
    std::fs::create_dir_all(agents_dir(user_home)).map_err(|e| e.to_string())?;
    if path.exists() {
        // launchd caches the plist at bootstrap: bootout before rewriting.
        bootout(lc, user_home, label);
    }
    std::fs::write(&path, render_plist(plist)).map_err(|e| e.to_string())?;
    let path_text = path.display().to_string();
    let (code, out) = lc.run(&["bootstrap", &gui_domain(), &path_text]);
    if code != 0 && !out.to_lowercase().contains("already bootstrapped") {
        let (legacy, legacy_out) = lc.run(&["load", "-w", &path_text]);
        if legacy != 0 {
            return Err(if out.is_empty() { legacy_out } else { out });
        }
    }
    let target = service_target(label);
    lc.run(&["enable", &target]);
    if label == LABEL {
        lc.run(&["kickstart", "-k", &target]);
    }
    Ok(())
}

/// Bootout and delete. `Ok(true)` if it was installed.
pub fn uninstall(lc: &mut dyn Launchctl, user_home: &Path, label: &str) -> Result<bool, String> {
    let path = plist_path(user_home, label);
    let existed = path.exists();
    let (_, out) = bootout(lc, user_home, label);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not delete {}: {e}", path.display())),
    }
    if status(lc, user_home, label)["loaded"] == json!(true) {
        return Err(format!("launchd still has {label} loaded: {out}"));
    }
    Ok(existed)
}

/// `bootout` the login daemon; true if it was installed.
pub fn pause(lc: &mut dyn Launchctl, user_home: &Path) -> bool {
    if !is_installed(user_home, LABEL) {
        return false;
    }
    bootout(lc, user_home, LABEL);
    true
}

/// `bootstrap` + `kickstart -k`.
pub fn resume(lc: &mut dyn Launchctl, user_home: &Path) {
    let plist = plist_path(user_home, LABEL).display().to_string();
    lc.run(&["bootstrap", &gui_domain(), &plist]);
    lc.run(&["kickstart", "-k", &service_target(LABEL)]);
}

/// Installed / loaded / pid / last exit, from `launchctl print`.
pub fn status(lc: &mut dyn Launchctl, user_home: &Path, label: &str) -> Value {
    let (code, out) = lc.run(&["print", &service_target(label)]);
    let mut pid = Value::Null;
    let mut last_exit = Value::Null;
    if code == 0 {
        for line in out.lines().map(str::trim) {
            if let Some(v) = line.strip_prefix("pid = ") {
                if let Ok(n) = v.trim().parse::<i64>() {
                    pid = json!(n);
                }
            } else if let Some(v) = line.strip_prefix("last exit code = ") {
                last_exit = v.trim().parse::<i64>().map_or(Value::Null, |n| json!(n));
            }
        }
    }
    json!({
        "label": label,
        "plist": plist_path(user_home, label).display().to_string(),
        "installed": is_installed(user_home, label),
        "loaded": code == 0,
        "pid": pid,
        "last_exit": last_exit,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Records calls; answers from a per-verb table (default: 0, "").
    #[derive(Default)]
    pub(crate) struct FakeLaunchctl {
        pub calls: Vec<String>,
        pub replies: Vec<(&'static str, i32, &'static str)>,
    }

    impl Launchctl for FakeLaunchctl {
        fn run(&mut self, args: &[&str]) -> (i32, String) {
            self.calls.push(args.join(" "));
            self.replies
                .iter()
                .find(|(verb, ..)| *verb == args[0])
                .map_or((0, String::new()), |(_, c, o)| (*c, o.to_string()))
        }
    }

    fn verbs(lc: &FakeLaunchctl) -> Vec<&str> {
        lc.calls
            .iter()
            .map(|c| c.split(' ').next().unwrap())
            .collect()
    }

    #[test]
    fn lock_is_exclusive_and_names_its_holder() {
        let home = TempDir::new();
        let path = lock_path(home.path());
        assert!(lock_holder(&path).is_none());
        let mut first = PadLock::new(path.clone(), "daemon");
        assert!(first.acquire(100.0));
        assert!(!PadLock::new(path.clone(), "run").acquire(101.0));
        let holder = lock_holder(&path).unwrap();
        assert_eq!(holder["role"], "daemon");
        assert_eq!(holder["pid"], json!(std::process::id()));
        assert!(describe_holder(&holder, 142.0).starts_with("the background daemon (pid "));
        assert!(describe_holder(&holder, 142.0).ends_with(", up 42s)"));
        first.release();
        assert!(lock_holder(&path).is_none());
        // A stale file with no holder does not wedge the pad.
        assert!(PadLock::new(path, "run").acquire(200.0));
    }

    #[test]
    fn describe_holder_without_details() {
        assert_eq!(
            describe_holder(&json!({}), 0.0),
            "another FreeMicro process"
        );
    }

    #[test]
    fn rotate_log_caps_a_runaway_log_and_leaves_small_ones() {
        let home = TempDir::new();
        let path = home.path().join("daemon.log");
        assert!(!rotate_log(&path, 100));
        std::fs::write(&path, "short\n").unwrap();
        assert!(!rotate_log(&path, 100));
        let body: String = (0..1000).map(|i| format!("line {i:03}\n")).collect();
        std::fs::write(&path, &body).unwrap();
        assert!(rotate_log(&path, 4096));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("--- log trimmed to the most recent 2KB ---\n"));
        assert!(text.ends_with("line 999\n"));
        assert!(text.lines().skip(1).all(|l| l.starts_with("line ")));
    }

    #[test]
    fn plist_starts_at_login_restarts_and_carries_a_relocated_home() {
        let argv = daemon_argv(Path::new("/opt/bin/freemicro-rs"));
        let plist = build_plist(&argv, Path::new("/h/.fm"), Path::new("/h"), Some("/h/.fm"));
        assert_eq!(plist.get("RunAtLoad"), Some(&Plist::Bool(true)));
        assert_eq!(plist.get("KeepAlive"), Some(&Plist::Bool(true)));
        assert_eq!(
            plist.get("StandardOutPath"),
            Some(&Plist::s("/h/.fm/logs/daemon.log"))
        );
        let env = plist.get("EnvironmentVariables").unwrap();
        assert_eq!(env.get("FREEMICRO_HOME"), Some(&Plist::s("/h/.fm")));
        let xml = render_plist(&plist);
        assert!(xml.contains("<key>ProgramArguments</key>\n\t<array>\n\t\t<string>/opt/bin/freemicro-rs</string>\n\t\t<string>daemon</string>"));
        assert!(xml.contains("<key>ThrottleInterval</key>\n\t<integer>10</integer>"));
        assert!(build_plist(&argv, Path::new("/h"), Path::new("/h"), None)
            .get("EnvironmentVariables")
            .unwrap()
            .get("FREEMICRO_HOME")
            .is_none());
    }

    #[test]
    fn onconnect_plist_matches_every_pad_over_hid_and_usb() {
        let argv = daemon_argv(Path::new("/bin/x"));
        let plist = build_onconnect_plist(&argv, Path::new("/h"), Path::new("/h"), None);
        assert_eq!(plist.get("RunAtLoad"), Some(&Plist::Bool(false)));
        assert_eq!(plist.get("KeepAlive"), Some(&Plist::Bool(false)));
        let matching = plist
            .get("LaunchEvents")
            .and_then(|e| e.get("com.apple.iokit.matching"))
            .unwrap();
        let Plist::Dict(entries) = matching else {
            panic!()
        };
        assert_eq!(entries.len(), 6);
        let usb = matching
            .get("com.freemicro.creatormicro2-usb-8298")
            .unwrap();
        assert_eq!(usb.get("idVendor"), Some(&Plist::Integer(0x303A)));
        assert_eq!(usb.get("idProduct"), Some(&Plist::Integer(0x8298)));
        let hid = matching.get("com.freemicro.codexmicro-hid").unwrap();
        assert_eq!(hid.get("ProductID"), Some(&Plist::Integer(0x8360)));
    }

    #[test]
    fn render_plist_escapes_text() {
        assert!(render_plist(&Plist::s("a&b<c>")).contains("<string>a&amp;b&lt;c&gt;</string>"));
    }

    #[test]
    fn install_writes_bootstraps_and_kickstarts_only_the_login_daemon() {
        let user = TempDir::new();
        let plist = Plist::dict(vec![("Label", Plist::s(LABEL))]);
        let mut lc = FakeLaunchctl::default();
        install(&mut lc, user.path(), LABEL, &plist).unwrap();
        assert!(is_installed(user.path(), LABEL));
        assert_eq!(verbs(&lc), ["bootstrap", "enable", "kickstart"]);
        // A reinstall boots the old job out first.
        let mut lc = FakeLaunchctl::default();
        install(&mut lc, user.path(), LABEL, &plist).unwrap();
        assert_eq!(verbs(&lc)[0], "bootout");

        let other = TempDir::new();
        let mut lc = FakeLaunchctl::default();
        install(&mut lc, other.path(), ONCONNECT_LABEL, &plist).unwrap();
        assert_eq!(verbs(&lc), ["bootstrap", "enable"]);
    }

    #[test]
    fn install_refuses_while_the_other_launcher_is_installed() {
        let user = TempDir::new();
        std::fs::create_dir_all(agents_dir(user.path())).unwrap();
        std::fs::write(plist_path(user.path(), ONCONNECT_LABEL), "").unwrap();
        let mut lc = FakeLaunchctl::default();
        let err = install(&mut lc, user.path(), LABEL, &Plist::Bool(true)).unwrap_err();
        assert!(err.contains("on-connect agent is already installed"));
        assert!(lc.calls.is_empty());
    }

    #[test]
    fn install_falls_back_to_load_and_reports_a_double_failure() {
        let user = TempDir::new();
        let mut lc = FakeLaunchctl {
            replies: vec![("bootstrap", 5, "Bootstrap failed: 5"), ("load", 1, "nope")],
            ..Default::default()
        };
        let err = install(&mut lc, user.path(), LABEL, &Plist::Bool(true)).unwrap_err();
        assert_eq!(err, "Bootstrap failed: 5");
    }

    #[test]
    fn uninstall_removes_and_confirms_the_job_is_gone() {
        let user = TempDir::new();
        std::fs::create_dir_all(agents_dir(user.path())).unwrap();
        std::fs::write(plist_path(user.path(), LABEL), "").unwrap();
        let mut lc = FakeLaunchctl {
            replies: vec![("print", 113, "Could not find service")],
            ..Default::default()
        };
        assert_eq!(uninstall(&mut lc, user.path(), LABEL), Ok(true));
        assert!(!is_installed(user.path(), LABEL));
        let mut still = FakeLaunchctl::default();
        assert!(uninstall(&mut still, user.path(), LABEL).is_err());
    }

    #[test]
    fn pause_only_boots_out_an_installed_daemon() {
        let user = TempDir::new();
        let mut lc = FakeLaunchctl::default();
        assert!(!pause(&mut lc, user.path()));
        assert!(lc.calls.is_empty());
        std::fs::create_dir_all(agents_dir(user.path())).unwrap();
        std::fs::write(plist_path(user.path(), LABEL), "").unwrap();
        assert!(pause(&mut lc, user.path()));
        resume(&mut lc, user.path());
        assert_eq!(verbs(&lc), ["bootout", "bootstrap", "kickstart"]);
    }

    #[test]
    fn status_parses_launchctl_print() {
        let user = TempDir::new();
        let mut lc = FakeLaunchctl {
            replies: vec![(
                "print",
                0,
                "state = running\n\tpid = 4242\n\tlast exit code = 0",
            )],
            ..Default::default()
        };
        let state = status(&mut lc, user.path(), LABEL);
        assert_eq!(state["loaded"], true);
        assert_eq!(state["pid"], 4242);
        assert_eq!(state["last_exit"], 0);
        assert_eq!(state["installed"], false);
    }
}
