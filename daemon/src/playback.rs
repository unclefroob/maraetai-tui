//! The playback engine: a dedicated OS thread owning the audio output device,
//! decode pipeline, and the queue. It's a plain thread (not async) because
//! `rodio`'s `OutputStream`/`Sink` wrap a `cpal` audio callback that must
//! live on the thread that created it — bridging that into the daemon's
//! async (tokio) world happens via two channels: a `Command` channel in, and
//! either a shared `Snapshot` (for cheap, frequent reads like MPRIS's
//! `Position` getter) or an `Event` channel out (for state transitions worth
//! reacting to, like emitting an MPRIS `PropertiesChanged` signal).
//!
//! The queue lives here (in the daemon), not in the TUI, so that MPRIS's
//! `Next`/`Previous` — which real hardware media keys and desktop widgets
//! call directly over D-Bus, with no TUI involved at all — actually do
//! something. A client-side-only queue would leave those buttons inert,
//! which defeats a core point of building MPRIS support in the first place.

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use tokio::sync::mpsc::UnboundedSender;

use crate::range_reader::RangeReader;
use crate::visualizer::{self, SpectrumAnalyzer, VisualizerTap};

/// How often the engine polls its own `Sink` for position/end-of-track while
/// idle-waiting on the command channel. Small enough that MPRIS `Seeked`
/// consumers and the idle timer both feel responsive; large enough not to
/// matter for CPU (this thread otherwise blocks entirely on `recv_timeout`).
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A seek-to-current-track-restart (via `Previous`) counts as "already at the
/// start" below this position — matches the common player convention of
/// "previous" restarting a track you're partway through rather than always
/// jumping back a full track.
const RESTART_THRESHOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub stream_url: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    /// Known/expected duration, if the caller has it (from library metadata)
    /// — independent of whatever `RangeReader` eventually learns from the
    /// HTTP response, since the engine reports playback *position*, not a
    /// duration derived from decoding.
    pub duration: Option<Duration>,
    /// A pre-formatted display label (e.g. "FLAC", "MP3 320") and whether
    /// it's lossless — computed client-side (see the TUI's
    /// `library::format_label`, the single source of truth for the
    /// format/lossless rules) and carried through as an opaque label rather
    /// than duplicating that logic here.
    pub format_label: String,
    pub lossless: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub status: Status,
    pub position: Duration,
    pub track: Option<TrackMeta>,
    pub volume: f32,
    /// 0-based position within the queue, and the queue's length — together
    /// enough for a "track 3 of 12" display and for `CanGoNext`/
    /// `CanGoPrevious`, without exposing the whole queue to every consumer.
    pub queue_index: usize,
    pub queue_len: usize,
    /// The full queue, in order — for a TUI "Queue" view. Cloned into the
    /// snapshot only when the queue actually changes (`PlayQueue`), not on
    /// every poll tick, so this doesn't add per-tick cost.
    pub queue: Vec<TrackMeta>,
    /// Current spectrum bar levels (0..=`visualizer::MAX_LEVEL` each),
    /// recomputed from real decoded audio every poll tick — see
    /// `visualizer.rs`. All-zero while stopped/paused or right after a
    /// track/seek change (not yet enough buffered samples).
    pub spectrum: [u8; visualizer::BARS],
}

impl Snapshot {
    pub fn has_next(&self) -> bool {
        self.queue_index + 1 < self.queue_len
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: Status::Stopped,
            position: Duration::ZERO,
            track: None,
            volume: 1.0,
            queue_index: 0,
            queue_len: 0,
            queue: Vec::new(),
            spectrum: [0; visualizer::BARS],
        }
    }
}

pub enum Command {
    /// Replaces the queue and starts playing at `start_index` immediately.
    PlayQueue {
        tracks: Vec<TrackMeta>,
        start_index: usize,
    },
    /// Jumps directly to `index` within the *current* queue (a TUI "Queue"
    /// view picking an arbitrary upcoming track) — distinct from `PlayQueue`,
    /// which replaces the queue itself.
    PlayAt(usize),
    Next,
    Previous,
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum Event {
    StatusChanged(Status),
    /// A new track started loading — consumers re-read fresh metadata via
    /// [`PlaybackHandle::snapshot`] rather than carrying it here, since the
    /// MPRIS bridge needs it in `mpris_server`'s own `Metadata` shape anyway.
    TrackChanged,
    /// The queue was exhausted (no next track) rather than just this one
    /// track ending into another — distinct from `TrackChanged` so the MPRIS
    /// bridge knows there's nothing more to report metadata for.
    QueueEnded,
    /// A track failed to load/stream/decode — carries a message suitable for
    /// surfacing to the user (e.g. as a TUI toast), not full error internals.
    PlaybackError(String),
    Seeked(Duration),
}

/// The handle every other part of the daemon (MPRIS interface, custom
/// control interface, idle timer) uses to talk to the playback engine.
/// Cheap to clone — cloning shares the same channel and snapshot.
#[derive(Clone)]
pub struct PlaybackHandle {
    cmd_tx: Sender<Command>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// Flipped whenever a command is sent or the engine's state changes —
    /// read (and reset) by the idle-shutdown timer so "activity" means real
    /// playback/control traffic, not just the timer's own polling.
    activity: Arc<AtomicBool>,
}

impl PlaybackHandle {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().expect("snapshot mutex poisoned").clone()
    }

    /// Reports and clears whether there's been any activity since the last
    /// call — see [`Self::activity`].
    pub fn take_activity(&self) -> bool {
        self.activity.swap(false, Ordering::SeqCst)
    }

    /// True while a track is actually playing — the idle timer must never
    /// fire during active playback even with `take_activity() == false`
    /// (e.g. the terminal was closed, but music should keep going).
    pub fn is_playing(&self) -> bool {
        self.snapshot().status == Status::Playing
    }

    fn send(&self, cmd: Command) {
        self.activity.store(true, Ordering::SeqCst);
        // The engine thread only exits on Shutdown/Drop; a send error here
        // means it already died (panicked), which the daemon should treat as
        // a bug to log, not a reason to also panic the async side.
        if self.cmd_tx.send(cmd).is_err() {
            tracing::error!("playback engine is no longer running");
        }
    }

    pub fn play_queue(&self, tracks: Vec<TrackMeta>, start_index: usize) {
        self.send(Command::PlayQueue { tracks, start_index });
    }
    pub fn play_at(&self, index: usize) {
        self.send(Command::PlayAt(index));
    }
    pub fn next(&self) {
        self.send(Command::Next);
    }
    pub fn previous(&self) {
        self.send(Command::Previous);
    }
    pub fn pause(&self) {
        self.send(Command::Pause);
    }
    pub fn resume(&self) {
        self.send(Command::Resume);
    }
    pub fn stop(&self) {
        self.send(Command::Stop);
    }
    pub fn seek(&self, position: Duration) {
        self.send(Command::Seek(position));
    }
    pub fn set_volume(&self, volume: f32) {
        self.send(Command::SetVolume(volume));
    }
    pub fn shutdown(&self) {
        self.send(Command::Shutdown);
    }
}

/// Spawns the engine thread and returns a handle to it. `events` is drained
/// by an async task elsewhere (see `main.rs`) to emit MPRIS signals and feed
/// the idle timer.
pub fn spawn(events: UnboundedSender<Event>) -> PlaybackHandle {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let activity = Arc::new(AtomicBool::new(false));

    let handle = PlaybackHandle {
        cmd_tx,
        snapshot: Arc::clone(&snapshot),
        activity,
    };

    std::thread::Builder::new()
        .name("maraetai-audio".into())
        .spawn(move || run_engine(cmd_rx, snapshot, events))
        .expect("failed to spawn audio thread");

    handle
}

struct EngineState {
    // Must stay alive for as long as `sink` plays anything — cpal tears down
    // the output device when it's dropped. Also the source of the `Mixer`
    // each new `Sink` connects to (rodio 0.21 folded the old separate
    // `OutputStreamHandle` into `OutputStream::mixer()`).
    stream: OutputStream,
    sink: Option<Sink>,
    http: reqwest::blocking::Client,
    queue: Vec<TrackMeta>,
    index: usize,
    analyzer: SpectrumAnalyzer,
    sample_ring: Arc<visualizer::SampleRing>,
    /// Channels/sample-rate of whatever's currently loaded — captured from
    /// the decoder at load time (needed for the FFT's frequency-bucket math,
    /// and no longer queryable once the decoder is consumed into the sink).
    current_format: Option<(u16, u32)>,
}

fn run_engine(cmd_rx: Receiver<Command>, snapshot: Arc<Mutex<Snapshot>>, events: UnboundedSender<Event>) {
    let stream = match OutputStreamBuilder::open_default_stream() {
        Ok(stream) => stream,
        Err(e) => {
            tracing::error!("no audio output device available: {e}");
            let _ = events.send(Event::PlaybackError(format!("no audio output device: {e}")));
            return;
        }
    };
    let (analyzer, sample_ring) = SpectrumAnalyzer::new();
    let mut state = EngineState {
        stream,
        sink: None,
        http: reqwest::blocking::Client::new(),
        queue: Vec::new(),
        index: 0,
        analyzer,
        sample_ring,
        current_format: None,
    };

    loop {
        match cmd_rx.recv_timeout(POLL_INTERVAL) {
            Ok(Command::PlayQueue { tracks, start_index }) => {
                state.queue = tracks;
                snapshot.lock().expect("poisoned").queue = state.queue.clone();
                let start_index = start_index.min(state.queue.len().saturating_sub(1));
                start_playback_at(&mut state, &snapshot, &events, start_index);
            }
            Ok(Command::PlayAt(index)) => {
                if index < state.queue.len() {
                    start_playback_at(&mut state, &snapshot, &events, index);
                }
            }
            Ok(Command::Next) => {
                if state.index + 1 < state.queue.len() {
                    let next = state.index + 1;
                    start_playback_at(&mut state, &snapshot, &events, next);
                }
                // No next track: matches the common player convention of
                // doing nothing on "next" at the end of the queue, rather
                // than stopping.
            }
            Ok(Command::Previous) => {
                let restart_current = current_position(&state) > RESTART_THRESHOLD || state.index == 0;
                if restart_current {
                    if let Some(sink) = &state.sink {
                        let _ = sink.try_seek(Duration::ZERO);
                        snapshot.lock().expect("poisoned").position = Duration::ZERO;
                    }
                } else {
                    let prev = state.index - 1;
                    start_playback_at(&mut state, &snapshot, &events, prev);
                }
            }
            Ok(Command::Pause) => {
                if let Some(sink) = &state.sink {
                    sink.pause();
                    set_status(&snapshot, &events, Status::Paused);
                }
            }
            Ok(Command::Resume) => {
                if let Some(sink) = &state.sink {
                    sink.play();
                    set_status(&snapshot, &events, Status::Playing);
                }
            }
            Ok(Command::Stop) => {
                if let Some(sink) = state.sink.take() {
                    sink.stop();
                }
                snapshot.lock().expect("poisoned").spectrum = [0; visualizer::BARS];
                set_status(&snapshot, &events, Status::Stopped);
            }
            Ok(Command::Seek(pos)) => {
                if let Some(sink) = &state.sink {
                    match sink.try_seek(pos) {
                        Ok(()) => {
                            snapshot.lock().expect("poisoned").position = pos;
                            let _ = events.send(Event::Seeked(pos));
                        }
                        Err(e) => {
                            tracing::warn!("seek failed: {e}");
                        }
                    }
                }
            }
            Ok(Command::SetVolume(v)) => {
                if let Some(sink) = &state.sink {
                    sink.set_volume(v);
                }
                snapshot.lock().expect("poisoned").volume = v;
            }
            Ok(Command::Shutdown) => {
                if let Some(sink) = state.sink.take() {
                    sink.stop();
                }
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                poll_progress(&mut state, &snapshot, &events);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn current_position(state: &EngineState) -> Duration {
    state.sink.as_ref().map(Sink::get_pos).unwrap_or_default()
}

/// Starts playing `state.queue[index]`, replacing whatever was playing.
fn start_playback_at(
    state: &mut EngineState,
    snapshot: &Arc<Mutex<Snapshot>>,
    events: &UnboundedSender<Event>,
    index: usize,
) {
    if let Some(sink) = state.sink.take() {
        sink.stop();
    }
    let Some(meta) = state.queue.get(index).cloned() else {
        return;
    };
    state.index = index;

    let mut reader = RangeReader::new(state.http.clone(), meta.stream_url.clone());
    // `RangeReader` only learns the resource's total length lazily, from the
    // first response it gets — but `DecoderBuilder::with_byte_len` needs it
    // supplied up front. Prime it with a throwaway 1-byte read, then seek
    // back to the start before handing the reader to the decoder. Without a
    // known byte length, Symphonia's FLAC seeking fails outright with
    // `SeekError::Unseekable` (confirmed against a real FLAC file over a
    // real HTTP Range server) — this priming step is what makes seeking
    // actually work, not just look supported.
    let mut probe = [0u8; 1];
    let byte_len = reader.read_exact(&mut probe).ok().and_then(|()| {
        let _ = reader.seek(SeekFrom::Start(0));
        reader.known_len()
    });
    let mut decoder_builder = Decoder::builder().with_data(reader).with_seekable(true);
    if let Some(len) = byte_len {
        decoder_builder = decoder_builder.with_byte_len(len);
    }
    let decoder = match decoder_builder.build() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("failed to decode stream: {e}");
            let _ = events.send(Event::PlaybackError(format!("could not play track: {e}")));
            set_status(snapshot, events, Status::Stopped);
            return;
        }
    };

    // Captured before the decoder is consumed into the tap/sink — needed by
    // the visualizer's FFT frequency-bucket math on every poll tick.
    state.current_format = Some((decoder.channels(), decoder.sample_rate()));
    state.analyzer.reset();
    let tapped = VisualizerTap::new(decoder, Arc::clone(&state.sample_ring));

    let sink = Sink::connect_new(state.stream.mixer());
    sink.append(tapped);
    {
        let mut snap = snapshot.lock().expect("poisoned");
        snap.status = Status::Playing;
        snap.position = Duration::ZERO;
        snap.track = Some(meta);
        snap.queue_index = index;
        snap.queue_len = state.queue.len();
    }
    state.sink = Some(sink);
    let _ = events.send(Event::TrackChanged);
    let _ = events.send(Event::StatusChanged(Status::Playing));
}

fn poll_progress(state: &mut EngineState, snapshot: &Arc<Mutex<Snapshot>>, events: &UnboundedSender<Event>) {
    let Some(sink) = &state.sink else { return };
    let ended = sink.empty();
    let currently_playing = snapshot.lock().expect("poisoned").status == Status::Playing;

    if ended && currently_playing {
        if state.index + 1 < state.queue.len() {
            let next = state.index + 1;
            start_playback_at(state, snapshot, events, next);
        } else {
            let mut snap = snapshot.lock().expect("poisoned");
            snap.status = Status::Stopped;
            snap.spectrum = [0; visualizer::BARS];
            drop(snap);
            let _ = events.send(Event::QueueEnded);
            let _ = events.send(Event::StatusChanged(Status::Stopped));
        }
        return;
    }

    // Only recompute the spectrum while actually producing new audio —
    // while paused, the last frame is simply left in place (a frozen
    // visualizer, matching the frozen position), not recomputed from a ring
    // buffer that isn't receiving new samples anyway.
    if currently_playing {
        if let Some((channels, sample_rate)) = state.current_format {
            let spectrum = state.analyzer.compute(channels, sample_rate);
            snapshot.lock().expect("poisoned").spectrum = spectrum;
        }
    }

    if let Some(sink) = &state.sink {
        snapshot.lock().expect("poisoned").position = sink.get_pos();
    }
}

fn set_status(snapshot: &Arc<Mutex<Snapshot>>, events: &UnboundedSender<Event>, status: Status) {
    snapshot.lock().expect("poisoned").status = status.clone();
    let _ = events.send(Event::StatusChanged(status));
}
