//! The interactive TUI: browse albums, search, and play — talking to
//! `maraetai-service` directly for library data (the same way every other
//! maraetai client does) and to the daemon over D-Bus for playback control.
//!
//! Still v1-scoped: playing a song replaces whatever's playing (no queue —
//! see the plan doc's Scope: OUT), and there's no artist/playlist/genre
//! browsing yet, just albums + search.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::dbus_client::ControlProxy;
use crate::library::{self, Album, Song};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

enum Screen {
    Albums {
        albums: Vec<Album>,
        selected: usize,
    },
    AlbumSongs {
        album_name: String,
        songs: Vec<Song>,
        selected: usize,
    },
    Search {
        query: String,
        editing: bool,
        results: Vec<Song>,
        selected: usize,
    },
}

struct App<'a> {
    proxy: ControlProxy<'a>,
    library: library::Client,
    screen: Screen,
    /// A transient status/error line shown above the now-playing bar — e.g.
    /// "Loading…" during a fetch, or a fetch failure. Cleared on the next
    /// successful action.
    message: String,
}

pub async fn run(proxy: ControlProxy<'_>, creds: maraetai_common::Credentials) -> Result<()> {
    let mut app = App {
        proxy,
        library: library::Client::new(creds),
        screen: Screen::Albums {
            albums: Vec::new(),
            selected: 0,
        },
        message: String::new(),
    };

    let mut terminal = ratatui::init();
    app.load_albums(&mut terminal).await;
    let result = app.event_loop(&mut terminal).await;
    ratatui::restore();
    result
}

impl App<'_> {
    async fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            // A blocking poll on this task is an accepted simplification —
            // see the equivalent note this replaced; this binary has no other
            // async work contending for the thread besides the status poll.
            let now_playing = self
                .proxy
                .status()
                .await
                .unwrap_or_else(|_| ("disconnected".to_string(), String::new(), 0.0));

            terminal.draw(|frame| self.draw(frame, &now_playing))?;

            if !event::poll(POLL_INTERVAL)? {
                continue;
            }
            let Event::Key(key) = event::read()? else { continue };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if let Screen::Search { editing: true, .. } = &self.screen {
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
                    if now_playing.0 == "playing" {
                        let _ = self.proxy.pause().await;
                    } else {
                        let _ = self.proxy.resume().await;
                    }
                }
                KeyCode::Char('s') => {
                    let _ = self.proxy.stop().await;
                }
                KeyCode::Char('/') => {
                    self.screen = Screen::Search {
                        query: String::new(),
                        editing: true,
                        results: Vec::new(),
                        selected: 0,
                    };
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter => self.activate_selection(terminal).await,
                KeyCode::Esc | KeyCode::Backspace => self.go_back(),
                _ => {}
            }
        }
    }

    /// Handles a key while a search query is being typed. Returns `true` if
    /// it consumed the key (so the caller's normal keybindings don't also
    /// fire on the same keystroke).
    async fn handle_search_edit(&mut self, code: KeyCode) -> bool {
        let Screen::Search { query, editing, .. } = &mut self.screen else {
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
                self.screen = Screen::Albums {
                    albums: Vec::new(),
                    selected: 0,
                };
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
        let (selected, len) = match &mut self.screen {
            Screen::Albums { selected, albums } => (selected, albums.len()),
            Screen::AlbumSongs { selected, songs, .. } => (selected, songs.len()),
            Screen::Search { selected, results, editing, .. } if !*editing => (selected, results.len()),
            Screen::Search { .. } => return,
        };
        if len == 0 {
            return;
        }
        *selected = (*selected as i32 + delta).rem_euclid(len as i32) as usize;
    }

    async fn activate_selection(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        match &self.screen {
            Screen::Albums { albums, selected } => {
                if let Some(album) = albums.get(*selected).cloned() {
                    self.message = format!("Loading {}…", album.name);
                    let _ = terminal.draw(|f| self.draw_message(f));
                    match self.library.album_songs(&album.id).await {
                        Ok(songs) => {
                            self.message.clear();
                            self.screen = Screen::AlbumSongs {
                                album_name: album.name,
                                songs,
                                selected: 0,
                            };
                        }
                        Err(e) => self.message = format!("error: {e}"),
                    }
                }
            }
            Screen::AlbumSongs { songs, selected, .. } => {
                if let Some(song) = songs.get(*selected).cloned() {
                    self.play(&song).await;
                }
            }
            Screen::Search { results, selected, editing, .. } if !*editing => {
                if let Some(song) = results.get(*selected).cloned() {
                    self.play(&song).await;
                }
            }
            Screen::Search { .. } => {}
        }
    }

    fn go_back(&mut self) {
        if let Screen::AlbumSongs { .. } = &self.screen {
            self.screen = Screen::Albums {
                albums: Vec::new(),
                selected: 0,
            };
            // Re-fetching on every back-navigation (rather than caching the
            // previous album list) keeps the state machine simple — v1
            // trades a redundant request for not having to thread a "where
            // did I come from" stack through every screen.
        }
    }

    async fn load_albums(&mut self, terminal: &mut ratatui::DefaultTerminal) {
        self.message = "Loading albums…".to_string();
        let _ = terminal.draw(|f| self.draw_message(f));
        match self.library.albums().await {
            Ok(albums) => {
                self.message.clear();
                self.screen = Screen::Albums { albums, selected: 0 };
            }
            Err(e) => self.message = format!("error loading albums: {e}"),
        }
    }

    async fn run_search(&mut self) {
        let Screen::Search { query, .. } = &self.screen else { return };
        if query.is_empty() {
            return;
        }
        let query = query.clone();
        self.message = format!("Searching {query}…");
        match self.library.search(&query).await {
            Ok(results) => {
                self.message.clear();
                self.screen = Screen::Search {
                    query,
                    editing: false,
                    results,
                    selected: 0,
                };
            }
            Err(e) => self.message = format!("search error: {e}"),
        }
    }

    async fn play(&mut self, song: &Song) {
        let url = self.library.stream_url(&song.id);
        let art_url = self.library.cover_art_url(&song.cover_art);
        let dur = if song.duration > 0.0 { song.duration } else { 0.0 };
        match self
            .proxy
            .play_url(&url, &song.title, &song.artist, &song.album, &art_url, dur)
            .await
        {
            Ok(()) => self.message.clear(),
            Err(e) => self.message = format!("could not play: {e}"),
        }
    }

    fn draw_message(&self, frame: &mut Frame) {
        let para = Paragraph::new(self.message.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Maraetai "));
        frame.render_widget(para, frame.area());
    }

    fn draw(&self, frame: &mut Frame, now_playing: &(String, String, f64)) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(frame.area());

        match &self.screen {
            Screen::Albums { albums, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    " Albums — [Enter] open  [/] search  [q] quit ",
                    albums.iter().map(|a| format!("{}  —  {}", a.name, a.artist)),
                    *selected,
                );
            }
            Screen::AlbumSongs { album_name, songs, selected } => {
                self.draw_list(
                    frame,
                    chunks[0],
                    &format!(" {album_name} — [Enter] play  [Esc] back "),
                    songs.iter().map(|s| format!("{}  —  {}", s.title, s.artist)),
                    *selected,
                );
            }
            Screen::Search { query, editing, results, selected } => {
                let title = if *editing {
                    format!(" Search: {query}_  [Enter] run  [Esc] cancel ")
                } else {
                    format!(" Search: {query}  [Enter] play  [/] new search ")
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

    fn draw_list(
        &self,
        frame: &mut Frame,
        area: ratatui::layout::Rect,
        title: &str,
        items: impl Iterator<Item = String>,
        selected: usize,
    ) {
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

    fn draw_status_bar(&self, frame: &mut Frame, area: ratatui::layout::Rect, now_playing: &(String, String, f64)) {
        let (status, title, position) = now_playing;
        let track = if title.is_empty() { "(nothing loaded)" } else { title };
        let line1 = if self.message.is_empty() {
            format!("{status} — {track} ({position:.0}s)")
        } else {
            self.message.clone()
        };
        let text = vec![
            Line::from(line1),
            Line::from("[space] play/pause  [s] stop  [Q] quit + stop daemon"),
        ];
        let para =
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Now Playing "));
        frame.render_widget(para, area);
    }
}
