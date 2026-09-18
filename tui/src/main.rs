mod app;
mod dbus_client;
mod library;
mod lifecycle;
mod login;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use maraetai_common::Credentials;

#[derive(Parser)]
#[command(name = "maraetai", about = "Terminal client for a Navidrome library via maraetai-service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Save server URL/username, and store the password in the OS keyring.
    Login,
    /// Control the background daemon directly, without opening the TUI.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Play a specific song id directly, without opening the TUI.
    Play {
        song_id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        artist: Option<String>,
        #[arg(long)]
        album: Option<String>,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Reports whether a daemon is running and what it's doing.
    Status,
    /// Asks a running daemon to shut down cleanly. This is the explicit
    /// "kill it" path — the daemon also shuts itself down automatically
    /// after being idle (see the plan doc).
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Login) => login::run(),
        Some(Command::Daemon { action }) => run_daemon_action(action).await,
        Some(Command::Play {
            song_id,
            title,
            artist,
            album,
        }) => run_play(song_id, title, artist, album).await,
        None => run_tui().await,
    }
}

async fn run_daemon_action(action: DaemonAction) -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("connecting to the D-Bus session bus")?;
    let proxy = dbus_client::connect(&connection).await;

    match action {
        DaemonAction::Status => match proxy {
            Ok(proxy) => match proxy.status().await {
                Ok((status, title, position, queue_index, queue_len)) => {
                    if title.is_empty() {
                        println!("daemon running — {status}");
                    } else if queue_len > 0 {
                        println!(
                            "daemon running — {status}: {title} ({position:.1}s) [{} of {}]",
                            queue_index + 1,
                            queue_len
                        );
                    } else {
                        println!("daemon running — {status}: {title} ({position:.1}s)");
                    }
                }
                Err(_) => println!("daemon not running"),
            },
            Err(_) => println!("daemon not running"),
        },
        DaemonAction::Stop => match proxy {
            Ok(proxy) if proxy.status().await.is_ok() => {
                proxy.quit().await.context("sending Quit to the daemon")?;
                println!("stop requested");
            }
            _ => println!("daemon not running — nothing to stop"),
        },
    }
    Ok(())
}

async fn run_play(
    song_id: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
) -> Result<()> {
    let creds = Credentials::load().context("run `maraetai login` first")?;
    let stream_url = library::Client::new(creds).stream_url(&song_id);

    let connection = lifecycle::ensure_daemon_running().await?;
    let proxy = dbus_client::connect(&connection)
        .await
        .context("connecting to daemon control interface")?;
    let track = (
        stream_url,
        title.unwrap_or_default(),
        artist.unwrap_or_default(),
        album.unwrap_or_default(),
        String::new(),
        0.0,
    );
    proxy
        .play_queue(vec![track], 0)
        .await
        .context("sending PlayQueue to the daemon")?;
    println!("playing song {song_id}");
    Ok(())
}

async fn run_tui() -> Result<()> {
    let creds = Credentials::load().context("run `maraetai login` first")?;
    let connection = lifecycle::ensure_daemon_running().await?;
    let proxy = dbus_client::connect(&connection)
        .await
        .context("connecting to daemon control interface")?;
    app::run(proxy, creds).await
}
