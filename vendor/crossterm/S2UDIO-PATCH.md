# s2udio patch: crossterm 0.29.0 + OSC parsing (round 90)

This directory is **crossterm 0.29.0 from crates.io**, copied into the tree and
patched. It is wired in through `[patch.crates-io]` in the repository root
`Cargo.toml`:

```toml
[patch.crates-io]
crossterm = { path = "vendor/crossterm" }
```

## Why

kitty delivers drag & drop as OSC 72 escapes (`ESC ] 72 ; … ESC \`). Upstream
crossterm has **no OSC parsing at all**: `ESC ]` falls into the generic
`ESC`-prefixed branch and the sequence reaches the application as a burst of
bogus key events (`]`, `7`, `2`, `;`, …). The only way to receive the drop —
without replacing crossterm's whole input path with a private parser — is to
teach the parser to read an OSC string to its terminator.

## What changed (see `s2udio-osc.patch`)

- `src/event/sys/unix/parse.rs`
  - `parse_event`: new arm `b']' => parse_osc(buffer)`.
  - new `parse_osc()`: scans for the string terminator (`ESC \`, or `BEL`),
    returns `Ok(None)` while the sequence is incomplete (the caller keeps the
    bytes, so a sequence split across reads still parses), `Err` when a body
    grows past 64 KiB without a terminator.
  - new `osc_event()`: wraps the decoded body in the new event.
- `src/event.rs`
  - new variant `Event::Osc(Vec<u8>)` (payload = the OSC body, without the
    leading `ESC ]` and without the terminator).
  - the `#[cfg_attr(not(feature = "bracketed-paste"), derive(Copy))]` line is
    gone: `Event` cannot be `Copy` with a `Vec<u8>` payload. With the default
    features (`bracketed-paste` is one of them, because of `Paste(String)`)
    `Event` was already not `Copy`, so nothing changes in practice.
- `src/terminal/sys/unix.rs`
  - one lint fix: `rustix` 1.x flags the parentheses in
    `.map(|file| (FileDesc::Owned(file.into())))` as `unused_parens`, which
    would land a warning on the repository's zero-warning build gate.

Everything else is byte-identical to 0.29.0 (and the files that were edited
here were re-normalized to the crate's CRLF line endings, so the diff stays
small).

## Upgrading crossterm

1. `cp -a <registry>/crossterm-<new>/` over this directory (drop `.cargo-ok`;
   the tree is trimmed: no `docs/`, `.github/`, `.travis.yml`, `Cargo.lock`).
2. `patch -p1 -d vendor/crossterm < vendor/crossterm/s2udio-osc.patch`
   (the diff paths are `pristine/…` and `patched/…`).
3. Check the two anchors still exist (`b'[' => parse_csi(buffer)` in
   `parse_event`, and the `pub enum Event` variant list) and fix by hand if the
   context moved.
4. `cargo build --release` (gate: zero warnings), then the round's drag & drop
   matrix (`/tmp/s2dnd-test/matrix.py`).
