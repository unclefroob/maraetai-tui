//! Auto-spawn: if no daemon currently owns the control bus name, start one
//! and wait for it to come up. This is what makes "just run `maraetai`" work
//! without a separate manual `maraetaid &` step.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use zbus::Connection;

use crate::dbus_client;

const SPAWN_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Connects to the session bus and makes sure a daemon is reachable on it,
/// spawning one if not. Liveness is checked with an actual RPC (`status()`),
/// not just proxy construction — a `Proxy` can be built successfully even
/// when nothing owns the destination name yet; only a real call surfaces
/// `ServiceUnknown`.
pub async fn ensure_daemon_running() -> Result<Connection> {
    let connection = Connection::session()
        .await
        .context("failed to connect to the D-Bus session bus")?;

    if daemon_is_alive(&connection).await {
        return Ok(connection);
    }

    tracing::info!("no daemon running — spawning maraetaid");
    let exe = daemon_binary_path()?;
    std::process::Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(daemon_log_stdio())
        .stderr(daemon_log_stdio())
        .spawn()
        .with_context(|| format!("failed to spawn {}", exe.display()))?;

    let deadline = tokio::time::Instant::now() + SPAWN_WAIT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(SPAWN_POLL_INTERVAL).await;
        if daemon_is_alive(&connection).await {
            return Ok(connection);
        }
    }

    bail!(
        "daemon did not become reachable within {:?} of starting {}",
        SPAWN_WAIT_TIMEOUT,
        exe.display()
    );
}

/// A fresh handle onto the daemon's log file, in append mode — never the
/// TUI's own stdout/stderr. Without this, the auto-spawned daemon inherits
/// the TUI's terminal directly, and raw libasound diagnostics (e.g. a PCM
/// underrun) bypass our own logging entirely and get written straight into
/// the alternate screen, corrupting the display. Falls back to discarding
/// output entirely if the log file can't be opened, rather than falling
/// back to inheriting the terminal (which is the exact problem being
/// avoided here).
fn daemon_log_stdio() -> Stdio {
    let log_path = maraetai_common::paths::runtime_dir().join("daemon.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_or_else(|_| Stdio::null(), Stdio::from)
}

async fn daemon_is_alive(connection: &Connection) -> bool {
    match dbus_client::connect(connection).await {
        Ok(proxy) => proxy.status().await.is_ok(),
        Err(_) => false,
    }
}

/// Locates the `maraetaid` binary: alongside our own executable first (the
/// normal case — a workspace build puts both binaries in the same
/// `target/{debug,release}` directory), falling back to relying on `PATH`
/// (the case once this is actually installed system-wide) so `Command::spawn`
/// can still find it and produce a clear "not found" error if not.
fn daemon_binary_path() -> Result<PathBuf> {
    if let Ok(mut exe) = std::env::current_exe() {
        exe.set_file_name("maraetaid");
        if exe.exists() {
            return Ok(exe);
        }
    }
    Ok(PathBuf::from("maraetaid"))
}
