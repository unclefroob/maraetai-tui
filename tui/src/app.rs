//! The interactive TUI: browse albums/artists/playlists/genres, search, and
//! play — talking to `maraetai-service` directly for library data (the same
//! way every other maraetai client does) and to the daemon over D-Bus for
//! playback/queue control.
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
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::dbus_client::{ControlProxy, QueueEntry};
use crate::library::{self, Album, Artist, Genre, Playlist, Song};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const MENU_ITEMS: [&str; 4] = ["Albums", "Artists", "Playlists", "Genres"];

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
}

/// A point-in-time playback status snapshot, as returned by `Status()`.
struct NowPlaying {
    status: String,
    title: String,
    position: f64,
    queue_index: u32,
    queue_len: u32,
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
                KeyCode::Esc | KeyCode::Backspace
                    if self.stack.len() > 1 => {
                        self.stack.pop();
                    }
                _ => {}
            }
        }
    }

    async fn fetch_status(&self) -> NowPlaying {
        match self.proxy.status().await {
            Ok((status, title, position, queue_index, queue_len)) => NowPlaying {
                status,
                title,
                position,
                queue_index,
                queue_len,
            },
            Err(_) => NowPlaying {
                status: "disconnected".to_string(),
                title: String::new(),
                position: 0.0,
                queue_index: 0,
                queue_len: 0,
            },
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
        }
    }

    async fn enter_menu_item(&mut self, terminal: &mut ratatui::DefaultTerminal, selected: usize) {
        match MENU_ITEMS.get(selected).copied() {
            Some("Albums") => {
                if let Some(albums) = self.load(terminal, "Loading albums…", |c| Box::pin(async move { c.albums().await }))
                    .await { self.stack.push(Screen::AlbumList { title: "Albums".into(), albums, selected: 0 }) }
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
                (stream_url, s.title.clone(), s.artist.clone(), s.album.clone(), art_url, s.duration)
            })
            .collect();
        match self.proxy.play_queue(tracks, 0).await {
            Ok(()) => self.message.clear(),
            Err(e) => self.message = format!("could not play: {e}"),
        }
    }

    fn draw_message(&self, frame: &mut Frame) {
        let para = Paragraph::new(self.message.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Maraetai "));
        frame.render_widget(para, frame.area());
    }

    fn draw(&self, frame: &mut Frame, now_playing: &NowPlaying) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(frame.area());

        match self.top() {
            Screen::Menu { selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Maraetai — [Enter] open  [/] search  [q] quit ",
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
                self.draw_list(
                    frame,
                    chunks[0],
                    &format!(" {title} — [Enter] play from here  [Esc] back "),
                    songs.iter().map(|s| format!("{}  —  {}", s.title, s.artist)),
                    *selected,
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
                self.draw_list(
                    frame,
                    chunks[0],
                    &title,
                    results.iter().map(|s| format!("{}  —  {}  ({})", s.title, s.artist, s.album)),
                    *selected,
                );
            }
        }

        self.draw_status_bar(frame, chunks[1], now_playing);
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect, title: &str, items: impl Iterator<Item = String>, selected: usize) {
        let items: Vec<ListItem> = items.map(ListItem::new).collect();
        let empty = items.is_empty();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title.to_string()))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default();
        if !empty {
            state.select(Some(selected));
        }
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect, now_playing: &NowPlaying) {
        let track = if now_playing.title.is_empty() { "(nothing loaded)" } else { &now_playing.title };
        let queue_info = if now_playing.queue_len > 0 {
            format!("  [{} of {}]", now_playing.queue_index + 1, now_playing.queue_len)
        } else {
            String::new()
        };
        let line1 = if self.message.is_empty() {
            format!("{} — {track} ({:.0}s){queue_info}", now_playing.status, now_playing.position)
        } else {
            self.message.clone()
        };
        let text = vec![
            Line::from(line1),
            Line::from("[space] play/pause  [n]ext  [p]revious  [s]top  [Q] quit + stop daemon"),
        ];
        let para =
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Now Playing "));
        frame.render_widget(para, area);
    }
}
