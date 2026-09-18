//! Shared D-Bus naming so the daemon and TUI can't drift apart on where to
//! find each other. The daemon runs **two** separate D-Bus connections:
//! - the MPRIS surface, via the `mpris-server` crate, which owns its own
//!   connection and constructs its bus name as `org.mpris.MediaPlayer2.<suffix>`
//!   internally (MPRIS requires that exact prefix, and requires the object
//!   path be exactly `/org/mpris/MediaPlayer2` — both fixed by the spec, not
//!   something this project can choose);
//! - this project's own control interface, on a second, separate connection
//!   under our own bus name/path, for everything MPRIS doesn't cover.
//!
//! Two connections instead of one is a deliberate simplification — the
//! `mpris-server` crate owns its connection fully and doesn't expose a way to
//! layer another interface onto it in its stable API — traded for not
//! fighting that crate's ownership model.

/// The `<suffix>` in `org.mpris.MediaPlayer2.<suffix>`, which is the MPRIS
/// server's actual well-known bus name.
pub const MPRIS_BUS_NAME_SUFFIX: &str = "maraetai";

/// This project's own control interface's well-known bus name.
pub const CONTROL_BUS_NAME: &str = "com.maraetai.Daemon";

/// Object path the control interface is served at.
pub const CONTROL_OBJECT_PATH: &str = "/com/maraetai/Daemon";

/// This project's own control interface name (versioned so a future
/// incompatible change doesn't silently mismatch an old TUI against a new
/// daemon or vice versa).
pub const CONTROL_INTERFACE: &str = "com.maraetai.Daemon1";
