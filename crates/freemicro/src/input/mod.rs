//! Pad input: keymap.json parsing, bindings, actions, chords, layers,
//! joystick, synthetic keystrokes (CGEvent), smart actions and the
//! frontmost-app forwarder. Port of `freemicro/padconfig.py` and
//! `freemicro/input/*`.
//!
//! Owner: input. Depends on `state` (AgentState, AgentKeyOwnership,
//! AgentKeysConfig), `runtime::lighting_config` (StateLight /
//! ActivityLight / LightingConfig types and their parsers), `herdr` and
//! `focus` (the `herdr` and `focus_session` action arms), and `device`
//! (`PadIo`, `PadInfo` for the focus forwarder).

pub mod actions;
pub mod bridge;
pub mod config;
pub mod frontmost;
pub mod keys;
pub mod latch;
pub mod pointer;
pub mod quartz;
pub mod smartactions;
