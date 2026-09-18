mod control;
mod lifecycle;
mod mpris;
mod playback;
mod range_reader;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use maraetai_common::{Credentials, dbus};
use mpris_server::{PlayerInterface, Property, Server, Signal, Time};
use tokio::sync::Notify;
use zbus::connection;

use control::ControlInterface;
use mpris::MprisPlayer;
use playback::Event;

/// How long the daemon may sit with nothing playing and no client activity
/// before it shuts itself down — see `lifecycle::run_idle_timer`. Not yet
/// user-configurable (planned: read from `Config`); 20 minutes is a
/// deliberately conservative starting default.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let _instance_guard = lifecycle::acquire_single_instance_lock()
        .context("could not start maraetaid — is another instance already running?")?;

    // Credentials are loaded but not *required* to start: the daemon should
    // still come up (and answer `status`/be visible over D-Bus) even before
    // `maraetai login` has been run, rather than crash-loop on a fresh
    // install.
    match Credentials::load() {
        Ok(creds) => {
            tracing::info!(server = %creds.server_url, user = %creds.username, "credentials loaded");
        }
        Err(e) => {
            tracing::warn!("not configured yet ({e}) — run `maraetai login`");
        }
    }

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let playback = playback::spawn(event_tx);
    let shutdown = Arc::new(Notify::new());

    let mpris_server = Arc::new(
        Server::new(
            dbus::MPRIS_BUS_NAME_SUFFIX,
            MprisPlayer {
                playback: playback.clone(),
            },
        )
        .await
        .context("failed to register MPRIS D-Bus service")?,
    );

    let control = ControlInterface::new(playback.clone(), Arc::clone(&shutdown));
    let _control_connection = connection::Builder::session()?
        .name(dbus::CONTROL_BUS_NAME)?
        .serve_at(dbus::CONTROL_OBJECT_PATH, control)?
        .build()
        .await
        .context("failed to register control D-Bus service")?;

    tokio::spawn(bridge_playback_events_to_mpris(
        event_rx,
        Arc::clone(&mpris_server),
    ));

    tokio::spawn(lifecycle::run_idle_timer(
        playback.clone(),
        Arc::clone(&shutdown),
        IDLE_TIMEOUT,
    ));

    tracing::info!(
        mpris_bus = %mpris_server.bus_name(),
        control_bus = dbus::CONTROL_BUS_NAME,
        "maraetaid ready"
    );

    tokio::select! {
        _ = shutdown.notified() => tracing::info!("shutdown requested"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl-C"),
        _ = wait_for_sigterm() => tracing::info!("received SIGTERM"),
    }

    playback.shutdown();
    Ok(())
}

/// Drains playback engine events and turns the ones that matter to MPRIS
/// clients into `PropertiesChanged`/`Seeked` signals. The engine itself only
/// updates a plain shared `Snapshot` (cheap, no async) — this task is the
/// only place that pays the cost of an actual D-Bus signal emission, and only
/// when something observable actually changed.
async fn bridge_playback_events_to_mpris(
    mut events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    server: Arc<Server<MprisPlayer>>,
) {
    while let Some(event) = events.recv().await {
        match event {
            Event::StatusChanged(status) => {
                let mpris_status = match status {
                    playback::Status::Playing => mpris_server::PlaybackStatus::Playing,
                    playback::Status::Paused => mpris_server::PlaybackStatus::Paused,
                    playback::Status::Stopped => mpris_server::PlaybackStatus::Stopped,
                };
                if let Err(e) = server
                    .properties_changed([Property::PlaybackStatus(mpris_status)])
                    .await
                {
                    tracing::warn!("failed to emit PlaybackStatus change: {e}");
                }
            }
            Event::TrackChanged => {
                if let Ok(meta) = server.imp().metadata().await {
                    if let Err(e) = server.properties_changed([Property::Metadata(meta)]).await {
                        tracing::warn!("failed to emit Metadata change: {e}");
                    }
                }
            }
            Event::Seeked(pos) => {
                if let Err(e) = server
                    .emit(Signal::Seeked {
                        position: Time::from_micros(pos.as_micros() as i64),
                    })
                    .await
                {
                    tracing::warn!("failed to emit Seeked signal: {e}");
                }
            }
            Event::TrackEnded => {
                tracing::debug!("track ended");
            }
            Event::PlaybackError(msg) => {
                tracing::warn!("playback error: {msg}");
            }
        }
    }
}

async fn wait_for_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut sig) => {
            sig.recv().await;
        }
        Err(e) => {
            tracing::warn!("could not install SIGTERM handler: {e}");
            std::future::pending::<()>().await;
        }
    }
}
