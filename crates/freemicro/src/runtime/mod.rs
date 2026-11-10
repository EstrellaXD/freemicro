//! The daemon proper: LED rendering, lighting ownership and ChatGPT
//! coexistence, the pad lock, launchd install code, keymap pull/push, and the
//! run loop. Port of `renderers/micro_leds.py`, `device/lighting.py`,
//! the lighting part of `padconfig.py`, `lighting_owner.py`, `daemon.py`,
//! `padkeymap.py` and `cli.py::_run_pipeline`.
//!
//! Owner: runtime (also owns `cli.rs`, `main.rs`, `lib.rs`, `clock.rs`,
//! `testutil.rs`, `Cargo.toml`). Depends on every other module.

pub mod daemon;
pub mod keymap;
pub mod leds;
pub mod lighting;
pub mod lighting_config;
pub mod owner;
pub mod run;
