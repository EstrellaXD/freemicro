//! Menu bar item with a compact status panel. It never opens the pad: the daemon is the one host.
//! Sources, all in `~/.freemicro`: `pad.lock` (daemon pid), `runtime.json`
//! (layer, link, battery, lock, dial; written by the daemon), `pad-keymap.json`
//! (layer names, cached on keymap push/pull), `state/` + `slots.json` (Agent
//! Keys).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use freemicro::herdr::{DIAL_MODES, EFFORT_LEVELS};
use freemicro::state::slots;
use freemicro::state::store::StateStore;
use gpui::{
    div, linear_color_stop, linear_gradient, point, prelude::*, px, rgb, size, AnyWindowHandle,
    App, Application, Bounds, Context, Div, FontWeight, Hsla, SharedString, Window, WindowBounds,
    WindowKind, WindowOptions,
};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use serde_json::Value;
use tray_icon::{Icon, MouseButtonState, TrayIconBuilder, TrayIconEvent};

const BG: u32 = 0x161616;
const CARD: u32 = 0x202020;
const LINE: u32 = 0x2E2E2E;
const TEXT: u32 = 0xF0F0F0;
const MUTED: u32 = 0x8C8C8C;
const ACCENT: u32 = 0xFF5000; // the Claude orange on the pad
const OK: u32 = 0x00FF4C;
const BAD: u32 = 0xFF0033;

/// One pad layer, from the cached keymap.
#[derive(Debug, PartialEq)]
struct Layer {
    name: String,
    /// Linked app's process (`com.apple.Safari/<site>` for a site layer).
    process: Option<String>,
    /// Linked app's name without the Input app's `-mac` / `-web` suffix.
    app: Option<String>,
    color: u32,
    effect: String,
    /// Keycodes by row, as the pad keymap lists them (2, 4, 4, 3 keys).
    keys: Vec<Vec<String>>,
    /// The dial: turn left, turn right, press.
    encoders: Vec<String>,
    /// Radial stick sectors; empty for the vendor (FreeMicro) stick.
    stick: Vec<String>,
}

/// What the keycodes on the pad are called: pad macros and multi-actions,
/// and FreeMicro's own bindings for the vendor keys.
#[derive(Default)]
struct Names {
    macros: Vec<(u64, String)>,
    multi: Vec<(u64, String)>,
    /// `ACT06` → (keycap, binding label).
    vendor: Vec<(String, Option<String>, Option<String>)>,
    /// FreeMicro's `joystick.mode` for the vendor stick.
    stick_mode: String,
}

fn named(list: &Value) -> Vec<(u64, String)> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m["id"].as_u64()?, m["name"].as_str()?.to_string())))
        .collect()
}

/// Names from the pad keymap and FreeMicro's `keymap.json`.
fn names_from(pad: &Value, freemicro: &Value) -> Names {
    let caps = &freemicro["keycaps"];
    let vendor = freemicro["bindings"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, b)| {
            let cap = caps[id].as_str().map(str::to_string);
            let label = b["label"].as_str().map(|l| {
                // "FAST - cycle permission mode": the cap is shown already.
                l.split_once(" - ").map_or(l, |(_, rest)| rest).to_string()
            });
            (id.clone(), cap, label)
        })
        .collect();
    Names {
        macros: named(&pad["macros"]),
        multi: named(&pad["multiActions"]),
        vendor,
        stick_mode: freemicro["joystick"]["mode"]
            .as_str()
            .unwrap_or("joystick")
            .to_string(),
    }
}

fn key_name(code: &str) -> String {
    match code {
        "ENT" => "Enter",
        "ESC" => "Esc",
        "LSFT" | "RSFT" => "Shift",
        "LALT" | "RALT" => "Option",
        "LGUI" | "RGUI" => "Cmd",
        "LCTL" | "RCTL" => "Ctrl",
        "SPC" => "Space",
        "BSPC" => "Delete",
        "TAB" => "Tab",
        "UP" => "↑",
        "DOWN" => "↓",
        "LEFT" => "←",
        "RGHT" | "RIGHT" => "→",
        other => other,
    }
    .to_string()
}

/// A key's title and optional detail, or `None` for an unbound key.
fn label(code: &str, names: &Names) -> Option<(String, Option<String>)> {
    let find = |list: &[(u64, String)], id: &str| {
        let id = id.parse::<u64>().ok()?;
        list.iter().find(|(i, _)| *i == id).map(|(_, n)| n.clone())
    };
    if code.is_empty() || code == "KC_NONE" || code == "KI_X" {
        return None;
    }
    if let Some(id) = code.strip_prefix("KV_OAI_") {
        let (cap, detail) = names
            .vendor
            .iter()
            .find(|(v, ..)| v == id)
            .map_or((None, None), |(_, c, l)| (c.clone(), l.clone()));
        return Some((cap.unwrap_or_else(|| id.to_string()), detail));
    }
    if let Some(id) = code.strip_prefix("KA_M").or(code.strip_prefix("KM_")) {
        return Some((
            find(&names.multi, id).unwrap_or_else(|| code.into()),
            Some("multi-action".into()),
        ));
    }
    if let Some(id) = code.strip_prefix("KA_A") {
        return Some((
            find(&names.macros, id).unwrap_or_else(|| code.into()),
            Some("macro".into()),
        ));
    }
    if let Some(id) = code.strip_prefix("SA_") {
        return Some((format!("Smart action {id}"), None));
    }
    if let Some(key) = code.strip_prefix("KC_") {
        return Some((key_name(key), Some("key".into())));
    }
    Some((code.to_string(), None))
}

/// The daemon's `runtime.json`.
#[derive(Default)]
struct Runtime {
    connected: bool,
    transport: Option<String>,
    layer: Option<u64>,
    app: Option<String>,
    process: Option<String>,
    locked: bool,
    battery: Option<u64>,
    charging: bool,
    dial: String,
    effort: String,
}

struct Key {
    name: SharedString,
    state: SharedString,
}

struct Status {
    home: PathBuf,
    daemon: Option<i32>,
    runtime: Runtime,
    layers: Vec<Layer>,
    names: Names,
    keys: Vec<Key>,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The daemon pid from `pad.lock`, if that process is alive.
fn daemon_pid(home: &Path) -> Option<i32> {
    let pid = read_json(&home.join("pad.lock"))?["pid"].as_i64()? as i32;
    (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
}

/// Layers of the active profile, in pad order (layer 1 first).
fn layers_from_keymap(keymap: &Value) -> Vec<Layer> {
    let active = keymap["activeProfileId"].as_u64().unwrap_or(0) as usize;
    let profiles = keymap["profiles"].as_array().cloned().unwrap_or_default();
    let Some(profile) = profiles.get(active).or(profiles.first()) else {
        return Vec::new();
    };
    let apps = keymap["linkedApps"].as_array().cloned().unwrap_or_default();
    profile["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|layer| {
            // The underglow is the light this pad shows for a layer.
            let light = &layer["lights"]["underglow"];
            let app = layer["linkedAppId"]
                .as_u64()
                .and_then(|id| apps.iter().find(|a| a["id"].as_u64() == Some(id)));
            let text = |key: &str| app.and_then(|a| a[key].as_str()).map(str::to_string);
            Layer {
                name: layer["name"].as_str().unwrap_or("?").to_string(),
                process: text("process"),
                app: text("name").map(|n| {
                    n.strip_suffix("-mac")
                        .or(n.strip_suffix("-web"))
                        .unwrap_or(&n)
                        .to_string()
                }),
                color: light["color"]
                    .as_u64()
                    .or(layer["color"].as_u64())
                    .unwrap_or(0x3A3A3A) as u32,
                effect: light["effect"].as_str().unwrap_or("").to_string(),
                keys: strings2(&layer["layout"]["keymap"]),
                encoders: strings2(&layer["layout"]["encoders"])
                    .into_iter()
                    .next()
                    .unwrap_or_default(),
                stick: layer["layout"]["joystick"]["sectors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s["k"].as_str().map(str::to_string))
                    .collect(),
            }
        })
        .collect()
}

/// `[["KC_A", …], …]` → rows of strings.
fn strings2(rows: &Value) -> Vec<Vec<String>> {
    rows.as_array()
        .into_iter()
        .flatten()
        .map(|row| {
            row.as_array()
                .into_iter()
                .flatten()
                .map(|k| k.as_str().unwrap_or("").to_string())
                .collect()
        })
        .collect()
}

/// `com.apple.Safari/example.org` → (bundle id, Some(site)).
fn split_site(process: &str) -> (&str, Option<&str>) {
    match process.split_once('/') {
        Some((bundle, site)) => (bundle, Some(site)),
        None => (process, None),
    }
}

fn runtime_from(v: &Value) -> Runtime {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Runtime {
        connected: v["connected"].as_bool().unwrap_or(false),
        transport: s("transport"),
        layer: v["layer"].as_u64(),
        app: s("app"),
        process: s("process"),
        locked: v["locked"].as_bool().unwrap_or(false),
        battery: v["battery"].as_u64(),
        charging: v["charging"].as_bool().unwrap_or(false),
        dial: s("dial").unwrap_or_default(),
        effort: s("effort").unwrap_or_default(),
    }
}

impl Status {
    fn refresh(&mut self) {
        self.daemon = daemon_pid(&self.home);
        // A stopped daemon leaves a stale runtime.json behind: ignore it then.
        self.runtime = match self.daemon {
            Some(_) => read_json(&self.home.join("runtime.json"))
                .map(|v| runtime_from(&v))
                .unwrap_or_default(),
            None => Runtime::default(),
        };
        let pad = read_json(&self.home.join("pad-keymap.json")).unwrap_or_default();
        self.layers = layers_from_keymap(&pad);
        let freemicro = read_json(&self.home.join("keymap.json")).unwrap_or_default();
        self.names = names_from(&pad, &freemicro);
        // include_stale + no decay: the window reads, it never rewrites state files.
        let sessions = StateStore::for_home(&self.home).sessions(now(), true, false);
        self.keys = slots::load(&slots::slots_path(&self.home))
            .iter()
            .map(|slot| {
                // "<cwd>#2" is the second session in that directory.
                let (cwd, nth) = match slot.rsplit_once('#') {
                    Some((c, n)) => (c, n.parse::<usize>().unwrap_or(1)),
                    None => (slot.as_str(), 1),
                };
                let state = sessions
                    .iter()
                    .filter(|s| !cwd.is_empty() && s.cwd == cwd)
                    .nth(nth - 1)
                    .map_or("empty", |s| s.claim().as_str());
                let base = Path::new(cwd)
                    .file_name()
                    .map_or("—".into(), |f| f.to_string_lossy().into_owned());
                Key {
                    name: if nth > 1 {
                        format!("{base} #{nth}")
                    } else {
                        base
                    }
                    .into(),
                    state: state.into(),
                }
            })
            .collect();
    }
}

/// The pad's own palette from keymap.json.
fn state_color(state: &str) -> Hsla {
    rgb(match state {
        "waiting" => 0xFF0000,
        "working" => 0xFFD000,
        "done" => OK,
        "error" => BAD,
        "idle" => 0x0000FF,
        _ => 0x3A3A3A,
    })
    .into()
}

fn dot(color: impl Into<Hsla>, side: f32) -> Div {
    div()
        .size(px(side))
        .flex_none()
        .rounded_full()
        .bg(color.into())
}

fn section(title: impl Into<SharedString>) -> Div {
    div().flex().flex_col().gap_1p5().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(MUTED))
            .child(title.into()),
    )
}

fn swatch(layer: &Layer) -> gpui::Background {
    if layer.effect == "rainbow" {
        linear_gradient(
            135.,
            linear_color_stop(rgb(0xFF3B30), 0.),
            linear_color_stop(rgb(0x2D7FF9), 1.),
        )
    } else {
        rgb(layer.color).into()
    }
}

/// A footer button. Every action the bar item offers lives here.
fn button(
    id: &'static str,
    label: &'static str,
    action: impl Fn(&mut App) + 'static,
) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .flex()
        .justify_center()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(LINE))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0x2C2C2C)).border_color(rgb(ACCENT)))
        .child(label)
        .on_click(move |_, _, cx| action(cx))
}

impl Status {
    fn header(&self) -> Div {
        let r = &self.runtime;
        let (color, link) = match (self.daemon, r.connected, r.transport.as_deref()) {
            (None, ..) => (BAD, "daemon stopped".to_string()),
            (_, false, _) => (BAD, "pad disconnected".to_string()),
            (_, true, Some("bluetooth")) => (0x2D7FF9, "Bluetooth".to_string()),
            (_, true, Some("usb")) => (OK, "USB".to_string()),
            (_, true, _) => (OK, "connected".to_string()),
        };
        let mut status = div()
            .flex()
            .items_center()
            .gap_1p5()
            .text_xs()
            .text_color(rgb(MUTED))
            .child(dot(rgb(color), 7.))
            .child(link);
        if let Some(level) = r.battery.filter(|_| r.connected) {
            let bolt = if r.charging { " ⚡" } else { "" };
            let tint = if level <= 20 { BAD } else { MUTED };
            status = status.child(
                div()
                    .text_color(rgb(tint))
                    .child(format!("· {level}%{bolt}")),
            );
        }
        div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .child("FreeMicro"),
            )
            .child(status)
    }

    fn layer_section(&self) -> Div {
        let r = &self.runtime;
        let current = r.layer.filter(|_| r.connected);
        let active = current.and_then(|n| self.layers.get(n as usize - 1));
        let name = match (current, active) {
            (Some(_), Some(layer)) => layer.name.clone(),
            (Some(n), None) => format!("Layer {n}"),
            (None, _) if r.connected => "checking…".into(),
            (None, _) => "—".into(),
        };
        let front = match (&r.app, r.process.as_deref().map(split_site)) {
            (Some(app), Some((_, Some(site)))) => format!("{app} · {site}"),
            (Some(app), _) => app.clone(),
            _ => String::new(),
        };
        let strip = self.layers.iter().enumerate().fold(
            div().flex().items_center().gap_1(),
            |row, (i, layer)| {
                let on = current == Some(i as u64 + 1);
                row.child(
                    div()
                        .p(px(2.))
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(if on { TEXT } else { BG }))
                        .child(div().size(px(9.)).rounded_full().bg(swatch(layer))),
                )
            },
        );
        let mut head = div().flex().items_center().gap_2();
        if let Some(layer) = active {
            head = head.child(div().size(px(12.)).rounded_sm().bg(swatch(layer)));
        }
        head = head
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(div().flex_1())
            .child(strip);
        let mut detail = div()
            .flex()
            .justify_between()
            .text_xs()
            .text_color(rgb(MUTED))
            .child(div().truncate().child(front));
        if let (Some(n), Some(layer)) = (current, active) {
            let effect = if n == 1 {
                "agent lights"
            } else {
                layer.effect.as_str()
            };
            detail = detail.child(format!("layer {n} · {effect}"));
        }
        section("LAYER").child(head).child(detail)
    }

    fn dial_section(&self) -> Div {
        let r = &self.runtime;
        let known = self.daemon.is_some() && !r.dial.is_empty();
        let modes = DIAL_MODES.iter().fold(
            div()
                .flex()
                .p(px(2.))
                .gap(px(2.))
                .rounded_md()
                .bg(rgb(BG))
                .border_1()
                .border_color(rgb(LINE)),
            |row, mode| {
                let on = known && r.dial == *mode;
                row.child(
                    div()
                        .flex_1()
                        .flex()
                        .justify_center()
                        .py_0p5()
                        .rounded_sm()
                        .text_xs()
                        .bg(rgb(if on { ACCENT } else { BG }))
                        .text_color(rgb(if on { 0x111111 } else { MUTED }))
                        .child(mode.to_string()),
                )
            },
        );
        let level = EFFORT_LEVELS.iter().position(|l| *l == r.effort);
        let bar =
            EFFORT_LEVELS
                .iter()
                .enumerate()
                .fold(div().flex_1().flex().gap_1(), |row, (i, _)| {
                    let filled = level.is_some_and(|l| i <= l);
                    row.child(div().flex_1().h(px(5.)).rounded_full().bg(rgb(if filled {
                        ACCENT
                    } else {
                        LINE
                    })))
                });
        section("DIAL").child(modes.w_full()).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .child(div().text_color(rgb(MUTED)).child("effort"))
                .child(bar)
                .child(div().w(px(48.)).flex().justify_end().child(if known {
                    r.effort.clone()
                } else {
                    "—".into()
                })),
        )
    }

    /// The pad, drawn 1:1 after the Input app's picture: a 4 × 4 grid with
    /// the dial top-left, the stick top-right and the round button
    /// bottom-left. Every key shows what it does on the layer the pad is on.
    fn pad_section(&self) -> Div {
        const CELL: f32 = 74.;
        let r = &self.runtime;
        let current = r.layer.filter(|_| r.connected).unwrap_or(1);
        let Some(layer) = self.layers.get(current as usize - 1) else {
            return section("PAD").child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("No cached keymap yet: it is saved when the daemon connects."),
            );
        };
        let key_at = |row: usize, col: usize| layer.keys.get(row).and_then(|r| r.get(col));
        // "Claude: New chat" on the Claude layer is just "New chat".
        let prefix = format!("{}: ", layer.name);
        let short = |t: &str| t.strip_prefix(prefix.as_str()).unwrap_or(t).to_string();
        let shell = |round: bool| {
            div()
                .size(px(CELL))
                .flex_none()
                .flex()
                .flex_col()
                .justify_center()
                .gap_0p5()
                .p_1p5()
                .overflow_hidden()
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(LINE))
                .when(round, |d| d.rounded_full().items_center())
                .when(!round, |d| d.rounded_lg())
        };
        let key = |code: Option<&String>| {
            let code = code.map_or("", String::as_str);
            if let Some(slot) = code.strip_prefix("KV_OAI_AG") {
                // An Agent Key: its herdr workspace and that agent's state.
                let k = slot.parse::<usize>().ok().and_then(|i| self.keys.get(i));
                let (name, state) = k.map_or(("—".into(), "empty".into()), |k| {
                    (k.name.clone(), k.state.clone())
                });
                return shell(false)
                    .border_color(state_color(&state))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(dot(state_color(&state), 6.))
                            .child(state),
                    );
            }
            match label(code, &self.names) {
                None => shell(false).bg(rgb(BG)),
                Some((title, detail)) => shell(false)
                    .text_ellipsis()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .line_clamp(2)
                            .child(short(&title)),
                    )
                    .children(detail.map(|d| {
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .line_clamp(2)
                            .child(d)
                    })),
            }
        };
        let round = |name: &'static str| {
            shell(true).child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(MUTED))
                    .child(name),
            )
        };
        let row = |cells: Vec<Div>| div().flex().gap(px(6.)).children(cells);
        let grid = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(row(vec![
                round("dial"),
                key(key_at(0, 0)),
                key(key_at(0, 1)),
                round("stick"),
            ]))
            .child(row((0..4).map(|c| key(key_at(1, c))).collect()))
            .child(row((0..4).map(|c| key(key_at(2, c))).collect()))
            .child(row(vec![
                shell(true)
                    .size(px(CELL * 0.6))
                    .m(px(CELL * 0.2))
                    .bg(rgb(BG)),
                key(key_at(3, 0)),
                key(key_at(3, 1)),
                key(key_at(3, 2)),
            ]));
        // The dial and the stick are too small for words: a legend says what they do.
        let name =
            |code: &str| label(code, &self.names).map_or("—".to_string(), |(t, _)| short(&t));
        let dial = match layer.encoders.as_slice() {
            // FreeMicro's dial: the turn steps the mode shown above.
            [cc, ..] if cc.starts_with("KV_OAI_") => {
                let mode = if r.dial.is_empty() {
                    "—"
                } else {
                    r.dial.as_str()
                };
                format!("dial  ⟲ ⟳ step {mode} · press: next mode")
            }
            [cc, cw, clk, ..] => format!(
                "dial  ⟲ {} · ⟳ {} · press {}",
                name(cc),
                name(cw),
                name(clk)
            ),
            _ => "dial  —".into(),
        };
        let stick = if layer.stick.is_empty() {
            format!("stick  FreeMicro {} mode", self.names.stick_mode)
        } else {
            let parts: Vec<String> = layer
                .stick
                .iter()
                .filter_map(|c| label(c, &self.names).map(|(t, _)| short(&t)))
                .collect();
            format!(
                "stick  {}",
                if parts.is_empty() {
                    "—".into()
                } else {
                    parts.join(" · ")
                }
            )
        };
        let legend = |text: String| {
            div()
                .text_xs()
                .text_color(rgb(MUTED))
                .truncate()
                .child(text)
        };
        section(format!("PAD · layer {current} · {}", layer.name))
            .child(grid)
            .child(legend(dial))
            .child(legend(stick))
    }

    fn footer(&self) -> Div {
        let log = self.home.join("logs/daemon.log");
        div()
            .flex()
            .gap_1()
            .child(button("restart", "Restart daemon", |_| {
                let target = format!("gui/{}/com.freemicro.daemon", unsafe { libc::getuid() });
                let _ = Command::new("launchctl")
                    .args(["kickstart", "-k", &target])
                    .spawn();
            }))
            .child(button("log", "Open log", move |_| {
                let _ = Command::new("open")
                    .args(["-a", "Console"])
                    .arg(&log)
                    .spawn();
            }))
            .child(button("quit", "Quit", |cx| cx.quit()))
    }
}

impl Render for Status {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut panel = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .bg(rgb(BG))
            .rounded_xl()
            .border_1()
            .border_color(rgb(LINE))
            .text_color(rgb(TEXT))
            .child(self.header());
        if self.runtime.locked {
            panel = panel.child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(CARD))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child("Mac locked · pad lights off"),
            );
        }
        panel
            .child(self.layer_section())
            .child(self.dial_section())
            .child(self.pad_section())
            .child(self.footer())
    }
}

/// The panel: at most one, opened under the bar icon, gone when it loses focus.
#[derive(Default)]
struct Panel {
    window: Option<AnyWindowHandle>,
    /// A click on the icon blurs the panel first; that click must not reopen it.
    closed_at: Option<Instant>,
}

const PANEL: (f32, f32) = (340., 680.);

/// A display in global points (top-left of the primary display is 0,0), with
/// its backing scale.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Screen {
    id: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

/// Where the panel goes for an icon rect in physical pixels (as tray-icon
/// reports it, scaled by the icon's own display): that display, and the
/// panel's top-left relative to it, centred under the icon and kept on it.
fn place(icon: (f64, f64, f64, f64), screens: &[Screen]) -> Option<(u32, f64, f64)> {
    let (x, y, w, h) = icon;
    screens.iter().find_map(|s| {
        // The centre: the rect itself pokes a few points above the display.
        let (cx, cy) = ((x + w / 2.0) / s.scale, (y + h / 2.0) / s.scale);
        let inside = cx >= s.x && cx < s.x + s.width && cy >= s.y && cy < s.y + s.height;
        inside.then(|| {
            let margin = 8.0;
            let left =
                (cx - s.x - PANEL.0 as f64 / 2.0).clamp(margin, s.width - PANEL.0 as f64 - margin);
            (s.id, left, (y + h) / s.scale - s.y + 4.0)
        })
    })
}

#[repr(C)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut std::ffi::c_void;
    fn CGDisplayModeGetPixelWidth(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeGetWidth(mode: *mut std::ffi::c_void) -> usize;
    fn CGDisplayModeRelease(mode: *mut std::ffi::c_void);
}

fn screens(cx: &App) -> Vec<Screen> {
    cx.displays()
        .iter()
        .map(|d| {
            let id = u32::from(d.id());
            unsafe {
                let b = CGDisplayBounds(id);
                let mode = CGDisplayCopyDisplayMode(id);
                let scale = if mode.is_null() {
                    1.0
                } else {
                    let s = CGDisplayModeGetPixelWidth(mode) as f64
                        / CGDisplayModeGetWidth(mode).max(1) as f64;
                    CGDisplayModeRelease(mode);
                    s
                };
                Screen {
                    id,
                    x: b.x,
                    y: b.y,
                    width: b.width,
                    height: b.height,
                    scale,
                }
            }
        })
        .collect()
}

fn open_panel(cx: &mut App, home: PathBuf, at: (u32, f64, f64), panel: Rc<RefCell<Panel>>) {
    let (display, x, y) = at;
    let display_id = cx
        .displays()
        .into_iter()
        .find(|d| u32::from(d.id()) == display)
        .map(|d| d.id());
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(x as f32), px(y as f32)),
            size(px(PANEL.0), px(PANEL.1)),
        ))),
        titlebar: None,
        kind: WindowKind::PopUp,
        is_movable: false,
        display_id,
        window_background: gpui::WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    let on_blur = panel.clone();
    let handle = cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            cx.observe_window_activation(window, move |_, window, _| {
                if !window.is_window_active() {
                    let mut panel = on_blur.borrow_mut();
                    panel.window = None;
                    panel.closed_at = Some(Instant::now());
                    window.remove_window();
                }
            })
            .detach();
            cx.spawn(async move |this, cx| loop {
                let alive = this.update(cx, |s: &mut Status, cx| {
                    s.refresh();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            })
            .detach();
            let mut status = Status {
                home,
                daemon: None,
                runtime: Runtime::default(),
                layers: Vec::new(),
                names: Names::default(),
                keys: Vec::new(),
            };
            status.refresh();
            status
        })
    });
    if let Ok(handle) = handle {
        panel.borrow_mut().window = Some(handle.into());
        cx.activate(true);
    }
}

/// A Lucide icon (ISC licence), rendered black-on-clear for a template image.
fn lucide(svg: &[u8]) -> Icon {
    const SIDE: u32 = 36; // 18 pt at 2x; AppKit scales the image to 18 pt.
    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default()).unwrap();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIDE, SIDE).unwrap();
    let scale = SIDE as f32 / tree.size().width();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Icon::from_rgba(pixmap.take(), SIDE, SIDE).unwrap()
}

/// Bar icon and tooltip for the daemon's current state.
fn bar_state(home: &Path) -> (bool, String) {
    let Some(pid) = daemon_pid(home) else {
        return (false, "FreeMicro: daemon stopped".into());
    };
    let r = read_json(&home.join("runtime.json"))
        .map(|v| runtime_from(&v))
        .unwrap_or_default();
    if !r.connected {
        return (false, format!("FreeMicro: pad disconnected (daemon {pid})"));
    }
    let mut parts = vec![format!("FreeMicro: {}", r.transport.unwrap_or_default())];
    if let Some(layer) = r.layer {
        parts.push(format!("layer {layer}"));
    }
    if let Some(level) = r.battery {
        parts.push(format!("battery {level}%"));
    }
    parts.push(format!("dial {} · effort {}", r.dial, r.effort));
    (true, parts.join(" · "))
}

fn main() {
    let home = PathBuf::from(std::env::var("HOME").unwrap()).join(".freemicro");
    Application::new().run(move |cx: &mut App| {
        let mtm = MainThreadMarker::new().unwrap();
        // A bar item only: no Dock icon, no app menu.
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let live = lucide(include_bytes!("../assets/keyboard.svg"));
        let down = lucide(include_bytes!("../assets/keyboard-off.svg"));
        // No menu: either click opens the panel, which holds every action.
        let tray = TrayIconBuilder::new()
            .with_icon(down.clone())
            .with_icon_as_template(true)
            .with_tooltip("FreeMicro")
            .build()
            .unwrap();
        let panel = Rc::new(RefCell::new(Panel::default()));
        cx.spawn(async move |cx| {
            let mut shown: Option<bool> = None;
            let mut checked = Instant::now() - Duration::from_secs(1);
            loop {
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    let TrayIconEvent::Click {
                        rect,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    else {
                        continue;
                    };
                    let open = panel.borrow().window;
                    if let Some(window) = open {
                        let _ = cx.update_window(window, |_, window, _| window.remove_window());
                        panel.borrow_mut().window = None;
                        continue;
                    }
                    let just_closed = panel
                        .borrow()
                        .closed_at
                        .is_some_and(|t| t.elapsed() < Duration::from_millis(300));
                    if just_closed {
                        continue;
                    }
                    let icon = (
                        rect.position.x,
                        rect.position.y,
                        rect.size.width as f64,
                        rect.size.height as f64,
                    );
                    let (home, panel) = (home.clone(), panel.clone());
                    let _ = cx.update(|cx| {
                        if let Some(at) = place(icon, &screens(cx)) {
                            open_panel(cx, home, at, panel);
                        }
                    });
                }
                if checked.elapsed() >= Duration::from_secs(1) {
                    checked = Instant::now();
                    let (up, tip) = bar_state(&home);
                    if shown != Some(up) {
                        let icon = if up { live.clone() } else { down.clone() };
                        let _ = tray.set_icon_with_as_template(Some(icon), true);
                        shown = Some(up);
                    }
                    let _ = tray.set_tooltip(Some(tip));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_panel_opens_under_the_icon_on_the_icon_display() {
        // The user's desk: the built-in Retina display, an HP monitor above it.
        let builtin = Screen {
            id: 1,
            x: 0.0,
            y: 0.0,
            width: 1512.0,
            height: 982.0,
            scale: 2.0,
        };
        let hp = Screen {
            id: 2,
            x: -1180.0,
            y: -1200.0,
            width: 1920.0,
            height: 1200.0,
            scale: 1.0,
        };
        let screens = [builtin, hp];
        // Icon in the HP menu bar (scale 1), as macOS reports it: the rect
        // starts 2 pt above the display. Centred under it, below the bar.
        assert_eq!(
            place((4.0, -1202.0, 34.0, 33.0), &screens),
            Some((2, 1031.0, 35.0))
        );
        // Icon on the Retina bar (scale 2, so physical = 2x points).
        assert_eq!(
            place((1800.0, 0.0, 72.0, 48.0), &screens),
            Some((1, 748.0, 28.0))
        );
        // Near the right edge: pulled back on the display.
        assert_eq!(
            place((2960.0, 0.0, 40.0, 48.0), &screens),
            Some((1, 1164.0, 28.0))
        );
    }

    #[test]
    fn keys_are_labelled_from_the_pad_keymap_and_freemicro_bindings() {
        let pad = json!({
            "macros": [{"id": 22, "name": "Claude: New chat"}],
            "multiActions": [{"id": 1, "name": "Frame/AutoLayout"}],
        });
        let freemicro = json!({
            "keycaps": {"ACT06": "FAST"},
            "bindings": {
                "ACT06": {"label": "FAST - cycle permission mode"},
                "ACT08": {"label": "herdr previous tab"},
            },
        });
        let names = names_from(&pad, &freemicro);
        let l = |code: &str| label(code, &names);
        let both = |t: &str, d: &str| Some((t.to_string(), Some(d.to_string())));
        assert_eq!(l("KV_OAI_ACT06"), both("FAST", "cycle permission mode"));
        assert_eq!(
            l("KV_OAI_ACT08"),
            both("ACT08", "herdr previous tab"),
            "no keycap"
        );
        assert_eq!(l("KA_A22"), both("Claude: New chat", "macro"));
        assert_eq!(l("KA_M1"), both("Frame/AutoLayout", "multi-action"));
        assert_eq!(l("KC_LSFT"), both("Shift", "key"));
        assert_eq!(l("SA_3"), Some(("Smart action 3".into(), None)));
        assert_eq!(l("KC_NONE"), None);
        assert_eq!(l("KI_X"), None);
    }

    #[test]
    fn layers_follow_the_active_profile_and_resolve_linked_apps() {
        let keymap = json!({
            "activeProfileId": 0,
            "linkedApps": [{"id": 4, "name": "MINT-web",
                            "process": "com.apple.Safari/example.org"}],
            "profiles": [{"layers": [
                {"name": "ChatGPT", "color": 0xFF0000, "linkedAppId": 9},
                {"name": "MINT", "linkedAppId": 4,
                 "lights": {"backlight": {"effect": "solid", "color": 0xFFFFFF},
                            "underglow": {"effect": "snake", "color": 0x00FF9C}}},
            ]}],
        });
        let layers = layers_from_keymap(&keymap);
        assert_eq!(layers[0].process, None, "an unknown app id");
        assert_eq!(layers[0].color, 0xFF0000, "layer colour without lights");
        assert_eq!(
            layers[1],
            Layer {
                name: "MINT".into(),
                process: Some("com.apple.Safari/example.org".into()),
                app: Some("MINT".into()),
                color: 0x00FF9C,
                effect: "snake".into(),
                keys: Vec::new(),
                encoders: Vec::new(),
                stick: Vec::new(),
            }
        );
        assert_eq!(
            split_site(layers[1].process.as_deref().unwrap()),
            ("com.apple.Safari", Some("example.org"))
        );
    }
}
