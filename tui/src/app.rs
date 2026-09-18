//! The interactive status/control screen. **Scope note:** this v1 has no
//! library browsing/search yet (see the plan doc's Scope: OUT and the
//! `play` subcommand for manual testing in the meantime) — its job here is
//! to prove the daemon/TUI/D-Bus architecture end-to-end: show live status,
//! and drive play/pause/stop/quit against whatever the daemon is already
//! playing (started via `maraetai play <song-id>`).

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::dbus_client::ControlProxy;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub async fn run(proxy: ControlProxy<'_>) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &proxy).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, proxy: &ControlProxy<'_>) -> Result<()> {
    loop {
        let status = proxy
            .status()
            .await
            .unwrap_or_else(|_| ("disconnected".to_string(), String::new(), 0.0));

        terminal.draw(|frame| draw(frame, &status))?;

        // A blocking poll on the TUI's own task is an accepted simplification
        // here — this binary has no other async work contending for the
        // thread besides the status poll above, so a 250ms blocking wait
        // between input checks costs nothing in practice.
        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('Q') => {
                        let _ = proxy.quit().await;
                        return Ok(());
                    }
                    KeyCode::Char(' ') => {
                        if status.0 == "playing" {
                            let _ = proxy.pause().await;
                        } else {
                            let _ = proxy.resume().await;
                        }
                    }
                    KeyCode::Char('s') => {
                        let _ = proxy.stop().await;
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, status: &(String, String, f64)) {
    let (state, title, position) = status;
    let track = if title.is_empty() { "(none)" } else { title };
    let text = format!(
        "Status: {state}\nTrack:  {track}\nPosition: {position:.1}s\n\n\
         [space] play/pause   [s] stop\n\
         [q] quit TUI (daemon keeps running)\n\
         [Q] quit TUI and stop the daemon"
    );
    let para = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Maraetai "),
    );
    frame.render_widget(para, frame.area());
}
