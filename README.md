# maraetai-tui

A Linux terminal client for a Navidrome library via [`maraetai-service`](https://github.com/unclefroob/maraetai-service),
with real OS media integration — media keys and desktop notification widgets
via MPRIS — that survives closing the terminal.

## Why two binaries

MPRIS/media-key control needs a process that outlives the terminal window a
TUI runs in, so this is split like `mpd`/`ncmpcpp`:

- **`maraetaid`** — the daemon. Owns the audio output device and decode
  pipeline, and exposes two D-Bus interfaces on the session bus: the standard
  `org.mpris.MediaPlayer2`/`.Player` (so GNOME/KDE/`playerctl`/media keys
  already know how to talk to it) and a small custom
  `com.maraetai.Daemon1` for everything MPRIS doesn't cover (loading a
  specific track, explicit shutdown).
- **`maraetai`** — the TUI. A thin client: talks to `maraetaid` over D-Bus
  for playback control, and talks directly to `maraetai-service`'s existing
  Subsonic API for browsing albums and search, the same way every other
  maraetai client already does. It also carries the CLI (`login`,
  `daemon status`/`stop`, `play <song-id>`).

## Not perpetually running by design

The whole point of splitting into a daemon is media-key integration that
survives the terminal closing — which means it's also very easy to forget
about and leave using resources. So:

- `maraetai` auto-spawns `maraetaid` on first use if it isn't already running
  (checked via a real D-Bus RPC, not just process/pidfile presence).
- The daemon shuts itself down automatically after being idle — nothing
  playing **and** no client activity — for a configurable timeout (currently
  a hardcoded 20 minutes; see `daemon/src/main.rs::IDLE_TIMEOUT`). It will
  never idle-shutdown while a track is actually playing, even with the
  terminal closed.
- `maraetai daemon stop` (or pressing `Q` in the TUI) asks it to quit
  immediately.
- A `flock`ed pidfile at `$XDG_RUNTIME_DIR/maraetai/daemon.pid` prevents two
  daemons from starting at once.

## Streaming, not buffer-then-play

`rodio`'s decoder requires `Seek`, but an HTTP response body only supports
sequential reads. `daemon/src/range_reader.rs` bridges that with real HTTP
Range requests — a seek drops the current connection and lazily reissues a
ranged GET for the new position on the next read, so no seek downloads more
than the bytes actually needed, and playback can start before the whole file
arrives. It degrades gracefully (read-and-discard) if the server ignores
`Range` and returns the whole resource from byte 0 instead. See that file's
tests for this verified against a real (if minimal) HTTP server, not mocked.

## Credentials

Server URL + username live in `~/.config/maraetai/config.toml`. The password
is stored in the OS keyring (`keyring` crate — Secret Service/KWallet on
Linux), never on disk — the one thing every *other* maraetai client (except
iOS's Keychain use) doesn't already do.

## Setup

```sh
cargo build --workspace
./target/debug/maraetai login       # prompts for server URL, username, password
./target/debug/maraetai             # launches the TUI: browse albums, / to search,
                                     # Enter to play, auto-spawning maraetaid
```

In the TUI: arrow keys/`j`/`k` to move, `Enter` to open an album or play a
song, `/` to search, `Esc`/`Backspace` to go back, `space` to play/pause,
`s` to stop, `q` to quit (daemon keeps running), `Q` to quit and stop it.

## Parity with the other maraetai clients

| Feature | iOS/macOS | Android | Web | **TUI** |
|---|---|---|---|---|
| Browse albums | ✅ | ✅ | ✅ | ✅ |
| Browse artists/playlists/genres | ✅ | ✅ | ✅ | ❌ *(planned)* |
| Search | ✅ | ✅ | ✅ | ✅ *(songs only)* |
| Playback (stream, seek, queue) | ✅ | ✅ | ✅ | ✅ *(single track; queue planned)* |
| Favourites / playlists edit | ✅ | ✅ | ✅ | ❌ *(planned)* |
| OS media-key / lock-screen integration | ✅ (native) | ✅ (native) | ❌ | ✅ (MPRIS) |
| Credential storage | Keychain | SharedPreferences | localStorage (plaintext) | **OS keyring** |
| Survives the app/window closing | n/a | n/a | ❌ | ✅ (daemon) |

## Scope: OUT (v1)

- No artist/playlist/genre browsing yet — only albums and song search.
  `maraetai play <song-id>` also exists for direct testing.
- No queue/playlist management — one track at a time.
- No systemd/launchd unit shipped by default — spawn-on-demand +
  idle-shutdown is the default experience, not an always-on service (see
  above). A `--user` systemd unit could be a documented *optional* opt-in
  later for people who want the daemon always warm.
- Linux/D-Bus only — MPRIS has no equivalent on other platforms.

See `.autofeature/designs/` for the full plan this was built from, including
the reasoning behind each of these decisions.
