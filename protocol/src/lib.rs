//! Warden's wire protocol (docs/protocol.md): the requests, responses and
//! events that travel over the apps' control sockets and wardend's socket as
//! newline-delimited JSON, and where those sockets live.
//!
//! The `warden` binary and the GUI both build against these types, so the
//! two cannot drift apart. The crate is serde only: no I/O, no runtime, no
//! system calls.

#![forbid(unsafe_code)]

pub mod control;
pub mod events;
pub mod paths;

use serde::{Deserialize, Serialize};

/// Log level (`[logging] level`, `warden log-level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}
