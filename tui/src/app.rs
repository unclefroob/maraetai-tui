//! The interactive TUI: browse albums/artists/playlists/genres, search, and
//! play — talking to `maraetai-service` directly for library data (the same
//! way every other maraetai client does) and to the daemon over D-Bus for
//! playback/queue control.
//!
//! Visually modeled on `cmus`: a colored, always-visible now-playing bar
//! with a real progress gauge, column-aligned track tables instead of
//! plain text lists, the currently-playing row highlighted wherever it
//! appears, and `1`/`2` as quick view switches (Library / Queue) alongside
//! the drill-down navigation stack.
//!
//! Navigation is a plain stack (`Vec<Screen>`) — drilling in pushes, `Esc`/
//! `Backspace` pops. Playing a song from a list queues the *rest* of that
//! list from the selected point onward (so picking track 3 of an album
//! naturally plays 3, 4, 5, ...); the daemon owns queue advancement, so
//! `Next`/`Previous` (here or from a hardware media key) work the same way
//! regardless of which screen started the queue.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, List, ListItem, ListState, Paragraph, Row, Table, TableState};

use crate::dbus_client::{ControlProxy, QueueEntry};
use crate::library::{self, Album, Artist, Genre, Playlist, Song};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const MENU_ITEMS: [&str; 4] = ["Albums", "Artists", "Playlists", "Genres"];
const SEEK_STEP: f64 = 5.0;
const VOLUME_STEP: f64 = 0.05;

/// A cmus-inspired palette — cyan accents on the default terminal
/// background, a blue selection bar, yellow column headers.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub fn accent() -> Style {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    pub fn header() -> Style {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    }
    pub fn selected() -> Style {
        Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
    }
    pub fn now_playing_row() -> Style {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    }
    pub fn muted() -> Style {
        Style::default().fg(Color::DarkGray)
    }
    pub fn border() -> Style {
        Style::default().fg(Color::Cyan)
    }
}

/// One entry in the "Queue" view — title/artist/album/duration/format_label/
/// lossless, as returned by the daemon's `Queue()` method.
type QueueRow = (String, String, String, f64, String, bool);

enum Screen {
    Menu {
        selected: usize,
    },
    AlbumList {
        title: String,
        albums: Vec<Album>,
        selected: usize,
    },
    SongList {
        title: String,
        songs: Vec<Song>,
        selected: usize,
    },
    ArtistList {
        artists: Vec<Artist>,
        selected: usize,
    },
    PlaylistList {
        playlists: Vec<Playlist>,
        selected: usize,
    },
    GenreList {
        genres: Vec<Genre>,
        selected: usize,
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
    /// Current spectrum bar levels (0..=7 each), polled alongside status.
    /// Empty (not all-zero — genuinely empty) while disconnected.
    spectrum: Vec<u8>,
}

struct App<'a> {
    proxy: ControlProxy<'a>,
    library: library::Client,
    stack: Vec<Screen>,
    /// A transient status/error line shown above the now-playing bar — e.g.
    /// "Loading…" during a fetch, or a fetch failure. Cleared on the next
    /// successful action.
    message: String,
}

pub async fn run(proxy: ControlProxy<'_>, creds: maraetai_common::Credentials) -> Result<()> {
    let mut app = App {
        proxy,
        library: library::Client::new(creds),
        stack: vec![Screen::Menu { selected: 0 }],
        message: String::new(),
    };

    let mut terminal = ratatui::init();
    let result = app.event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

fn fmt_time(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl App<'_> {
    fn top(&self) -> &Screen {
        self.stack.last().expect("stack is never empty")
    }

    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            // A blocking poll on this task is an accepted simplification —
            // this binary has no other async work contending for the thread
            // besides the status poll below.
            let now_playing = self.fetch_status().await;
            terminal.draw(|frame| self.draw(frame, &now_playing))?;

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if let Screen::Search { editing: true, .. } = self.top() {
                if self.handle_search_edit(key.code).await {
                    continue;
                }
            }

            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('Q') => {
                    let _ = self.proxy.quit().await;
                    return Ok(());
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
                KeyCode::Char('1') => {
                    self.stack = vec![Screen::Menu { selected: 0 }];
                }
                KeyCode::Char('2') => {
                    self.load_queue_view(terminal).await;
                }
                KeyCode::Char('/') => {
                    self.stack.push(Screen::Search {
                        query: String::new(),
                        editing: true,
                        results: Vec::new(),
                        selected: 0,
                    });
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter => self.activate_selection(terminal).await,
                KeyCode::Esc | KeyCode::Backspace if self.stack.len() > 1 => {
                    self.stack.pop();
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
                spectrum: Vec::new(),
            },
        }
    }

    async fn load_queue_view(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        self.message = "Loading queue…".to_string();
        let _ = terminal.draw(|f| self.draw_message(f));
        match self.proxy.queue().await {
            Ok(tracks) => {
                self.message.clear();
                self.stack.push(Screen::Queue { tracks, selected: 0 });
            }
            Err(e) => self.message = format!("error: {e}"),
        }
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
                self.stack.pop();
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

    fn move_selection(&mut self, delta: i32) {
        let Some(screen) = self.stack.last_mut() else { return };
        let (selected, len) = match screen {
            Screen::Menu { selected } => (selected, MENU_ITEMS.len()),
            Screen::AlbumList { selected, albums, .. } => (selected, albums.len()),
            Screen::SongList { selected, songs, .. } => (selected, songs.len()),
            Screen::ArtistList { selected, artists } => (selected, artists.len()),
            Screen::PlaylistList { selected, playlists } => (selected, playlists.len()),
            Screen::GenreList { selected, genres } => (selected, genres.len()),
            Screen::Queue { selected, tracks } => (selected, tracks.len()),
            Screen::Search { selected, results, editing, .. } if !*editing => (selected, results.len()),
            Screen::Search { .. } => return,
        };
        if len == 0 {
            return;
        }
        *selected = (*selected as i32 + delta).rem_euclid(len as i32) as usize;
    }

    async fn activate_selection(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        match self.top() {
            Screen::Menu { selected } => {
                let selected = *selected;
                self.enter_menu_item(terminal, selected).await;
            }
            Screen::AlbumList { albums, selected, .. } => {
                if let Some(album) = albums.get(*selected).cloned() {
                    self.push_song_list(terminal, album.name.clone(), |c| {
                        let id = album.id.clone();
                        Box::pin(async move { c.album_songs(&id).await })
                    })
                    .await;
                }
            }
            Screen::ArtistList { artists, selected } => {
                if let Some(artist) = artists.get(*selected).cloned() {
                    self.push_album_list(terminal, artist.name.clone(), |c| {
                        let id = artist.id.clone();
                        Box::pin(async move { c.artist_albums(&id).await })
                    })
                    .await;
                }
            }
            Screen::PlaylistList { playlists, selected } => {
                if let Some(pl) = playlists.get(*selected).cloned() {
                    self.push_song_list(terminal, pl.name.clone(), |c| {
                        let id = pl.id.clone();
                        Box::pin(async move { c.playlist_songs(&id).await })
                    })
                    .await;
                }
            }
            Screen::GenreList { genres, selected } => {
                if let Some(genre) = genres.get(*selected).cloned() {
                    self.push_album_list(terminal, genre.value.clone(), |c| {
                        let value = genre.value.clone();
                        Box::pin(async move { c.albums_by_genre(&value).await })
                    })
                    .await;
                }
            }
            Screen::SongList { songs, selected, .. } => {
                if !songs.is_empty() {
                    self.play_from(songs.clone(), *selected).await;
                }
            }
            Screen::Search { results, selected, editing, .. } if !*editing => {
                if !results.is_empty() {
                    self.play_from(results.clone(), *selected).await;
                }
            }
            Screen::Search { .. } => {}
            Screen::Queue { selected, tracks } => {
                if !tracks.is_empty() {
                    let index = *selected as u32;
                    match self.proxy.play_at(index).await {
                        Ok(()) => self.message.clear(),
                        Err(e) => self.message = format!("could not jump to track: {e}"),
                    }
                }
            }
        }
    }

    async fn enter_menu_item(&mut self, terminal: &mut ratatui::DefaultTerminal, selected: usize) {
        match MENU_ITEMS.get(selected).copied() {
            Some("Albums") => {
                if let Some(albums) =
                    self.load(terminal, "Loading albums…", |c| Box::pin(async move { c.albums().await })).await
                {
                    self.stack.push(Screen::AlbumList { title: "Albums".into(), albums, selected: 0 });
                }
            }
            Some("Artists") => {
                if let Some(artists) = self
                    .load(terminal, "Loading artists…", |c| Box::pin(async move { c.artists().await }))
                    .await
                {
                    self.stack.push(Screen::ArtistList { artists, selected: 0 });
                }
            }
            Some("Playlists") => {
                if let Some(playlists) = self
                    .load(terminal, "Loading playlists…", |c| Box::pin(async move { c.playlists().await }))
                    .await
                {
                    self.stack.push(Screen::PlaylistList { playlists, selected: 0 });
                }
            }
            Some("Genres") => {
                if let Some(genres) = self
                    .load(terminal, "Loading genres…", |c| Box::pin(async move { c.genres().await }))
                    .await
                {
                    self.stack.push(Screen::GenreList { genres, selected: 0 });
                }
            }
            _ => {}
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
            self.stack.push(Screen::AlbumList { title, albums, selected: 0 });
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
            self.stack.push(Screen::SongList { title, songs, selected: 0 });
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
        let tracks: Vec<QueueEntry> = songs[start_index..]
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
                )
            })
            .collect();
        match self.proxy.play_queue(tracks, 0).await {
            Ok(()) => self.message.clear(),
            Err(e) => self.message = format!("could not play: {e}"),
        }
    }

    fn draw_message(&self, frame: &mut Frame) {
        let para = Paragraph::new(self.message.as_str())
            .block(Block::default().borders(Borders::ALL).border_style(theme::border()).title(" Maraetai "));
        frame.render_widget(para, frame.area());
    }

    fn draw(&self, frame: &mut Frame, now_playing: &NowPlaying) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(6)])
            .split(frame.area());

        match self.top() {
            Screen::Menu { selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Maraetai — [Enter] open  [/] search  [1] library  [2] queue  [q] quit ",
                    MENU_ITEMS.iter().map(|s| s.to_string()),
                    *selected,
                );
            }
            Screen::AlbumList { title, albums, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    &format!(" {title} — [Enter] open  [Esc] back "),
                    albums.iter().map(|a| format!("{}  —  {}", a.name, a.artist)),
                    *selected,
                );
            }
            Screen::SongList { title, songs, selected } => {
                let rows = songs.iter().map(|s| {
                    let (fmt, lossless) = library::format_label(&s.suffix, s.bit_rate);
                    (s.title.as_str(), s.artist.as_str(), s.album.as_str(), s.duration, fmt, lossless)
                });
                self.draw_song_table(
                    frame,
                    chunks[0],
                    &format!(" {title} — [Enter] play from here  [Esc] back "),
                    rows,
                    *selected,
                    &now_playing.title,
                );
            }
            Screen::ArtistList { artists, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Artists — [Enter] open  [Esc] back ",
                    artists.iter().map(|a| format!("{}  ({} albums)", a.name, a.album_count)),
                    *selected,
                );
            }
            Screen::PlaylistList { playlists, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Playlists — [Enter] open  [Esc] back ",
                    playlists.iter().map(|p| format!("{}  ({} songs)", p.name, p.song_count)),
                    *selected,
                );
            }
            Screen::GenreList { genres, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Genres — [Enter] open  [Esc] back ",
                    genres
                        .iter()
                        .map(|g| format!("{}  ({} albums, {} songs)", g.value, g.album_count, g.song_count)),
                    *selected,
                );
            }
            Screen::Search { query, editing, results, selected } => {
                let title = if *editing {
                    format!(" Search: {query}_  [Enter] run  [Esc] cancel ")
                } else {
                    format!(" Search: {query}  [Enter] play from here  [Esc] back ")
                };
                let rows = results.iter().map(|s| {
                    let (fmt, lossless) = library::format_label(&s.suffix, s.bit_rate);
                    (s.title.as_str(), s.artist.as_str(), s.album.as_str(), s.duration, fmt, lossless)
                });
                self.draw_song_table(frame, chunks[0], &title, rows, *selected, &now_playing.title);
            }
            Screen::Queue { tracks, selected } => {
                let rows = tracks
                    .iter()
                    .map(|(t, a, al, d, fmt, lossless)| (t.as_str(), a.as_str(), al.as_str(), *d, fmt.clone(), *lossless));
                self.draw_song_table(
                    frame,
                    chunks[0],
                    " Queue — [Enter] jump to track  [Esc] back ",
                    rows,
                    *selected,
                    &now_playing.title,
                );
            }
        }

        self.draw_status_bar(frame, chunks[1], now_playing);
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, title: &str, items: impl Iterator<Item = String>, selected: usize) {
        let items: Vec<ListItem> = items.map(ListItem::new).collect();
        let empty = items.is_empty();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).border_style(theme::border()).title(title.to_string()))
            .highlight_style(theme::selected());
        let mut state = ListState::default();
        if !empty {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }

    /// Renders a column-aligned track table (Title / Artist / Album / Format
    /// / Time), cmus-style, with the currently-playing row (matched by
    /// title — the only stable identifier available client-side)
    /// highlighted regardless of cursor position. Lossless formats (FLAC,
    /// ALAC, WAV, ...) are shown in green; lossy ones in the default color.
    fn draw_song_table<'r>(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        rows: impl Iterator<Item = (&'r str, &'r str, &'r str, f64, String, bool)>,
        selected: usize,
        now_playing_title: &str,
    ) {
        let mut any = false;
        let table_rows: Vec<Row> = rows
            .map(|(t, artist, album, dur, format, lossless)| {
                any = true;
                let is_playing = !now_playing_title.is_empty() && t == now_playing_title;
                let row_style = if is_playing { theme::now_playing_row() } else { Style::default() };
                let format_style = if lossless {
                    Style::default().fg(Color::Green).add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    row_style
                };
                Row::new(vec![
                    Cell::from(t.to_string()),
                    Cell::from(artist.to_string()),
                    Cell::from(album.to_string()),
                    Cell::from(format).style(format_style),
                    Cell::from(fmt_time(dur)),
                ])
                .style(row_style)
            })
            .collect();

        let header = Row::new(vec!["Title", "Artist", "Album", "Format", "Time"]).style(theme::header());
        let widths = [
            Constraint::Percentage(36),
            Constraint::Percentage(22),
            Constraint::Percentage(22),
            Constraint::Length(10),
            Constraint::Length(6),
        ];
        let table = Table::new(table_rows, widths)
            .header(header)
            .block(Block::default().borders(Borders::ALL).border_style(theme::border()).title(title.to_string()))
            .highlight_style(theme::selected());

        let mut state = TableState::default();
        if any {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(table, area, &mut state);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect, now_playing: &NowPlaying) {
        // `Block::inner` already accounts for the border on all four sides —
        // an additional `.margin(1)` on the Layout on top of that left only
        // 2 rows of space for the 3 requested (Length(1) x3), silently
        // clipping the gauge/status line. No extra margin needed here.
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
            .split(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme::border())
                    .title(" Now Playing ")
                    .inner(area),
            );
        frame.render_widget(
            Block::default().borders(Borders::ALL).border_style(theme::border()).title(" Now Playing "),
            area,
        );

        // Line 1: track — artist [format], styled like cmus's colored track line.
        let track = if now_playing.title.is_empty() { "(nothing loaded)" } else { &now_playing.title };
        let mut spans = vec![Span::styled(track, theme::accent())];
        if !now_playing.artist.is_empty() {
            spans.push(Span::raw("  —  "));
            spans.push(Span::styled(&now_playing.artist, Style::default().fg(Color::White)));
        }
        if !now_playing.format_label.is_empty() {
            let format_style = if now_playing.lossless {
                Style::default().fg(Color::Green).add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                theme::muted()
            };
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
        frame.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

        // Line 2: a real progress gauge (position/duration), cmus-style.
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
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(Color::Cyan))
            .ratio(ratio)
            .label(label);
        frame.render_widget(gauge, rows[1]);

        // Line 3: a real spectrum visualizer — bar heights come from an
        // actual FFT of the currently decoding audio (see
        // daemon/src/visualizer.rs), not a simulated animation.
        frame.render_widget(Paragraph::new(spectrum_line(&now_playing.spectrum)), rows[2]);

        // Line 4: transport state + volume + keybinding hints.
        let vol_pct = (now_playing.volume * 100.0).round() as i32;
        let state_line = format!(
            "{}   vol {vol_pct}%   [space] play/pause  [\u{2190}/\u{2192}] seek  [+/-] volume  [n]ext [p]rev  [s]top  [Q] quit+stop",
            now_playing.status
        );
        frame.render_widget(Paragraph::new(state_line).style(theme::muted()), rows[3]);
    }
}

/// Renders spectrum bar levels as a single line of block characters (one per
/// bar, so an N-bar spectrum fits in exactly one terminal row regardless of
/// N) — `▁` for silence up through `█` for the loudest level, cyan and
/// brighter for taller bars so the line has some visual "pop" even in a
/// screenshot, not just a flat color block.
fn spectrum_line(levels: &[u8]) -> Line<'static> {
    const CHARS: [char; 8] = ['\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}'];
    if levels.is_empty() {
        return Line::from(Span::styled("(no signal)", theme::muted()));
    }
    let spans: Vec<Span<'static>> = levels
        .iter()
        .flat_map(|&level| {
            let idx = (level as usize).min(CHARS.len() - 1);
            let color = if idx >= 6 {
                Color::LightCyan
            } else if idx >= 3 {
                Color::Cyan
            } else {
                Color::DarkGray
            };
            [
                Span::styled(CHARS[idx].to_string(), Style::default().fg(color)),
                Span::raw(" "),
            ]
        })
        .collect();
    Line::from(spans)
}
