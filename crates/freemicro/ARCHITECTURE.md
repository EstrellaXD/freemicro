# freemicro-rs architecture

Rust port of the FreeMicro **runtime daemon**. The source of truth is the
Python package `../../src/freemicro`: this port copies its behaviour, not its
style. The binary is `freemicro-rs`. Config and state are shared with Python
(`~/.freemicro`, overridden by `FREEMICRO_HOME`), so either build can run
against the files of the other.

## 1. Module map (Python -> Rust)

| Python | Rust | Owner |
|---|---|---|
| `device/codex_micro.py` (constants, `Device.send/request/self_test`) | `src/device/mod.rs` | device |
| `codex_micro.frame_message`, `FrameDecoder` | `src/device/frame.rs` | device |
| `codex_micro` ctypes IOKit (`_matching_service`, `open_device`, `stream`) | `src/device/iokit.rs` | device |
| (test fake) | `src/device/fake.rs` | device |
| `device/padfs.py` | `src/device/padfs.rs` | device |
| `config.py` | `src/config.rs` | state |
| `state/engine.py` (`AgentState`, `SessionState`, `DecayPolicy`, signals) | `src/state/mod.rs` | state |
| `state/engine.py` (`StateStore`, `claim_ttl`, `effective_state`, `as_read`) | `src/state/store.rs` | state |
| `state/engine.py` (tty / pid probes, `ProcessLiveness`) | `src/state/process.rs` | state |
| `state/hooks.py`, `cli.cmd_hook`, `cli._apply_hook_event`, `cli._log_raw_event` | `src/state/hooks.rs` | state |
| `state/slots.py` | `src/state/slots.rs` | state |
| `agentkeys.py` (incl. `AgentKeyOwnership`) | `src/state/agentkeys.rs` | state |
| `padconfig.py` (all but the lighting section) | `src/input/config.rs` | input |
| `input/actions.py` (`Action`, `Backend`, `RecordingBackend`, perform) | `src/input/actions.rs` | input |
| `input/bridge.py` | `src/input/bridge.rs` | input |
| `input/latch.py` | `src/input/latch.rs` | input |
| `input/pointer.py` (no `PointerLoop` thread) | `src/input/pointer.rs` | input |
| `input/keys.py` | `src/input/keys.rs` | input |
| `input/quartz.py`, `actions.AppleScriptBackend`, `actions.CGEventBackend` | `src/input/quartz.rs` | input |
| `input/frontmost.py` (incl. `PadFocusForwarder`) | `src/input/frontmost.rs` | input |
| `input/smartactions.py` | `src/input/smartactions.rs` | input |
| `herdr.py` | `src/herdr.rs` | herdr+focus |
| `focus.py` | `src/focus.rs` | herdr+focus |
| `device/lighting.py` | `src/runtime/lighting.rs` | runtime |
| `padconfig.py` lighting section (`StateLight`, `ActivityLight`, `LightingConfig`, themes, battery, reassert) | `src/runtime/lighting_config.rs` | runtime |
| `renderers/micro_leds.py` | `src/runtime/leds.rs` | runtime |
| `lighting_owner.py` (`ActivityOverlay`, `LightingOwner`, coexistence advice) | `src/runtime/owner.rs` | runtime |
| `daemon.py` (`PadLock`, plists, launchctl) | `src/runtime/daemon.rs` | runtime |
| `padkeymap.py` | `src/runtime/keymap.rs` | runtime |
| `cli._run_pipeline`, `device.run_with_reconnect`, `staleness.ConfigWatcher` (mtime part) | `src/runtime/run.rs` | runtime |
| `cli.py` (the subcommands below) | `src/cli.rs`, `src/main.rs` | runtime |
| (time helpers, test temp dir) | `src/clock.rs`, `src/testutil.rs` | runtime |

There is one deliberate move from the task's grouping. The lighting types of
`padconfig.py` go to `runtime/lighting_config.rs`, next to the wire builders
they share `parse_color` and `parse_effect` with. `input::config::parse`
calls `runtime::lighting_config::parse_lighting` and
`parse_activity_light`. This also makes the input module a little smaller,
because it is the largest module.

## 2. Dependency direction

```
           runtime  (run loop, cli, leds, owner, daemon, keymap, lighting*)
          /   |    \
     input  herdr+focus
       |  \   /   |
       |  state   |
        \   |    /
         device   (leaf)        state (leaf: uses only config, clock)
```

- `device` and `state` are leaves. They never import `input`, `herdr`,
  `focus` or `runtime`.
- `input` uses `state` (`AgentKeyOwnership`, `AgentKeysConfig`,
  `AgentState`), `device` (`PadIo`, `PadInfo` for the focus forwarder),
  `runtime::lighting_config` (types and parsers only), and `herdr` / `focus`
  for the `herdr` and `focus_session` action arms.
- `herdr` uses `state`. `focus` uses `state`, `herdr` and
  `input::actions::Backend`.
- `runtime` uses everything.

## 3. Shared types and traits

| Item | Where | Purpose |
|---|---|---|
| `trait PadIo { info, send, poll(timeout) }` | `device` | The only way to talk to the pad. Real: `iokit::HidPad`. Tests: `fake::FakePad` (always compiled). |
| `PadInfo { transport, product_id }`, `Framing` | `device` | `is_creator_micro_2()` selects the 64-byte prefixed frame with no CRLF. |
| `device::request(io, &Request, stray)` | `device` | Send, then wait for the reply on the same thread. Every other message goes to `stray`, so a mid-loop `device.status` loses no key event. |
| `PadFs<'a>` | `device` | `fs.readbin` / `fs.writebin` / `fs.list` with checksum; long envelope; ids 500..999; `gap` field (zero in tests). |
| `AgentState`, `SessionState`, `DecayPolicy`, `TerminalInfo`, `SessionSignals` | `state` | Fully defined in the scaffold. `SessionState::to_json` writes the 16 Python keys. |
| `StateStore`, `SessionMeta`, `ProcessProbe`, `ProcessLiveness` | `state` | The store takes a probe so tests do not depend on real pids. |
| `AgentKeysConfig`, `AgentSlot`, `resolve_slots`, `SlotResolver` | `state::agentkeys` | Slot resolution. |
| `AgentKeyOwnership` | `state::agentkeys` | One instance in the run loop. It is passed by `&` to `Bridge::handle` and `LedRenderer::render`, so press gating and lighting cannot disagree. |
| `PadConfig`, `JoystickConfig`, `Bindings` | `input::config` | The host keymap. |
| `Action { kind: ActionKind, params, label, comment, light }` | `input::actions` | `ActionKind::Unsupported(name)` keeps Python-only kinds loadable. |
| `trait Backend` (11 methods) | `input::actions` | Synthetic input. Real: `quartz::CGEventBackend`, `quartz::AppleScriptBackend`. Dry run and tests: `RecordingBackend`. |
| `ActionEnv { home, herdr }` | `input::actions` | What `focus_session` and `herdr` actions need. |
| `Bridge`, `Dispatch`, `InputEvent`, `ActivityNote` | `input::bridge` | Events in, dispatches out. `next_deadline()` and `expire(now)` replace the timer threads. |
| `Herdr { socket }` | `herdr` | One connection per request. Tests bind a `UnixListener` in a temp dir. |
| `FocusPlan`, `plan_for_slot`, `perform` | `focus` | Focus a session's terminal. |
| `StateLight`, `ActivityLight`, `LightingConfig`, `Zone` | `runtime` | Lighting types. |
| `trait Launchctl`, `trait PadSource` | `runtime` | Seams that keep tests away from launchd and the device. |

**No `Clock` trait.** Components take `now: f64` as an argument instead.
Wall seconds are used for persisted values (`updated_at`, `started`), which
match Python's `time.time()` floats. Monotonic seconds are used for intervals
and deadlines. Only `run.rs`, `cli.rs` and the hook entry read the real clock,
through `clock::wall()` and `clock::mono()`. A test passes literal times and
needs no fake object.

**Explicit paths.** Library functions take `home: &Path` (and `user_home`
for `~/Library/LaunchAgents`). Only `cli.rs` / `run.rs` call
`config::config_home()`. Environment variables are process-global and tests
run in parallel, so tests never set `FREEMICRO_HOME`, `HERDR_SOCKET_PATH`
or `FREEMICRO_KEYMAP`.

## 4. Threading model

- **One pad-owner thread: the process main thread.** It holds the only
  `Box<dyn PadIo>`. It does all pad writes (LED frames, `host.focused_app`,
  `device.status`, file operations) and all pad reads. No other thread gets
  the pad, a sender for it, or a lock around it. Two concurrent writers once
  bricked a pad, so the type system enforces this: `PadIo` is not `Send` in
  the IOKit implementation, and nothing hands it out.
- The main thread is required. `HidPad::poll` pumps the CFRunLoop of the
  thread that opened the device, and NSWorkspace's `frontmostApplication`
  updates from the main run loop.
- **The loop has no timer threads.** Each pass calls
  `io.poll(min(next tick, bridge.next_deadline(), overlay.next_deadline()))`,
  then `bridge.expire(now)`. This replaces Python's `SettleTimer` (chord
  settle, hold cap, latch, double tap) and `PointerLoop` (pointer ticks at
  `tick_hz`). Thus the bridge needs no `Mutex`.
- **Background threads are used only for herdr.** `herdr::run_in_background`
  runs a preset on a detached thread. Such a thread touches only the herdr
  socket, never the pad and never shared state. The dial debounce state is a
  `static Mutex`.
- Actions (`osascript`, CGEvent posts, `open`) run inline on the main thread,
  as in Python.
- **Cross-process exclusion uses the same `flock` as Python:**
  `<home>/pad.lock`, payload `{"pid","role","started"}`. `keymap push` also
  pauses the launchd job before it takes the lock.

## 5. Compatibility contract with the Python build

- State files: `<home>/state/<safe id>.json`, written by tempfile + rename,
  with exactly the keys of Python `SessionState.to_dict()`.
  `<home>/slots.json` = `{"slots": [6 strings]}`.
- `<home>/config.json` (`state` timers) and `<home>/keymap.json`. A keymap
  that Python accepts must load here. An unknown or Python-only binding kind
  gives a warning and a no-op. It is never a load error.
- `<home>/pad.lock`, `<home>/logs/daemon.log`, `<home>/keymap-backups/`, and
  `<home>/status.json` (battery cache).
- launchd labels `com.freemicro.daemon` and `com.freemicro.onconnect`, so
  the two builds are alternatives for the same job, and Python's
  `keymap push` pause works on the Rust daemon.
- Wire JSON is compact and ASCII-escaped (`device::frame::to_ascii_json`).
  `serde_json` has `preserve_order`, so a pulled keymap re-encodes to the same
  bytes.

## 6. CLI (scope)

`run`, `daemon run|install [--on-connect]|uninstall|status [--on-connect]`,
`hook`, `keymap pull [OUT]|push [FILE] [--test]`, `status [--json]` (state
only, no device), `version`. `--interval` defaults to 1.0 s, as in Python.
To point Claude Code's hooks at `freemicro-rs hook`, edit the settings by
hand. This port has no hook installer.

Runtime notes:

- The LED renderer never holds the pad. `render` and `apply_config` take the
  pad-owner's `&mut dyn PadIo`, plus the monotonic `now` (dim, flash, blink,
  backoff) and the `wall` time (slot resolution). Write-failure notices are
  queued and printed by the run loop.
- With `lighting.battery.enabled`, the run loop asks the pad for
  `device.status` every `battery.poll_seconds` (never while keys arrive) and
  writes `<home>/status.json`, the cache the battery cue reads. Key events
  that arrive during the round trip are handled after it.
- `keymap::with_pad` opens the pad through `run::PadSource`, so tests use a
  fake source and a fake `Launchctl`.

## 7. Out of scope

These Python parts are not ported:

- The web UI, the menu bar, onboarding, doctor, selftest and the uninstall
  wizard (`webui/`, `menubar/`, `onboarding.py`, `diagnostics.py`,
  `selftest.py`, `uninstall.py`, `permissions.py`).
- `hooks_install.py` (writes `~/.claude/settings.json`).
- `alerts.py` (off-pad sounds and notifications).
- `permission_prompt.py` and the `answer_permission` action. The action
  loads as `Unsupported`.
- `staleness.py` code watcher, self-restart and `reclaim_stale_lock`. `flock`
  has no stale state. The mtime config reload is kept in `run.rs`.
- `renderers/base.py` registry (there is only the LED renderer),
  `detector/`, `compat.py`, and the `lights`, `config`, `keys`,
  `demo`, `emit`, `render` and `update` commands.
- `lighting_owner.contention` (doctor-only conflict report).

## 8. Rules for every owner

- Edit only the files your module owns (see the module list). If you need a
  change in another module's signature, ask the integrator. Do not edit the
  other file.
- Never open, probe or write the physical pad. Never run `run`, `daemon`,
  `keymap`, or the binary at all. Tests use `FakePad`, `RecordingBackend`,
  fake `ProcessProbe`, fake `Launchctl`, temp dirs (`testutil::TempDir`)
  and a temp-dir `UnixListener` for herdr.
- `cargo fmt`, then `cargo clippy --all-targets -- -D warnings`, then
  `cargo test`. Use standard `#[test]` names. Add a test only where the code
  branches, and parametrize similar cases. Use the scratchpad probe logs as
  wire fixtures.
- Dependencies are fixed in `Cargo.toml`: `serde_json` (`preserve_order`),
  `serde` (derive), `base64`, `sha1`, `libc`, `anyhow`. All OS frameworks
  use hand-written FFI.
- The crate-level `#![allow(...)]` in `lib.rs` is temporary. The Integrate
  phase removes it.
