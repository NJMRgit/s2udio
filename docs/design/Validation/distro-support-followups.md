# Distro-support loose-end coverage — dispatch plan for subagents (2026-08-09)

Status: DONE — all dispatchable tasks (T1–T8) executed and committed on
`working` (2026-08-09); this file is the master plan the top-level agent
(parent of setup-dispatcher) used to dispatch ONE subagent per task,
sequentially. Each task below is a self-contained spec: objective, context,
steps, acceptance criteria, constraints and report-back contract. Per-task
status lines below carry the commit that closed each task; the consolidated
results live in `docs/design/Validation/distro-support.md` §12 and
`docs/design/Sessions/2026-08-09.md`.

Repo: /home/stoned/NJMRgit/s2udio-working (branch `working`). Subagents work
in that repo, commit locally on `working`, DO NOT push. Ephemeral containers
only; G12 discipline (zero s2u-distro-* leftovers); never touch unrelated
running containers (navidrome/jellyfin/nginx/copyparty/devworkspace/searxng)
or /home/stoned/Projects. Report back via agent_message.send(..., receiver_role
="parent") when done; the parent dispatches the NEXT task only after the
previous one reported.

Background (what is already done and validated):
- setup.sh is a distro dispatcher (plan docs/design/Validation/distro-support
  .md §6.2; commits 8f50330 + c77c317): pacman/dnf5/apt/apk/xbps/nix
  backends, shared step functions, s2u-svc services, privilege-elevation
  helper, frozen Arch byte-identity baseline
  (scripts/dev/fixtures/setup.sh.arch-baseline).
- Validated: mock matrix 50/50 (scripts/dev/test-setup-mock.py); real
  container runs fedora-41 (dnf5), debian-12 (apt), alpine-320 (apk, incl.
  --no-sudo root path); real CachyOS host run (Arch path, no installs).
- Harness: scripts/dev/test-setup-distro.sh <key> runs the NEW setup.sh
  inside an ephemeral container and asserts end-state gates G0+S1..S9+G12.
  scripts/dev/containers/<key>/ holds the target images.

Execution order (each task depends only on the previous being DONE):

----------------------------------------------------------------------------
T1 — Real Void (xbps) container run of the new setup.sh
Status: **DONE** — commit `002f10b` (run_xbps runit prerequisites + clean launcher takeover); run `scripts/dev/artifacts/setup-void-glibc/20260809T214604Z`: G0 + S1–S8 + G12 green (runit-user; first run was a false green — see commit).
- Objective: validate run_xbps end-to-end for real: xbps installs, mpd
  `setcap -r`, runit-user services via s2u-svc (services_step_runit writes
  ~/.config/runit service dirs + runsvdir + sv).
- Steps: `bash scripts/dev/test-setup-distro.sh void-glibc`
- Context: setup.sh run_xbps; scripts/dev/containers/void-glibc/; scripts/
  s2u-svc (runit-user backend); plan §12 (Void deltas).
- Acceptance: G0 + S1-S8 + G12 all green (S8 = runit run files exist);
  zero s2u-distro-* leftovers; unrelated containers untouched.
- If a bug appears in run_xbps/services_step_runit: fix, `bash -n`, re-run
  the mock matrix (scripts/dev/test-setup-mock.py), commit on `working`.
- Report: task, run id (artifacts/setup-void-glibc/<ts>), gates table,
  any code changes + commit hash.

----------------------------------------------------------------------------
T2 — Real Nix (nix) container run of the new setup.sh
Status: **DONE** — commit `72e466c` (os-release bake, sudo-shim mkdir, SIGPIPE-safe profile check); run `scripts/dev/artifacts/setup-nix/20260809T220901Z`: G0 + S1–S8 + G12 green.
- Objective: validate run_nix for real: `nix profile install .#s2udio
  .#bridgePython` + nixpkgs runtime deps, upstream python mpDris2 fetch,
  launcher services via s2u-svc.
- Steps: `bash scripts/dev/test-setup-distro.sh nix`
- Context: setup.sh run_nix; flake.nix; scripts/dev/containers/nix/.
- Acceptance: G0 + S1-S8 + G12 green; zero leftovers.
- Fix+commit any run_nix bug (same discipline as T1).
- Report: task, run id, gates table, changes/commit hash.

----------------------------------------------------------------------------
T3 — Real Ubuntu 24.04 (apt) container run
Status: **DONE** — no code change; run `scripts/dev/artifacts/setup-ubuntu-2404/20260809T222041Z`: G0 + S1–S9 + G12 green.
- Objective: second apt-family distro (Debian 12 already green): system mpd
  stopped+disabled, user-level instance, stale-yt-dlp pip hint.
- Steps: `bash scripts/dev/test-setup-distro.sh ubuntu-2404`
- Acceptance: G0 + S1-S9 + G12 green; zero leftovers.
- Report: task, run id, gates table (no code change expected).

----------------------------------------------------------------------------
T4 — Detection coverage: artix + devuan in the mock matrix
Status: **DONE** — commit `8aa9c4f`; mock matrix 62/62 (was 51/51).
- Objective: prove ID/ID_LIKE routing for the two remaining routed distros:
  artix (ID_LIKE=arch -> pacman) and devuan (ID_LIKE=debian -> apt).
- Steps: add `artix` and `devuan` os-release fixtures + assertions to
  scripts/dev/test-setup-mock.py; `python3 scripts/dev/test-setup-mock.py`
  (must stay 50/50 + new checks green); `bash -n setup.sh` unaffected.
- Acceptance: new checks pass, full matrix green, commit on `working`.
- Report: task, diff summary, matrix result, commit hash.

----------------------------------------------------------------------------
T5 — Full G1-G11 gate loop through setup.sh (fedora-41)
Status: **DONE** — commit `75a5b24` (driver gate-prep + G1 packaged-unit acceptance); run `scripts/dev/artifacts/setup-fedora-41/20260809T225236Z`: G1–G11 all pass via the setup.sh path (G7 hard), G12 clean.
- Objective: strongest end-to-end validation — run the harness's full
  feature gate suite (build/version, unit tests, MPD+MPRIS, mpv headless,
  cava, TUI smoke, yt-dlp soft, s2u-svc round-trip) inside a container
  provisioned by the NEW setup.sh instead of the harness deploy path.
- Steps: extend scripts/dev/test-setup-distro.sh (or add a sibling driver)
  with a gate-prep step (test media ~/media/test.mp3/.mp4 + cava config, as
  scripts/dev/containers/common/deploy-common.sh does) then run
  scripts/dev/gates/run-gates.sh <key>; validate on fedora-41 first.
- Context: scripts/dev/gates/run-gates.sh + common.sh (gate definitions);
  deploy-common.sh (media/cava glue to reuse, NOT the unit/drop-in parts).
- Acceptance: G1-G11 green via the setup.sh path on fedora-41 (G7 soft ok);
  G12 clean; driver change committed.
- Report: task, gates table, driver diff, commit hash.

----------------------------------------------------------------------------
T6 — Arch install-path validation (two parts; T6b after T6a)
T6a Fix the stale `python-yt-dlp` package name in run_arch
Status: **DONE** — commit `dc96a4d`; mock 62/62 incl. refreshed byte-identity fixture (delta #2 documented).
- Why: on modern Arch/CachyOS the package is `yt-dlp` (extra/yt-dlp);
  `python-yt-dlp` is NOT in the repos, so `setup.sh -y` would abort at
  step 1 ("target not found"). Confirmed on this host (CachyOS).
- Steps: in setup.sh run_arch change the step-1 package list entry
  python-yt-dlp -> yt-dlp; regenerate scripts/dev/fixtures/setup.sh
  .arch-baseline from the updated run_arch (master's pre-dispatcher setup.sh
  + this delta; DOCUMENT in the fixture header that this is delta #2 vs
  master, alongside the existing comment delta #1); `python3 scripts/dev/
  test-setup-mock.py` byte-identity + matrix must stay green; commit.
- Acceptance: mock green incl. refreshed byte-identity; fixture delta
  documented; commit on `working`.
- Report: task, diff summary, matrix result, fixture delta description,
  commit hash.
T6b Real Arch container run (fresh archlinux, no AUR helper)
Status: **DONE** — commit `b2e8a11` (+`scripts/dev/containers/arch/`); run `scripts/dev/artifacts/setup-arch/20260809T231731Z`: G0 + S1–S9 + G12 green; AUR branch exercised via the mock only.
- Objective: prove the Arch path's install branch for real: detection
  (arch -> pacman), real `pacman -S` of the system packages (with the fixed
  yt-dlp name), cargo build, scripts, systemd-user services, fifo.
- Steps: fresh archlinux:base-devel container, install systemd, systemd
  user session (start_user_session pattern), NO yay/paru -> exercises the
  "no AUR helper" warn paths (mpdris2-git + mpv-full stay mock/AUR-covered;
  mpv-full is AUR-only on stock Arch and would take hours to build).
  Use the same ephemeral harness pattern (G12).
- Acceptance: detection + installs + build + services green; G12 clean.
- Report: task, what was and was not exercised (AUR branch explicitly
  deferred to the mock), run details, any code change + commit hash.

----------------------------------------------------------------------------
T7 — Tracker rewiring: scripts/s2u-mpv-tracker -> s2u-svc
Status: **DONE** — commit `e338004`; tracker 6/6 + mpdris2-shim 1/1 green, no `systemctl --user` left in the tracker.
- Objective: remove the tracker's direct `systemctl --user` call (mpDris2
  stop/start during video) in favour of scripts/s2u-svc (plan §6.1 remaining
  item); keep behavior identical on systemd targets.
- Steps: edit scripts/s2u-mpv-tracker; run its test suite (session log:
  "tracker 6/6") and the shim tests to stay green; `bash -n`; commit.
- Context: plan §6.1; scripts/s2u-svc; scripts/s2u-mpv-tracker.
- Acceptance: tests green, no systemctl --user left in the tracker,
  commit on `working`.
- Report: task, diff summary, test results, commit hash.

----------------------------------------------------------------------------
T8 — README install-matrix section + doc consolidation
Status: **DONE** — this commit (README "Install" section; distro-support.md §12 per-distro deltas 12.1–12.8 + T1–T8 results; session log; §11 checklist). Cosmetic summary note documented, not fixed (fix would touch run_arch output + byte-identity).
- Objective: document the distro support matrix for users: supported
  distros/backends, per-distro notes (RPM Fusion, system-mpd handling,
  cava-from-source, upstream mpDris2, setcap, rustup), and the Arch-path
  yt-dlp package-name note; consolidate results into the session log.
- Steps: add an "Install matrix" section to README.md; update
  docs/design/Validation/distro-support.md §12 + docs/design/Sessions/
  2026-08-09.md with the completed follow-up results; mark the plan file
  tasks done; commit.
- Acceptance: docs accurate vs the implemented behavior; commit on `working`.
- Report: task, diff summary, commit hash.

----------------------------------------------------------------------------
NOT DISPATCHABLE (documented reasons — parent should NOT spawn agents for):
- NixOS module + nixosTest VM: needs a real boot VM (out-of-container by
  design, plan §4.3/§10); containers cannot run it.
- Real Artix (OpenRC/s6): s6-user backend is a stub in s2u-svc; stretch.
- Master-repo setup.sh sync: separate host decision (two-repo policy).
- Push origin/working: host/user action (currently ahead by 2 commits).

----------------------------------------------------------------------------
Standing constraints for every task: `bash -n` clean on anything touched;
re-run the mock matrix when setup.sh or the mock changes; ephemeral
containers only with EXIT-trap teardown; commit locally on `working`, never
push; never touch unrelated containers or /home/stoned/Projects; record
results (artifacts stay host-side in scripts/dev/artifacts/, gitignored).
