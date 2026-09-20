# maraetai-tui

A Linux terminal client for a Navidrome library via [`maraetai-service`](https://github.com/unclefroob/maraetai-service),
with real OS media integration — media keys and desktop notification widgets
via MPRIS — that survives closing the terminal.

## Why two binaries

MPRIS/media-key control needs a process that outlives the terminal window a
TUI runs in, so this is split like `mpd`/`ncmpcpp`:

- **`maraetaid`** — the daemon. Owns the audio output device, decode
  pipeline, and the **queue**, and exposes two D-Bus interfaces on the
  session bus: the standard `org.mpris.MediaPlayer2`/`.Player` (so
  GNOME/KDE/`playerctl`/media keys already know how to talk to it — including
  real `Next`/`Previous`) and a small custom `com.maraetai.Daemon1` for
  everything MPRIS doesn't cover (loading a whole queue, explicit shutdown).
  The queue lives here, not in the TUI, specifically so hardware media-key
  Next/Previous — which never go anywhere near the TUI — actually work.
- **`maraetai`** — the TUI. A thin client: talks to `maraetaid` over D-Bus
  for playback/queue control, and talks directly to `maraetai-service`'s
  existing Subsonic API for browsing/search, the same way every other
  maraetai client already does. It also carries the CLI (`login`,
  `daemon status`/`stop`, `play <song-id>`).

## Not perpetually running by design

The whole point of splitting into a daemon is media-key integration that
survives the terminal closing — which means it's also very easy to forget
about and leave using resources. So:

- `maraetai` auto-spawns `maraetaid` on first use if it isn't already running
  (checked via a real D-Bus RPC, not just process/pidfile presence).
- The daemon shuts itself down automatically after being idle — nothing
  playing **and** no client activity — for a configurable timeout (default
  20 minutes; set `idle_timeout_secs` in `config.toml` to change it). It will
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

## Installation

### From source

```sh
cargo build --workspace --release
sudo install -Dm755 target/release/maraetai  /usr/local/bin/maraetai
sudo install -Dm755 target/release/maraetaid /usr/local/bin/maraetaid
```

Or skip the `install` step and just run the binaries straight out of
`target/release/` (or `target/debug/` for a `cargo build --workspace`
without `--release`) — see [Usage](#usage) below either way.

### AUR (Arch Linux) — coming soon

A `maraetai-tui-git` package (tracking `master`'s latest commit, since
there are no tagged releases) is packaged in `packaging/aur/` and builds
cleanly with `makepkg`, but isn't published to the AUR yet — that needs an
AUR account, which hasn't been set up. Once it's live:

```sh
paru -S maraetai-tui-git   # or: yay -S maraetai-tui-git
```

or manually:

```sh
git clone https://aur.archlinux.org/maraetai-tui-git.git
cd maraetai-tui-git
makepkg -si
```

### Fedora / COPR — coming soon

A spec file is packaged in `packaging/rpm/` for a COPR repo (Fedora's
community package repo), but isn't published yet — that needs a Fedora
account, which hasn't been set up. Once it's live:

```sh
sudo dnf copr enable unclefroob/maraetai-tui
sudo dnf install maraetai-tui
```

## Usage

```sh
maraetai login       # prompts for server URL, username, password
maraetai             # launches the TUI: browse albums, / to search,
                      # Enter to play, auto-spawning maraetaid
```

In the TUI: arrow keys/`j`/`k` to move, `Enter` to open a list or play from
the selected song onward (queuing the rest of that list), `/` to search,
`Esc`/`Backspace` to go back, `space` to play/pause, `n`/`p` for next/
previous track, `s` to stop, `q` to quit (daemon keeps running), `Q` to quit
and stop it.

## Parity with the other maraetai clients

| Feature | iOS/macOS | Android | Web | **TUI** |
|---|---|---|---|---|
| Browse albums/artists/playlists/genres | ✅ | ✅ | ✅ | ✅ |
| Search | ✅ | ✅ | ✅ | ✅ *(songs only)* |
| Playback (stream, seek, queue, next/previous) | ✅ | ✅ | ✅ | ✅ |
| Favourites / playlists edit | ✅ | ✅ | ✅ | ❌ *(planned)* |
| OS media-key / lock-screen integration | ✅ (native) | ✅ (native) | ❌ | ✅ (MPRIS, incl. Next/Previous) |
| Credential storage | Keychain | SharedPreferences | localStorage (plaintext) | **OS keyring** |
| Survives the app/window closing | n/a | n/a | ❌ | ✅ (daemon) |

## Scope: OUT (v1)

- No favourites/playlist editing (create/rename/add-to/remove-from) — read
  and play only.
- No systemd/launchd unit shipped by default — spawn-on-demand +
  idle-shutdown is the default experience, not an always-on service (see
  above). A `--user` systemd unit could be a documented *optional* opt-in
  later for people who want the daemon always warm.
- Linux/D-Bus only — MPRIS has no equivalent on other platforms.

See `.autofeature/designs/` for the full plan this was built from, including
the reasoning behind each of these decisions.
