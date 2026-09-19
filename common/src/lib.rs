//! Shared types between `maraetaid` (the daemon) and `maraetai` (the TUI):
//! config/credential loading, Subsonic auth-token generation, D-Bus naming,
//! and runtime-path resolution. Keeping these here means the two binaries
//! cannot drift apart on how they authenticate or where they find each other.

pub mod auth;
pub mod config;
pub mod dbus;
pub mod error;
pub mod paths;
pub mod spectrum;

pub use config::{Config, Credentials};
pub use error::{Error, Result};
