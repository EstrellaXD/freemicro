//! FreeMicro runtime daemon, ported from the Python package `freemicro`.
//!
//! See `ARCHITECTURE.md` for the module map, the shared traits and the
//! threading model. Config and state live in the same `~/.freemicro`
//! (`FREEMICRO_HOME`) as the Python build, so the two are drop-in compatible.

pub mod cli;
pub mod clock;
pub mod config;
pub mod device;
pub mod focus;
pub mod herdr;
pub mod input;
pub mod permission_prompt;
pub mod runtime;
pub mod state;

#[cfg(test)]
pub mod testutil;
