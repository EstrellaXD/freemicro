//! The frontmost app (NSWorkspace via hand-written objc_msgSend FFI), the
//! profile watcher, and the Creator Micro 2 layer forwarder
//! (`host.focused_app`). Port of `input/frontmost.py`.
//!
//! NSWorkspace updates `frontmostApplication` from the main run loop, which
//! the pad-owner (main) thread pumps in `PadIo::poll`; call from there.

use std::ffi::{c_char, c_void, CStr};
use std::sync::OnceLock;

use serde_json::json;

use crate::device::{PadInfo, PadIo};

pub const METHOD_FOCUSED_APP: &str = "host.focused_app";
/// `{"l": <1-based layer>}`; firmware 0.7.0 and later.
pub const METHOD_SET_LAYER: &str = "kb.pl.set";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontApp {
    /// `localizedName`, e.g. `"Ghostty"`.
    pub name: String,
    /// `bundleIdentifier`, e.g. `"com.mitchellh.ghostty"`.
    pub bundle_id: String,
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// `objc_msgSend` cast to the one prototype used here: no arguments, a
/// pointer result (an object or a `const char *`).
unsafe fn send(receiver: *mut c_void, selector: &CStr) -> *mut c_void {
    if receiver.is_null() {
        return std::ptr::null_mut();
    }
    type Msg = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    let msg: Msg = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    msg(receiver, sel_registerName(selector.as_ptr()))
}

unsafe fn ns_string(object: *mut c_void) -> String {
    let utf8 = send(object, c"UTF8String") as *const c_char;
    if utf8.is_null() {
        return String::new();
    }
    CStr::from_ptr(utf8).to_string_lossy().into_owned()
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        text: *const c_char,
        enc: u32,
    ) -> *const c_void;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
}

/// True while the Mac's screen is locked (`CGSSessionScreenIsLocked`).
pub fn screen_locked() -> bool {
    // SAFETY: plain CoreFoundation calls; every created object is released.
    unsafe {
        let session = CGSessionCopyCurrentDictionary();
        if session.is_null() {
            return false;
        }
        let key = CFStringCreateWithCString(
            std::ptr::null(),
            c"CGSSessionScreenIsLocked".as_ptr(),
            0x0800_0100, // kCFStringEncodingUTF8
        );
        let value = CFDictionaryGetValue(session, key);
        let locked = !value.is_null() && CFBooleanGetValue(value) != 0;
        CFRelease(key);
        CFRelease(session);
        locked
    }
}

/// `NSWorkspace.sharedWorkspace.frontmostApplication`. `None` when nothing
/// is frontmost or AppKit cannot load. Call from the main thread.
pub fn detect_frontmost_app() -> Option<FrontApp> {
    // Loading AppKit is what makes the NSWorkspace class exist.
    static APPKIT: OnceLock<bool> = OnceLock::new();
    let loaded = *APPKIT.get_or_init(|| unsafe {
        !libc::dlopen(
            c"/System/Library/Frameworks/AppKit.framework/AppKit".as_ptr(),
            libc::RTLD_LAZY,
        )
        .is_null()
    });
    if !loaded {
        return None;
    }
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let class = objc_getClass(c"NSWorkspace".as_ptr());
        let app = send(send(class, c"sharedWorkspace"), c"frontmostApplication");
        let name = ns_string(send(app, c"localizedName"));
        let bundle_id = ns_string(send(app, c"bundleIdentifier"));
        objc_autoreleasePoolPop(pool);
        (!name.is_empty()).then_some(FrontApp { name, bundle_id })
    }
}

type Provider = Box<dyn FnMut() -> Option<FrontApp>>;

/// Cached frontmost app name for per-app profiles, re-read at most every
/// `interval` seconds.
pub struct FrontmostWatcher {
    pub interval: f64,
    provider: Provider,
    current: Option<String>,
    last_at: Option<f64>,
}

impl FrontmostWatcher {
    pub fn new(interval: f64, provider: Provider) -> FrontmostWatcher {
        FrontmostWatcher {
            interval,
            provider,
            current: None,
            last_at: None,
        }
    }
    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }
    pub fn poll(&mut self, now: f64) -> Option<String> {
        if self
            .last_at
            .is_some_and(|at| now - at < self.interval.max(0.0))
        {
            return self.current.clone();
        }
        self.last_at = Some(now);
        self.current = (self.provider)()
            .map(|app| app.name)
            .filter(|n| !n.is_empty());
        self.current.clone()
    }
}

/// Tell a Creator Micro 2 the frontmost app on change (1 s poll) and once
/// after every attach. A no-op for other pads.
const SAFARI: &str = "com.apple.Safari";

/// Safari's front-tab URL. macOS asks once to let this binary control Safari.
pub fn safari_url() -> Option<String> {
    let script = "with timeout of 1 second\ntell application \"Safari\" to get URL of current tab of front window\nend timeout";
    let out = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The entry of `sites` that `url`'s host is, or is a subdomain of.
pub fn site_for<'a>(url: &str, sites: &'a [String]) -> Option<&'a str> {
    let host = url
        .split("://")
        .nth(1)?
        .split(['/', ':', '?', '#'])
        .next()?;
    sites
        .iter()
        .map(String::as_str)
        .find(|site| host == *site || host.ends_with(&format!(".{site}")))
}

pub struct PadFocusForwarder {
    pub interval: f64,
    provider: Provider,
    /// Hosts that get their own pad layer while Safari shows them.
    pub sites: Vec<String>,
    /// Safari's front-tab URL; read only when `sites` is set.
    pub url: Box<dyn FnMut() -> Option<String>>,
    /// Process → pad layer (1-based) when the pad takes `kb.pl.set`. `None`:
    /// tell the firmware the app and let its `linkedApps` pick the layer.
    pub layers: Option<Vec<(String, u64)>>,
    /// The layer last set with `kb.pl.set`.
    pub layer: Option<u64>,
    attached: bool,
    sent: Option<FrontApp>,
    last_at: Option<f64>,
}

impl PadFocusForwarder {
    pub fn new(interval: f64, provider: Provider) -> PadFocusForwarder {
        PadFocusForwarder {
            interval,
            provider,
            sites: Vec::new(),
            url: Box::new(safari_url),
            layers: None,
            layer: None,
            attached: false,
            sent: None,
            last_at: None,
        }
    }
    /// `Some(info)` on connect, `None` on drop; forgets what was sent. The
    /// layer map belongs to one connection.
    pub fn attach(&mut self, info: Option<&PadInfo>) {
        self.attached = info.is_some_and(PadInfo::is_creator_micro_2);
        if info.is_none() {
            self.layers = None;
        }
        self.sent = None;
        self.layer = None;
        self.last_at = None;
    }
    /// If due and the front app changed: `kb.pl.set` its layer (nothing for
    /// an app without one: the pad stays, as the firmware itself does), or
    /// without a layer map `host.focused_app`. Returns the app acted on.
    pub fn poll(&mut self, io: &mut dyn PadIo, now: f64) -> Option<FrontApp> {
        if !self.attached || self.last_at.is_some_and(|at| now - at < self.interval) {
            return None;
        }
        self.last_at = Some(now);
        let mut app = (self.provider)()?;
        if app.bundle_id == SAFARI && !self.sites.is_empty() {
            // A site is its own "app" to the firmware: linkedApps match on
            // this text, and any other site falls back to the Safari layer.
            if let Some(site) =
                (self.url)().and_then(|u| site_for(&u, &self.sites).map(str::to_owned))
            {
                app.bundle_id = format!("{SAFARI}/{site}");
            }
        }
        if app.bundle_id.is_empty() || self.sent.as_ref() == Some(&app) {
            return None;
        }
        let layer = self.layers.as_ref().map(|map| {
            map.iter()
                .find(|(process, _)| *process == app.bundle_id)
                .map(|(_, layer)| *layer)
        });
        let message = match layer {
            None => json!({
                "m": METHOD_FOCUSED_APP,
                "p": {"appName": app.name, "process": app.bundle_id},
                "id": 1,
            }),
            Some(Some(l)) if self.layer != Some(l) => {
                json!({"m": METHOD_SET_LAYER, "p": {"l": l}, "id": 1})
            }
            Some(_) => {
                self.sent = Some(app.clone());
                return Some(app);
            }
        };
        // A dropped pad must not kill the loop; the next attach resends.
        io.send(&message).ok()?;
        if let Some(l) = layer.flatten() {
            self.layer = Some(l);
        }
        self.sent = Some(app.clone());
        Some(app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::fake::FakePad;
    use crate::device::{DeviceError, Transport};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn app(name: &str, bundle: &str) -> FrontApp {
        FrontApp {
            name: name.to_string(),
            bundle_id: bundle.to_string(),
        }
    }

    /// The provider's current answer and how many lookups it served.
    type Script = Rc<RefCell<(Option<FrontApp>, u32)>>;

    /// A provider whose answer the test changes; counts lookups.
    fn scripted(start: Option<FrontApp>) -> (Script, Provider) {
        let cell = Rc::new(RefCell::new((start, 0)));
        let inner = cell.clone();
        let provider: Provider = Box::new(move || {
            let mut s = inner.borrow_mut();
            s.1 += 1;
            s.0.clone()
        });
        (cell, provider)
    }

    #[test]
    fn watcher_rereads_at_most_once_per_interval() {
        let (cell, provider) = scripted(Some(app("Ghostty", "com.mitchellh.ghostty")));
        let mut watcher = FrontmostWatcher::new(0.3, provider);
        assert_eq!(watcher.poll(0.0).as_deref(), Some("Ghostty"));
        cell.borrow_mut().0 = Some(app("Mail", "com.apple.mail"));
        assert_eq!(watcher.poll(0.1).as_deref(), Some("Ghostty"));
        assert_eq!(watcher.poll(0.4).as_deref(), Some("Mail"));
        assert_eq!(cell.borrow().1, 2);
        cell.borrow_mut().0 = None;
        assert_eq!(watcher.poll(1.0), None);
        assert_eq!(watcher.current(), None);
    }

    #[test]
    fn forwarder_sends_on_change_and_once_per_attach() {
        let (cell, provider) = scripted(Some(app("Ghostty", "com.mitchellh.ghostty")));
        let mut fwd = PadFocusForwarder::new(1.0, provider);
        let mut pad = FakePad::creator_micro_2();
        assert_eq!(fwd.poll(&mut pad, 0.0), None, "not attached");
        fwd.attach(Some(&pad.info.clone()));
        assert!(fwd.poll(&mut pad, 0.0).is_some());
        assert_eq!(
            pad.sent,
            vec![json!({"m": "host.focused_app",
                        "p": {"appName": "Ghostty", "process": "com.mitchellh.ghostty"},
                        "id": 1})]
        );
        assert_eq!(fwd.poll(&mut pad, 1.5), None, "unchanged");
        cell.borrow_mut().0 = Some(app("Mail", "com.apple.mail"));
        assert_eq!(fwd.poll(&mut pad, 2.0), None, "throttled");
        assert!(fwd.poll(&mut pad, 2.6).is_some());
        // A reconnect resends the same app at once.
        fwd.attach(Some(&pad.info.clone()));
        assert!(fwd.poll(&mut pad, 2.7).is_some());
        assert_eq!(pad.sent.len(), 3);
        // No bundle id: nothing to match linkedApps on.
        cell.borrow_mut().0 = Some(app("Odd", ""));
        assert_eq!(fwd.poll(&mut pad, 9.0), None);
        // A failed send is not remembered as sent.
        cell.borrow_mut().0 = Some(app("Notes", "com.apple.Notes"));
        pad.fail = Some(DeviceError("gone".into()));
        assert_eq!(fwd.poll(&mut pad, 11.0), None);
        assert!(fwd.poll(&mut pad, 12.5).is_some());
    }

    #[test]
    fn site_for_matches_a_host_or_its_subdomains() {
        let sites = vec!["app.example.com".to_string(), "github.com".to_string()];
        for (url, expected) in [
            ("https://app.example.com/admin?x=1", Some("app.example.com")),
            ("https://gist.github.com/a", Some("github.com")),
            ("http://github.com:8080", Some("github.com")),
            ("https://notgithub.com/", None),
            ("https://example.org/github.com", None),
            ("favorites://", None),
        ] {
            assert_eq!(site_for(url, &sites), expected, "{url}");
        }
    }

    #[test]
    fn forwarder_names_a_configured_safari_site() {
        let (_cell, provider) = scripted(Some(app("Safari", "com.apple.Safari")));
        let mut fwd = PadFocusForwarder::new(1.0, provider);
        let url = Rc::new(RefCell::new("https://app.example.com/admin".to_string()));
        let tab = url.clone();
        fwd.url = Box::new(move || Some(tab.borrow().clone()));
        fwd.sites = vec!["app.example.com".into()];
        let mut pad = FakePad::creator_micro_2();
        fwd.attach(Some(&pad.info.clone()));
        fwd.poll(&mut pad, 0.0);
        *url.borrow_mut() = "https://example.org/".into();
        fwd.poll(&mut pad, 2.0);
        let processes: Vec<_> = pad.sent.iter().map(|m| m["p"]["process"].clone()).collect();
        assert_eq!(
            processes,
            [
                json!("com.apple.Safari/app.example.com"),
                json!("com.apple.Safari")
            ]
        );
    }

    #[test]
    fn with_a_layer_map_the_forwarder_sets_linked_layers_and_skips_the_rest() {
        let (cell, provider) = scripted(Some(app("Ghostty", "com.mitchellh.ghostty")));
        let mut fwd = PadFocusForwarder::new(1.0, provider);
        let mut pad = FakePad::creator_micro_2();
        fwd.attach(Some(&pad.info.clone()));
        fwd.layers = Some(vec![
            ("com.mitchellh.ghostty".into(), 1),
            ("com.apple.Safari".into(), 3),
        ]);
        fwd.poll(&mut pad, 0.0);
        cell.borrow_mut().0 = Some(app("Finder", "com.apple.finder")); // no layer: stay
        fwd.poll(&mut pad, 2.0);
        cell.borrow_mut().0 = Some(app("Safari", "com.apple.Safari"));
        fwd.poll(&mut pad, 4.0);
        assert_eq!(
            pad.sent,
            [
                json!({"m": "kb.pl.set", "p": {"l": 1}, "id": 1}),
                json!({"m": "kb.pl.set", "p": {"l": 3}, "id": 1}),
            ]
        );
        assert_eq!(fwd.layer, Some(3));
    }

    #[test]
    fn forwarder_ignores_a_codex_micro() {
        let (_cell, provider) = scripted(Some(app("Ghostty", "com.mitchellh.ghostty")));
        let mut fwd = PadFocusForwarder::new(1.0, provider);
        let mut pad = FakePad::with_info(PadInfo {
            transport: Transport::Usb,
            product_id: 0x8360,
        });
        fwd.attach(Some(&pad.info.clone()));
        assert_eq!(fwd.poll(&mut pad, 0.0), None);
        fwd.attach(None);
        assert_eq!(fwd.poll(&mut pad, 5.0), None);
        assert!(pad.sent.is_empty());
    }
}
