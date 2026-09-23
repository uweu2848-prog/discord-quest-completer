# Discord Quest Completer

Own build of the "run a tiny stand-in exe so Discord's game-detection
thinks the real game is running" trick — Rust/Tauri backend, Vue frontend.

## How it works

1. `src-tauri` fetches Discord's public list of detectable games
   (`GET /api/v10/applications/detectable`) for the search/picker.
2. You pick the game and duration shown on your Discord Quests tab
   (e.g. "Play Marvel Rivals — 1 hour") and hit **Start Quest**.
3. The backend copies `src-runner` (a ~100–150KB dummy binary) into
   `games/<app-id>/<path-discord-expects>.exe` under the app's data
   dir — recreating whatever relative folder Discord's
   `executables[].name` field specifies — and spawns it.
4. `src-runner` just opens a real (hidden) window and idles, forwarding
   its title from the launch arg. Discord's client-side detection sees
   the process/window and treats it as the game running, so the quest
   bar fills in real time same as actually playing.
5. The backend schedules its own countdown and kills the process the
   moment the duration is up — no need to babysit it. **Cancel** on the
   active-quest card stops it early.

Only games with a `win32` executable entry show up, since the runner
only targets Windows — same constraint as Discord's own detection.

### Why this doesn't read your actual Discord quests automatically

An auto-detect version would need to pull your Discord account token
out of the client and hit Discord's private, undocumented quest-state
API with it — that's the same token-extraction technique used by
Discord account-stealer malware, and automating your account against a
private API is separate, explicit self-bot territory under Discord's
ToS. This app stays one step removed: you tell it what the Quests tab
says, it handles the rest.

## Project layout

```
src-tauri/       Tauri app (Rust) — commands, process manager, Discord API client
src-runner/      The dummy "game" binary that gets copied per-game and spawned
src/             Vue 3 + TS frontend
scripts/         Build helper to stage the runner exe as a Tauri resource
```

## Requirements

- Rust + the [Tauri prerequisites](https://tauri.app/start/prerequisites/) for Windows
- Node.js 20+
- npm (swap for pnpm/yarn if you prefer — just update the lockfile)

## Development

```bash
npm install

# Build the dummy runner and stage it where Tauri expects a bundled resource
npm run build:runner:win
npm run copy:runner:win

npm run tauri dev
```

Re-run the two runner steps any time you change `src-runner`.

## Production build

```bash
npm run build:runner:win
npm run copy:runner:win
npm run tauri build
```

Installers land in `src-tauri/target/release/bundle/`.

## Notes / things to extend

- **Persistence**: nothing is cached to disk yet — `fetch_games` hits the
  Discord API on every launch. Worth caching `games.json` locally with a
  TTL so the app still opens offline.
- **Icons**: `DetectableApp.icon` is a Discord CDN hash, not a URL. Build
  the URL as `https://cdn.discordapp.com/app-icons/<id>/<icon>.png` if
  you want thumbnails in the picker.
- **Multiple executables**: some entries list more than one `win32`
  executable (e.g. launcher + real binary); `launch` only spawns the
  first match.
- **Restart on app relaunch**: active quests aren't persisted, so if you
  close the app mid-quest the countdown is lost. Worth writing active
  runs (game id, started_at, duration) to a small JSON file on start/stop
  and resuming them in `setup()`.
- **Stream quests**: untested, same as upstream — this only covers the
  "play for N minutes" quest type.
- **RPC/Rich Presence spoofing** (showing "Playing X" using someone
  else's Discord App ID) and **auto-reading your active quests** are
  both left out — see above.

## Disclaimer

For educational/personal use. This walks right up to the edge of
Discord's ToS around Quest completion — use at your own risk, on your
own account.
