# s2u-yt

Contained, reversible fix for **"s2udio fails to play YouTube videos"** (and any
other mpv/yt-dlp consumer) on this box — and easy to copy to other machines.

## The problem it solves

`s2udio` plays YouTube by handing the URL to `mpv`, whose built-in ytdl hook runs
`yt-dlp`. On this network, YouTube flags the IP and answers **every**
`googlevideo.com/videoplayback` stream URL with **HTTP 403 Forbidden**, so mpv
exits with code 2 (`No video or audio streams selected`) and s2udio reports a
playback failure. The missing piece is a **Proof-of-Origin (PO) token**: YouTube
now requires one for stream URLs on the `web`/`web_safari`/`mweb` clients.

This package provisions a PO-token provider for yt-dlp. Because mpv, s2udio's
radio/search/downloads and the CLI all run the same `yt-dlp` binary, one install
fixes every playback path.

## What it contains

```
s2u-yt/
├── install.sh      # idempotent installer: fetch → configure → service → verify
├── uninstall.sh    # full reverse (or --keep-data for a quick re-install)
├── status.sh       # health check incl. a live HTTP-200 stream test
├── conf/config     # the only yt-dlp config delta (android_vr first, web_safari fallback)
└── README.md
```

Everything the package owns lives in **one directory**
`~/.local/share/s2u-yt/` (server binary, plugin, config, wrapper, manifest).
The only changes made outside it:

| Touchpoint | Change | Reversed by uninstall |
|---|---|---|
| `~/.local/bin/yt-dlp` | replaced by a wrapper (`--plugin-dirs` + `--config-locations`); the previous binary is preserved at `~/.local/bin/.yt-dlp.s2u-yt.bak` | yes |
| `~/.config/systemd/user/s2u-yt-bgutil.service` | runs the token server on `127.0.0.1:4416` | yes |
| `~/.config/yt-dlp/config` | **untouched** — your cookies/runtime settings still apply (wrapper passes your config first, the package config second; for the anonymous `android_vr` pass the wrapper strips only the cookie options and retries with them if that pass fails) | n/a |

## Install

```bash
cd s2u-yt
./install.sh                 # provider=bgutil (recommended, no browser windows)
./install.sh --dry-run       # see the plan without changing anything
```

What it does:

1. Downloads the **bgutil PO-token provider** — a single static Rust binary
   (`bgutil-pot-linux-x86_64`) and its yt-dlp plugin — from the
   [jim60105/bgutil-ytdlp-pot-provider-rs](https://github.com/jim60105/bgutil-ytdlp-pot-provider-rs)
   GitHub release (no node, no npm, no docker). Override the version with
   `RS_VERSION=0.8.1 ./install.sh`.
2. Installs the yt-dlp wrapper (uses your existing yt-dlp, which must be
   `>= 2025.05.22`).
3. Starts the token server as a `systemd --user` service (pidfile/manual
   fallback if systemd isn't available).
4. Verifies end-to-end: provider registered, resolved stream URL returns
   **HTTP 200** (it returned 403 before).

### Client strategy: `android_vr` first, `web_safari` fallback

The wrapper makes two attempts per yt-dlp call:

1. **Anonymous `android_vr`** — the wrapper strips the cookie options from
   your `~/.config/yt-dlp/config` for this attempt, because yt-dlp skips
   `android_vr` (and the other `SUPPORTS_COOKIES=false` clients) when the
   session is authenticated. On this bot-checked IP it is the only client
   whose googlevideo URLs are accepted (verified live: HTTP 200/206), and it
   serves **DASH video up to 2160p** (vs web_safari's 1080p HLS ceiling)
   plus **single-file audio** (opus/m4a) that MPD can range-seek — so stream
   seeking in s2udio works again, at the same time.
2. **Authenticated `web_safari`** — if the anonymous pass fails (or a video
   needs the session), the wrapper retries with your cookies; yt-dlp then
   auto-skips `android_vr` and uses web_safari (HLS up to 1080p), the
   previously verified-good path.

The `android_vr` pass depends on the bgutil server (its GVS policy requires a
PO token) — `status.sh` checks both the server and which client won, and
warns when a test URL fell back to HLS. If YouTube ever extends the SABR
experiment to `android_vr` (it currently hits the plain `android` client,
yt-dlp #12482), the web_safari fallback keeps playback working at 1080p.

### Fallback provider: `wpc` (browser-minted tokens)

If the bgutil tokens are ever rejected by YouTube, the package can use the
browser-minted provider instead (needs Chromium):

```bash
./install.sh --provider wpc
```

Caveat: it opens a Chromium window while yt-dlp runs (visible briefly during
each playback start) — fine for testing, not ideal for a background player.

## Verify / status

```bash
./status.sh                        # all checks incl. live stream 200 test
./status.sh --test-url <url>
yt-dlp -v <url> 2>&1 | grep -i "PO Token Providers"   # expect bgutil:http-…, not "none"
```

Then play the video in s2udio: the mpv window should open and the log
(`/tmp/s2udio_1000.log`) should no longer show `mpv exited ... 512` seconds
after launch. Radio and downloads use the same fixed path.

## Uninstall

```bash
./uninstall.sh          # stops service, restores yt-dlp, removes data root
./uninstall.sh --keep-data   # keep downloaded binaries for a quick re-install
```

## Distribution / shipping to another machine

The package is self-contained — copy the folder (or `git clone`/zip it), then
run `./install.sh`. At install time it needs: internet (first run), `curl`,
`python3`, an existing `yt-dlp >= 2025.05.22` on the target, and (bgutil mode)
nothing else. Linux x86_64/aarch64 supported.

Tips:

- **Offline installs**: pre-download the two release assets into the package's
  `artifacts/` dir and point `RS_VERSION` at the matching release; or just
  re-run `./install.sh` on a machine that already has `~/.local/share/s2u-yt`.
- **Pinning**: `RS_VERSION` controls the provider version; upgrade by re-running
  `./install.sh` with a newer value (it re-fetches and restarts the service).
- If the target machine has no `~/.config/yt-dlp/config`, consider adding
  `--js-runtimes node` (or installing deno) and, if you have them, a
  `--cookies-from-browser` line — see Troubleshooting.

## Troubleshooting

- **Still HTTP 403 after install**: the tokens "make traffic look legitimate"
  but are not guaranteed to clear a hard IP block. Try `./install.sh
  --provider wpc`, then a different egress (VPN/proxy — `--proxy` in your
  yt-dlp config applies to mpv/s2udio too), or wait for the flag to decay.
  Confirm breadth with: `yt-dlp --extractor-args "youtube:player_client=web_embedded"
  -J <url>` (a no-token client — if even that 403s on fetch, it's IP-level).
- **`PO Token Providers: none`**: the plugin dir isn't being loaded — check
  `~/.local/share/s2u-yt/plugins/bgutil-ytdlp-pot-provider/yt_dlp_plugins/`
  exists and that no *other* `bgutil-ytdlp-pot-provider` plugin (e.g. the old
  PyPI one) is installed, which would conflict.
- **"Requested format is not available"** for `-f 251`: harmless — YouTube now
  names formats `251-0`/`251-drc`; s2udio/mpv use default selection.
- **SABR-only warning**: YouTube is migrating some clients to the SABR
  streaming protocol; the plain `android` client currently returns formats
  without URLs (yt-dlp #12482). `android_vr` is not affected yet — if it
  ever is, the web_safari fallback keeps playback working at 1080p.
- **Maintenance**: YouTube actively breaks providers. Keep this package's
  `RS_VERSION` and yt-dlp itself current; re-running `./install.sh` after a
  version bump re-provisions the server and plugin.

## How the pieces fit

```
s2udio ─▶ mpv ─▶ yt-dlp (wrapper: plugin-dirs + config-locations)
                     │
                     ├─▶ pass 1: anon (cookies stripped) + android_vr → DASH ≤2160p, seekable audio
                     ├─▶ pass 2: your cookies + web_safari → HLS ≤1080p (fallback)
                     ├─▶ ~/.local/share/s2u-yt/conf (android_vr,web_safari client)
                     └─▶ plugin ─▶ bgutil-pot server (127.0.0.1:4416)
                                     └─▶ mints GVS PO token
yt-dlp gets a token → googlevideo URL returns 200 → mpv plays → s2udio happy
```
