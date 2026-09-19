//! The interactive TUI: browse albums/artists/playlists/genres, search, and
//! play — talking to `maraetai-service` directly for library data (the same
//! way every other maraetai client does) and to the daemon over D-Bus for
//! playback/queue control.
//!
//! Styled after [rmpc](https://github.com/mierak/rmpc) (itself inspired by
//! ncmpcpp): rounded borders, a blue-centric palette (black-on-blue
//! selection, not white-on-blue), a persistent top tab bar instead of a
//! "menu screen" you enter, scrollbars on lists, artist-first columns with
//! right-aligned duration, and playback state shown as a bracketed
//! `[State]` tag. Not a port of rmpc's full (very elaborate, RON-based)
//! theming DSL — just its visual vocabulary, applied directly.
//!
//! Each tab has its own drill-down stack (`Vec<Screen>`) — picking a tab
//! resets to that tab's root list; `Esc`/`Backspace` pops back up within it.
//! Playing a song from a list queues the *rest* of that list from the
//! selected point onward (so picking track 3 of an album naturally plays
//! 3, 4, 5, ...); the daemon owns queue advancement, so `Next`/`Previous`
//! (here or from a hardware media key) work the same way regardless of
//! which screen started the queue.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use maraetai_common::spectrum;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Gauge, List, ListItem, ListState, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, TableState,
};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use tokio::sync::mpsc;

use crate::art;
use crate::dbus_client::{ControlProxy, QueueEntry};
use crate::library::{self, Album, Artist, Genre, Playlist, Song};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f64 = 0.05;

/// The persistent top-level tabs — always visible, switched with `1`-`6`
/// (rmpc itself uses configurable keys per tab; fixed number keys are a
/// reasonable single-user default here). `Search` and `Queue` are tabs like
/// any other, not special-cased screens. Playlists leads since it's the
/// most common starting point (a saved playlist beats rebrowsing albums).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Playlists,
    Albums,
    Artists,
    Genres,
    Favourites,
    RecentlyPlayed,
    Search,
    Queue,
    Lyrics,
    OnRepeat,
    SongsForYou,
}

/// Digits `1`-`9` jump directly to the first 9 tabs; `[`/`]` cycle through
/// *all* of them (including the last two, which have no digit of their
/// own) — the two discovery tabs trail at the end since they're the least
/// frequently visited, not because they matter less.
const TABS: [Tab; 11] = [
    Tab::Playlists,
    Tab::Albums,
    Tab::Artists,
    Tab::Genres,
    Tab::Favourites,
    Tab::RecentlyPlayed,
    Tab::Search,
    Tab::Queue,
    Tab::Lyrics,
    Tab::OnRepeat,
    Tab::SongsForYou,
];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Playlists => "Playlists",
            Tab::Albums => "Albums",
            Tab::Artists => "Artists",
            Tab::Genres => "Genres",
            Tab::Favourites => "Favourites",
            Tab::RecentlyPlayed => "Recent",
            Tab::Search => "Search",
            Tab::Queue => "Queue",
            Tab::Lyrics => "Lyrics",
            Tab::OnRepeat => "On Repeat",
            Tab::SongsForYou => "For You",
        }
    }
}

/// An rmpc-inspired palette: blue borders/accents, black-on-blue selection
/// (not white-on-blue), rounded corners everywhere. Yellow is reserved
/// specifically for the bracketed playback-state tag (matching rmpc's own
/// scoping of that color to just that one element) and green specifically
/// for "this is lossless" — neither is used as a general accent.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub fn accent() -> Style {
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
    }
    pub fn header() -> Style {
        Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    }
    pub fn selected() -> Style {
        Style::default().bg(Color::Blue).fg(Color::Black).add_modifier(Modifier::BOLD)
    }
    pub fn now_playing_row() -> Style {
        Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
    }
    pub fn muted() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn border() -> Style {
        Style::default().fg(Color::Blue)
    }
    pub fn active_tab() -> Style {
        selected()
    }
    pub fn inactive_tab() -> Style {
        Style::default()
    }
    pub fn state_tag() -> Style {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    }
    pub fn lossless() -> Style {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    }
}

fn rounded_block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(title.into())
}

/// One entry in the "Queue" view — title/artist/album/duration/format_label/
/// lossless, as returned by the daemon's `Queue()` method.
type QueueRow = (String, String, String, f64, String, bool);

enum Screen {
    AlbumList {
        title: String,
        albums: Vec<Album>,
        selected: usize,
        /// A local, client-side substring filter over this screen's own
        /// items — entered by pressing `/` while this screen is on top (see
        /// `handle_filter_edit`). `selected` is a position within the
        /// *filtered* view, not a raw index — see `visible_indices`.
        filter: String,
    },
    SongList {
        title: String,
        songs: Vec<Song>,
        selected: usize,
        filter: String,
    },
    ArtistList {
        artists: Vec<Artist>,
        selected: usize,
        filter: String,
    },
    PlaylistList {
        playlists: Vec<Playlist>,
        selected: usize,
        filter: String,
    },
    GenreList {
        genres: Vec<Genre>,
        selected: usize,
        filter: String,
    },
    Search {
        query: String,
        editing: bool,
        results: Vec<Song>,
        selected: usize,
    },
    /// The daemon's actual current queue — `Enter` jumps straight to that
    /// track via `PlayAt`, unlike other lists which start a *new* queue.
    Queue {
        tracks: Vec<QueueRow>,
        selected: usize,
        filter: String,
    },
    /// Lyrics for whatever's currently playing. The actual lines (and the
    /// fetch that produced them) live on `App` (`lyrics`,
    /// `current_lyrics_song_id`) rather than here, keyed by song id and
    /// refreshed automatically as the track changes — this variant only
    /// carries the view's own scroll state. `follow`: auto-scroll to keep
    /// the currently-sung line in view (for time-synced lyrics); becomes
    /// `false` as soon as the user manually scrolls, and `true` again on
    /// `Enter` or a track change.
    Lyrics {
        scroll: usize,
        follow: bool,
    },
}

/// A point-in-time playback status snapshot, as returned by `Status()`.
struct NowPlaying {
    status: String,
    title: String,
    artist: String,
    position: f64,
    duration: f64,
    queue_index: u32,
    queue_len: u32,
    volume: f64,
    format_label: String,
    lossless: bool,
    /// Empty when there's no current track or it has no cover art — distinct
    /// from a fetch simply not having completed yet, which is tracked
    /// separately in `App` (`art_lines`) since the art itself survives
    /// across `NowPlaying` snapshots rather than being refetched every poll.
    art_url: String,
    /// Empty when there's no current track — the Subsonic song id, used to
    /// fetch lyrics (and, server-side, to record scrobbles) for it.
    song_id: String,
    /// "off" / "track" / "queue".
    repeat: String,
    shuffle: bool,
    /// Current spectrum bar levels (0..=`spectrum::MAX_LEVEL` each), polled
    /// alongside status. Empty (not all-zero — genuinely empty) while
    /// disconnected.
    spectrum: Vec<u8>,
}

struct App<'a> {
    proxy: ControlProxy<'a>,
    library: library::Client,
    active_tab: Tab,
    /// The active tab's drill-down stack — switching tabs replaces this
    /// wholesale with a single root screen; `Esc`/`Backspace` pops within it.
    stack: Vec<Screen>,
    /// A transient status/error line shown above the now-playing bar — e.g.
    /// "Loading…" during a fetch, or a fetch failure. Cleared on the next
    /// successful action.
    message: String,
    /// The art URL currently being shown/fetched — `None` for "no art" so
    /// this is directly comparable to `NowPlaying::art_url` (empty string)
    /// via a small conversion at the call site, avoiding refetching the same
    /// track's art every 250ms poll tick.
    current_art_url: Option<String>,
    /// Built from the decoded image once its background fetch completes,
    /// via `picker.new_resize_protocol` — `None` shows `art::placeholder()`
    /// instead, while fetching or when there's simply no art to show.
    art_protocol: Option<Box<dyn StatefulProtocol>>,
    /// Detects the terminal's real graphics-protocol support once at
    /// startup (Kitty/Sixel/iTerm2, falling back to half-blocks) — see
    /// `art::make_picker`. `new_resize_protocol` needs `&mut self` on this,
    /// which is why `draw`/`draw_status_bar` take `&mut self` too.
    picker: Picker,
    art_tx: mpsc::UnboundedSender<art::Fetched>,
    art_rx: mpsc::UnboundedReceiver<art::Fetched>,
    /// The song id lyrics are currently shown/being fetched for — `None` for
    /// "nothing to show", directly comparable against `NowPlaying::song_id`
    /// (empty string) via a small conversion at the call site, the same
    /// pattern as `current_art_url`.
    current_lyrics_song_id: Option<String>,
    /// `None` while fetching or when there's nothing to show; `Some(vec![])`
    /// specifically means "fetched successfully, this track really has no
    /// lyrics" — distinct from "still loading", so the Lyrics tab can show
    /// the right message for each.
    lyrics: Option<Vec<library::LyricLine>>,
    lyrics_tx: mpsc::UnboundedSender<LyricsFetched>,
    lyrics_rx: mpsc::UnboundedReceiver<LyricsFetched>,
    /// Whether `/` on the current screen is capturing keystrokes into its
    /// `filter` field. A single App-level flag, not one per screen, since
    /// only the top of the stack can ever be being edited.
    filter_editing: bool,
    /// Whether the full-screen keybind reference is showing, overlaying
    /// whatever the active tab would otherwise render.
    show_help: bool,
}

/// One lyrics fetch's result — see `art::Fetched` for why this carries the
/// song id it was fetched for rather than assuming it's still current by
/// the time it arrives.
struct LyricsFetched {
    song_id: String,
    lines: Vec<library::LyricLine>,
}

pub async fn run(proxy: ControlProxy<'_>, creds: maraetai_common::Credentials) -> Result<()> {
    let (art_tx, art_rx) = mpsc::unbounded_channel();
    let (lyrics_tx, lyrics_rx) = mpsc::unbounded_channel();
    let mut terminal = ratatui::init();
    let picker = art::make_picker();
    let mut app = App {
        proxy,
        library: library::Client::new(creds),
        active_tab: Tab::Playlists,
        stack: Vec::new(),
        message: String::new(),
        picker,
        current_art_url: None,
        art_protocol: None,
        art_tx,
        art_rx,
        current_lyrics_song_id: None,
        lyrics: None,
        lyrics_tx,
        lyrics_rx,
        filter_editing: false,
        show_help: false,
    };

    app.switch_tab(Tab::Playlists, &mut terminal).await;
    let result = app.event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Builds a list/table title showing the active filter, if any, alongside
/// the screen's usual keybind hint — e.g. `" Albums — filter: mez  [Enter]
/// open  [Esc] back "` versus the plain `" Albums — [Enter] open  [Esc]
/// back "` when nothing is being filtered.
/// Builds a list/table title showing the active filter (if any) and whether
/// it's currently being typed into. Critically, `editing` shows a cursor
/// even with an empty `filter` — pressing `/` must be *visibly* confirmed
/// immediately, not just once the first character lands, otherwise it's
/// easy to press `/` again thinking the first press didn't register (which
/// used to type a literal `/` into the query — see `handle_filter_edit`).
/// Pure key-handling logic behind `handle_filter_edit`, factored out so
/// it's unit-testable without a real `App` (which needs a live D-Bus
/// connection to a daemon just to construct). Returns `None` for a key this
/// mode doesn't handle at all (falls through to normal key handling
/// unchanged); otherwise `Some((stop_editing, consumed))` — `stop_editing`
/// ends filter-edit mode, `consumed` says whether the key should *also* be
/// processed as a normal keybinding on the same press (only `Enter` isn't:
/// stopping edit mode and immediately activating the selection in one
/// keypress is what actually fixes the confusion that used to cause a
/// stray `/` in the query).
fn apply_filter_key(filter: &mut String, selected: &mut usize, code: KeyCode) -> Option<(bool, bool)> {
    match code {
        // Swallowed, not inserted: `/` is the very key that opens
        // filtering, and an empty filter shows no visible change at all
        // (see `filter_hint_title`) — so a second press, easy to do before
        // realizing the first one already worked, must never end up as a
        // literal `/` in the query.
        KeyCode::Char('/') => Some((false, true)),
        KeyCode::Char(c) => {
            filter.push(c);
            *selected = 0;
            Some((false, true))
        }
        KeyCode::Backspace => {
            filter.pop();
            *selected = 0;
            Some((false, true))
        }
        KeyCode::Esc => {
            filter.clear();
            *selected = 0;
            Some((true, true))
        }
        KeyCode::Enter => Some((true, false)),
        _ => None,
    }
}

fn filter_hint_title(base: &str, filter: &str, editing: bool, hint: &str) -> String {
    if editing {
        format!(" {base} — filter: {filter}_  [Enter] apply  [Esc] cancel ")
    } else if filter.is_empty() {
        format!(" {base} — {hint} ")
    } else {
        format!(" {base} — filter: {filter}  {hint} ")
    }
}

impl App<'_> {
    fn top(&self) -> &Screen {
        self.stack.last().expect("stack is never empty once a tab is active")
    }

    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            // A blocking poll on this task is an accepted simplification —
            // this binary has no other async work contending for the thread
            // besides the status poll below.
            let now_playing = self.fetch_status().await;
            self.update_art(&now_playing);
            self.update_lyrics(&now_playing);
            terminal.draw(|frame| self.draw(frame, &now_playing))?;

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // The help overlay eats every key while it's up — anything
            // dismisses it, nothing underneath should react to the same
            // keystroke (e.g. `?` again shouldn't also do whatever `?`
            // would otherwise mean, there isn't anything else bound to it,
            // but the principle holds for any key).
            if self.show_help {
                self.show_help = false;
                continue;
            }

            if let Screen::Search { editing: true, .. } = self.top() {
                if self.handle_search_edit(key.code).await {
                    continue;
                }
            }
            if self.filter_editing && self.handle_filter_edit(key.code) {
                continue;
            }

            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('Q') => {
                    let _ = self.proxy.quit().await;
                    return Ok(());
                }
                KeyCode::Char('?') => {
                    self.show_help = true;
                }
                KeyCode::Char(' ') => {
                    if now_playing.status == "playing" {
                        let _ = self.proxy.pause().await;
                    } else {
                        let _ = self.proxy.resume().await;
                    }
                }
                KeyCode::Char('s') => {
                    let _ = self.proxy.stop().await;
                }
                KeyCode::Char('n') => {
                    let _ = self.proxy.next().await;
                }
                KeyCode::Char('p') => {
                    let _ = self.proxy.previous().await;
                }
                KeyCode::Char('r') => {
                    let _ = self.proxy.cycle_repeat().await;
                }
                KeyCode::Char('x') => {
                    let _ = self.proxy.toggle_shuffle().await;
                }
                KeyCode::Left => {
                    let target = (now_playing.position - SEEK_STEP).max(0.0);
                    let _ = self.proxy.seek_to(target).await;
                }
                KeyCode::Right => {
                    let _ = self.proxy.seek_to(now_playing.position + SEEK_STEP).await;
                }
                KeyCode::Char('-') => {
                    let _ = self.proxy.set_volume((now_playing.volume - VOLUME_STEP).max(0.0)).await;
                }
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    let _ = self.proxy.set_volume((now_playing.volume + VOLUME_STEP).min(1.0)).await;
                }
                KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                    let idx = c.to_digit(10).expect("checked is_ascii_digit") as usize - 1;
                    if let Some(&tab) = TABS.get(idx) {
                        self.switch_tab(tab, terminal).await;
                    }
                }
                KeyCode::Char('[') => {
                    let current = TABS.iter().position(|&t| t == self.active_tab).unwrap_or(0);
                    let prev = (current + TABS.len() - 1) % TABS.len();
                    self.switch_tab(TABS[prev], terminal).await;
                }
                KeyCode::Char(']') => {
                    let current = TABS.iter().position(|&t| t == self.active_tab).unwrap_or(0);
                    let next = (current + 1) % TABS.len();
                    self.switch_tab(TABS[next], terminal).await;
                }
                // Context-sensitive: `/` filters *this* list in place on any
                // screen that has one, rather than always jumping to the
                // universal Search tab — that's now reserved for screens
                // with no local list to filter (Search itself restarts a
                // fresh query; Lyrics has nothing to filter).
                KeyCode::Char('/') => {
                    let filterable = matches!(
                        self.top(),
                        Screen::AlbumList { .. }
                            | Screen::SongList { .. }
                            | Screen::ArtistList { .. }
                            | Screen::PlaylistList { .. }
                            | Screen::GenreList { .. }
                            | Screen::Queue { .. }
                    );
                    if filterable {
                        self.filter_editing = true;
                    } else {
                        self.switch_tab(Tab::Search, terminal).await;
                    }
                }
                KeyCode::Char('a') => self.append_selection().await,
                KeyCode::Char('f') => self.toggle_star().await,
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter => self.activate_selection(terminal).await,
                KeyCode::Esc | KeyCode::Backspace => {
                    // An active filter is cleared first (like most apps'
                    // "clear search, then go back"); only pop the stack
                    // once there's nothing left to clear.
                    let has_filter = matches!(
                        self.top(),
                        Screen::AlbumList { filter, .. }
                        | Screen::SongList { filter, .. }
                        | Screen::ArtistList { filter, .. }
                        | Screen::PlaylistList { filter, .. }
                        | Screen::GenreList { filter, .. }
                        | Screen::Queue { filter, .. } if !filter.is_empty()
                    );
                    if has_filter {
                        if let Some(
                            Screen::AlbumList { filter, selected, .. }
                            | Screen::SongList { filter, selected, .. }
                            | Screen::ArtistList { filter, selected, .. }
                            | Screen::PlaylistList { filter, selected, .. }
                            | Screen::GenreList { filter, selected, .. }
                            | Screen::Queue { filter, selected, .. },
                        ) = self.stack.last_mut()
                        {
                            filter.clear();
                            *selected = 0;
                        }
                    } else if self.stack.len() > 1 {
                        self.stack.pop();
                    }
                }
                _ => {}
            }
        }
    }

    async fn fetch_status(&self) -> NowPlaying {
        // Two independent calls rather than folding spectrum into Status()
        // — spectrum is optional/best-effort visual flair, so a failure
        // there (or an older daemon without it) shouldn't blank the rest of
        // the now-playing info.
        let spectrum = self.proxy.spectrum().await.unwrap_or_default();
        match self.proxy.status().await {
            Ok((
                status,
                title,
                artist,
                _album,
                position,
                duration,
                queue_index,
                queue_len,
                volume,
                format_label,
                lossless,
                art_url,
                song_id,
                repeat,
                shuffle,
            )) => NowPlaying {
                status,
                title,
                artist,
                position,
                duration,
                queue_index,
                queue_len,
                volume,
                format_label,
                lossless,
                art_url,
                song_id,
                repeat,
                shuffle,
                spectrum,
            },
            Err(_) => NowPlaying {
                status: "disconnected".to_string(),
                title: String::new(),
                artist: String::new(),
                position: 0.0,
                duration: 0.0,
                queue_index: 0,
                queue_len: 0,
                volume: 1.0,
                format_label: String::new(),
                lossless: false,
                art_url: String::new(),
                song_id: String::new(),
                repeat: "off".to_string(),
                shuffle: false,
                spectrum: Vec::new(),
            },
        }
    }

    /// Kicks off a background art fetch when the current track's art has
    /// changed since the last poll, and drains any completed fetch(es) from
    /// earlier ticks into `self.art_lines`. Cheap to call every tick: both
    /// the URL comparison and the channel drain are no-ops in the (by far
    /// most common) case where nothing has changed.
    fn update_art(&mut self, now_playing: &NowPlaying) {
        let new_url = (!now_playing.art_url.is_empty()).then(|| now_playing.art_url.clone());
        if new_url != self.current_art_url {
            self.current_art_url = new_url.clone();
            self.art_protocol = None;
            if let Some(url) = new_url {
                art::spawn_fetch(url, self.art_tx.clone());
            }
        }
        while let Ok(fetched) = self.art_rx.try_recv() {
            // Guards against a fetch for a track the user has since skipped
            // past arriving late and clobbering the *current* track's art.
            if self.current_art_url.as_deref() == Some(fetched.url.as_str()) {
                self.art_protocol = Some(self.picker.new_resize_protocol(fetched.image));
            }
        }
    }

    /// Same pattern as `update_art`, keyed by song id instead of art URL:
    /// kicks off a background lyrics fetch when the current track changes,
    /// and drains any completed fetch into `self.lyrics`. Also resets the
    /// Lyrics screen's scroll/follow state, if it's the active screen, so a
    /// new track always starts reading from the top rather than wherever
    /// the previous track's lyrics happened to be scrolled to.
    fn update_lyrics(&mut self, now_playing: &NowPlaying) {
        let new_id = (!now_playing.song_id.is_empty()).then(|| now_playing.song_id.clone());
        if new_id != self.current_lyrics_song_id {
            self.current_lyrics_song_id = new_id.clone();
            self.lyrics = None;
            if let Some(id) = new_id {
                let client = self.library.clone();
                let tx = self.lyrics_tx.clone();
                let artist = now_playing.artist.clone();
                let title = now_playing.title.clone();
                tokio::spawn(async move {
                    let lines = client.lyrics(&id, &artist, &title).await;
                    let _ = tx.send(LyricsFetched { song_id: id, lines });
                });
            }
            if let Some(Screen::Lyrics { scroll, follow }) = self.stack.first_mut() {
                *scroll = 0;
                *follow = true;
            }
        }
        while let Ok(fetched) = self.lyrics_rx.try_recv() {
            if self.current_lyrics_song_id.as_deref() == Some(fetched.song_id.as_str()) {
                self.lyrics = Some(fetched.lines);
            }
        }
    }

    /// Switches the active tab, replacing the drill-down stack with that
    /// tab's freshly-fetched root screen. A fetch failure still leaves a
    /// (empty) screen in place — `self.message` carries the error — rather
    /// than an empty stack, which `top()` never tolerates. Also drops any
    /// in-progress filter edit — it belonged to whatever screen is being
    /// replaced.
    async fn switch_tab(&mut self, tab: Tab, terminal: &mut ratatui::DefaultTerminal) {
        self.active_tab = tab;
        self.stack.clear();
        self.filter_editing = false;
        match tab {
            Tab::Albums => {
                let albums = self
                    .load(terminal, "Loading albums…", |c| Box::pin(async move { c.albums().await }))
                    .await
                    .unwrap_or_default();
                self.stack.push(Screen::AlbumList { title: "Albums".into(), albums, selected: 0, filter: String::new() });
            }
            Tab::Artists => {
                let artists = self
                    .load(terminal, "Loading artists…", |c| Box::pin(async move { c.artists().await }))
                    .await
                    .unwrap_or_default();
                self.stack.push(Screen::ArtistList { artists, selected: 0, filter: String::new() });
            }
            Tab::Playlists => {
                let playlists = self
                    .load(terminal, "Loading playlists…", |c| Box::pin(async move { c.playlists().await }))
                    .await
                    .unwrap_or_default();
                self.stack.push(Screen::PlaylistList { playlists, selected: 0, filter: String::new() });
            }
            Tab::Genres => {
                let genres = self
                    .load(terminal, "Loading genres…", |c| Box::pin(async move { c.genres().await }))
                    .await
                    .unwrap_or_default();
                self.stack.push(Screen::GenreList { genres, selected: 0, filter: String::new() });
            }
            // Favourites/RecentlyPlayed/OnRepeat/SongsForYou are all just
            // maraetai-service's native song-list endpoints — they reuse
            // `Screen::SongList` wholesale (filtering, star-toggle, "play
            // from here", scrobbling — everything a song list already
            // does) rather than needing their own screen variant.
            Tab::Favourites => {
                self.push_native_song_list(terminal, "Favourites", |c| Box::pin(async move { c.favourites().await }))
                    .await;
            }
            Tab::RecentlyPlayed => {
                self.push_native_song_list(terminal, "Recently Played", |c| {
                    Box::pin(async move { c.recently_played().await })
                })
                .await;
            }
            Tab::OnRepeat => {
                self.push_native_song_list(terminal, "On Repeat", |c| Box::pin(async move { c.on_repeat().await }))
                    .await;
            }
            Tab::SongsForYou => {
                self.push_native_song_list(terminal, "Songs For You", |c| {
                    Box::pin(async move { c.songs_for_you().await })
                })
                .await;
            }
            Tab::Search => {
                self.stack.push(Screen::Search {
                    query: String::new(),
                    editing: true,
                    results: Vec::new(),
                    selected: 0,
                });
            }
            Tab::Queue => {
                self.message = "Loading queue…".to_string();
                let _ = terminal.draw(|f| self.draw_message(f));
                match self.proxy.queue().await {
                    Ok(tracks) => {
                        self.message.clear();
                        self.stack.push(Screen::Queue { tracks, selected: 0, filter: String::new() });
                    }
                    Err(e) => {
                        self.message = format!("error: {e}");
                        self.stack.push(Screen::Queue { tracks: Vec::new(), selected: 0, filter: String::new() });
                    }
                }
            }
            // No fetch here: lyrics are fetched by `update_lyrics` as soon
            // as a track starts playing, independent of whether this tab
            // has ever been visited, so by the time the user switches to it
            // the fetch is usually already underway or done.
            Tab::Lyrics => {
                self.stack.push(Screen::Lyrics { scroll: 0, follow: true });
            }
        }
    }

    /// Shared loader for the four maraetai-service-native tabs (Favourites,
    /// Recently Played, On Repeat, Songs For You) — each is just a
    /// differently-sourced song list, so this always lands on the same
    /// `Screen::SongList` the regular browsing screens use.
    #[allow(clippy::type_complexity)]
    async fn push_native_song_list(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        title: &str,
        fetch: impl FnOnce(
            &library::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Song>>> + '_>>,
    ) {
        let songs = self.load(terminal, &format!("Loading {title}…"), fetch).await.unwrap_or_default();
        self.stack.push(Screen::SongList { title: title.to_string(), songs, selected: 0, filter: String::new() });
    }

    /// Handles a key while a search query is being typed. Returns `true` if
    /// it consumed the key (so the caller's normal keybindings don't also
    /// fire on the same keystroke).
    async fn handle_search_edit(&mut self, code: KeyCode) -> bool {
        let Some(Screen::Search { query, editing, .. }) = self.stack.last_mut() else {
            return false;
        };
        match code {
            KeyCode::Char(c) => {
                query.push(c);
                true
            }
            KeyCode::Backspace => {
                query.pop();
                true
            }
            KeyCode::Esc => {
                *editing = false;
                true
            }
            KeyCode::Enter => {
                *editing = false;
                self.run_search().await;
                true
            }
            _ => false,
        }
    }

    /// Handles a key while `self.filter_editing` is on — typing narrows the
    /// current screen's own list live; `Enter` stops capturing keystrokes
    /// but keeps the filter active; `Esc` clears it and stops. Only the six
    /// screens with a `filter` field respond; anything else (Search, which
    /// has its own query mechanism, or Lyrics) leaves this a no-op.
    fn handle_filter_edit(&mut self, code: KeyCode) -> bool {
        let Some(screen) = self.stack.last_mut() else { return false };
        let (filter, selected) = match screen {
            Screen::AlbumList { filter, selected, .. }
            | Screen::SongList { filter, selected, .. }
            | Screen::ArtistList { filter, selected, .. }
            | Screen::PlaylistList { filter, selected, .. }
            | Screen::GenreList { filter, selected, .. }
            | Screen::Queue { filter, selected, .. } => (filter, selected),
            _ => return false,
        };
        let Some((stop_editing, consumed)) = apply_filter_key(filter, selected, code) else {
            return false;
        };
        if stop_editing {
            self.filter_editing = false;
        }
        consumed
    }

    /// Indices into the top screen's underlying list that match its current
    /// `filter` (case-insensitive substring, matched against whatever text
    /// that screen already shows) — empty filter matches everything.
    /// `selected` is always a *position within this list*, not a raw index,
    /// so filtering never requires separately tracking "hidden" rows: it's
    /// computed fresh wherever it's needed (`move_selection`,
    /// `activate_selection`, `draw`) rather than stored.
    fn visible_indices(&self) -> Vec<usize> {
        fn matches(text: &str, filter: &str) -> bool {
            filter.is_empty() || text.to_lowercase().contains(&filter.to_lowercase())
        }
        match self.top() {
            Screen::AlbumList { albums, filter, .. } => albums
                .iter()
                .enumerate()
                .filter(|(_, a)| matches(&format!("{} {}", a.name, a.artist), filter))
                .map(|(i, _)| i)
                .collect(),
            Screen::SongList { songs, filter, .. } => songs
                .iter()
                .enumerate()
                .filter(|(_, s)| matches(&format!("{} {}", s.title, s.artist), filter))
                .map(|(i, _)| i)
                .collect(),
            Screen::ArtistList { artists, filter, .. } => {
                artists.iter().enumerate().filter(|(_, a)| matches(&a.name, filter)).map(|(i, _)| i).collect()
            }
            Screen::PlaylistList { playlists, filter, .. } => {
                playlists.iter().enumerate().filter(|(_, p)| matches(&p.name, filter)).map(|(i, _)| i).collect()
            }
            Screen::GenreList { genres, filter, .. } => {
                genres.iter().enumerate().filter(|(_, g)| matches(&g.value, filter)).map(|(i, _)| i).collect()
            }
            Screen::Queue { tracks, filter, .. } => tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| matches(&format!("{} {}", t.0, t.1), filter))
                .map(|(i, _)| i)
                .collect(),
            Screen::Search { results, .. } => (0..results.len()).collect(),
            Screen::Lyrics { .. } => Vec::new(),
        }
    }

    fn move_selection(&mut self, delta: i32) {
        match self.stack.last_mut() {
            None => return,
            // Lyrics scrolls a line count, not a wrapping list selection —
            // and manually scrolling disengages auto-follow, same as most
            // lyrics views (Enter re-engages it, see `activate_selection`).
            Some(Screen::Lyrics { scroll, follow }) => {
                *follow = false;
                *scroll = (*scroll as i32 + delta).max(0) as usize;
                return;
            }
            Some(Screen::Search { selected, results, editing, .. }) => {
                if *editing || results.is_empty() {
                    return;
                }
                *selected = (*selected as i32 + delta).rem_euclid(results.len() as i32) as usize;
                return;
            }
            _ => {}
        }
        // Every other screen: `selected` is a position within the filtered
        // view — see `visible_indices`.
        let len = self.visible_indices().len();
        if len == 0 {
            return;
        }
        if let Some(
            Screen::AlbumList { selected, .. }
            | Screen::SongList { selected, .. }
            | Screen::ArtistList { selected, .. }
            | Screen::PlaylistList { selected, .. }
            | Screen::GenreList { selected, .. }
            | Screen::Queue { selected, .. },
        ) = self.stack.last_mut()
        {
            *selected = (*selected as i32 + delta).rem_euclid(len as i32) as usize;
        }
    }

    async fn activate_selection(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        if let Some(Screen::Lyrics { follow, .. }) = self.stack.last_mut() {
            *follow = true;
            return;
        }
        if let Screen::Search { results, selected, editing, .. } = self.top() {
            if !*editing && !results.is_empty() {
                self.play_from(results.clone(), *selected).await;
            }
            return;
        }
        let visible = self.visible_indices();
        match self.top() {
            Screen::AlbumList { albums, selected, .. } => {
                let Some(album) = visible.get(*selected).and_then(|&i| albums.get(i)).cloned() else { return };
                self.push_song_list(terminal, album.name.clone(), |c| {
                    let id = album.id.clone();
                    Box::pin(async move { c.album_songs(&id).await })
                })
                .await;
            }
            Screen::ArtistList { artists, selected, .. } => {
                let Some(artist) = visible.get(*selected).and_then(|&i| artists.get(i)).cloned() else { return };
                self.push_album_list(terminal, artist.name.clone(), |c| {
                    let id = artist.id.clone();
                    Box::pin(async move { c.artist_albums(&id).await })
                })
                .await;
            }
            Screen::PlaylistList { playlists, selected, .. } => {
                let Some(pl) = visible.get(*selected).and_then(|&i| playlists.get(i)).cloned() else { return };
                self.push_song_list(terminal, pl.name.clone(), |c| {
                    let id = pl.id.clone();
                    Box::pin(async move { c.playlist_songs(&id).await })
                })
                .await;
            }
            Screen::GenreList { genres, selected, .. } => {
                let Some(genre) = visible.get(*selected).and_then(|&i| genres.get(i)).cloned() else { return };
                self.push_album_list(terminal, genre.value.clone(), |c| {
                    let value = genre.value.clone();
                    Box::pin(async move { c.albums_by_genre(&value).await })
                })
                .await;
            }
            Screen::SongList { songs, selected, .. } => {
                let Some(&real) = visible.get(*selected) else { return };
                if !songs.is_empty() {
                    self.play_from(songs.clone(), real).await;
                }
            }
            Screen::Queue { selected, tracks, .. } => {
                let Some(&real) = visible.get(*selected) else { return };
                if !tracks.is_empty() {
                    match self.proxy.play_at(real as u32).await {
                        Ok(()) => self.message.clear(),
                        Err(e) => self.message = format!("could not jump to track: {e}"),
                    }
                }
            }
            // Handled above (Search's own indexing; Lyrics re-engages follow).
            Screen::Search { .. } | Screen::Lyrics { .. } => {}
        }
    }

    /// `a`: like `activate_selection`, but appends the selected song (and
    /// the rest of its list) to the end of the current queue instead of
    /// replacing it — the only screens where "append instead of replace"
    /// means anything are the song-shaped ones.
    async fn append_selection(&mut self) {
        let visible = self.visible_indices();
        let (songs, real): (Vec<Song>, usize) = match self.top() {
            Screen::SongList { songs, selected, .. } => {
                let Some(&real) = visible.get(*selected) else { return };
                (songs.clone(), real)
            }
            Screen::Search { results, selected, editing, .. } if !*editing => {
                let Some(&real) = visible.get(*selected) else { return };
                (results.clone(), real)
            }
            _ => return,
        };
        if songs.is_empty() {
            return;
        }
        self.append_from(songs, real).await;
    }

    /// `f`: toggles star/favorite on the selected song, optimistically
    /// updating the local copy so the star glyph flips immediately rather
    /// than waiting on a re-fetch.
    async fn toggle_star(&mut self) {
        let visible = self.visible_indices();
        let song = match self.stack.last_mut() {
            Some(Screen::SongList { songs, selected, .. }) => visible.get(*selected).and_then(|&i| songs.get_mut(i)),
            Some(Screen::Search { results, selected, editing, .. }) if !*editing => {
                visible.get(*selected).and_then(|&i| results.get_mut(i))
            }
            _ => None,
        };
        let Some(song) = song else { return };
        let new_starred = !song.is_starred();
        let id = song.id.clone();
        match self.library.set_starred(&id, new_starred).await {
            Ok(()) => {
                // Re-borrow rather than reuse `song`: the match above already
                // released it, and re-fetching by id keeps this correct even
                // if the list was mutated in between (it isn't, here, but
                // this way that's not an invariant this code has to keep).
                let target = match self.stack.last_mut() {
                    Some(Screen::SongList { songs, .. }) => songs.iter_mut().find(|s| s.id == id),
                    Some(Screen::Search { results, .. }) => results.iter_mut().find(|s| s.id == id),
                    _ => None,
                };
                if let Some(target) = target {
                    target.starred = new_starred.then(String::new);
                }
                self.message.clear();
            }
            Err(e) => self.message = format!("could not update favorite: {e}"),
        }
    }

    #[allow(clippy::type_complexity)]
    async fn load<T>(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        loading_message: &str,
        fetch: impl FnOnce(
            &library::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<T>> + '_>>,
    ) -> Option<T> {
        self.message = loading_message.to_string();
        let _ = terminal.draw(|f| self.draw_message(f));
        match fetch(&self.library).await {
            Ok(v) => {
                self.message.clear();
                Some(v)
            }
            Err(e) => {
                self.message = format!("error: {e}");
                None
            }
        }
    }

    #[allow(clippy::type_complexity)]
    async fn push_album_list(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        title: String,
        fetch: impl FnOnce(
            &library::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Album>>> + '_>>,
    ) {
        if let Some(albums) = self.load(terminal, &format!("Loading {title}…"), fetch).await {
            self.stack.push(Screen::AlbumList { title, albums, selected: 0, filter: String::new() });
        }
    }

    #[allow(clippy::type_complexity)]
    async fn push_song_list(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        title: String,
        fetch: impl FnOnce(
            &library::Client,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Song>>> + '_>>,
    ) {
        if let Some(songs) = self.load(terminal, &format!("Loading {title}…"), fetch).await {
            self.stack.push(Screen::SongList { title, songs, selected: 0, filter: String::new() });
        }
    }

    async fn run_search(&mut self) {
        let Some(Screen::Search { query, .. }) = self.stack.last() else { return };
        if query.is_empty() {
            return;
        }
        let query = query.clone();
        self.message = format!("Searching {query}…");
        match self.library.search(&query).await {
            Ok(results) => {
                self.message.clear();
                if let Some(Screen::Search { results: r, editing, selected, .. }) = self.stack.last_mut() {
                    *r = results;
                    *editing = false;
                    *selected = 0;
                }
            }
            Err(e) => self.message = format!("search error: {e}"),
        }
    }

    /// Plays `songs[start_index..]` as a queue — clicking track 3 of an
    /// album/playlist/search-results list naturally plays 3, 4, 5, ... with
    /// `Next`/`Previous` (from the TUI or a hardware media key) walking it.
    async fn play_from(&mut self, songs: Vec<Song>, start_index: usize) {
        let tracks = self.build_queue_entries(&songs[start_index..]);
        match self.proxy.play_queue(tracks, 0).await {
            Ok(()) => self.message.clear(),
            Err(e) => self.message = format!("could not play: {e}"),
        }
    }

    /// Same track selection as `play_from` (this song and the rest of its
    /// list), but appends to the current queue instead of replacing it —
    /// for the `a` ("add to queue") key.
    async fn append_from(&mut self, songs: Vec<Song>, start_index: usize) {
        let tracks = self.build_queue_entries(&songs[start_index..]);
        let count = tracks.len();
        match self.proxy.append_queue(tracks).await {
            Ok(()) => self.message = format!("added {count} track(s) to the queue"),
            Err(e) => self.message = format!("could not add to queue: {e}"),
        }
    }

    fn build_queue_entries(&self, songs: &[Song]) -> Vec<QueueEntry> {
        songs
            .iter()
            .map(|s| {
                let stream_url = self.library.stream_url(&s.id);
                let art_url = self.library.cover_art_url(&s.cover_art);
                let (format_label, lossless) = library::format_label(&s.suffix, s.bit_rate);
                (
                    stream_url,
                    s.title.clone(),
                    s.artist.clone(),
                    s.album.clone(),
                    art_url,
                    s.duration,
                    format_label,
                    lossless,
                    s.id.clone(),
                )
            })
            .collect()
    }

    fn draw_message(&self, frame: &mut Frame) {
        let para = Paragraph::new(self.message.as_str()).block(rounded_block(" Maraetai "));
        frame.render_widget(para, frame.area());
    }

    fn draw(&mut self, frame: &mut Frame, now_playing: &NowPlaying) {
        // Now-playing panel: 2 border rows + `art::HEIGHT` (art, with
        // title/gauge/spectrum filling the rest of that height beside it)
        // + 1 (full-width state line) — see `draw_status_bar`.
        let now_playing_height = art::HEIGHT + 1 + 2;
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(3), Constraint::Length(now_playing_height)])
            .split(frame.area());

        self.draw_tab_bar(frame, chunks[0]);

        if self.show_help {
            self.draw_help(frame, chunks[1]);
            self.draw_status_bar(frame, chunks[2], now_playing);
            return;
        }

        let visible = self.visible_indices();
        match self.top() {
            Screen::AlbumList { title, albums, selected, filter } => {
                let items = visible.iter().map(|&i| format!("{}  —  {}", albums[i].name, albums[i].artist));
                self.draw_list(frame, chunks[1], &filter_hint_title(title, filter, self.filter_editing, "[Enter] open  [Esc] back"), items, *selected);
            }
            Screen::SongList { title, songs, selected, filter } => {
                let rows = visible.iter().map(|&i| {
                    let s = &songs[i];
                    let (fmt, lossless) = library::format_label(&s.suffix, s.bit_rate);
                    (s.title.as_str(), s.artist.as_str(), s.album.as_str(), s.duration, fmt, lossless, s.is_starred())
                });
                let title = filter_hint_title(title, filter, self.filter_editing, "[Enter] play  [a]dd  [f]avorite  [Esc] back");
                self.draw_song_table(frame, chunks[1], &title, rows, *selected, &now_playing.title);
            }
            Screen::ArtistList { artists, selected, filter } => {
                let items = visible.iter().map(|&i| format!("{}  ({} albums)", artists[i].name, artists[i].album_count));
                self.draw_list(frame, chunks[1], &filter_hint_title("Artists", filter, self.filter_editing, "[Enter] open  [Esc] back"), items, *selected);
            }
            Screen::PlaylistList { playlists, selected, filter } => {
                let items = visible.iter().map(|&i| format!("{}  ({} songs)", playlists[i].name, playlists[i].song_count));
                self.draw_list(
                    frame,
                    chunks[1],
                    &filter_hint_title("Playlists", filter, self.filter_editing, "[Enter] open  [Esc] back"),
                    items,
                    *selected,
                );
            }
            Screen::GenreList { genres, selected, filter } => {
                let items = visible.iter().map(|&i| {
                    let g = &genres[i];
                    format!("{}  ({} albums, {} songs)", g.value, g.album_count, g.song_count)
                });
                self.draw_list(frame, chunks[1], &filter_hint_title("Genres", filter, self.filter_editing, "[Enter] open  [Esc] back"), items, *selected);
            }
            Screen::Search { query, editing, results, selected } => {
                let title = if *editing {
                    format!(" Search: {query}_  [Enter] run  [Esc] stop editing ")
                } else {
                    format!(" Search: {query}  [Enter] play  [a]dd  [f]avorite  [/] new search ")
                };
                let rows = results.iter().map(|s| {
                    let (fmt, lossless) = library::format_label(&s.suffix, s.bit_rate);
                    (s.title.as_str(), s.artist.as_str(), s.album.as_str(), s.duration, fmt, lossless, s.is_starred())
                });
                self.draw_song_table(frame, chunks[1], &title, rows, *selected, &now_playing.title);
            }
            Screen::Queue { tracks, selected, filter } => {
                let rows = visible.iter().map(|&i| {
                    let (t, a, al, d, fmt, lossless) = &tracks[i];
                    (t.as_str(), a.as_str(), al.as_str(), *d, fmt.clone(), *lossless, false)
                });
                let title = filter_hint_title("Queue", filter, self.filter_editing, "[Enter] jump to track  [Esc] back");
                self.draw_song_table(frame, chunks[1], &title, rows, *selected, &now_playing.title);
            }
            Screen::Lyrics { scroll, follow } => {
                self.draw_lyrics(frame, chunks[1], *scroll, *follow, now_playing);
            }
        }

        self.draw_status_bar(frame, chunks[2], now_playing);
    }

    /// A full-screen keybind reference, overlaying the content area (the
    /// now-playing panel stays visible underneath) — opened with `?`,
    /// closed by any key.
    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        const BINDINGS: &[(&str, &str)] = &[
            ("1-9  [ ]", "switch / cycle tabs"),
            ("Up/k Down/j", "move selection"),
            ("Enter", "open / play from here"),
            ("a", "add to queue (don't replace it)"),
            ("f", "toggle favorite on the selected song"),
            ("/", "filter this list (Esc clears it)"),
            ("Esc / Backspace", "clear filter, then back"),
            ("space", "play / pause"),
            ("n / p", "next / previous track"),
            ("Left / Right", "seek -5s / +5s"),
            ("- / +", "volume down / up"),
            ("r", "cycle repeat: off -> track -> queue"),
            ("x", "toggle shuffle"),
            ("s", "stop"),
            ("q", "quit"),
            ("Q", "quit and stop the daemon"),
            ("?", "toggle this help"),
        ];
        let lines: Vec<Line> = BINDINGS
            .iter()
            .map(|(key, desc)| {
                Line::from(vec![
                    Span::styled(format!("{key:<16}"), theme::accent()),
                    Span::raw(*desc),
                ])
            })
            .collect();
        let block = rounded_block(" Keybindings — press any key to close ");
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// The persistent top tab bar — rmpc's signature "always-visible
    /// navigation", replacing a menu screen you'd otherwise have to enter.
    fn draw_tab_bar(&self, frame: &mut Frame, area: Rect) {
        let mut spans = Vec::with_capacity(TABS.len() * 2);
        for (i, &tab) in TABS.iter().enumerate() {
            let style = if tab == self.active_tab { theme::active_tab() } else { theme::inactive_tab() };
            spans.push(Span::styled(format!(" {} {} ", i + 1, tab.label()), style));
            spans.push(Span::raw(" "));
        }
        let para = Paragraph::new(Line::from(spans)).block(rounded_block(""));
        frame.render_widget(para, area);
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, title: &str, items: impl Iterator<Item = String>, selected: usize) {
        let items: Vec<ListItem> = items.map(ListItem::new).collect();
        let len = items.len();
        let list = List::new(items).block(rounded_block(title.to_string())).highlight_style(theme::selected());
        let mut state = ListState::default();
        if len > 0 {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(list, area, &mut state);
        self.render_scrollbar(frame, area, len, selected);
    }

    /// Renders a column-aligned track table — Artist / Title / Album /
    /// Format / Time, duration right-aligned, rmpc's convention — with the
    /// currently-playing row (matched by title, the only stable identifier
    /// available client-side) highlighted regardless of cursor position.
    /// Lossless formats are green; lossy ones use the row's own color.
    fn draw_song_table<'r>(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        rows: impl Iterator<Item = (&'r str, &'r str, &'r str, f64, String, bool, bool)>,
        selected: usize,
        now_playing_title: &str,
    ) {
        let mut len = 0;
        let table_rows: Vec<Row> = rows
            .map(|(t, artist, album, dur, format, lossless, starred)| {
                len += 1;
                let is_playing = !now_playing_title.is_empty() && t == now_playing_title;
                let row_style = if is_playing { theme::now_playing_row() } else { Style::default() };
                let format_style = if lossless { theme::lossless() } else { row_style };
                let title_text = if starred { format!("\u{2605} {t}") } else { t.to_string() };
                let title_style = if starred { theme::state_tag() } else { row_style };
                Row::new(vec![
                    Cell::from(artist.to_string()),
                    Cell::from(title_text).style(title_style),
                    Cell::from(album.to_string()),
                    Cell::from(format).style(format_style),
                    Cell::from(Text::from(fmt_time(dur)).alignment(Alignment::Right)),
                ])
                .style(row_style)
            })
            .collect();

        let header = Row::new(vec!["Artist", "Title", "Album", "Format", "Time"]).style(theme::header());
        let widths = [
            Constraint::Percentage(22),
            Constraint::Percentage(36),
            Constraint::Percentage(22),
            Constraint::Length(10),
            Constraint::Length(6),
        ];
        let table = Table::new(table_rows, widths)
            .header(header)
            .block(rounded_block(title.to_string()))
            .highlight_style(theme::selected());

        let mut state = TableState::default();
        if len > 0 {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(table, area, &mut state);
        self.render_scrollbar(frame, area, len, selected);
    }

    /// A vertical scrollbar drawn over the pane's own right border — a
    /// small rmpc touch (it themes scrollbars explicitly) that also just
    /// makes "how much more is there" legible at a glance in a long list.
    /// Skipped entirely when everything already fits without scrolling
    /// (a full-height thumb there would just paint over the border for no
    /// reason), and inset by one row top/bottom so the track doesn't
    /// collide with the pane's corner glyphs.
    fn render_scrollbar(&self, frame: &mut Frame, area: Rect, len: usize, position: usize) {
        let viewport = area.height.saturating_sub(2) as usize;
        if len == 0 || len <= viewport {
            return;
        }
        let mut state = ScrollbarState::new(len).position(position);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(theme::border());
        frame.render_stateful_widget(scrollbar, area.inner(Margin { vertical: 1, horizontal: 0 }), &mut state);
    }

    /// Renders whatever's in `self.lyrics` for the current track. Time-synced
    /// lyrics highlight the line matching `now_playing.position` and, while
    /// `follow` is on, keep it a third of the way down the visible area
    /// (auto-scrolling as playback advances); unsynced lyrics are just a
    /// static block the user scrolls manually. `follow` scroll is computed
    /// fresh every frame from the live position rather than written back
    /// into the screen's own `scroll` field, since it's meaningless the
    /// moment the user takes over with a manual scroll anyway.
    fn draw_lyrics(&self, frame: &mut Frame, area: Rect, scroll: usize, follow: bool, now_playing: &NowPlaying) {
        let hint = if follow { "[Up/Down] scroll" } else { "[Up/Down] scroll  [Enter] follow" };
        let block = rounded_block(format!(" Lyrics — {hint} "));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(lines) = &self.lyrics else {
            let msg = if self.current_lyrics_song_id.is_some() { "Loading lyrics…" } else { "(nothing playing)" };
            frame.render_widget(Paragraph::new(Span::styled(msg, theme::muted())), inner);
            return;
        };
        if lines.is_empty() {
            frame.render_widget(Paragraph::new(Span::styled("No lyrics found for this track.", theme::muted())), inner);
            return;
        }

        let position_ms = (now_playing.position * 1000.0).round() as i64;
        let active_index = lines.iter().rposition(|l| l.start_ms.is_some_and(|start| start <= position_ms));

        let effective_scroll = if follow {
            match active_index {
                Some(i) => i.saturating_sub((inner.height as usize) / 3),
                None => 0,
            }
        } else {
            scroll
        };
        let effective_scroll = effective_scroll.min(lines.len().saturating_sub(1)) as u16;

        let rendered: Vec<Line> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let style = if Some(i) == active_index { theme::accent() } else { Style::default() };
                Line::from(Span::styled(l.text.clone(), style))
            })
            .collect();
        frame.render_widget(Paragraph::new(rendered).scroll((effective_scroll, 0)), inner);
    }

    fn draw_status_bar(&mut self, frame: &mut Frame, area: Rect, now_playing: &NowPlaying) {
        // `Block::inner` already accounts for the border on all four sides —
        // an additional `.margin(1)` on the Layout on top of that would
        // leave too little space for the requested rows (found the hard way
        // once already). No extra margin needed here.
        let inner = rounded_block(" Now Playing ").inner(area);
        frame.render_widget(rounded_block(" Now Playing "), area);

        // Art-height section on top (art on the left, title/gauge/spectrum
        // stacked in the rest of the space on the right), state line
        // pinned full-width at the very bottom.
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(art::HEIGHT), Constraint::Length(1)])
            .split(inner);

        // `art::WIDTH + 2` gives the art one column of breathing room on
        // each side rather than butting straight up against the border;
        // a separate gap column then keeps the info column from butting
        // straight up against the art itself.
        const GAP: u16 = 3;
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(art::WIDTH + 2), Constraint::Length(GAP), Constraint::Min(20)])
            .split(sections[0]);

        if let Some(protocol) = self.art_protocol.as_mut() {
            // `Fit`, not `Crop`: ratatui-image's `Crop` clips raw pixels out
            // of the *original* image at the box's target pixel size with no
            // scaling step, which on a real (large) cover zooms into an
            // unrecognizable corner rather than showing a shrunk crop. The
            // art is already pre-cropped to a square in `art.rs`
            // (`center_crop_to_square`), so `Fit`'s aspect-preserving scale
            // now fills this near-square box with little to no letterboxing.
            let widget = StatefulImage::new(None).resize(Resize::Fit(None));
            frame.render_stateful_widget(widget, cols[0], protocol);
        } else {
            let placeholder = Paragraph::new(art::placeholder()).alignment(Alignment::Center);
            frame.render_widget(placeholder, cols[0]);
        }

        // Title and gauge get one row each; the spectrum then fills
        // whatever's left of the column — the entire rest of the space to
        // the right of the art, both width- and height-wise.
        let info_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
            .split(cols[2]);

        // Track — artist [format], accent-colored like rmpc's title.
        let track = if now_playing.title.is_empty() { "(nothing loaded)" } else { &now_playing.title };
        let mut spans = vec![Span::styled(track, theme::accent())];
        if !now_playing.artist.is_empty() {
            spans.push(Span::raw("  —  "));
            spans.push(Span::raw(now_playing.artist.clone()));
        }
        if !now_playing.format_label.is_empty() {
            let format_style = if now_playing.lossless { theme::lossless() } else { theme::muted() };
            spans.push(Span::raw("   "));
            spans.push(Span::styled(format!("[{}]", now_playing.format_label), format_style));
        }
        if now_playing.queue_len > 0 {
            spans.push(Span::styled(
                format!("   [{} of {}]", now_playing.queue_index + 1, now_playing.queue_len),
                theme::muted(),
            ));
        }
        if !self.message.is_empty() {
            spans = vec![Span::raw(self.message.clone())];
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), info_rows[0]);

        // A real progress gauge (position/duration).
        let ratio = if now_playing.duration > 0.0 {
            (now_playing.position / now_playing.duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let label = if now_playing.duration > 0.0 {
            format!("{} / {}", fmt_time(now_playing.position), fmt_time(now_playing.duration))
        } else {
            fmt_time(now_playing.position)
        };
        let gauge = Gauge::default().gauge_style(Style::default().fg(Color::Blue)).ratio(ratio).label(label);
        frame.render_widget(gauge, info_rows[1]);

        // A real spectrum visualizer — bar heights come from an actual FFT
        // of the currently decoding audio (see daemon/src/visualizer.rs),
        // not a simulated animation — filling whatever space is left beside
        // the art (both width and height), rendered across multiple rows of
        // sub-cell vertical resolution rather than flattened into one row.
        let spectrum_area = info_rows[2];
        frame.render_widget(
            Paragraph::new(spectrum_rows(&now_playing.spectrum, spectrum_area.width, spectrum_area.height)),
            spectrum_area,
        );

        // [state] tag (rmpc's bracketed-yellow convention) + a compact
        // inline volume slider + keybinding hints — spans the full panel
        // width, under both the art and the info column.
        let mut state_spans = vec![
            Span::styled("[", theme::state_tag()),
            Span::styled(now_playing.status.to_uppercase(), theme::state_tag()),
            Span::styled("]", theme::state_tag()),
        ];
        // Repeat/shuffle only take up space on the line when they're
        // actually doing something — "off" and "not shuffled" are the
        // common case and don't need announcing every frame.
        if now_playing.repeat != "off" {
            state_spans.push(Span::raw(" "));
            state_spans.push(Span::styled(format!("[repeat:{}]", now_playing.repeat), theme::accent()));
        }
        if now_playing.shuffle {
            state_spans.push(Span::raw(" "));
            state_spans.push(Span::styled("[shuffle]", theme::accent()));
        }
        let muted_from = state_spans.len();
        state_spans.push(Span::raw("  "));
        state_spans.push(Span::raw(volume_slider(now_playing.volume)));
        state_spans.push(Span::raw("   [space] play/pause  [\u{2190}/\u{2192}] seek  [n]ext [p]rev  [s]top  [?] help"));
        for span in &mut state_spans[muted_from..] {
            span.style = theme::muted().patch(span.style);
        }
        frame.render_widget(Paragraph::new(Line::from(state_spans)), sections[1]);
    }
}

/// A compact inline volume slider — `───●──────  60%`, rmpc's slider
/// component condensed into the one spare line this layout has for it.
fn volume_slider(volume: f64) -> String {
    const WIDTH: usize = 10;
    // Clamped to WIDTH - 1, not WIDTH: the marker is drawn at index
    // `filled` inside a 0..WIDTH loop, so leaving it at WIDTH would put
    // full volume's marker one cell past the visible bar — i.e. nowhere.
    let filled = ((volume.clamp(0.0, 1.0) * WIDTH as f64).round() as usize).min(WIDTH - 1);
    let mut bar = String::with_capacity(WIDTH);
    for i in 0..WIDTH {
        bar.push(if i == filled { '\u{25cf}' } else { '\u{2500}' });
    }
    format!("{bar} {:>3}%", (volume * 100.0).round() as i32)
}

/// Renders spectrum bar levels across `height` terminal rows — real
/// vertical resolution (each bar can fill part-way into a row via the
/// `▁▂▃▄▅▆▇` sub-level glyphs, not just "on or off" per row) — stretched to
/// fill the full `width` and `height` given rather than a fixed handful of
/// rows/columns, so the visualizer fills whatever space it's given and
/// scales up as the terminal is resized. Levels arrive quantized against
/// `spectrum::MAX_LEVEL` (a fixed wire-format granularity, independent of
/// any particular terminal's size) and are rescaled here to however many
/// rows are actually available. Rows nearer the top (i.e. reached only by
/// louder bars) are brighter, like a classic equalizer's hot/cool gradient
/// recolored into this theme's blues.
fn spectrum_rows(levels: &[u8], width: u16, height: u16) -> Vec<Line<'static>> {
    const SUB: [char; 8] = ['\u{0020}', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}'];
    let rows = (height as usize).max(1);

    if levels.is_empty() {
        return (0..rows)
            .map(|r| {
                if r == rows / 2 {
                    Line::from(Span::styled("(no signal)", theme::muted()))
                } else {
                    Line::default()
                }
            })
            .collect();
    }

    // Each bar gets an equal slot of the available width, with the last
    // column of a slot left blank as a gap between bars (skipped once
    // slots get too narrow to spare it).
    let slot = ((width as usize) / levels.len()).max(1);
    let gap = usize::from(slot > 1);
    let fill_width = slot - gap;

    (0..rows)
        .map(|r| {
            let row_from_bottom = rows - 1 - r;
            let color = if r < rows / 3 {
                Color::LightBlue
            } else if r < rows * 2 / 3 {
                Color::Blue
            } else {
                Color::DarkGray
            };
            let spans: Vec<Span<'static>> = levels
                .iter()
                .flat_map(|&level| {
                    // Rescale the fixed-granularity wire level to however
                    // many eighths-of-a-row actually fit in `rows`, so a
                    // maxed-out level always reaches the very top row
                    // regardless of how tall this render happens to be.
                    let normalized = level as f32 / spectrum::MAX_LEVEL as f32;
                    let eighths = (normalized * (rows * 8) as f32).round() as usize;
                    let full_rows = eighths / 8;
                    let remainder = eighths % 8;
                    let ch = if row_from_bottom < full_rows {
                        '\u{2588}'
                    } else if row_from_bottom == full_rows {
                        SUB[remainder]
                    } else {
                        ' '
                    };
                    [
                        Span::styled(ch.to_string().repeat(fill_width), Style::default().fg(color)),
                        Span::raw(" ".repeat(gap)),
                    ]
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_slash_while_editing_is_swallowed_not_inserted() {
        let mut filter = String::new();
        let mut selected = 0;
        // First `/` is what enters edit mode in the caller — this test
        // starts from "already editing" (the state that matters) and
        // checks a `/` reaching this function never ends up in the text.
        let result = apply_filter_key(&mut filter, &mut selected, KeyCode::Char('/'));
        assert_eq!(result, Some((false, true)), "must stay in edit mode and consume the key");
        assert_eq!(filter, "", "a `/` must never be inserted into the filter text");
    }

    #[test]
    fn ordinary_characters_are_typed_into_the_filter() {
        let mut filter = String::from("ab");
        let mut selected = 3;
        let result = apply_filter_key(&mut filter, &mut selected, KeyCode::Char('c'));
        assert_eq!(result, Some((false, true)));
        assert_eq!(filter, "abc");
        assert_eq!(selected, 0, "typing must reset the selection to the top of the new filtered view");
    }

    #[test]
    fn backspace_removes_the_last_character() {
        let mut filter = String::from("abc");
        let mut selected = 1;
        let result = apply_filter_key(&mut filter, &mut selected, KeyCode::Backspace);
        assert_eq!(result, Some((false, true)));
        assert_eq!(filter, "ab");
        assert_eq!(selected, 0);
    }

    #[test]
    fn escape_clears_the_filter_and_stops_editing() {
        let mut filter = String::from("abc");
        let mut selected = 2;
        let result = apply_filter_key(&mut filter, &mut selected, KeyCode::Esc);
        assert_eq!(result, Some((true, true)));
        assert_eq!(filter, "");
        assert_eq!(selected, 0);
    }

    #[test]
    fn enter_stops_editing_without_consuming_the_key_or_touching_the_filter() {
        // Not consumed is the important part: it's what lets the same
        // Enter press also activate the current selection, in the same
        // event-loop iteration, instead of needing a second Enter.
        let mut filter = String::from("abc");
        let mut selected = 2;
        let result = apply_filter_key(&mut filter, &mut selected, KeyCode::Enter);
        assert_eq!(result, Some((true, false)));
        assert_eq!(filter, "abc", "Enter must not alter the filter text");
        assert_eq!(selected, 2, "Enter must not alter the selection");
    }

    #[test]
    fn navigation_keys_are_not_handled_here() {
        let mut filter = String::from("abc");
        let mut selected = 2;
        assert_eq!(apply_filter_key(&mut filter, &mut selected, KeyCode::Up), None);
        assert_eq!(apply_filter_key(&mut filter, &mut selected, KeyCode::Down), None);
        // Unchanged either way.
        assert_eq!(filter, "abc");
        assert_eq!(selected, 2);
    }

    #[test]
    fn filter_hint_title_shows_a_cursor_while_editing_even_when_empty() {
        // This is the actual fix for "pressed / and nothing seemed to
        // happen" — there must be a visible difference the instant editing
        // starts, before any character has been typed.
        let editing_empty = filter_hint_title("Albums", "", true, "[Enter] open");
        let not_editing_empty = filter_hint_title("Albums", "", false, "[Enter] open");
        assert_ne!(editing_empty, not_editing_empty);
        assert!(editing_empty.contains("filter:"));
    }

    #[test]
    fn filter_hint_title_shows_the_filter_text_after_editing_stops() {
        let title = filter_hint_title("Albums", "mez", false, "[Enter] open");
        assert!(title.contains("filter: mez"));
        assert!(!title.contains('_'), "no cursor once editing has stopped");
    }
}
