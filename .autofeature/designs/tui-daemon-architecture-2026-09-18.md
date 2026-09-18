# Feature: Rust Daemon + TUI Client (Linux desktop) for Maraetai
**Date:** 2026-09-18
**Stack:** Rust (new — no existing specialist architect covers this in AutoFeature)
**Status:** Draft — plan only, no repo created, no code written
**Mode:** AUTOMATED
**Model tier:** sonnet throughout, no escalation (project preference)

## Context (from sibling-repo survey)
- Auth: Subsonic salt+token (`u`,`t=MD5(password+salt)`,`s`,`c`,`v=1.16.1`,`f`) against `maraetai-service`, which itself forward-and-validates against real Navidrome — no local user table anywhere. Every existing client (iOS/macOS, Android, web) regenerates salt/token per request from a stored server URL + username + password; none persist a long-lived token.
- Custom endpoints already available beyond stock Subsonic: `getRecentlyPlayed`, `getOnRepeat`, `getSongsForYou`, `getArtistSongs`, `getFavourites`, `getArtistList`, `getTrackVideo`, `getUsers` (admin-only, just fixed). `stream.view`/`getCoverArt.view` are plain passthrough.
- Repo naming: `maraetai-<platform-or-role>` (`maraetai-android`, `maraetai-service`; the flagship iOS/macOS app is just `maraetai`). No `CLAUDE.md`/`CHANGELOG.md`/`LICENSE` in any sibling repo; docs are dense, technical, first-person, rationale-heavy — no marketing tone. `maraetai-android`'s README includes an explicit parity/mapping table against iOS; a new client's README should do the same against the others.
- Credential storage precedent: iOS uses Keychain behind a storage-agnostic protocol (`SecureCredentialStoring`); Android uses plaintext-ish SharedPreferences; web uses plaintext `localStorage`/`sessionStorage`. **No existing client does this well** — none use hardware-backed/OS-secret-service storage on the platforms where one exists.
- No daemon/systemd/launchd/IPC precedent anywhere in the three sibling repos — this is genuinely new ground for the product family.

## Problem
There's no way to control/browse/play a Navidrome library from a Linux terminal with real OS media integration (MPRIS media keys, notification-widget now-playing info). The web app works in a browser but has no OS-level presence; a full GUI (Tauri) or native GUI (egui/iced) are the other options already discussed and rejected/deferred in favor of a TUI for lower build effort and a better fit for keyboard-driven usage.

## Solution
A Rust **daemon** (`maraetaid`) that owns audio playback + the MPRIS D-Bus service + queue state, and a thin **TUI client** (`maraetai`) that connects to it to browse/search/control. Splitting these (rather than one combined binary) means the MPRIS/media-key integration and playback survive even if you close the terminal — mirroring the `mpd`/`ncmpcpp` split, which is the closest prior art for this exact shape of tool. The **explicit ask driving this plan** — being able to kill it so it doesn't sit around consuming resources — is solved primarily by an **idle auto-shutdown timer** in the daemon, plus an explicit manual stop command as a backstop.

## User Story
As a Linux desktop user of the self-hosted Navidrome library, I want to browse/search/play music from a terminal with real media-key and notification-widget integration, and I want the background process to go away on its own when I'm not using it (or on command), so it never becomes a forgotten resource-hog.

## Scope: IN
- New repo `maraetai-tui` (Cargo workspace, two binaries — see Architecture below).
- Daemon: connects to `maraetai-service` via the same Subsonic API every other client uses (no changes to `maraetai-service` needed); owns playback (stream, pause, seek, next/prev, queue, shuffle/repeat — same feature set as the web/mobile players); exposes an MPRIS (`org.mpris.MediaPlayer2.Player`) D-Bus interface for media keys + desktop notification widgets; exposes a control interface (own D-Bus interface, see IPC below) for the TUI.
- TUI: browse (albums/artists/genres/playlists), search, queue management, now-playing view, connects to the daemon over D-Bus for all playback control; makes its own direct HTTP calls to `maraetai-service` for browsing/search metadata (matching how every other client independently talks to the API — the daemon is not a second backend).
- Daemon lifecycle: auto-spawn on first TUI launch if not already running; idle-timeout auto-shutdown (configurable, default ~20 min with nothing playing and no TUI attached); explicit `maraetai daemon stop`/`status` subcommands; a pidfile + lockfile so a second daemon can't accidentally start.
- Config file at `~/.config/maraetai/config.toml` (server URL, username; **password via the OS keyring**, not plaintext — see Open Questions).
- A README with a parity/mapping table against the iOS/Android/web clients, matching the sibling-repo doc convention.

## Scope: OUT
- No systemd/launchd unit shipped by default — the daemon is spawn-on-demand + idle-shutdown, not an always-on system service. (A `--user` systemd unit could be a documented *optional* fast-follow for people who want it always running, but that's the opposite of what was asked for here.)
- No changes to `maraetai-service` — this is purely a new client against the existing public API.
- No system tray icon (that's the Tauri path, already discussed/deferred separately) and no GUI at all — text-only.
- No offline caching/local library mirror in v1 — every browse/search call hits the live API, same as the other clients.
- No Windows/macOS support for the daemon's control+MPRIS layer (D-Bus is Linux-only) — this is explicitly a Linux desktop tool.

## Architecture

### Process split
```
┌─────────────┐   D-Bus (session bus)   ┌──────────────────────┐
│ maraetai-tui │◄───────────────────────►│      maraetaid        │
│  (ratatui)   │  com.maraetai.Daemon    │  - playback engine    │
└──────┬───────┘                         │  - queue state        │
       │ direct HTTP (browse/search)     │  - org.mpris.Media... │
       ▼                                 │  - com.maraetai.Daemon│
┌─────────────┐                          └──────────┬───────────┘
│ maraetai-    │◄─────────────────────────────────────┘ HTTP (stream/scrobble)
│ service      │
└─────────────┘
```
- **Why split at all (vs. one binary):** MPRIS/media-key control needs a process that outlives the terminal window; a monolithic TUI-only binary loses media-key control the moment you close the terminal, which defeats the actual goal of this whole feature line (media integration).
- **Why the TUI talks to `maraetai-service` directly for browsing** (not proxied through the daemon): every other client (iOS/Android/web) independently calls the same public API rather than routing through a shared intermediary — keeping that pattern means the daemon's job stays narrowly "own playback + media integration," which is a much smaller, more testable surface than "reimplement a second backend inside the daemon." A metadata-caching layer in the daemon is a reasonable fast-follow if TUI startup latency from re-fetching library data becomes annoying, not a v1 requirement.

### IPC: D-Bus for everything (not a bespoke socket protocol)
Use `zbus` for both:
1. The standard **MPRIS2** interfaces (`org.mpris.MediaPlayer2`, `org.mpris.MediaPlayer2.Player`) — this is what desktop environments/media-key daemons/`playerctl` already know how to talk to, for free.
2. A **custom** `com.maraetai.Daemon1` interface for everything MPRIS doesn't cover (queue add/remove/reorder, search-triggered "play this track from this list", auth/login, daemon status/idle-timer, explicit `Quit()`).

This avoids inventing and hand-maintaining a bespoke socket wire protocol — one library, one transport, one thing to get right — at the cost of being Linux/D-Bus-only, which is already the accepted constraint for MPRIS itself.

### Lifecycle & the "killable" requirement (the actual point of this ask)
1. **Auto-spawn:** `maraetai` (the TUI) checks for the daemon (via a pidfile at `$XDG_RUNTIME_DIR/maraetai/daemon.pid` + a D-Bus name-ownership probe); if absent, spawns `maraetaid` as a detached child and waits for it to claim its D-Bus name before proceeding.
2. **Idle auto-shutdown:** the daemon runs an idle timer, reset on any playback activity or D-Bus method call. When it expires (nothing playing, no client interaction, default ~20 min, configurable in `config.toml`) the daemon calls its own `Quit()` and exits — this is the primary answer to "doesn't perpetually use resources if unwanted."
3. **Explicit kill:** `maraetai daemon stop` sends the D-Bus `Quit()` call for a clean shutdown; if the daemon is unresponsive (D-Bus name present but not answering), fall back to `SIGTERM` on the pidfile's PID, then `SIGKILL` after a short grace period. `maraetai daemon status` reports running/idle-timer-remaining/not-running.
4. **Single instance:** a lockfile (`flock` on the pidfile) prevents a second `maraetaid` from starting while one is already up — the TUI's auto-spawn path checks this before forking.

## Crate choices
| Concern | Crate | Note |
|---|---|---|
| TUI rendering/input | `ratatui` + `crossterm` | standard, actively maintained |
| Audio output + decode | `rodio` (symphonia-backed decoding) | covers flac/mp3/aac/ogg/opus; **verify seek behavior during implementation** — this is the one piece with no prior art in the maraetai family |
| Async runtime + HTTP | `tokio` + `reqwest` (streaming body) | for `stream.view` piped straight into the decoder |
| D-Bus (MPRIS + control) | `zbus` | async-native, good MPRIS examples in the ecosystem |
| CLI parsing | `clap` (derive) | `maraetai` (TUI, default) / `maraetai daemon start\|stop\|status` |
| Config/XDG paths | `serde` + `toml` + `directories` | `~/.config/maraetai/config.toml`, `$XDG_RUNTIME_DIR/maraetai/` |
| Auth token | `md5` | matches the exact scheme every other client already uses |
| Password storage | `keyring` (see Open Questions) | OS secret service instead of plaintext |
| Desktop notifications (nice-to-have) | `notify-rust` | track-change toast, independent of MPRIS |

## Edge Cases to Handle
- TUI launched while a daemon from a *previous* session is a zombie (process alive, D-Bus name not claimed, or vice versa) — auto-spawn logic must detect and recover (kill-and-restart) rather than hang.
- Idle timer must NOT fire while a track is actively playing, even with zero TUI clients attached (e.g., you closed the terminal but music should keep playing) — "idle" means no playback AND no client, not just no client.
- Network/server unreachable at daemon startup — daemon should still start (so MPRIS/media keys are available) and surface a clear "not connected" state to the TUI/notifications rather than crash-looping.
- Multiple TUI instances attaching to one daemon (e.g. two terminal tabs) — both should see consistent state; this is a natural consequence of D-Bus signals (`PropertiesChanged` on the MPRIS interface) but needs the custom interface's queue/browse state to emit equivalent signals.
- Config file holding a plaintext password if keyring is unavailable/declined — must be created with `0600` permissions at minimum, and the daemon should still function (degrade, not refuse) if the keyring backend isn't present on a minimal Linux install.

## Test Scenarios
- Start the TUI cold (no daemon running) → daemon auto-spawns, TUI connects, browse/search/play all work.
- Let a track play, close the terminal, use OS media keys / a notification widget → playback controls still work via MPRIS.
- Leave the daemon idle (nothing playing, no TUI) past the timeout → process exits on its own; verify via `maraetai daemon status` and `ps`.
- `maraetai daemon stop` while playing → clean shutdown, no orphaned audio device lock.
- Kill `-9` the daemon mid-playback, then relaunch the TUI → auto-spawn recovers cleanly (stale pidfile/lockfile handled).
- Two TUI instances open simultaneously → both reflect the same now-playing/queue state.

## Decisions (resolved 2026-09-18)
- **Password storage: `keyring` crate, confirmed.** Server URL + username live in `~/.config/maraetai/config.toml`; password goes to the OS secret service (Secret Service/kwallet on Linux via `keyring`'s platform backend). Config file never contains the password — if the keyring backend is unavailable, the daemon prompts and fails closed (no silent plaintext fallback) rather than degrading to the web app's precedent.
- **Streaming/performance is a first-class requirement, not an afterthought.** Before any daemon/TUI/D-Bus scaffolding, build a standalone spike: `reqwest` streaming GET against `/rest/stream.view` → piped into `rodio`'s `symphonia`-backed decoder → measure (a) time-to-first-audible-sample, (b) seek latency (both a small backward seek and a large forward seek into unbuffered territory), (c) steady-state CPU/memory while playing, against a real track from the actual Navidrome library. This determines real architecture choices downstream (how much read-ahead buffering the daemon needs, whether seeking requires a fresh ranged HTTP request per seek vs. buffering the whole file, whether `rodio` is sufficient or a lower-level `symphonia`+`cpal` pipeline is needed for acceptable seek latency). The daemon's playback engine is built around whatever this spike finds, not around the assumption in the crate table above.

## Remaining open question
- **Repo name:** still recommending `maraetai-tui` over `maraetai-cli`/`maraetai-daemon` — no existing convention forces this either way; will use it unless told otherwise.
- **Idle-timeout default (~20 min):** still an arbitrary starting point, trivially configurable later.
