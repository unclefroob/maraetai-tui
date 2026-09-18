//! The daemon's own control interface (`com.maraetai.Daemon1`), for
//! everything MPRIS doesn't cover: starting playback of a specific track
//! (MPRIS has no "load and play this URL with this metadata" concept beyond
//! the barely-standardized `OpenUri`), and explicit shutdown. Queue
//! management (what's "next") intentionally isn't here yet — v1's TUI drives
//! one track at a time; see the plan doc's Scope: OUT.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use zbus::interface;

use crate::playback::{PlaybackHandle, Status, TrackMeta};

pub struct ControlInterface {
    playback: PlaybackHandle,
    /// Notified by `quit()` — `main.rs` awaits this to begin graceful
    /// shutdown (stop the audio thread, release both D-Bus names, exit).
    /// Kept separate from `PlaybackHandle::shutdown()` (which only stops the
    /// *audio thread*) because quitting the daemon is a bigger action than
    /// stopping playback.
    shutdown: Arc<Notify>,
}

impl ControlInterface {
    pub fn new(playback: PlaybackHandle, shutdown: Arc<Notify>) -> Self {
        Self { playback, shutdown }
    }
}

#[interface(name = "com.maraetai.Daemon1")]
impl ControlInterface {
    /// Starts streaming and playing `stream_url` immediately. `art_url`/
    /// `duration_secs` may be empty/zero if unknown.
    async fn play_url(
        &self,
        stream_url: String,
        title: String,
        artist: String,
        album: String,
        art_url: String,
        duration_secs: f64,
    ) {
        let meta = TrackMeta {
            title,
            artist,
            album,
            art_url: (!art_url.is_empty()).then_some(art_url),
            duration: (duration_secs > 0.0).then(|| Duration::from_secs_f64(duration_secs)),
        };
        self.playback.play(stream_url, meta);
    }

    async fn pause(&self) {
        self.playback.pause();
    }

    async fn resume(&self) {
        self.playback.resume();
    }

    async fn stop(&self) {
        self.playback.stop();
    }

    /// Seeks to an absolute position, in seconds — distinct from MPRIS's
    /// `Seek` (which is a relative offset).
    async fn seek_to(&self, position_secs: f64) {
        self.playback
            .seek(Duration::from_secs_f64(position_secs.max(0.0)));
    }

    async fn set_volume(&self, volume: f64) {
        self.playback.set_volume(volume.clamp(0.0, 1.0) as f32);
    }

    /// A compact status summary for `maraetai daemon status`: playback
    /// status, current track title (empty if none), and position in
    /// seconds. Kept as a plain method (not properties) since it's a
    /// point-in-time snapshot read by a one-shot CLI command, not something a
    /// D-Bus client watches for changes — that's what MPRIS's properties
    /// (which do emit `PropertiesChanged`) are for.
    async fn status(&self) -> (String, String, f64) {
        let snap = self.playback.snapshot();
        let status = match snap.status {
            Status::Playing => "playing",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        };
        let title = snap.track.map(|t| t.title).unwrap_or_default();
        (status.to_string(), title, snap.position.as_secs_f64())
    }

    /// Begins graceful daemon shutdown — stops playback, releases both D-Bus
    /// names, and exits the process. This is the explicit "kill it" path
    /// (`maraetai daemon stop`); the idle timer is the automatic one.
    async fn quit(&self) {
        self.shutdown.notify_one();
    }
}
