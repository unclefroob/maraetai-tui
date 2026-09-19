//! Client-side proxy for the daemon's `com.maraetai.Daemon1` control
//! interface. The service/path/interface names are hardcoded literals here
//! (the `#[proxy]` macro needs literals, not `const` references) — the
//! `service_matches_common_dbus_constants` test below guards against these
//! ever drifting from `maraetai_common::dbus`, which stays the source of
//! truth for what the *daemon* actually registers.

use zbus::Connection;
use zbus::proxy;

/// One queue entry: (stream_url, title, artist, album, art_url,
/// duration_secs, format_label, lossless) — must match
/// `daemon::control::QueueEntry` exactly.
pub type QueueEntry = (String, String, String, String, String, f64, String, bool);

#[proxy(
    interface = "com.maraetai.Daemon1",
    default_service = "com.maraetai.Daemon",
    default_path = "/com/maraetai/Daemon"
)]
pub trait Control {
    async fn play_queue(&self, tracks: Vec<QueueEntry>, start_index: u32) -> zbus::Result<()>;
    async fn play_at(&self, index: u32) -> zbus::Result<()>;
    #[allow(clippy::type_complexity)]
    async fn queue(&self) -> zbus::Result<Vec<(String, String, String, f64, String, bool)>>;
    async fn spectrum(&self) -> zbus::Result<Vec<u8>>;
    async fn next(&self) -> zbus::Result<()>;
    async fn previous(&self) -> zbus::Result<()>;
    async fn pause(&self) -> zbus::Result<()>;
    async fn resume(&self) -> zbus::Result<()>;
    async fn stop(&self) -> zbus::Result<()>;
    async fn seek_to(&self, position_secs: f64) -> zbus::Result<()>;
    async fn set_volume(&self, volume: f64) -> zbus::Result<()>;
    #[allow(clippy::type_complexity)]
    async fn status(&self) -> zbus::Result<(String, String, String, String, f64, f64, u32, u32, f64, String, bool, String)>;
    async fn quit(&self) -> zbus::Result<()>;
}

/// Connects to the running daemon's control interface. Returns an error if
/// no daemon currently owns the bus name — callers use this to decide
/// whether to auto-spawn one (see `lifecycle::ensure_daemon_running`).
pub async fn connect(connection: &Connection) -> zbus::Result<ControlProxy<'_>> {
    ControlProxy::new(connection).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn service_matches_common_dbus_constants() {
        assert_eq!(maraetai_common::dbus::CONTROL_BUS_NAME, "com.maraetai.Daemon");
        assert_eq!(maraetai_common::dbus::CONTROL_OBJECT_PATH, "/com/maraetai/Daemon");
        assert_eq!(maraetai_common::dbus::CONTROL_INTERFACE, "com.maraetai.Daemon1");
    }
}
