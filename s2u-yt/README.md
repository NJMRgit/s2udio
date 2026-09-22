# s2u-yt

Contained, reversible fix for **"s2udio fails to play YouTube videos"** (and any
other mpv/yt-dlp consumer) on this box — and easy to copy to other machines.

## The problem it solves

`s2udio` plays YouTube by handing the URL to `mpv`, whose built-in ytdl hook runs
`yt-dlp`. YouTube flags this IP and answers `googlevideo.com/videoplayback`
stream URLs with **HTTP 403 Forbidden**, so mpv exits with code 2 and s2udio
reports a playback failure. Two things are needed to stay ahead of that:

1. a **Proof-of-Origin (PO) token** for the InnerTube clients, and
2. media URLs that survive googlevideo's redirect validation.

This package provisions both: a PO-token provider server (bgutil, running
natively), the matching yt-dlp plugin, and a wrapper that resolves YouTube
through a **phase ladder** and only hands back URLs it has actually probed.
Because mpv, s2udio's radio/search/downloads and the CLI all run the same
`yt-dlp` binary, one install fixes every playback path.

## What it contains

```
s2u-yt/
├── install.sh                       # idempotent installer: provision → configure → service → verify
├── uninstall.sh                     # full reverse (--keep-data for a quick re-install)
├── status.sh                        # health check incl. a live HTTP-200 stream test
├── conf/config                      # the only yt-dlp config delta (no player_client pin — the wrapper chooses)
├── bin/yt-dlp-wrap.sh               # wrapper template rendered into the data root by install.sh
├── bin/s2u-yt-probe.py              # media-URL probe used by every wrapper phase
├── plugins/bgutil-ytdlp-pot-provider/…   # yt-dlp plugins: the bgutil 2.x PO-token provider
└── README.md
```

Everything the package owns lives in **one directory**
`~/.local/share/s2u-yt/` (server build, plugins, helpers, config, wrapper,
manifest). The only changes made outside it:

| Touchpoint | Change | Reversed by uninstall |
|---|---|---|
| `~/.local/bin/yt-dlp` | replaced by a wrapper (`--plugin-dirs` + `--config-locations`); the previous binary is preserved at `~/.local/bin/.yt-dlp.s2u-yt.bak` | yes |
| `~/.config/systemd/user/s2u-yt-bgutil.service` | runs the token server on `127.0.0.1:4416` | yes |
| `~/.config/yt-dlp/config` | **untouched** — your cookies/runtime settings still apply (the wrapper passes your config first, the package config second; for the anonymous pass it strips only the cookie options) | n/a |

## Requirements

| Need | Why | Notes |
|---|---|---|
| Linux x86_64 / aarch64 | — | the server is plain node + npm packages |
| **node >= 22 + npm >= 9** | the bgutil 2.x provider server is the official Node.js server, built locally | `node --version`; the build runs `npm ci` + `npx tsc` |
| **yt-dlp >= 2026.08.19** | DASH-manifest URLs + the current client set | `pipx install/upgrade yt-dlp` |
| `curl`, `python3`, `tar` | downloads, JSON handling, helpers | preinstalled nearly everywhere |
| `systemd --user` | auto-start of the token server | without it install.sh prints a manual start command |
| build tools (rarely) | only if `canvas` (a server dependency) has no prebuilt binary for your platform | `npm ci` fails loudly; install gcc/make + cairo/pango/jpeg dev packages and re-run |

## Install

```bash
cd s2u-yt
./install.sh                 # provider=bgutil (recommended, no browser windows)
./install.sh --dry-run       # see the plan without changing anything
```

What it does:

1. Downloads the **bgutil provider release source**
   ([Brainicism/bgutil-ytdlp-pot-provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider),
   tag `2.0.0` by default — override with `BGUTIL_TAG=… ./install.sh`), runs
   `npm ci` + `npx tsc` and keeps the built server in
   `~/.local/share/s2u-yt/server/` (`build/main.js`). **Native — no container, no docker.**
2. Copies the yt-dlp plugins from the package (bgutil 2.x + the Android-VR
   client patch) into `~/.local/share/s2u-yt/plugins/`.
3. Installs the wrapper + helper scripts and links `~/.local/bin/yt-dlp` to it
   (previous file preserved as `.yt-dlp.s2u-yt.bak`).
4. Writes and starts the `systemd --user` unit
   `s2u-yt-bgutil.service` (`node build/main.js --host 127.0.0.1 --port 4416`).
   If a same-named **podman quadlet** from an earlier containerized setup
   exists, it is disabled and renamed `.bak-native-<date>` so the native unit
   owns the name.
5. Verifies: `/ping` answers with the server version, yt-dlp reports a PO
   token provider (not `none`), and a resolved stream URL returns **HTTP 200**.

## How resolution works (wrapper phases)

Every `yt-dlp` call goes through the wrapper, which tries a ladder and returns
the first result whose media URLs it could actually fetch:

| Phase | Client | Output |
|---|---|---|
| **P1** anonymous | no pin; cookie options stripped from your config | probed (since v2026-09-22); on a dead URL the ladder continues (a failed pass is discarded — its leading `null` corrupts consumers) |
| **P2** authenticated | no pin; **your cookies** | probed; on a dead URL the ladder continues |
| **P3** HLS safety net | `web_safari` (HLS, ≤1080p60) | always playable, lower quality |

Details that matter:

- **`--youtube-skip-dash-manifest` is stripped.** It is deprecated, and with it
  yt-dlp returns *player-response* URLs that fail googlevideo's redirect
  validation (302 → 403) whereas *DASH-manifest* URLs answer 206 to the same
  request (verified 2026-09-09/10: A/B 3/3, wrapper runs 6/6).
- **Every pass that returns googlevideo URLs is probed** by
  `bin/s2u-yt-probe.py` before its output is handed over — the anonymous P1 pass
  included, which used to be returned unchecked. The probe asks the URLs the
  **request shape the players use**: a plain GET (no `Range` header) through
  `ffprobe` (libavformat, the stack mpv and MPD decode with) or `curl` when
  ffprobe is missing. That is the shape googlevideo's range enforcement answers
  403 to, while a small bounded `Range: bytes=0-1023` — what an earlier probe
  sent — can answer 206 for the very same URL (measured 2026-09-22). A URL the
  players cannot open now makes the ladder continue instead of dying in MPD.
  A single 403 is retried after 8 s first (a fresh URL can 403 on its first
  request and 206 seconds later).
- Decisions are logged (cheaply) to `/tmp/s2u-yt-wrapper.log`
  (`PHASE1_PROBE403`, `PHASE2_PROBE403`, `FALLBACK_HLS`, `DROP deprecated …`).
- The probe has a network-free self-test: `bin/s2u-yt-probe.py --self-test`.
- YouTube's behaviour still **flaps window-to-window** (an open window means
  DASH/206, a closed one means 403s). The ladder means playback keeps working
  instead of failing; which phase answered is visible in the log.

## When YouTube answers SABR-only

YouTube is rolling out **SABR** — server-side adaptive bitrate. The player
response still lists the full ladder (2160p/1440p/1080p …) but the adaptive
formats come back **without URLs**; only `serverAbrStreamingUrl` is offered, and
using it means speaking YouTube's own streaming protocol. Anything that plays
through per-format URLs — yt-dlp, mpv, MPD — cannot, so such a client is left
with the muxed 360p stream. yt-dlp tracks it as
[#12482](https://github.com/yt-dlp/yt-dlp/issues/12482) (`core:downloader`; its
SABR downloader is the upstream fix), and mpv/ffmpeg have no SABR support of
their own ([mpv #16645](https://github.com/mpv-player/mpv/issues/16645)).

**What this package does about it:** nothing client-side, by design. Measured on
2026-09-10 with the window open (the anonymous client resolving 4K fine):

| attempt | result |
|---|---|
| anonymous default client (VISIONOS) | 188 formats with URLs, `401` → **206** (the working path) |
| cookies-authenticated default order | 117 formats with URLs, `401`/`315` → **403** |
| `mweb` + PO token (upstream's suggested workaround) | 162 formats with URLs *carrying `pot=`*, `401`/`315` → **403** |
| authenticated `android_vr` (VR-OAuth route, removed) | SABR-only: 1 playable format, 360p |

So neither the cookie pass nor `mweb` yields fetchable URLs here, and the ladder
keeps them only as probed attempts that fall through to the **HLS safety net**
(≤1080p60, always playable). That is why there is no `mweb` phase: it would cost
an extra resolve and change nothing. When YouTube migrates the remaining clients
too, or upstream lands its SABR downloader, the wrapper's fallback list is where
a fix plugs in.

## Verify / status

```bash
./status.sh                        # all checks incl. live stream 200 test
./status.sh --test-url <url>
yt-dlp -v <url> 2>&1 | grep -i "PO Token Providers"   # expect bgutil:http-2.x, not "none"
curl -s http://127.0.0.1:4416/ping                    # expect {"version":"2.x",…}
tail -20 /tmp/s2u-yt-wrapper.log                      # which phase answered
```

Then play the video in s2udio: the mpv window should open and
`/tmp/s2udio_1000.log` should not show `mpv exited … 512` right after launch.
Radio and downloads use the same fixed path.

## Uninstall

```bash
./uninstall.sh               # stops service, restores yt-dlp, removes data root + VR login
./uninstall.sh --keep-data   # keep the server build/plugins/venv and the VR login
```

## Distribution / shipping to another machine

The package is self-contained — copy the folder (or zip it) and run
`./install.sh`. At install time it needs internet, `curl`, `tar`, `python3`,
node ≥ 22 + npm, `systemd --user` (optional) and an existing
`yt-dlp ≥ 2026.08.19`.

Tips:

- **Offline installs**: pre-download the release source tarball into
  `~/.local/share/s2u-yt/state/` (name it `bgutil-<tag>.tar.gz`) and the
  `node_modules` tree, or copy a whole provisioned `~/.local/share/s2u-yt`.
- **Pinning**: `BGUTIL_TAG` selects the provider release; re-running
  `./install.sh` with a newer tag re-downloads, rebuilds and restarts the
  service.
- **Non-standard prefixes / test installs**: `S2U_BIN_DIR` (wrapper link dir),
  `S2U_UNIT` (unit path) and `S2U_QUADLET_DIR` (where a same-named podman quadlet
  would be looked for) override the defaults; `S2U_SYSTEMD=0` makes both
  install.sh and uninstall.sh skip *all* systemd interaction — install.sh then
  prints the manual start command, so an isolated trial install cannot disturb a
  live service.
- **Version note**: the official provider's latest release is **2.0.0**
  (2026-09-08). There is no "2.0.6" — if you see that version referenced
  anywhere, it is wrong; pin a real tag.
- If the target machine has no `~/.config/yt-dlp/config`, consider adding
  `--js-runtimes node` (or installing deno) and, if you have them, a
  `--cookies-from-browser` line — see Troubleshooting.

## Troubleshooting

- **`npm ci` fails**: missing build tooling for the `canvas` native module on
  your platform. Install gcc/make plus cairo/pango/jpeg/gif dev packages (the
  package name varies by distro) and re-run; or use a node version for which
  canvas ships prebuilt binaries.
- **Still HTTP 403 after install**: the tokens "make traffic look legitimate"
  but are not guaranteed to clear a hard IP block. Confirm breadth with
  `yt-dlp --extractor-args "youtube:player_client=web_embedded" -J <url>`, then
  consider a different egress (VPN/proxy —
  `--proxy` in your yt-dlp config applies to mpv/s2udio too), or waiting for
  the flag to decay.
- **`PO Token Providers: none`**: the plugin dir isn't being loaded — check
  `~/.local/share/s2u-yt/plugins/bgutil-ytdlp-pot-provider/yt_dlp_plugins/`
  exists and that no *other* `bgutil-ytdlp-pot-provider` plugin (e.g. an old
  PyPI install) shadows it.
- **Server won't start**: `systemctl --user status s2u-yt-bgutil.service`,
  `journalctl --user -u s2u-yt-bgutil.service -n 50`. Check the unit's
  `ExecStart` points at a node that exists and at
  `~/.local/share/s2u-yt/server/build/main.js`.
- **"Requested format is not available"** for `-f 251`: harmless — YouTube now
  names formats `251-0`/`251-drc`; s2udio/mpv use default selection.
- **SABR-only warning**: YouTube answers some clients with formats that carry
  no URL at all (`WARNING: … client https formats have been skipped as they are
  missing a URL … SABR-only streaming experiment`). That is expected for the
  clients YouTube has migrated — see *When YouTube answers SABR-only* above for
  the measurements. The ladder absorbs it (the HLS safety net still plays); the
  maintenance moves are a yt-dlp upgrade and/or a `BGUTIL_TAG` bump.
- **Maintenance**: YouTube actively breaks providers. Keep this package's
  `BGUTIL_TAG`, yt-dlp itself and (if used) the VR token current; re-running
  `./install.sh` re-provisions everything.

## How the pieces fit

```
s2udio / mpv / CLI ─▶ yt-dlp (wrapper: plugin-dirs + per-phase config-locations)
                          │
                          ├─ P1 anon  (cookies stripped, unpinned) ──▶ probe ─┐
                          ├─ P2 auth  (your cookies) ────────────────▶ probe ─┤ dead
                          ├─ P3 web_safari HLS  (≤1080p60 safety net)         │ URL?
                          └─▶ plugins ─▶ bgutil server 127.0.0.1:4416 (native node 2.x)
                                            └─▶ mints GVS PO token
yt-dlp gets a token + manifest URLs → probe opens the URL like the player ─▶ plays
```
