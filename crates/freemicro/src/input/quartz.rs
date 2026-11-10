//! CGEvent keystrokes, text and mouse: hand-written CoreGraphics FFI, plus
//! the osascript fallback backend. Port of `input/quartz.py` and the
//! `AppleScriptBackend` / `CGEventBackend` classes of `input/actions.py`.
//!
//! A `hold` presses real modifier keys. Every key pressed is registered, so
//! [`release_all`] can send the key-ups on every exit path (signal, exit,
//! `Bridge::close`), else macOS keeps Ctrl/Cmd "down" after we die.

use std::ffi::c_void;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::actions::{ActionError, Backend};
use super::keys::{
    applescript_for, applescript_for_text, cgevent_spec, escape_applescript, Modifier,
};

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;
pub const FLAG_SECONDARY_FN: u64 = 0x0080_0000;

const MOUSE_MOVED: u32 = 5;
const MOUSE_DELTA_X: u32 = 96;
const MOUSE_DELTA_Y: u32 = 97;
const MOUSE_CLICK_STATE: u32 = 1;
const TAP_HID: u32 = 0;
const SOURCE_HID_SYSTEM: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

type Ref = *mut c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventSourceCreate(state: u32) -> Ref;
    fn CGEventCreateKeyboardEvent(source: Ref, keycode: u16, down: bool) -> Ref;
    fn CGEventSetFlags(event: Ref, flags: u64);
    fn CGEventKeyboardSetUnicodeString(event: Ref, length: usize, text: *const u16);
    fn CGEventPost(tap: u32, event: Ref);
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventGetLocation(event: Ref) -> CGPoint;
    fn CGEventCreateMouseEvent(source: Ref, kind: u32, at: CGPoint, button: u32) -> Ref;
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
}

fn flag_of(modifier: Modifier) -> u64 {
    match modifier {
        Modifier::Shift => FLAG_SHIFT,
        Modifier::Control => FLAG_CONTROL,
        Modifier::Option => FLAG_OPTION,
        Modifier::Command => FLAG_COMMAND,
        Modifier::Fn => FLAG_SECONDARY_FN,
    }
}

/// Keycodes of the modifier keys themselves (a held chord needs their own
/// events; flags alone never say "Cmd is down").
fn modifier_key_code(modifier: Modifier) -> u16 {
    match modifier {
        Modifier::Command => 55,
        Modifier::Shift => 56,
        Modifier::Option => 58,
        Modifier::Control => 59,
        Modifier::Fn => 63,
    }
}

pub fn modifier_flags(modifiers: &[Modifier]) -> u64 {
    modifiers.iter().fold(0, |acc, m| acc | flag_of(*m))
}

/// True when CoreGraphics loaded and the process may post events. The
/// framework is linked, so this is true on macOS, as Python's load check.
pub fn is_available() -> bool {
    cfg!(target_os = "macos")
}

/// Post `event` and release it and `source`.
unsafe fn post(event: Ref, source: Ref) {
    if !event.is_null() {
        CGEventPost(TAP_HID, event);
        CFRelease(event);
    }
    if !source.is_null() {
        CFRelease(source);
    }
}

/// One key event, its flags set explicitly (0 included: a modifier key-up
/// without `CGEventSetFlags` keeps its own flag and the modifier sticks).
pub fn key_event(keycode: u16, down: bool, flags: u64) {
    unsafe {
        let source = CGEventSourceCreate(SOURCE_HID_SYSTEM);
        let event = CGEventCreateKeyboardEvent(source, keycode, down);
        if !event.is_null() {
            CGEventSetFlags(event, flags);
        }
        post(event, source);
    }
}

pub fn tap_key(keycode: u16, flags: u64) {
    key_event(keycode, true, flags);
    key_event(keycode, false, flags);
}

/// Unicode text via `CGEventKeyboardSetUnicodeString` in chunks.
pub fn type_text(text: &str) {
    let units: Vec<u16> = text.encode_utf16().collect();
    for chunk in units.chunks(16) {
        for down in [true, false] {
            unsafe {
                let source = CGEventSourceCreate(SOURCE_HID_SYSTEM);
                let event = CGEventCreateKeyboardEvent(source, 0, down);
                if !event.is_null() {
                    CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                }
                post(event, source);
            }
        }
    }
}

/// Keycodes held by [`hold_chord`], in press order.
static HELD: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Press or release `keycode` with its modifiers held as real keys.
/// Down: modifiers first with accumulating flags, then the key. Up: the key,
/// then the modifiers in reverse, each with the flags still held after it.
pub fn hold_chord(keycode: u16, modifiers: &[Modifier], down: bool) {
    let mods: Vec<(u16, u64)> = modifiers
        .iter()
        .map(|m| (modifier_key_code(*m), flag_of(*m)))
        .collect();
    let flags = modifier_flags(modifiers);
    if down {
        install_release_guard();
        if let Ok(mut held) = HELD.lock() {
            held.extend(mods.iter().map(|(code, _)| *code));
            held.push(keycode);
        }
        let mut accumulated = 0;
        for (code, flag) in &mods {
            accumulated |= flag;
            key_event(*code, true, accumulated);
        }
        key_event(keycode, true, flags);
    } else {
        key_event(keycode, false, flags);
        let mut remaining = flags;
        for (code, flag) in mods.iter().rev() {
            remaining &= !flag;
            key_event(*code, false, remaining);
        }
        if let Ok(mut held) = HELD.lock() {
            for code in mods.iter().map(|(c, _)| *c).chain([keycode]) {
                if let Some(at) = held.iter().position(|c| *c == code) {
                    held.remove(at);
                }
            }
        }
    }
}

/// Keycodes this process holds down right now.
pub fn held_keys() -> Vec<u16> {
    HELD.lock().map(|h| h.clone()).unwrap_or_default()
}

/// Release every held key; count released. Never panics; safe to repeat.
pub fn release_all() -> usize {
    // try_lock: this also runs in a signal handler, which must not block.
    let codes = match HELD.try_lock() {
        Ok(mut held) => std::mem::take(&mut *held),
        Err(_) => return 0,
    };
    for code in codes.iter().rev() {
        key_event(*code, false, 0);
    }
    codes.len()
}

const GUARDED: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
static PREVIOUS: Mutex<Option<[libc::sigaction; 3]>> = Mutex::new(None);

extern "C" fn release_at_exit() {
    release_all();
}

extern "C" fn release_and_chain(signum: libc::c_int) {
    release_all();
    let index = GUARDED.iter().position(|s| *s == signum);
    let previous = PREVIOUS
        .try_lock()
        .ok()
        .and_then(|p| *p)
        .zip(index)
        .map(|(p, i)| p[i]);
    unsafe {
        match previous.map(|p| (p.sa_sigaction, p.sa_flags)) {
            Some((libc::SIG_IGN, _)) => {}
            Some((handler, flags)) if handler != libc::SIG_DFL => {
                if flags & libc::SA_SIGINFO != 0 {
                    type Info = extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void);
                    let chained: Info = std::mem::transmute(handler);
                    chained(signum, std::ptr::null_mut(), std::ptr::null_mut());
                } else {
                    let chained: extern "C" fn(libc::c_int) = std::mem::transmute(handler);
                    chained(signum);
                }
            }
            // SIG_DFL: restore it and re-raise, so the process dies as asked.
            _ => {
                libc::signal(signum, libc::SIG_DFL);
                libc::raise(signum);
            }
        }
    }
}

/// SIGTERM / SIGINT / SIGHUP handlers that release held keys, then chain.
/// Armed lazily, the first time a key is held; idempotent.
pub fn install_release_guard() {
    let Ok(mut previous) = PREVIOUS.lock() else {
        return;
    };
    if previous.is_some() {
        return;
    }
    unsafe {
        let mut saved: [libc::sigaction; 3] = std::mem::zeroed();
        for (slot, signum) in saved.iter_mut().zip(GUARDED) {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = release_and_chain as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signum, &action, slot);
        }
        *previous = Some(saved);
        libc::atexit(release_at_exit);
    }
}

pub fn cursor_position() -> (f64, f64) {
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return (0.0, 0.0);
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        (point.x, point.y)
    }
}

/// Move the pointer; a relative move also carries its delta, which apps
/// that read motion (pointer lock, 3D views) need.
pub fn mouse_move(x: f64, y: f64, relative: bool) {
    let at = if relative {
        let (cx, cy) = cursor_position();
        CGPoint {
            x: cx + x,
            y: cy + y,
        }
    } else {
        CGPoint { x, y }
    };
    unsafe {
        let source = CGEventSourceCreate(SOURCE_HID_SYSTEM);
        let event = CGEventCreateMouseEvent(source, MOUSE_MOVED, at, 0);
        if !event.is_null() && relative {
            CGEventSetIntegerValueField(event, MOUSE_DELTA_X, x as i64);
            CGEventSetIntegerValueField(event, MOUSE_DELTA_Y, y as i64);
        }
        post(event, source);
    }
}

fn mouse_events(button: &str) -> Option<(u32, u32, u32)> {
    match button {
        "left" => Some((1, 2, 0)),
        "right" => Some((3, 4, 1)),
        "middle" => Some((25, 26, 2)),
        _ => None,
    }
}

/// Click at the pointer. An unknown button does nothing (the backend
/// checks it first).
pub fn mouse_click(button: &str, count: u32) {
    let Some((down, up, index)) = mouse_events(button) else {
        return;
    };
    let (x, y) = cursor_position();
    for click in 1..=count.max(1) {
        for kind in [down, up] {
            unsafe {
                let source = CGEventSourceCreate(SOURCE_HID_SYSTEM);
                let event = CGEventCreateMouseEvent(source, kind, CGPoint { x, y }, index);
                if !event.is_null() {
                    // Click state lets the OS see a double click.
                    CGEventSetIntegerValueField(event, MOUSE_CLICK_STATE, i64::from(click));
                }
                post(event, source);
            }
        }
    }
}

/// Run `command`, kill it after `timeout`; `Err` carries stderr or the exit.
pub(crate) fn run_bounded(mut command: Command, timeout: Duration) -> Result<(), ActionError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ActionError(e.to_string()))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ActionError(format!(
                    "timed out after {} seconds",
                    timeout.as_secs_f64()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(ActionError(e.to_string())),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let stderr = stderr.trim();
    Err(ActionError(if stderr.is_empty() {
        format!("exit {}", status.code().unwrap_or(-1))
    } else {
        stderr.to_string()
    }))
}

/// Runs `osascript` with a timeout; keys via System Events.
#[derive(Debug)]
pub struct AppleScriptBackend {
    pub timeout_seconds: f64,
}

impl AppleScriptBackend {
    fn osascript(&self, script: &str) -> Result<(), ActionError> {
        let mut command = Command::new("osascript");
        command.args(["-e", script]);
        run_bounded(
            command,
            Duration::from_secs_f64(self.timeout_seconds.max(0.0)),
        )
        .map_err(|ActionError(mut message)| {
            if message.contains("not allowed") || message.contains("1002") {
                message += " - grant Accessibility to this terminal in System Settings → \
                                Privacy & Security → Accessibility";
            }
            ActionError(message)
        })
    }
}

fn cannot(kind: &str) -> ActionError {
    ActionError(format!("backend cannot perform '{kind}'"))
}

impl Backend for AppleScriptBackend {
    fn description(&self) -> &str {
        "AppleScript (System Events)"
    }
    fn type_text(&mut self, text: &str) -> Result<(), ActionError> {
        self.osascript(&applescript_for_text(text))
    }
    fn press_key(&mut self, combo: &str) -> Result<(), ActionError> {
        let script = applescript_for(combo).map_err(|e| ActionError(e.0))?;
        self.osascript(&script)
    }
    fn hold_key(&mut self, _combo: &str, _down: bool) -> Result<(), ActionError> {
        Err(cannot("hold"))
    }
    fn run_shell(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        wait: bool,
    ) -> Result<(), ActionError> {
        let mut shell = Command::new("/bin/sh");
        shell.args(["-c", command]);
        if let Some(cwd) = cwd {
            shell.current_dir(cwd);
        }
        if wait {
            return run_bounded(shell, Duration::from_secs(120));
        }
        // Fire and forget: a slow command must not stall key handling.
        let mut child = shell
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ActionError(e.to_string()))?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    fn run_applescript(&mut self, script: &str) -> Result<(), ActionError> {
        self.osascript(script)
    }
    fn activate_app(&mut self, name: &str, cycle: bool) -> Result<(), ActionError> {
        let safe = escape_applescript(name);
        if !cycle {
            return self.osascript(&format!("tell application \"{safe}\" to activate"));
        }
        // Already frontmost: Cmd-` cycles its windows.
        self.osascript(&format!(
            "tell application \"System Events\" to set frontApp to name of first \
             application process whose frontmost is true\n\
             if frontApp is \"{safe}\" then\n  \
             tell application \"System Events\" to keystroke \"`\" using command down\n\
             else\n  tell application \"{safe}\" to activate\nend if"
        ))
    }
    fn move_mouse(&mut self, _x: f64, _y: f64, _relative: bool) -> Result<(), ActionError> {
        Err(cannot("mouse"))
    }
    fn click_mouse(&mut self, _button: &str, _count: u32) -> Result<(), ActionError> {
        Err(cannot("mouse"))
    }
    fn release_held_keys(&mut self) -> usize {
        0
    }
    fn settle(&mut self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(seconds));
        }
    }
}

/// CGEvent for keys, text and mouse; osascript for scripts and apps.
#[derive(Debug)]
pub struct CGEventBackend {
    pub script: AppleScriptBackend,
}

impl Backend for CGEventBackend {
    fn description(&self) -> &str {
        "CGEvent (frontmost app, supports fn and hold)"
    }
    fn type_text(&mut self, text: &str) -> Result<(), ActionError> {
        type_text(text);
        Ok(())
    }
    fn press_key(&mut self, combo: &str) -> Result<(), ActionError> {
        let (code, modifiers) = cgevent_spec(combo).map_err(|e| ActionError(e.0))?;
        tap_key(code, modifier_flags(&modifiers));
        Ok(())
    }
    fn hold_key(&mut self, combo: &str, down: bool) -> Result<(), ActionError> {
        let (code, modifiers) = cgevent_spec(combo).map_err(|e| ActionError(e.0))?;
        hold_chord(code, &modifiers, down);
        Ok(())
    }
    fn run_shell(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        wait: bool,
    ) -> Result<(), ActionError> {
        self.script.run_shell(command, cwd, wait)
    }
    fn run_applescript(&mut self, script: &str) -> Result<(), ActionError> {
        self.script.run_applescript(script)
    }
    fn activate_app(&mut self, name: &str, cycle: bool) -> Result<(), ActionError> {
        self.script.activate_app(name, cycle)
    }
    fn move_mouse(&mut self, x: f64, y: f64, relative: bool) -> Result<(), ActionError> {
        mouse_move(x, y, relative);
        Ok(())
    }
    fn click_mouse(&mut self, button: &str, count: u32) -> Result<(), ActionError> {
        if mouse_events(button).is_none() {
            return Err(ActionError(format!(
                "unknown mouse button '{button}'; expected left, middle, right"
            )));
        }
        mouse_click(button, count);
        Ok(())
    }
    fn release_held_keys(&mut self) -> usize {
        release_all()
    }
    fn settle(&mut self, seconds: f64) {
        self.script.settle(seconds);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_bounded_kills_a_command_past_its_timeout() {
        let mut sleep = Command::new("sleep");
        sleep.arg("10");
        let started = Instant::now();
        let error = run_bounded(sleep, Duration::from_millis(100)).unwrap_err();
        assert!(error.0.starts_with("timed out"), "{}", error.0);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn modifier_flags_combine() {
        assert_eq!(modifier_flags(&[]), 0);
        assert_eq!(
            modifier_flags(&[Modifier::Command, Modifier::Control]),
            FLAG_COMMAND | FLAG_CONTROL
        );
        assert_eq!(modifier_flags(&[Modifier::Fn]), FLAG_SECONDARY_FN);
    }

    #[test]
    fn applescript_backend_refuses_what_it_cannot_express() {
        let mut backend = AppleScriptBackend {
            timeout_seconds: 1.0,
        };
        assert!(backend.hold_key("f5", true).is_err());
        assert!(backend.move_mouse(1.0, 1.0, true).is_err());
        assert!(backend.click_mouse("left", 1).is_err());
        assert!(backend.press_key("fn+f5").is_err());
    }
}
