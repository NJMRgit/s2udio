# scripts/dev — distro-support test harness

> **Lean copy note (2026-08-22):** this tree has **no test suite**
> (`src/tests/`, inline `#[cfg(test)]`, `tests/` removed), so harness
> steps invoking `cargo test --release` (G3 gate) will fail here. Run
> the harness against the full tree (`~/Projects/s2udio`) instead.
> `ui-metrics.py`'s guardrail doc pointer `docs/design/Rewrite/` was
> removed in the cleanup; the tool still works standalone.

Ephemeral rootless-podman validation harness for the distro-support plan
(`docs/design/Validation/distro-support.md`). Every run creates a
`s2u-distro-<key>` container and removes it (4-way ephemerality: `--rm`,
EXIT trap, start-of-run sweep, end-of-run G12 assertion).

## Usage

    scripts/dev/test-distro.sh <matrix-key> [--gate G0..G12] [--no-cache-vol] [--artifacts DIR]

Matrix keys: `fedora-41`, `debian-12`, `ubuntu-2404` (Phase 1);
`nix`, `alpine-320`, `void-glibc`, … land with Phases 2–3.

Lifecycle (plan §4.1): sweep → create (`--systemd=always`, `/sbin/init`)
→ copy the repo in (no `.git`/`target/`) → provision (packages + rustup) →
`cargo build --release` + `cargo test --release` → deploy (scripts, config,
services) → gates G1–G11 (in-container) → collect artifacts → teardown →
G0/G12 recorded host-side. Artifacts land in
`scripts/dev/artifacts/<key>/<timestamp>/` (run.log + per-gate JSON +
gates.jsonl).

## Layout

- `test-distro.sh` — driver.
- `lib.sh` — shared helpers (`write_gate`, session plumbing).
- `containers/<key>/` — per-target `Dockerfile` (init image: systemd as
  PID 1), `image.env` (IMAGE/INIT/INIT_MODE), `provision.sh` (package map +
  rust toolchain), `deploy.sh` (setup.sh-equivalent steps).
- `containers/common/deploy-common.sh` — distro-agnostic deploy steps.
- `gates/run-gates.sh` + `gates/common.sh` — in-container gate runner and
  the G1–G11 implementations.

## Notes

- Distro rustc is too old for edition-2024 everywhere in Phase 1
  (Fedora 41 ~1.80, Debian 12 1.63, Ubuntu 24.04 1.75; Cargo.toml needs
  ≥1.88) → rustup minimal profile.
- Fedora's official repos dropped the `mpd` server → RPM Fusion free is
  enabled in the Fedora init image (Arch-AUR-analogous; also brings full
  ffmpeg/mpv).
- Named volumes `s2u-cargo-<key>` cache the cargo registry across runs
  (`podman volume rm s2u-cargo-*` to prune; volumes are not containers).
