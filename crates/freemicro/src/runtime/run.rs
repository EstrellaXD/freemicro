//! The run loop behind `run` and `daemon run`: keys in, agent-state lights
//! out, surviving the pad coming and going. Port of `cli.py::_run_pipeline`
//! and `device/__init__.py::run_with_reconnect`.
//!
//! Runs on the main thread, which owns the only `PadIo`. Each pass: poll the
//! pad with timeout = min(next tick, bridge deadline, overlay deadline), feed
//! messages to smart actions then the bridge, run due bridge work, and on the
//! tick: lighting owner, Codex ownership refresh, overlay timeout, config
//! reload, focus forwarder, profile frontmost, state resolve + LED render.
//! Presence is re-checked once a second; a drop calls `on_disconnect`, then
//! the loop retries every 2 s while still ticking.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use super::daemon::{self, PadLock};
use super::leds::LedRenderer;
use super::owner::{
    coexist_advice, vendor_app_running, ActivityOverlay, LightingEvent, LightingOwner,
};
use crate::clock::{mono, wall};
use crate::config::Config;
use crate::device::{DeviceError, PadIo, Transport};
use crate::herdr::Herdr;
use crate::input::actions::{best_backend, ActionEnv, Backend, RecordingBackend};
use crate::input::bridge::{Bridge, Dispatch};
use crate::input::config::{self as padconfig, PadConfig};
use crate::input::frontmost::{
    detect_frontmost_app, screen_locked, FrontApp, FrontmostWatcher, PadFocusForwarder,
    METHOD_FOCUSED_APP, METHOD_SET_LAYER,
};
use crate::input::smartactions::{handle_smart_action, open_detached};
use crate::state::agentkeys::{default_codex_probe, AgentKeyOwnership};
use crate::state::{AgentState, StateStore};

pub const RETRY_INTERVAL: f64 = 2.0;
pub const PRESENCE_CHECK_SECONDS: f64 = 1.0;
pub const LOG_ROTATE_SECONDS: f64 = 300.0;
pub const CONFIG_CHECK_SECONDS: f64 = 1.0;
pub const FOCUS_POLL_SECONDS: f64 = 1.0;
const LIGHTING_PREFIX: &str = "  [lighting] ";

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// SIGINT / SIGTERM end the loop cleanly (bridge close, LED hand-back).
pub fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub home: PathBuf,
    pub dry_run: bool,
    /// Tick interval, seconds (Python `--interval`, default 1.0).
    pub interval: f64,
    /// Stop after this long; 0 = forever.
    pub seconds: f64,
    pub config: Option<PathBuf>,
    pub take_pad: bool,
    pub verbose: bool,
    /// `daemon run`: no Ctrl-C banner, log rotation.
    pub headless: bool,
}

/// Runs `open <args>` for a smart action.
pub type Opener = Box<dyn FnMut(&[String])>;

type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Polls the keymap file's mtime and size (at most once a second) and
/// reloads on change; a broken edit keeps the running config.
pub struct ConfigWatcher {
    pub path: PathBuf,
    stamp: Option<Stamp>,
    next_check: f64,
}

impl ConfigWatcher {
    pub fn new(path: PathBuf) -> ConfigWatcher {
        ConfigWatcher {
            stamp: stamp(&path),
            path,
            next_check: f64::NEG_INFINITY,
        }
    }
    /// `Some(Ok)` = adopt, `Some(Err)` = changed but would not load. A
    /// deleted file keeps the running config silently.
    pub fn poll(&mut self, now: f64) -> Option<Result<PadConfig, String>> {
        if now < self.next_check {
            return None;
        }
        self.next_check = now + CONFIG_CHECK_SECONDS;
        let current = stamp(&self.path);
        if current == self.stamp {
            return None;
        }
        self.stamp = current;
        current?;
        Some(padconfig::load(&self.path).map_err(|e| e.to_string()))
    }
}

/// What changed between two configs, for one log line.
fn describe_changes(old: &PadConfig, new: &PadConfig) -> String {
    fn count<K: Ord, V: PartialEq>(
        a: &std::collections::BTreeMap<K, V>,
        b: &std::collections::BTreeMap<K, V>,
    ) -> usize {
        let keys: std::collections::BTreeSet<&K> = a.keys().chain(b.keys()).collect();
        keys.into_iter().filter(|k| a.get(*k) != b.get(*k)).count()
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut parts = Vec::new();
    let bindings = count(&old.bindings, &new.bindings);
    if bindings > 0 {
        parts.push(format!("{bindings} binding{} changed", plural(bindings)));
    }
    let chords = count(&old.chords, &new.chords);
    if chords > 0 {
        parts.push(format!("{chords} chord{} changed", plural(chords)));
    }
    if old.chord_settle_ms != new.chord_settle_ms {
        parts.push(format!("chord settle {}ms", new.chord_settle_ms));
    }
    if old.lighting != new.lighting {
        if old.lighting.enabled != new.lighting.enabled {
            let on = if new.lighting.enabled { "on" } else { "off" };
            parts.push(format!("LEDs {on}"));
        } else {
            parts.push("lighting changed".into());
        }
    }
    if old.agent_keys != new.agent_keys {
        parts.push(format!("agent keys: {}", new.agent_keys.policy.as_str()));
    }
    if old.joystick != new.joystick {
        parts.push("joystick retuned".into());
    }
    if parts.is_empty() {
        "no functional changes".into()
    } else {
        parts.join(", ")
    }
}

/// A multi-line lighting notice under one prefix, continuation indented.
fn lighting_lines(message: &str) -> Vec<String> {
    let mut lines = message.lines();
    let first = format!("{LIGHTING_PREFIX}{}", lines.next().unwrap_or(""));
    std::iter::once(first)
        .chain(lines.map(|l| format!("{}{l}", " ".repeat(LIGHTING_PREFIX.len()))))
        .collect()
}

/// Everything the loop owns except the pad itself.
/// The layer that holds the `KV_OAI_` keys.
// ponytail: fixed at layer 1 (the pad's first); read it from keymap.json if a pad differs.
const VENDOR_LAYER: u64 = 1;
const LAYER_STATUS_ID: u32 = 902;
/// `<home>/runtime.json`: layer, link, lock, battery and dial, for the GUI.
pub const RUNTIME_FILENAME: &str = "runtime.json";

/// Reads a just-connected pad's app → layer map (see `layer_probe`).
pub type LayerProbe = Box<dyn FnMut(&mut dyn PadIo) -> Option<super::keymap::LayerMap>>;

pub struct Pipeline {
    pub opts: RunOptions,
    pub config: PadConfig,
    pub store: StateStore,
    pub bridge: Bridge,
    pub ownership: AgentKeyOwnership,
    pub overlay: ActivityOverlay,
    pub owner: LightingOwner,
    pub leds: Option<LedRenderer>,
    pub frontmost: FrontmostWatcher,
    pub forwarder: PadFocusForwarder,
    /// The pad's layer (1-based), from `device.status`; `None` until known.
    pub pad_layer: Option<u64>,
    /// When to ask the pad its layer: after connect and each focus forward.
    pub layer_check_at: Option<f64>,
    /// Reads whether the Mac is locked; the pad goes dark while it is.
    pub lock_probe: Box<dyn FnMut() -> bool>,
    /// On connect: the pad's app → layer map if it takes `kb.pl.set`, else
    /// `None` (firmware 0.6.3: `host.focused_app`).
    pub layer_probe: LayerProbe,
    locked: bool,
    /// The last app told to the pad, and the one seen to select the vendor
    /// layer (sent at lock time, so the dark frame is allowed out).
    last_forwarded: Option<FrontApp>,
    vendor_app: Option<FrontApp>,
    /// What `runtime.json` shows: the link, and the pad's last battery report.
    transport: Option<&'static str>,
    battery: Option<(u64, bool)>,
    runtime_written: Value,
    pub watcher: ConfigWatcher,
    pub last: Option<AgentState>,
    /// Log sink (stdout in production, a Vec in tests).
    pub say: Box<dyn FnMut(&str)>,
    /// Runs `open <arg>` for smart actions.
    pub opener: Opener,
    /// Handed to each new LED renderer; `None` in tests.
    pub herdr: Option<Herdr>,
    next_tick: f64,
    rotated_at: f64,
}

impl Pipeline {
    /// The production pipeline: config from disk, real backend and probes.
    pub fn new(opts: RunOptions) -> anyhow::Result<Pipeline> {
        let path = padconfig::resolve_path(&opts.home, opts.config.as_deref())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let config = padconfig::load(&path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let decay = Config::load(&opts.home).decay;
        let mut store = StateStore::for_home(&opts.home);
        store.decay = decay;
        let backend: Box<dyn Backend> = if opts.dry_run {
            Box::new(RecordingBackend::default())
        } else {
            best_backend()
        };
        let herdr = Herdr::from_env();
        let env = ActionEnv {
            home: opts.home.clone(),
            herdr: herdr.clone(),
        };
        let mut pipeline = Pipeline::with_parts(
            opts,
            config,
            store,
            BridgeParts(backend, env),
            Box::new(default_codex_probe),
            Box::new(vendor_app_running),
            Box::new(detect_frontmost_app),
            Box::new(detect_frontmost_app),
            path,
        );
        pipeline.herdr = Some(herdr);
        pipeline.opener = Box::new(open_detached);
        Ok(pipeline)
    }

    /// Assemble from injected parts (tests pass fakes).
    #[allow(clippy::too_many_arguments)]
    pub fn with_parts(
        opts: RunOptions,
        config: PadConfig,
        store: StateStore,
        bridge: BridgeParts,
        codex_probe: Box<dyn FnMut() -> bool>,
        vendor_probe: Box<dyn FnMut() -> bool>,
        profile_provider: Box<dyn FnMut() -> Option<crate::input::frontmost::FrontApp>>,
        focus_provider: Box<dyn FnMut() -> Option<crate::input::frontmost::FrontApp>>,
        watch_path: PathBuf,
    ) -> Pipeline {
        let BridgeParts(backend, env) = bridge;
        Pipeline {
            bridge: Bridge::new(config.clone(), backend, env),
            ownership: AgentKeyOwnership::new(config.agent_keys.clone(), codex_probe),
            overlay: ActivityOverlay::default(),
            owner: LightingOwner::new(vendor_probe),
            leds: None,
            frontmost: FrontmostWatcher::new(config.profile_poll_ms / 1000.0, profile_provider),
            forwarder: {
                let mut forwarder = PadFocusForwarder::new(FOCUS_POLL_SECONDS, focus_provider);
                forwarder.sites = config.sites.clone();
                forwarder
            },
            pad_layer: None,
            layer_check_at: None,
            lock_probe: Box::new(screen_locked),
            layer_probe: {
                let cache = super::keymap::cache_path(&opts.home);
                Box::new(move |io| super::keymap::read_layer_map(io, Some(&cache)))
            },
            locked: false,
            last_forwarded: None,
            vendor_app: None,
            transport: None,
            battery: None,
            runtime_written: Value::Null,
            watcher: ConfigWatcher::new(watch_path),
            last: None,
            say: Box::new(|line| println!("{line}")),
            opener: Box::new(|_| {}),
            herdr: None,
            next_tick: f64::NEG_INFINITY,
            rotated_at: mono(),
            config,
            store,
            opts,
        }
    }

    /// The startup banner (Python `_run_pipeline`'s prints).
    pub fn banner(&mut self, vendor_running: bool) {
        let label = if self.opts.headless {
            "freemicro-rs daemon"
        } else {
            "freemicro-rs run"
        };
        let mut lines = vec![
            format!("{label} - keys: {}", self.bridge.backend().description()),
            format!("Config: {}", self.config.origin()),
        ];
        let lighting = &self.config.lighting;
        if lighting.enabled {
            lines.push(
                "LEDs:   driven from agent state (freemicro lights --disable to stop)".into(),
            );
            let zones: Vec<&str> = lighting.zones.iter().map(|z| z.as_str()).collect();
            lines.push(format!("        zones: {}", zones.join(", ")));
        } else {
            lines.push("LEDs:   OFF - FreeMicro does not take over your pad uninvited.".into());
            lines.push("        Turn them on once:  freemicro lights --enable".into());
        }
        for line in coexist_advice(lighting, vendor_running).lines() {
            lines.push(format!("        {line}"));
        }
        for warning in &self.config.warnings {
            lines.push(format!("  ! {warning}"));
        }
        lines.push(if self.opts.headless {
            String::new()
        } else {
            "Ctrl-C to stop.\n".into()
        });
        for line in lines {
            (self.say)(&line);
        }
    }

    fn print_lighting(&mut self, event: Option<LightingEvent>) {
        if let Some(event) = event {
            if !event.verbose_only() || self.opts.verbose {
                (self.say)(&format!("{LIGHTING_PREFIX}{}", event.message));
            }
        }
    }

    fn print_dispatches(&mut self, dispatches: Vec<Dispatch>) {
        for d in dispatches {
            let line = format!("  {:9} {}", d.input_id, d.describe());
            if d.ok {
                (self.say)(&line);
            } else {
                (self.say)(&format!("{line}  FAILED: {}", d.error));
            }
        }
    }

    /// Activity-light changes from the bridge go to the overlay.
    fn drain_activity(&mut self, now: f64) {
        for (input_id, light) in self.bridge.drain_activity() {
            self.overlay.note(&input_id, light, now);
        }
    }

    fn print_notices(&mut self) {
        let notices = self
            .leds
            .as_mut()
            .map(LedRenderer::take_notices)
            .unwrap_or_default();
        for notice in notices {
            for line in lighting_lines(&notice) {
                (self.say)(&line);
            }
        }
    }

    /// A pad (re)connected: new renderer, forwarder attach, reassert.
    pub fn on_connect(&mut self, io: &mut dyn PadIo, now: f64) {
        let mut leds = LedRenderer::new(&self.config, self.store.decay, self.opts.home.clone());
        leds.herdr = self.herdr.clone();
        self.last = None;
        self.forwarder.attach(Some(io.info()));
        if io.info().is_creator_micro_2() {
            self.forwarder.layers = (self.layer_probe)(io);
        }
        self.transport = Some(match io.info().transport {
            Transport::Usb => "usb",
            Transport::Ble => "bluetooth",
            Transport::Other(_) => "other",
        });
        self.pad_layer = None;
        self.layer_check_at = Some(now);
        let note = if self.config.lighting.enabled {
            "keys live, LEDs live"
        } else {
            "keys live, LEDs off"
        };
        (self.say)(&format!(
            "  [pad connected over {} - {note}]",
            io.info().transport.as_str()
        ));
        if let Some(map) = &self.forwarder.layers {
            (self.say)(&format!(
                "  [layers] kb.pl.set, {} linked app(s)",
                map.len()
            ));
        }
        let event = self.owner.attach(&mut leds, now);
        self.leds = Some(leds);
        self.next_tick = now;
        self.print_lighting(event);
    }

    /// The pad went away: stop latches, clear the overlay, detach.
    pub fn on_disconnect(&mut self, now: f64) {
        self.leds = None;
        self.last = None;
        self.transport = None;
        self.pad_layer = None;
        self.forwarder.attach(None);
        let latches = self.bridge.stop_latches("the pad disconnected", now);
        self.print_dispatches(latches);
        let event = self.overlay.clear("the pad disconnected");
        self.print_lighting(event);
        (self.say)("  [pad disconnected - waiting for it to come back. This is normal;\n   the pad drops on sleep, on range, on a nudged cable.]");
    }

    /// One pad message: smart action, else the bridge.
    pub fn handle(&mut self, _io: &mut dyn PadIo, message: &Value, now: f64) {
        self.owner.note_input(now);
        if let Some(leds) = self.leds.as_mut() {
            leds.note_activity(now);
        }
        let backend = self.bridge.backend();
        let done = handle_smart_action(
            message,
            &mut |text| {
                let _ = backend.type_text(text);
            },
            &mut *self.opener,
        );
        if let Some(done) = done {
            (self.say)(&format!("  [{done}]"));
            return;
        }
        let dispatches = self.bridge.handle(message, &self.ownership, now);
        self.print_dispatches(dispatches);
        self.drain_activity(now);
    }

    /// Run due bridge work (chord settle, hold caps, latches, pointer).
    pub fn expire(&mut self, now: f64) {
        let dispatches = self.bridge.expire(&self.ownership, now);
        self.print_dispatches(dispatches);
        self.drain_activity(now);
    }

    fn adopt(&mut self, new: PadConfig, io: Option<&mut dyn PadIo>, now: f64) {
        let summary = describe_changes(&self.config, &new);
        self.bridge.set_config(new.clone());
        self.ownership.set_config(new.agent_keys.clone());
        if let Some(leds) = self.leds.as_mut() {
            leds.apply_config(&new, io, &self.ownership, now);
        }
        self.frontmost.interval = new.profile_poll_ms / 1000.0;
        self.forwarder.sites = new.sites.clone();
        (self.say)(&format!("  [config] reloaded - {summary}"));
        for warning in &new.warnings {
            (self.say)(&format!("  ! {warning}"));
        }
        self.config = new;
    }

    /// Periodic work; `io` is `None` while no pad is attached.
    pub fn tick(&mut self, mut io: Option<&mut dyn PadIo>, now: f64, wall: f64) {
        self.next_tick = now + self.opts.interval;
        if let Some(leds) = self.leds.as_mut() {
            let events = self.owner.poll(
                leds,
                &self.config.lighting,
                self.config.agent_keys.subset(),
                now,
            );
            for event in events {
                self.print_lighting(Some(event));
            }
        }
        if self.ownership.refresh(now) {
            if let Some(leds) = self.leds.as_mut() {
                leds.invalidate();
            }
        }
        let event = self.overlay.poll(now);
        self.print_lighting(event);
        match self.watcher.poll(now) {
            Some(Ok(config)) => self.adopt(config, reborrow(&mut io), now),
            Some(Err(error)) => (self.say)(&format!(
                "  [config] changed but would not load - still running the one it started with ({error})"
            )),
            None => {}
        }
        if let Some(io) = reborrow(&mut io) {
            if let Some(sent) = self.forwarder.poll(io, now) {
                // kb.pl.set: the layer is known at once.
                if let Some(layer) = self.forwarder.layer.filter(|l| self.pad_layer != Some(*l)) {
                    self.pad_layer = Some(layer);
                    if let Some(leds) = self.leds.as_mut() {
                        leds.invalidate();
                    }
                }
                // Give the firmware time to switch before asking.
                self.layer_check_at = Some(now + 0.5);
                self.last_forwarded = Some(sent.clone());
                if self.opts.verbose {
                    (self.say)(&format!(
                        "  [layers] told the pad {} ({}) is frontmost",
                        sent.name, sent.bundle_id
                    ));
                }
            }
        }
        if !self.config.profiles.is_empty() {
            self.frontmost.interval = self.config.profile_poll_ms / 1000.0;
            let app = self.frontmost.poll(now);
            if self.bridge.set_frontmost(app) && self.opts.verbose {
                let name = self.bridge.frontmost().unwrap_or("(none)").to_string();
                (self.say)(&format!("  [profile] frontmost app: {name}"));
            }
        }
        if let Some(io) =
            reborrow(&mut io).filter(|_| self.layer_check_at.is_some_and(|at| now >= at))
        {
            self.layer_check_at = None;
            let mut stray = Vec::new();
            let layer =
                crate::device::status(io, LAYER_STATUS_ID, Duration::from_millis(1500), &mut |m| {
                    stray.push(m)
                })
                .ok()
                .flatten();
            if let Some(status) = &layer {
                if let Some(level) = status["battery"].as_u64() {
                    let charging = status["is_charging"].as_bool().unwrap_or(false);
                    self.battery = Some((level, charging));
                }
            }
            let layer = layer.and_then(|status| status["layer_index"].as_u64());
            for message in stray {
                self.handle(io, &message, now);
            }
            if layer.is_some() && layer != self.pad_layer {
                if layer == Some(VENDOR_LAYER) {
                    self.vendor_app = self.last_forwarded.clone();
                }
                self.pad_layer = layer;
                // A layer picked on the pad itself: the next kb.pl.set must not be skipped.
                if self.forwarder.layers.is_some() {
                    self.forwarder.layer = layer;
                }
                if let Some(leds) = self.leds.as_mut() {
                    leds.invalidate();
                }
            }
        }
        let locked = (self.lock_probe)();
        if locked != self.locked {
            self.locked = locked;
            if let Some(leds) = self.leds.as_mut() {
                leds.set_locked(locked);
            }
            (self.say)(if locked {
                "  [lock] the Mac is locked - pad dark"
            } else {
                "  [lock] the Mac is unlocked - pad lit"
            });
            if let Some(io) = reborrow(&mut io) {
                if !locked {
                    // Tell the pad the real front app again: layer and lights return.
                    self.forwarder.attach(Some(io.info()));
                } else if self.forwarder.layers.is_some() {
                    // Only the vendor layer takes a dark frame: go there first.
                    let switch = serde_json::json!({"m": METHOD_SET_LAYER, "p": {"l": VENDOR_LAYER}, "id": 1});
                    if self.pad_layer != Some(VENDOR_LAYER) && io.send(&switch).is_ok() {
                        self.pad_layer = Some(VENDOR_LAYER);
                        self.forwarder.layer = Some(VENDOR_LAYER);
                    }
                } else if let Some(app) = self
                    .vendor_app
                    .clone()
                    .filter(|_| self.pad_layer != Some(VENDOR_LAYER))
                {
                    // Only the vendor layer takes a dark frame: go there first.
                    let switch = serde_json::json!({
                        "m": METHOD_FOCUSED_APP,
                        "p": {"appName": app.name, "process": app.bundle_id},
                        "id": 1,
                    });
                    if io.send(&switch).is_ok() {
                        self.pad_layer = Some(VENDOR_LAYER);
                    }
                }
            }
        }
        let (state, sessions) = self.store.resolved_and_sessions(wall);
        // Agent Key frames only on the vendor layer: elsewhere they would hide
        // the layer's own lights, which the firmware restores on a switch.
        let vendor = self.pad_layer.is_none_or(|layer| layer == VENDOR_LAYER);
        if let (Some(leds), Some(io), true) = (self.leds.as_mut(), reborrow(&mut io), vendor) {
            leds.set_overlay(self.overlay.current());
            leds.render(io, state, &sessions, &self.ownership, now, wall);
        }
        self.print_notices();
        self.publish_runtime();
        if self.last != Some(state) {
            (self.say)(&format!("  state: {}", state.as_str()));
            self.last = Some(state);
        }
        if self.opts.headless && now - self.rotated_at > LOG_ROTATE_SECONDS {
            self.rotated_at = now;
            daemon::rotate_log(&daemon::log_path(&self.opts.home), daemon::LOG_CAP_BYTES);
        }
    }

    /// Write `<home>/runtime.json` when it changes: the status window reads it,
    /// because only this process may talk to the pad.
    fn publish_runtime(&mut self) {
        let (dial, effort) = crate::herdr::dial_state();
        let app = self
            .last_forwarded
            .as_ref()
            .filter(|_| self.transport.is_some());
        let snapshot = json!({
            "connected": self.transport.is_some(),
            "transport": self.transport,
            "layer": self.pad_layer,
            "app": app.map(|a| &a.name),
            "process": app.map(|a| &a.bundle_id),
            "locked": self.locked,
            "battery": self.battery.map(|(level, _)| level),
            "charging": self.battery.map(|(_, charging)| charging),
            "dial": dial,
            "effort": effort,
        });
        if snapshot == self.runtime_written {
            return;
        }
        let path = self.opts.home.join(RUNTIME_FILENAME);
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, snapshot.to_string())
            .and_then(|()| std::fs::rename(&tmp, &path))
            .is_ok()
        {
            self.runtime_written = snapshot;
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Monotonic time the loop must wake by.
    pub fn next_wake(&self, now: f64) -> f64 {
        [
            Some(self.next_tick),
            self.bridge.next_deadline(),
            self.overlay.next_deadline(),
        ]
        .into_iter()
        .flatten()
        .fold(now + self.opts.interval, f64::min)
    }

    /// Release held keys and hand the LEDs back; every exit path.
    pub fn close(&mut self, io: Option<&mut dyn PadIo>) {
        self.bridge.close();
        let vendor = self.pad_layer.is_none_or(|layer| layer == VENDOR_LAYER);
        if let (Some(leds), Some(io), true) = (self.leds.as_mut(), io, vendor) {
            leds.hand_back(io, &self.ownership);
        }
    }
}

/// Backend and action env for [`Pipeline::with_parts`].
pub struct BridgeParts(pub Box<dyn Backend>, pub ActionEnv);

/// Reborrow an optional pad for one call (trait-object lifetimes are
/// invariant behind `&mut`, so `as_deref_mut` does not fit).
fn reborrow<'a>(io: &'a mut Option<&mut dyn PadIo>) -> Option<&'a mut dyn PadIo> {
    match io {
        Some(io) => Some(&mut **io),
        None => None,
    }
}

/// How the loop finds the pad; tests inject `FakePad`s.
pub trait PadSource {
    fn open(&mut self) -> Result<Option<Box<dyn PadIo>>, DeviceError>;
    /// [`PadIo::service`] of the pad `open` would pick now.
    fn present(&mut self) -> Option<u64>;
}

/// `device::open` / `device::present`.
pub struct SystemPads;

impl PadSource for SystemPads {
    fn open(&mut self) -> Result<Option<Box<dyn PadIo>>, DeviceError> {
        crate::device::open()
    }
    fn present(&mut self) -> Option<u64> {
        crate::device::present()
    }
}

/// Tick every interval for `RETRY_INTERVAL` with no pad. False = stop.
fn wait_ticking(pipeline: &mut Pipeline, deadline: Option<f64>) -> bool {
    let until = mono() + RETRY_INTERVAL;
    loop {
        let now = mono();
        if stop_requested() || deadline.is_some_and(|d| now >= d) {
            return false;
        }
        if now >= until {
            return true;
        }
        if now >= pipeline.next_tick {
            pipeline.tick(None, now, wall());
        }
        let wake = pipeline.next_tick.min(until);
        std::thread::sleep(Duration::from_secs_f64((wake - mono()).clamp(0.001, 1.0)));
    }
}

/// Stream one open pad. True = it dropped; false = stop or deadline.
fn stream(
    pipeline: &mut Pipeline,
    io: &mut dyn PadIo,
    pads: &mut dyn PadSource,
    deadline: Option<f64>,
) -> bool {
    let mut last_check = mono();
    loop {
        if stop_requested() {
            return false;
        }
        let now = mono();
        if now >= pipeline.next_tick {
            pipeline.tick(Some(io), now, wall());
            if now - last_check >= PRESENCE_CHECK_SECONDS {
                last_check = now;
                // Gone, or back on another service (BLE during sleep, then
                // the cable): the open handle is dead either way.
                if pads.present() != Some(io.service()) {
                    return true;
                }
            }
            if deadline.is_some_and(|d| now >= d) {
                return false;
            }
        }
        let timeout = (pipeline.next_wake(now) - mono()).max(0.0);
        match io.poll(Duration::from_secs_f64(timeout)) {
            Ok(messages) => {
                for message in messages {
                    pipeline.handle(io, &message, mono());
                }
            }
            Err(_) => return true,
        }
        pipeline.expire(mono());
    }
}

/// Drive `pipeline` until `opts.seconds` elapse (or forever), reconnecting.
/// Returns on SIGINT / SIGTERM after `close`.
pub fn run_with_reconnect(pipeline: &mut Pipeline, pads: &mut dyn PadSource) -> anyhow::Result<()> {
    let seconds = pipeline.opts.seconds;
    let deadline = (seconds > 0.0).then(|| mono() + seconds);
    let mut kept: Option<Box<dyn PadIo>> = None;
    let mut last_error: Option<String> = None;
    while !stop_requested() && !deadline.is_some_and(|d| mono() >= d) {
        let opened = pads.open();
        if let Err(error) = &opened {
            // Once per distinct error: the loop retries every couple of seconds.
            if last_error.as_deref() != Some(error.0.as_str()) {
                (pipeline.say)(&format!("  [pad] {}", error.0));
                last_error = Some(error.0.clone());
            }
        }
        let Some(mut io) = opened.ok().flatten() else {
            if !wait_ticking(pipeline, deadline) {
                break;
            }
            continue;
        };
        pipeline.on_connect(io.as_mut(), mono());
        if !stream(pipeline, io.as_mut(), pads, deadline) {
            kept = Some(io);
            break;
        }
        drop(io);
        pipeline.on_disconnect(mono());
        if !wait_ticking(pipeline, deadline) {
            break;
        }
    }
    pipeline.close(kept.as_mut().map(|io| -> &mut dyn PadIo { io.as_mut() }));
    Ok(())
}

/// `run` / `daemon run`: lock, pipeline, loop. Exit code.
pub fn run(opts: RunOptions) -> anyhow::Result<i32> {
    let home = opts.home.clone();
    let role = if opts.headless { "daemon" } else { "run" };
    let mut lock = PadLock::new(daemon::lock_path(&home), role);
    if opts.headless {
        daemon::rotate_log(&daemon::log_path(&home), daemon::LOG_CAP_BYTES);
        if !lock.acquire(wall()) {
            let holder = daemon::lock_holder(&lock.path).unwrap_or_default();
            println!(
                "[freemicro] not starting: {} already owns the pad.",
                daemon::describe_holder(&holder, wall())
            );
            // 0, or launchd restarts us into the same conflict every 10 s.
            return Ok(0);
        }
        println!(
            "[freemicro] daemon up (pid {}), logging to {}",
            std::process::id(),
            daemon::log_path(&home).display()
        );
    } else if !opts.take_pad {
        if let Some(holder) = daemon::lock_holder(&lock.path) {
            eprintln!(
                "{} already has the pad.\n  Only one process can usefully hold this device, so FreeMicro will\n  not fight it - your pad is already live.\n  → freemicro-rs daemon status        what it's doing\n  → freemicro-rs daemon uninstall     stop it and take the pad back\n  → freemicro-rs run --take-pad       take it anyway (expect a mess)",
                daemon::describe_holder(&holder, wall())
            );
            return Ok(1);
        }
        lock.acquire(wall());
    }
    let mut pipeline = match Pipeline::new(opts) {
        Ok(pipeline) => pipeline,
        Err(err) => {
            eprintln!("Config error: {err}");
            return Ok(2);
        }
    };
    pipeline.banner(vendor_app_running());
    if SystemPads.present().is_none() {
        println!("Starting anyway - FreeMicro will connect as soon as the pad appears.\n");
    }
    install_signal_handlers();
    let result = run_with_reconnect(&mut pipeline, &mut SystemPads);
    lock.release();
    result.map(|_| 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::fake::FakePad;
    use crate::input::frontmost::FrontApp;
    use crate::runtime::lighting::Zone;
    use crate::state::process::{ProcessLiveness, ProcessProbe};
    use crate::state::DecayPolicy;
    use crate::testutil::TempDir;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Dead;
    impl ProcessProbe for Dead {
        fn alive(&self, _: i32) -> bool {
            false
        }
        fn started(&self, _: i32, _: f64) -> Option<f64> {
            None
        }
    }

    type Log = Rc<RefCell<Vec<String>>>;

    fn pipeline(home: &TempDir, lighting: bool, interval: f64, seconds: f64) -> (Pipeline, Log) {
        let mut config = padconfig::load_default();
        config.lighting.enabled = lighting;
        config.lighting.zones = vec![Zone::AgentKeys];
        config.agent_keys.policy = crate::state::agentkeys::Policy::Mirror;
        let store = StateStore::new(
            home.path().join("state"),
            DecayPolicy::default(),
            ProcessLiveness::new(Box::new(Dead)),
        );
        let opts = RunOptions {
            home: home.path().to_path_buf(),
            dry_run: true,
            interval,
            seconds,
            config: None,
            take_pad: false,
            verbose: true,
            headless: false,
        };
        let env = ActionEnv {
            home: home.path().to_path_buf(),
            herdr: Herdr {
                socket: home.path().join("no-herdr.sock"),
            },
        };
        let mut p = Pipeline::with_parts(
            opts,
            config,
            store,
            BridgeParts(Box::new(RecordingBackend::default()), env),
            Box::new(|| false),
            Box::new(|| false),
            Box::new(|| None),
            Box::new(|| {
                Some(FrontApp {
                    name: "Ghostty".into(),
                    bundle_id: "com.mitchellh.ghostty".into(),
                })
            }),
            home.path().join("keymap.json"),
        );
        let log: Log = Rc::default();
        let sink = log.clone();
        p.say = Box::new(move |line| sink.borrow_mut().push(line.to_string()));
        p.lock_probe = Box::new(|| false);
        p.layer_probe = Box::new(|_| None); // firmware 0.6.3: host.focused_app
        (p, log)
    }

    #[test]
    fn connect_tick_and_drop_follow_the_python_log() {
        let home = TempDir::new();
        let (mut p, log) = pipeline(&home, true, 1.0, 0.0);
        let mut pad = FakePad::creator_micro_2();
        p.on_connect(&mut pad, 0.0);
        p.tick(Some(&mut pad), 0.0, 1000.0);
        p.tick(Some(&mut pad), 1.0, 1001.0);
        p.on_disconnect(2.0);
        let log = log.borrow();
        assert_eq!(log[0], "  [pad connected over USB - keys live, LEDs live]");
        assert_eq!(
            log[1],
            "  [layers] told the pad Ghostty (com.mitchellh.ghostty) is frontmost"
        );
        assert_eq!(log[2], "  state: idle");
        assert_eq!(log.len(), 4, "{log:?}");
        assert!(log[3].starts_with("  [pad disconnected"));
        let methods: Vec<_> = pad.sent.iter().map(|m| m["m"].clone()).collect();
        assert_eq!(
            methods,
            vec![
                json!("host.focused_app"),
                json!("v.oai.thstatus"),
                json!("device.status")
            ]
        );
    }

    #[test]
    fn leds_go_out_only_while_the_pad_is_on_the_vendor_layer() {
        // Off the vendor layer an Agent Key frame hides the layer's own lights.
        for (layer, thstatus_after_check) in [(3, 0), (1, 1)] {
            let home = TempDir::new();
            let (mut p, _) = pipeline(&home, true, 1.0, 0.0);
            let mut pad = FakePad::creator_micro_2().respond(move |m| {
                if m["m"] == "device.status" {
                    vec![json!({"result": {"layer_index": layer}, "id": m["id"]})]
                } else {
                    Vec::new()
                }
            });
            p.on_connect(&mut pad, 0.0);
            p.tick(Some(&mut pad), 0.0, 1000.0);
            let before = pad.sent.len();
            // The first check learns the layer; on the vendor layer the
            // frame goes out again, off it nothing does.
            p.tick(Some(&mut pad), 1.0, 1001.0);
            let after: Vec<_> = pad.sent[before..].iter().map(|m| m["m"].clone()).collect();
            let frames = after.iter().filter(|m| *m == "v.oai.thstatus").count();
            assert_eq!(frames, thstatus_after_check, "layer {layer}: {after:?}");
        }
    }

    #[test]
    fn runtime_json_follows_the_pad_and_clears_on_a_drop() {
        let home = TempDir::new();
        let (mut p, _log) = pipeline(&home, true, 1.0, 0.0);
        p.lock_probe = Box::new(|| false);
        let mut pad = FakePad::creator_micro_2().respond(|m| {
            if m["m"] == "device.status" {
                let status = json!({"layer_index": 3, "battery": 87, "is_charging": true});
                vec![json!({"result": status, "id": m["id"]})]
            } else {
                Vec::new()
            }
        });
        let read = || -> Value {
            serde_json::from_slice(&std::fs::read(home.path().join(RUNTIME_FILENAME)).unwrap())
                .unwrap()
        };
        p.on_connect(&mut pad, 0.0);
        p.tick(Some(&mut pad), 0.0, 1000.0); // forwards the front app
        p.tick(Some(&mut pad), 1.0, 1001.0); // asks the pad its layer
        let up = read();
        assert_eq!(up["connected"], true);
        assert_eq!(up["layer"], 3);
        assert_eq!(
            (up["battery"].clone(), up["charging"].clone()),
            (json!(87), json!(true))
        );
        assert_eq!(up["locked"], false);
        assert!(up["dial"].is_string() && up["effort"].is_string());
        p.on_disconnect(2.0);
        p.tick(None, 2.0, 1002.0);
        let down = read();
        assert_eq!(
            (down["connected"].clone(), down["layer"].clone()),
            (json!(false), Value::Null)
        );
        assert_eq!(down["app"], Value::Null);
    }

    #[test]
    fn with_kb_pl_set_the_daemon_knows_the_layer_and_a_lock_sets_layer_1() {
        let home = TempDir::new();
        let (mut p, _) = pipeline(&home, true, 1.0, 0.0);
        p.layer_probe = Box::new(|_| Some(vec![("com.mitchellh.ghostty".into(), 2)]));
        let mut pad = FakePad::creator_micro_2().respond(|m| {
            if m["m"] == "device.status" {
                vec![json!({"result": {"layer_index": 2}, "id": m["id"]})]
            } else {
                Vec::new()
            }
        });
        let locked = Rc::new(std::cell::Cell::new(false));
        let probe = locked.clone();
        p.lock_probe = Box::new(move || probe.get());
        p.on_connect(&mut pad, 0.0);
        p.tick(Some(&mut pad), 0.0, 1000.0);
        assert_eq!(
            pad.sent[0],
            json!({"m": "kb.pl.set", "p": {"l": 2}, "id": 1})
        );
        assert_eq!(
            p.pad_layer,
            Some(2),
            "known at once, before any status reply"
        );
        let before = pad.sent.len();
        locked.set(true);
        p.tick(Some(&mut pad), 1.0, 1001.0);
        // The layer check scheduled by the forward goes out first; skip it.
        let sent: Vec<_> = pad.sent[before..]
            .iter()
            .filter(|m| m["m"] != "device.status")
            .collect();
        assert_eq!(*sent[0], json!({"m": "kb.pl.set", "p": {"l": 1}, "id": 1}));
        assert_eq!(sent[1]["m"], "v.oai.thstatus");
        assert_eq!(sent[1]["p"][0]["e"], 0, "dark");
    }

    #[test]
    fn a_lock_moves_the_pad_to_the_vendor_layer_and_darkens_it() {
        let home = TempDir::new();
        let (mut p, _) = pipeline(&home, true, 1.0, 0.0);
        let layer = Rc::new(std::cell::Cell::new(1u64));
        let reported = layer.clone();
        let mut pad = FakePad::creator_micro_2().respond(move |m| {
            if m["m"] == "device.status" {
                vec![json!({"result": {"layer_index": reported.get()}, "id": m["id"]})]
            } else {
                Vec::new()
            }
        });
        let locked = Rc::new(std::cell::Cell::new(false));
        let probe = locked.clone();
        p.lock_probe = Box::new(move || probe.get());
        p.on_connect(&mut pad, 0.0);
        p.tick(Some(&mut pad), 0.0, 1000.0); // forwards Ghostty
        p.tick(Some(&mut pad), 1.0, 1001.0); // layer 1: Ghostty is the vendor app
        layer.set(3); // the user moved to a layer of its own
        p.layer_check_at = Some(1.5);
        p.tick(Some(&mut pad), 2.0, 1002.0);
        let before = pad.sent.len();
        locked.set(true);
        p.tick(Some(&mut pad), 3.0, 1003.0);
        let sent = &pad.sent[before..];
        assert_eq!(sent[0]["m"], "host.focused_app");
        assert_eq!(sent[0]["p"]["process"], "com.mitchellh.ghostty");
        assert_eq!(sent[1]["m"], "v.oai.thstatus");
        assert_eq!(sent[1]["p"][0]["e"], 0, "dark");
        let before = pad.sent.len();
        locked.set(false);
        p.tick(Some(&mut pad), 4.0, 1004.0);
        p.tick(Some(&mut pad), 5.0, 1005.0);
        let after: Vec<_> = pad.sent[before..].iter().map(|m| m["m"].clone()).collect();
        assert!(
            after.contains(&json!("host.focused_app")),
            "real app again: {after:?}"
        );
        assert!(pad.sent[before..]
            .iter()
            .any(|m| m["m"] == "v.oai.thstatus" && m["p"][0]["e"] == 1));
    }

    #[test]
    fn a_broken_config_edit_keeps_the_running_one() {
        let home = TempDir::new();
        let path = home.path().join("keymap.json");
        let (mut p, log) = pipeline(&home, false, 1.0, 0.0);
        std::fs::write(&path, "{ broken").unwrap();
        p.tick(None, 0.0, 1000.0);
        assert!(log.borrow()[0].starts_with("  [config] changed but would not load"));
        std::fs::write(&path, r#"{"bindings": {}, "lighting": {"enabled": true}}"#).unwrap();
        p.tick(None, 2.0, 1002.0);
        assert!(
            log.borrow()
                .iter()
                .any(|l| l.starts_with("  [config] reloaded - ") && l.contains("LEDs on")),
            "{:?}",
            log.borrow()
        );
        assert!(p.config.lighting.enabled);
    }

    #[test]
    fn smart_actions_bypass_the_bridge() {
        let home = TempDir::new();
        let (mut p, log) = pipeline(&home, false, 1.0, 0.0);
        let opened: Rc<RefCell<Vec<Vec<String>>>> = Rc::default();
        let sink = opened.clone();
        p.opener = Box::new(move |argv| sink.borrow_mut().push(argv.to_vec()));
        let mut pad = FakePad::creator_micro_2();
        p.handle(
            &mut pad,
            &json!({"m": "kb.sa.openurl", "p": {"url": "https://example.com"}}),
            0.0,
        );
        assert_eq!(opened.borrow().len(), 1);
        assert!(log.borrow()[0].starts_with("  ["));
    }

    #[test]
    fn next_wake_is_the_earliest_deadline() {
        let home = TempDir::new();
        let (mut p, _) = pipeline(&home, false, 1.0, 0.0);
        p.tick(None, 10.0, 1000.0);
        assert_eq!(p.next_wake(10.0), 11.0);
        p.overlay.note(
            "ACT06",
            Some(crate::runtime::lighting_config::ActivityLight {
                light: crate::runtime::lighting_config::StateLight::solid(1),
                zones: vec![Zone::Underglow],
                timeout_seconds: 0.5,
            }),
            10.0,
        );
        assert_eq!(p.next_wake(10.0), 10.5);
    }

    /// Opens a pad once; reports `service` present for `present_for` checks.
    struct Flaky {
        opens: usize,
        present_for: usize,
        service: u64,
    }

    impl PadSource for Flaky {
        fn open(&mut self) -> Result<Option<Box<dyn PadIo>>, DeviceError> {
            self.opens += 1;
            Ok((self.opens == 1).then(|| Box::new(FakePad::creator_micro_2()) as Box<dyn PadIo>))
        }
        fn present(&mut self) -> Option<u64> {
            if self.present_for == 0 {
                return None;
            }
            self.present_for -= 1;
            Some(self.service)
        }
    }

    #[test]
    fn run_with_reconnect_survives_a_drop_and_keeps_ticking() {
        let home = TempDir::new();
        let (mut p, log) = pipeline(&home, false, 0.05, 1.3);
        let mut pads = Flaky {
            opens: 0,
            present_for: 0,
            service: 0,
        };
        run_with_reconnect(&mut p, &mut pads).unwrap();
        let log = log.borrow();
        assert_eq!(log[0], "  [pad connected over USB - keys live, LEDs off]");
        assert!(
            log.iter().any(|l| l.starts_with("  [pad disconnected")),
            "{log:?}"
        );
        assert_eq!(pads.opens, 1, "retries every 2 s, so one open in 1.3 s");
    }

    #[test]
    fn run_with_reconnect_drops_a_handle_when_the_pad_moved_to_another_service() {
        let home = TempDir::new();
        let (mut p, log) = pipeline(&home, false, 0.05, 1.3);
        // Still present, but not the service that was opened (FakePad is 0).
        let mut pads = Flaky {
            opens: 0,
            present_for: usize::MAX,
            service: 7,
        };
        run_with_reconnect(&mut p, &mut pads).unwrap();
        assert!(
            log.borrow()
                .iter()
                .any(|l| l.starts_with("  [pad disconnected")),
            "{:?}",
            log.borrow()
        );
    }
}
