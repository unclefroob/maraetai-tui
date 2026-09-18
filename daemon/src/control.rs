//! The daemon's own control interface (`com.maraetai.Daemon1`), for
//! everything MPRIS doesn't cover: loading a queue of tracks, and explicit
//! shutdown. `Next`/`Previous` are *not* here — they're real MPRIS methods
//! (see `mpris.rs`), since the queue lives in the daemon precisely so that
//! MPRIS's Next/Previous (callable by hardware media keys, with no TUI
//! involved) actually work.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use zbus::interface;

use crate::playback::{PlaybackHandle, Status, TrackMeta};

/// One queue entry as it crosses D-Bus: (stream_url, title, artist, album,
/// art_url, duration_secs). A plain tuple rather than a named struct because
/// zbus/zvariant encode it identically either way (`a(sssssd)`), and a tuple
/// needs no extra type wiring on either side of the connection.
pub type QueueEntry = (String, String, String, String, String, f64);

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

fn to_track_meta(entry: QueueEntry) -> TrackMeta {
    let (stream_url, title, artist, album, art_url, duration_secs) = entry;
    TrackMeta {
        stream_url,
        title,
        artist,
        album,
        art_url: (!art_url.is_empty()).then_some(art_url),
        duration: (duration_secs > 0.0).then(|| Duration::from_secs_f64(duration_secs)),
    }
}

#[interface(name = "com.maraetai.Daemon1")]
impl ControlInterface {
    /// Replaces the queue and starts playing at `start_index` immediately.
    /// A single-track "play just this" is simply a one-entry queue with
    /// `start_index: 0`.
    async fn play_queue(&self, tracks: Vec<QueueEntry>, start_index: u32) {
        let tracks = tracks.into_iter().map(to_track_meta).collect();
        self.playback.play_queue(tracks, start_index as usize);
    }

    /// Jumps directly to `index` within the *current* queue — for a TUI
    /// "Queue" view where the user picks an arbitrary upcoming track,
    /// distinct from `PlayQueue` (which replaces the queue).
    async fn play_at(&self, index: u32) {
        self.playback.play_at(index as usize);
    }

    /// The full current queue: (title, artist, album, duration_secs) per
    /// track, in order — for a TUI "Queue" view. Not a property (like
    /// MPRIS's `Metadata`) since it's a list, not a single value, and this
    /// project doesn't implement MPRIS's TrackList interface.
    async fn queue(&self) -> Vec<(String, String, String, f64)> {
        self.playback
            .snapshot()
            .queue
            .into_iter()
            .map(|t| (t.title, t.artist, t.album, t.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0)))
            .collect()
    }

    /// Also exposed here (identical to MPRIS's own `Next`) purely so the TUI
    /// only needs one D-Bus connection/proxy — real hardware media keys go
    /// through the standard `org.mpris.MediaPlayer2.Player.Next` in
    /// `mpris.rs`, which calls the exact same `PlaybackHandle::next()`.
    async fn next(&self) {
        self.playback.next();
    }

    async fn previous(&self) {
        self.playback.previous();
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

    /// A compact status summary for `maraetai daemon status` and the TUI's
    /// now-playing bar: playback status, current track (title, artist,
    /// album), position/duration in seconds (duration 0 if unknown),
    /// (queue index, queue length), and volume (0.0-1.0). Kept as a plain
    /// method (not properties) since it's a point-in-time snapshot read by a
    /// one-shot CLI command or a polling loop, not something a D-Bus client
    /// watches for changes — that's what MPRIS's properties (which do emit
    /// `PropertiesChanged`) are for.
    #[allow(clippy::type_complexity)]
    async fn status(&self) -> (String, String, String, String, f64, f64, u32, u32, f64) {
        let snap = self.playback.snapshot();
        let status = match snap.status {
            Status::Playing => "playing",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        };
        let (title, artist, album, duration) = match snap.track {
            Some(t) => (t.title, t.artist, t.album, t.duration.map(|d| d.as_secs_f64()).unwrap_or(0.0)),
            None => (String::new(), String::new(), String::new(), 0.0),
        };
        (
            status.to_string(),
            title,
            artist,
            album,
            snap.position.as_secs_f64(),
            duration,
            snap.queue_index as u32,
            snap.queue_len as u32,
            snap.volume as f64,
        )
    }

    /// Begins graceful daemon shutdown — stops playback, releases both D-Bus
    /// names, and exits the process. This is the explicit "kill it" path
    /// (`maraetai daemon stop`); the idle timer is the automatic one.
    async fn quit(&self) {
        self.shutdown.notify_one();
    }
}
