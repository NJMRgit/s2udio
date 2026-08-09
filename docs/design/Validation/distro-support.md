---
title: "Distro Support — Test & Development Plan (Nix / Fedora / Debian / Ubuntu + non-systemd init)"
section: validation
doc_type: plan
id: "validation/distro-support"
description: >
  Plan for making s2udio install and run on Nix, Fedora, Debian and Ubuntu
  plus non-systemd init systems (OpenRC / runit / s6 / sysvinit), developed
  and validated exclusively in EPHEMERAL rootless podman containers that are
  always removed after testing. Covers the harness, the target matrix, the
  service-manager and package-provider abstractions, per-target validation
  gates, and the phased roadmap.
status: "draft"
updated: "2026-08-09"
source_files:
  - setup.sh
  - scripts/s2u-mpv-tracker
  - scripts/s2udio-mpris
  - scripts/s2u-mpdris2
  - scripts/mpvSockets.lua
  - src/core/mpv.rs (tracker spawn)
  - src/core/event_loop.rs (systemd couplings)
related:
  - backend/mpv-session
  - backend/config-sidecars
  - validation/mpris
tags: [validation, distro-support, podman, packaging, init, plan]
---

# Distro Support — Test & Development Plan

## 1. Purpose & definition of done

Today s2udio is Arch/CachyOS-only: `setup.sh` is pacman/AUR-specific and the
runtime assumes a **systemd user session** (`systemctl --user` for MPD and
the mpDris2 shim, incl. the tracker's stop/start mutual exclusion while a
video plays). Everything else — the Rust app, the `~/.config/s2udio` /
`~/.cache/s2udio` paths, the python bridge scripts, `mpvSockets.lua` — is
already distro-agnostic.

**Goal:** s2udio installs and runs on **Fedora, Debian, Ubuntu, Nix**
(NixOS + the nix package manager) and on **non-systemd init systems**
(OpenRC, runit, s6, sysvinit), developed and validated **only in ephemeral
rootless podman containers that are removed when testing completes**.

**Definition of done:**

1. Every target in §3 passes the full gate list (§7) in an ephemeral podman
   container — install, build, unit tests, services, MPRIS, yt-dlp/mpv
   headless playback, cava, TUI smoke.
2. **No container outlives a run** — success or failure (§4 ephemerality
   contract, gate G12).
3. `setup.sh` dispatches on distro family; no Arch-only hardcodes left in
   the install path.
4. A service-manager abstraction (`s2u-svc`) replaces direct `systemctl
   --user` calls in setup, the tracker and docs; it supports systemd-user,
   runit-user, s6-user and a plain-launcher fallback (OpenRC/sysvinit).
5. Nix ships as a flake (`nix develop` / `nix profile install`) plus a
   NixOS module validated with a `nixosTest` VM.
6. The README gains an install-matrix section; this plan records per-target
   results as phases land.

## 2. Current distro coupling inventory

| Component | Today | Distro-specific assumption |
| --- | --- | --- |
| `setup.sh` | 9-step installer | `pacman -Q/-S`, AUR helper (`yay`/`paru`), mpv-full choice, systemd user units |
| Service mgmt | `systemctl --user` for `mpd.service`, `mpDris2.service` (+ drop-in ExecStart swap); tracker stops/starts `mpDris2.service` during video | systemd user session, logind |
| Packages | `mpd ffmpeg cava python-yt-dlp` (pacman), `mpdris2-git`, `mpv-full` (AUR/CachyOS), rqbit (NOT installed — cargo/static binary, `S2UDIO_RQBIT_BIN`) | Arch names |
| Python deps | dbus-python + PyGObject in `s2udio-mpris`, `s2u-mpdris2`, `s2u-mpv-tracker` | distro python package names differ |
| Rust build | `cargo build --release` → `s2u` → `~/.local/bin/s2udio` | rust toolchain per distro |
| Config/cache paths | `~/.config/s2udio`, `~/.cache/s2udio` (round 23) | already distro-agnostic ✓ |
| mpv | `mpv-full` recommended (Arch-only concept), `mpvSockets.lua` → `~/.config/mpv/scripts/` | plain `mpv` everywhere else |
| MPD | user-level MPD (`~/.config/mpd/mpd.conf`, user unit), fifo output appended | Debian/Ubuntu ship a *system* mpd — differs |

## 3. Target matrix

| Matrix key | Distro / base image | Pkg mgr | Init | Service backend (target) | Priority |
| --- | --- | --- | --- | --- | --- |
| `fedora-41` | `registry.fedoraproject.org/fedora:41` | dnf5 | systemd | systemd-user | P1 (first — closest to Arch) |
| `debian-12` | `docker.io/library/debian:12` | apt | systemd | systemd-user | P1 |
| `ubuntu-2404` | `docker.io/library/ubuntu:24.04` | apt | systemd | systemd-user | P1 |
| `nix` | `ghcr.io/nixos/nix` (nix pkg mgr, any distro) | nix | — | nix profile / `nix develop` | P2 |
| `nixos` | `nixos/nixos:24.05` (+ **nixosTest VM** for the module) | nix | systemd | NixOS module (systemd user unit) | P2 |
| `alpine-320` | `docker.io/library/alpine:3.20` | apk | **OpenRC** | openrc / launcher | P3 (first non-systemd) |
| `void-glibc` | `ghcr.io/void-linux/void-glibc` | xbps | **runit** | runit-user (`runsvdir` + `sv`) | P3 |
| `artix-s6` / `artix-openrc` | `docker.io/artixlinux/...` (verify tags) | pacman | **s6 / OpenRC** | s6-user / rc-service | P3 (stretch) |
| `devuan-daedalus` | `docker.io/devuan/devuan:daedalus` | apt | **sysvinit** | `/etc/init.d` / launcher | P3 (stretch) |

Stretch extras: `void-musl` (musl pass), `artix-runit`, `devuan-openrc`,
NixOS-with-openrc/s6 (unofficial flakes — out of scope v1).

Init systems covered: **systemd** (baseline), **OpenRC**, **runit**, **s6**,
**sysvinit** — exactly the non-systemd set.

## 4. Ephemeral podman harness (the contract)

**Non-negotiable:** *containers are created, used, and removed. No container
is kept after testing — success or failure.*

### 4.1 Driver

One script: `scripts/dev/test-distro.sh <matrix-key> [--gate G0..G12] [--no-cache-vol] [--artifacts DIR]`

Lifecycle:

1. **Sweep** — remove any stale leftovers from a crashed run:
   `podman rm -f $(podman ps -aq --filter name=s2u-distro-)` (no-op if none).
2. **Create** — `podman run --rm -d --name s2u-distro-<key>` with:
   - `--systemd=always` + `/sbin/init` as CMD for systemd targets;
     `sleep infinity` for non-systemd targets (init validated via its
     service commands, not a full boot — see §4.3);
   - the repo bind-mounted read-only, then **copied** into the container
     (`podman cp` or `cp -a` from the ro mount) so `target/` builds
     container-local and never dirties the host tree;
   - optional named cargo cache volume `s2u-cargo-<key>` for iteration
     (`--no-cache-vol` disables; volumes are documented for
     `podman volume rm s2u-cargo-*` — they are *not* containers).
3. **Provision** — install packages (per §5 map) + rust toolchain inside.
4. **Build & unit-test** — `cargo build --release` + `cargo test --release`
   (baseline: the 1280-ish suite must stay green).
5. **Deploy** — install support scripts, seed config/theme, install service
   files through the abstraction (§6.1).
6. **Gates G1–G11** — run the per-target checks (§7); every gate writes a
   JSON result line.
7. **Collect artifacts** — `podman cp <cid>:/s2udio/artifacts/…`
   `scripts/dev/artifacts/<key>/<timestamp>/` on the host, then…
8. **Teardown (always)** — `trap 'podman rm -f $CID' EXIT` wraps the whole
   run; `--rm` is belt-and-braces; the sweep in step 1 covers crashes.
9. **Gate G12** — assert `podman ps -a --filter name=s2u-distro-` is empty
   (the run *fails* if any s2u-distro-* container remains).

`--rm` + the EXIT trap + the start-of-run sweep + the end-of-run assertion =
the ephemerality contract is enforced four ways.

### 4.2 Container-environment notes (rootless podman 6.x)

- Rootless podman is already active on the dev host; containers run as root
  *inside* (mapped to the host user) → **no sudo needed inside** for
  package installs; `sudo` shims in the setup path are no-ops.
- D-Bus session: start `dbus-daemon --session` (or wrap gates in
  `dbus-run-session`) and set `DBUS_SESSION_BUS_ADDRESS`; MPRIS gates assert
  via `busctl`/`dbus-send` against the real session bus *in the container*.
- `XDG_RUNTIME_DIR=/run/user/1000` (create + chmod 700) for systemd-user /
  dbus bits.
- TUI smoke (G10) needs a pty: run under `tmux` with `TERM=xterm-256color`,
  capture text state with `tmux capture-pane -p` (headless — no video
  output; the TUI renders fine on a pty).
- Headless playback: `mpv --vo=null --ao=null` (video *processing* still
  runs; the mpv IPC socket + state file + MPRIS bridge are what we assert).

### 4.3 Init-in-container reality check

Containers do not boot an init. We validate the **service files and the
abstraction's commands** against each init:

- **systemd targets**: `--systemd=always` runs real systemd as PID 1.
  User units are the sticking point — there is **no logind** in containers,
  so `loginctl enable-linger` is unavailable. Approach: start `systemd
  --user` explicitly (works without logind when `XDG_RUNTIME_DIR` +
  `DBUS_SESSION_BUS_ADDRESS` are set) and drive it via `systemctl --user`;
  **fallback** (used for the non-systemd targets anyway): start `mpd` and
  the `s2u-mpdris2` shim as plain processes for the service gates.
- **OpenRC (alpine)**: install `openrc`, set `rc_sys="docker"` in
  `/etc/rc.conf`, run services via `rc-service`; if openrc refuses to
  bootstrap in the container, fall back to launching the service scripts
  directly (still validates the files + `s2u-svc` openrc backend).
- **runit (void)**: per-user `runsvdir` + `sv` (no root needed) — the
  cleanest non-systemd path and the template for the runit-user backend.
- **s6 (artix)**: per-user `s6-svscan` + `s6-svc`.
- **sysvinit (devuan)**: `/etc/init.d/<svc> start|stop` directly (root).

## 5. Package map (grounded probes + open items)

Verified 2026-08-09:

| pkg | Debian 12 (in-container, **verified**) | nixpkgs 24.05 (raw, **verified**) | Fedora/Alpine/Void/Ubuntu |
| --- | --- | --- | --- |
| mpdris2 | ✓ `mpdris2` | ✓ `tools/audio/mpdris2` | probe pages exist → **confirm in-container (P1)** |
| cava | ✓ | ✓ | **confirm in-container** |
| yt-dlp | ✓ `yt-dlp` | ✓ | **confirm in-container** |
| mpv | ✓ | ✓ | ✓ (plain `mpv`) |
| mpd | ✓ | ✓ | ✓ |
| ffmpeg | ✓ | ✓ | ✓ |
| python3-dbus / python3-gi | ✓ ✓ | (nixpkgs `python3.pkgs.dbus-python` / `pygobject3`) | **confirm names** (`python3-dbus`, `python3-gi` on apt; `python3-dbus`/`python3-gobject` on Fedora; `py3-dbus`/`py3-gobject` on Alpine; `python3-dbus`/`python3-gobject` on Void) |
| rustc / cargo | ✓ ✓ | ✓ | ✓ |
| rqbit | ✗ (expected) | — | ✗ everywhere → cargo/static binary, `S2UDIO_RQBIT_BIN` (stays optional) |

Key consequences for the design:

- **mpv-full is Arch/CachyOS-only.** On every other target the installer
  installs plain `mpv` and prints an informational note ("Arch-only
  mpv-full recommendation not applicable here"). The mpv-choice block in
  setup.sh becomes the Arch branch only.
- **rqbit is never a distro package** — keep the existing
  cargo/static-binary guidance; the torrent gates use a fake
  `S2UDIO_RQBIT_BIN` (existing test pattern).
- **mpdris2 exists on Debian/Ubuntu/Fedora and in nixpkgs** — the shim
  approach (official binary + runtime `find_cover` patch) transfers;
  Alpine/Void availability must be confirmed in P3 (fallback: run the
  upstream python mpDris2 from source, or s2udio's own minimal MPD MPRIS
  player as a last-resort backend — decision point).

## 6. Development workstreams

### 6.1 Service-manager abstraction — `scripts/s2u-svc`

`s2u-svc <start|stop|restart|enable|is-active> <svc>` dispatching on
detected init:

| Backend | Detect | Commands |
| --- | --- | --- |
| systemd-user | `systemctl --user` works | `systemctl --user …` |
| runit-user | `runsvdir` + `~/.config/runit/…` | `sv start/stop/restart/status` |
| s6-user | `s6-svscan` + `~/.config/s6/…` | `s6-svc -u/-d/-r` |
| openrc | `/sbin/openrc-run` | `rc-service …` (root-level or launcher) |
| sysvinit | `/etc/init.d` | `service … start/stop` |
| launcher (fallback) | anything else | start/stop `mpd` and `s2u-mpdris2` as plain user processes, pidfiles under `~/.cache/s2udio/` |

Callers rewired: `setup.sh` (enable/start/restart mpd + shim),
`s2u-mpv-tracker` (the mpDris2 stop/start mutual-exclusion during video —
today a hard `systemctl --user`), docs.

**Design decision (P3):** v1 = systemd-user + runit-user + s6-user +
plain-launcher; OpenRC/sysvinit go through the launcher unless a root
service install is acceptable (user-level services are the s2udio model —
avoid root).

### 6.2 Installer refactor — `setup.sh` becomes a dispatcher

- Detect via `/etc/os-release` (`ID` / `ID_LIKE`): pacman (Arch/CachyOS/
  Artix), `dnf5` (Fedora), `apt` (Debian/Ubuntu/Devuan), `apk` (Alpine),
  `xbps` (Void), `nix` (nix profile).
- A per-backend **package-name map** (§5) feeds the existing shared step
  functions (scripts install, config/theme seed, MPD fifo append, mpv
  choice — Arch branch keeps the mpv-full choice; other backends install
  plain `mpv`).
- The Arch/CachyOS path stays byte-for-byte as today (regression risk
  zero); new backends are additive.
- **MPD service handling (Debian/Ubuntu gotcha):** those distros ship mpd
  as a *system* service (mpd user, `/etc/mpd.conf`, port 6600). The
  installer must either (a) create a user-level mpd unit +
  `~/.config/mpd/mpd.conf` and stop/disable the system one, or (b) reuse
  the system mpd. Decide in P1 by what the container shows; s2udio's
  current model is user-level.

### 6.3 Nix workstream

- `flake.nix` with: a `package` (build the rust crate), a `devShell`
  (mpd, mpv, yt-dlp, cava, mpdris2, ffmpeg, rustToolchain, python deps),
  and an install path (`nix profile install .#s2udio` + the support
  scripts/config seed).
- NixOS `module` (enable mpd + mpdris2 user services + install the
  binaries/scripts) validated with a **`nixosTest` VM** — the one place a
  real boot + systemd integration is checked (VMs, not containers).
- The `nix` matrix key (nix pkg manager on any distro, `ghcr.io/nixos/nix`
  container) validates the flake path in the podman loop.

### 6.4 What does NOT change

- App code (`src/`) — only the tracker's `systemctl` call and any other
  direct init calls get rewired to `s2u-svc`.
- Config/cache paths, bridge scripts' logic, `mpvSockets.lua` install path,
  cava fifo append, MPRIS state schema.

## 7. Validation gates (per target)

| Gate | Check |
| --- | --- |
| G0 | Container created ephemeral; start-of-run sweep ran |
| G1 | Install completes (packages + scripts + config seed + service files) |
| G2 | `cargo build --release` → `s2udio version` works |
| G3 | `cargo test --release` green (baseline suite) |
| G4 | mpd up (MPD protocol answers), shim process up |
| G5 | MPRIS audio: `org.mpris.MediaPlayer2.mpd` serves Metadata/Position for a local file |
| G6 | MPRIS video: `s2u-mpv-tracker` + `s2udio-mpris` up; `org.mpris.MediaPlayer2.s2udio` serves title/artist/art/position; Seek routes to the mpv socket |
| G7 | yt-dlp resolves a real YouTube URL (recorded as soft gate — network) |
| G8 | mpv headless play (`--vo=null --ao=null`) runs; `mpvSockets.lua` socket appears; state file written |
| G9 | cava fifo configured; cava runs briefly headless |
| G10 | TUI smoke: tmux pty launch, `capture-pane` shows the Queue tab |
| G11 | Service abstraction: `s2u-svc start/stop/is-active` round-trips on the target init |
| G12 | **No `s2u-distro-*` container remains** (ephemerality assertion) |

Artifacts: per-gate JSON + logs →
`scripts/dev/artifacts/<key>/<timestamp>/` (host-side; containers never
carry state between runs).

## 12. Phase results (recorded per run)

Harness: `scripts/dev/test-distro.sh <key>` (Phase 0, committed `5bda69d`).
Every run is ephemeral — G12 (no `s2u-distro-*` container remains) is
asserted by the EXIT trap on success AND failure. Artifacts (run.log +
per-gate JSON + gates.jsonl) live in `scripts/dev/artifacts/<key>/<ts>/`
(not committed; host-side only).

### Phase 0 — harness proof of life (2026-08-09)

- First full `fedora-41` run: **all 12 gates green** (G1–G11 in-container,
  G0/G12 host-side). Run: `artifacts/fedora-41/20260809T151104Z`.

### Phase 1 — Fedora 41 / Debian 12 / Ubuntu 24.04 (2026-08-09)

| Target | G0 | G1 | G2 | G3 | G4 | G5 | G6 | G7* | G8 | G9 | G10 | G11 | G12 | Artifacts |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| fedora-41 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | `artifacts/fedora-41/20260809T151104Z` |
| debian-12 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | soft | ✓ | ✓ | ✓ | ✓ | ✓ | `artifacts/debian-12/20260809T151514Z` |
| ubuntu-2404 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | soft | ✓ | ✓ | ✓ | ✓ | ✓ | `artifacts/ubuntu-2404/20260809T151753Z` |

\* G7 (yt-dlp resolution) is a **soft** gate by design (§7). Findings:

- **yt-dlp version gap (Debian/Ubuntu)**: Debian 12 ships yt-dlp
  2023.03.04, Ubuntu 24.04 ships 2024.04.09 — both fail the `android_vr`
  client probe ("No video formats found"), while Fedora 41's 2025.06.30
  resolves. A current pip yt-dlp (2026.07.04) resolves the same URL from
  the same Debian container → egress is fine, the distro pin is stale.
  **Recommendation**: on Debian/Ubuntu, the installer (or README) should
  suggest `pip install -U --break-system-packages yt-dlp` (or note that
  resolution needs a current yt-dlp — consistent with the existing
  "keep yt-dlp current" guidance in setup.sh).

### Phase 1 decisions (recorded per plan §5/§6)

1. **Fedora mpd**: the official Fedora repos dropped the `mpd` server —
   the init image enables **RPM Fusion free** (`mpd`, full `ffmpeg`, full
   `mpv`), the Fedora analogue of Arch's AUR usage for mpdris2-git.
2. **MPD system-vs-user (Debian/Ubuntu, plan §6.2)**: verified in the
   container — both ship `mpd` as an auto-started **system** service
   (`/lib/systemd/system/mpd.service`, mpd user, port 6600). Decision:
   **(a) user-level instance** — the deploy stops+disables the system
   unit and runs s2udio's user-level `mpd.service`
   (`~/.config/mpd/mpd.conf` + fifo). Confirmed working in-container
   (gates G4/G5 green). Ubuntu's mpd package behaves identically.
3. **Rust toolchain**: distro rustc is too old for edition-2024
   everywhere (Fedora 41 ~1.80, Debian 12 1.63, Ubuntu 24.04 1.75;
   Cargo.toml MSRV 1.88) → rustup minimal profile (stable 1.97.1 in
   the runs above). This is a per-distro delta the dispatcher must keep.
4. **mpv-full stays Arch-only** (plan §5): all three targets install
   plain `mpv`; the deploy prints the informational note.
5. **mpDris2**: `mpdris2` exists on all three (Debian/Ubuntu/Fedora);
   Fedora's package ships **no** systemd user unit, so the deploy
   provides one (drop-in applies everywhere).
6. **s2u-svc v1**: systemd-user + plain-launcher backends implemented and
   gate-tested (G11). runit/s6/openrc/sysvinit backends are stubbed and
   land with Phase 3 (Alpine → Void → Artix).

## 8. Phased roadmap

- **Phase 0 — harness skeleton + proof of life.** `test-distro.sh` with
  matrix definitions, gates, artifact collection, teardown trap. *Proof of
  life already done 2026-08-09:* ephemeral `debian:12` probe — apt package
  map grounded (§5), `--rm` + sweep verified, no leftover container.
  Next: first full `fedora-41` run.
- **Phase 1 — systemd glibc family (P1).** Fedora → Debian → Ubuntu:
  dnf5/apt backends, package map confirmation, python dbus/gi names, MPD
  user-vs-system service handling, mpv-full branch isolation. *Exit:* G0–G12
  green on all three.
- **Phase 2 — Nix (P2).** flake (`devShell` + package + install), `nix`
  matrix key via `ghcr.io/nixos/nix`, NixOS module + `nixosTest` VM.
  *Exit:* `nix profile install` + `nixosTest` pass.
- **Phase 3 — non-systemd init (P3).** Alpine/OpenRC → Void/runit →
  Artix/s6 (stretch: Devuan/sysvinit): `s2u-svc` backends land, tracker
  rewired, installer init handling. *Exit:* G0–G12 green on ≥3 non-systemd
  targets.
- **Phase 4 — hardening & docs.** README install-matrix section,
  per-target notes, musl pass (Void-musl), optional CI workflow (podman on
  a runner) + optional .deb/.rpm artifact builds (stretch). *Exit:* full
  matrix green; this plan updated with per-target results.

## 9. Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| musl targets (Alpine/Void-musl) missing packages | Rust builds fine on musl; prefer `void-glibc` for the runit gate; Alpine confirmed to carry mpv/yt-dlp/cava |
| OpenRC won't bootstrap in a container | `rc_sys="docker"` in `/etc/rc.conf`; fallback = launch service scripts directly (validates files + backend) |
| No logind → `systemctl --user` limited in containers | Start `systemd --user` explicitly; fallback = direct-process service gates (same path non-systemd uses) |
| YouTube egress flaky / bot checks from container IPs | yt-dlp gate is a *soft* gate (recorded, not failing); use a fixed public test stream; keep yt-dlp current in images |
| Debian/Ubuntu system mpd occupies port 6600 | P1 decides user-unit + disable system mpd vs reuse (container-tested) |
| rqbit never packaged | Keep cargo/static-binary path + `S2UDIO_RQBIT_BIN`; torrent gates use the existing fake-engine pattern |
| Image/tag availability (Artix, Devuan, nixos/nix) | Verify at phase start; swap to equivalent images if a tag moved |
| Cold cargo builds slow iteration | Named cache volumes `s2u-cargo-<key>` (prunable, not containers); `--no-cache-vol` for zero-persistence runs |
| mpdris2 missing on Alpine/Void | P3 decision point: upstream source install or minimal in-house MPD MPRIS player fallback |

## 10. Out of scope (v1)

- Real audio/video output, GPU rendering (containers are headless; mpv runs
  `--vo=null --ao=null`; visuals stay host-validated).
- KDE media-controls widget appearance (host-validated; the container
  validates the D-Bus surface only).
- .deb/.rpm/AppImage artifacts and official CI (stretch goals, Phase 4).
- Windows/macOS.
- NixOS with non-systemd init (unofficial flakes — out of scope).

## 11. Deliverables checklist

- [ ] `scripts/dev/test-distro.sh` (+ `scripts/dev/containers/<key>/` per-target
      Dockerfiles/flake and the gate runner)
- [ ] `scripts/s2u-svc` (init abstraction) + tracker/setup rewiring
- [ ] `setup.sh` distro dispatcher + package-name maps
- [ ] `flake.nix` + NixOS module + `nixosTest`
- [ ] README install-matrix section
- [ ] This plan updated with per-target results and timestamps
