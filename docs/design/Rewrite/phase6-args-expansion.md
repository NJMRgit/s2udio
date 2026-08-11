---
title: "Phase 6 — Args Expansion (handoff plan)"
section: rewrite
doc_type: plan
id: "rewrite/phase6"
description: >
  Handoff plan for Phase 6 of the UI-reuse rewrite: move pane-specific
  constants (tree min-width / hide threshold, info-box cap) into
  PaneType/config args with serde defaults, wire them through the browser
  family, and demonstrate/decide the "new browser tab = config block +
  adapter" construction pattern. Prepared for a fresh agent to execute;
  parent spec is `docs/design/Rewrite/ui-reuse-rewrite.md` (phase-6 row,
  §3 rules).
status: "active — awaiting implementer"
parent: "rewrite/ui-reuse"
updated: "2026-08-11"
---

# Phase 6 — Args Expansion (handoff plan)

> **Status: PLAN.** Phase 5 (shared drawing widgets) is DONE
> (`483a73c`+`490c62e`+`2fcb10c`+`ef4863f`+`671bc91`, 1328/1328, host
> live-validated 2026-08-11: behavior + appearance identical to master,
> blur auto-theme/hover/selection/marquee all verified). This plan covers
> **6** only. A new agent completes it; the host reviews/live-checks.

## 1. Context — read this first

- **Repo**: `~/Projects/s2udio`, branch **`rewrite`** (private
  `NJMRgit/s2udio-working` only; distribution `master` untouched). Work in
  this branch; commit per sub-step below. **NEVER push** — local commits
  and pulls only (user rule; the host pushes separately).
- **Environment**: Rust toolchain at `~/.cargo/bin` — run
  `export PATH="$HOME/.cargo/bin:$PATH"` first, plus
  `export RUSTUP_HOME="$HOME/.rustup" CARGO_HOME="$HOME/.cargo"`. No
  nightly toolchain (the repo's `rustfmt.toml` needs nightly) — format by
  hand to match style (max_width 100, imports grouped); the host runs
  `cargo fmt` (nightly).
- **Build/test loop**: `cargo check --release` for fast compile feedback,
  `cargo test --release` for the suite (baseline **1328/1328**, warnings
  **3** baseline: `config/mod.rs` unused-mut, `language.rs` unused ctx,
  `paste.rs` `AddAfterCurrent` dead variant — do not introduce more).
- **The rewrite**: UI-reuse consolidation — one master implementation per
  shape, panes become thin adapters/config. Full spec:
  `docs/design/Rewrite/ui-reuse-rewrite.md`. **User priority (2026-08-10):
  the aim is extensibility + predictable behavior; LOC is a proxy, not a
  gate** (outline §1, §6 DoD 4). **User rule (2026-08-11): never push.**
- **Phase 6 in the outline**: "Args expansion: pane-specific constants
  move into `PaneType`/config args" — targets `src/config/tabs.rs` +
  panes; exit: *"Adding a new browser tab = config block + adapter, no new
  pane file; sidecars migrate cleanly."* The precedent already exists:
  `PaneType::Volume { kind }`, `PaneType::Property { content, align,
  scroll_speed }`, `PaneType::Browser { root_tag, separator }`.

## 2. Goal & scope

**Goal**: extend the args-first pattern to the browser family
(`Directories` / `Playlists` / `Jellyfin` / `Radio`) so the user-tuned
pane constants become config args with **serde defaults that reproduce
today's behavior exactly** (existing `config.ron` parses unchanged →
"sidecars migrate cleanly"), and the panes read the args instead of
hard-coding them. Then demonstrate (or document) the "new browser tab =
config block + adapter" construction pattern.

**In scope**:
- New args struct + `#[serde(default)]` fields on the four browser-family
  `PaneTypeFile` variants (and the mirrored `PaneType` variants).
- Wire-through: `TreeBrowserCore` + `directories.rs` / `playlists.rs` /
  `jellyfin.rs` / `radio.rs` read the args for the tree layout + info-box
  cap; the shared `tree_width()` helper and the `info_h` formula keep
  their current behavior as the default.
- The mechanical match churn (exhaustive `PaneType` matches gain `{ .. }`).
- New tests: args-parse tests + default-parity pins.
- Docs close-out.

**Out of scope**: behavior changes to the panes themselves; the queue/
search/lyrics/controls constants (not user-tuned; can be a later
iteration); modal/list/tree core rework beyond reading args;
`core/`/`mpd/`/`radio/`/`jellyfin/`/`shared/`.

## 3. Config-shape proposal (the design decision)

Add ONE shared args struct, used by both `PaneTypeFile` (serde side) and
`PaneType` (internal side — plain data, no conversion needed):

```rust
/// src/config/tabs.rs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TreeBrowserArgs {
    pub tree_min_width: u16,        // default 50   (round-7/8 behavior)
    pub tree_hide_below: u16,       // default 120  (TUI width <= this hides the tree)
    pub info_box_cap: Option<u16>,  // default Some(15) — None = uncapped (round-8/9 behavior)
}
impl Default for TreeBrowserArgs {
    fn default() -> Self { Self { tree_min_width: 50, tree_hide_below: 120, info_box_cap: Some(15) } }
}
```

Variant changes (both enums):

```rust
PaneType::Directories { #[serde(default)] tree: TreeBrowserArgs }
PaneType::Playlists  { #[serde(default)] tree: TreeBrowserArgs }
PaneType::Jellyfin   { #[serde(default)] tree: TreeBrowserArgs }
PaneType::Radio      { #[serde(default)] tree: TreeBrowserArgs }
```

- **Backward compatibility is the hard requirement**: a config.ron with
  bare `Directories` (today's syntax) must parse and behave identically —
  `#[serde(default)]` + `Default` gives that for free; pin it with a
  parse test on the current example config syntax.
- The shared `tree_width(total)` helper stays as the *default*
  implementation; per-pane args override only when set. Keep the helper's
  current signature used by `playlists.rs:954` and `tree_browser.rs:150`
  working (default path) — or move it behind the args struct's method
  (see §5), whichever keeps the tree-width tests untouched.
- `info_h` (`directories.rs:1066`, `playlists.rs:969`):
  `(right.height - tips_h) * 2 / 3` capped by `info_box_cap` (`.min(15)`
  today → `.min(cap)` with `cap = args.info_box_cap.unwrap_or(h)` —
  `None` = uncapped, the round-8 behavior the `info_box_height_cap`
  tests pin).

## 4. Candidate inventory (measured @ `ec5d553`)

**Constants to make args**:
- `directories.rs:55 pub(crate) fn tree_width(total: u16) -> u16` — min 50
  cols, hidden when `total <= 120`; callers: `directories.rs` render,
  `playlists.rs:954`, `tree_browser.rs:150` (jellyfin/radio). Pinned by
  `tree_width(120)==0`, `tree_width(80)==0`, `tree_width(200)==60`,
  `tree_width(160)==50` tests (`directories.rs` tests 1954–1981) +
  `left_pane_hidden_on_narrow_tui`/`left_pane_keeps_min_width_on_wide_tui`
  (playlists tests) + `tree_pane_hidden_on_narrow_tui`/`tree_pane_keeps_
  min_width_on_wide_tui` (jellyfin tests).
- `directories.rs:1066` + `playlists.rs:969`:
  `info_h = (right.height.saturating_sub(tips_h) * 2 / 3).min(15)` —
  pinned by the `info_box_height_cap` tests (160×40 → 15; 160×20 →
  uncapped 11).

**Churn surface (exhaustive `PaneType` matches gaining `{ .. }`)** — ~17
files match `PaneType::`; the ones with bare variant patterns that need
`{ .. }` when fields are added: `src/config/tabs.rs` (conversion +
tests), `src/config/mod.rs:371`, `src/ui/panes/mod.rs` (`get_mut` ~253 +
`pane_call!` macro + tests 2928+), `src/ui/mod.rs` (`init_tabs` 133 +
matches), `src/ctx.rs:642`, `src/ui/modals/tab_help.rs:77`, plus any
construction sites in `settings.rs` (tab customization) — the compiler
lists every site; `cargo check` drives it. Do NOT `#[allow]` or
destructure without `..` — the point is the fields are reachable.

**Where args are read from**: `PaneType` instances flow into
`Panes::get_mut` and the pane constructors via `init_tabs`. The browser
panes' structs (`DirectoriesPane`, `PlaylistsPane`, `JellyfinPane`,
`RadioPane`) and `TreeBrowserCore` must receive the args (constructor
arg or a `ctx.config` lookup at render — prefer threading the args
through the existing pane construction so the panes stay testable
without a full config).

## 5. Method — commit per step, each green

**6.1 — args plumbing** (config side):
1. Add `TreeBrowserArgs` (serde + Default) and the four variant fields
   (both `PaneTypeFile` and `PaneType`) in `src/config/tabs.rs`.
2. Fix the conversion (`TryFrom<PaneTypeFile> for PaneType`) and every
   exhaustive match the compiler flags (use `{ .. }` where fields are
   ignored; carry args where a pane is constructed).
3. Tests: (a) today's bare-`Directories` config syntax parses with default
   args; (b) a config setting `Directories { tree: { tree_min_width: 60,
   tree_hide_below: 100, info_box_cap: None } }` round-trips.
4. Gate (`cargo check --release` + `cargo test --release` 1328/1328 +
   `ui-metrics.py --ref HEAD` ≤ 60).
5. Commit: `phase 6.1: PaneType tree-browser args (serde defaults, backward compatible)`.

**6.2 — wire through the panes**:
1. `TreeBrowserCore` + the four panes read the args for tree layout +
   info cap (default = today's `tree_width()` / `.min(15)` behavior).
2. Keep the tree-width + info-cap tests green **unchanged** (they are the
   parity pin); add one test per pane proving a non-default arg changes
   the layout (e.g. `tree_min_width: 60` → 60-col tree at 160 cols;
   `info_box_cap: None` → uncapped tall render).
3. Gate + commit: `phase 6.2: browser panes read tree/info args (defaults = current behavior)`.

**6.3 — construction pattern (decision rule)**:
Attempt the outline's exit-criteria demonstration: "adding a new browser
tab = config block + adapter, no new pane file". Concrete option: a
parameterized browser-pane path where a config block (e.g.
`PaneType::Browser { backend: TreeBackendFile::MpdRoot, tree: … }` or a
`Directories`-with-args second instance) instantiates an existing core
without a new pane file; OR the docs recipe "add a browser tab" that an
adapter implements. **Decision rule (Phase-5 §3)**: if unifying the four
panes into one config-driven backend enum would change observable
behavior (radio focus handling, jellyfin shared-selection, playlists
song pane, directories Downloads), do NOT force it — document the recipe
+ why adapters stay per-backend, and close the phase on that. Minimum:
the close-out proves a new browser tab needs a config block + thin
adapter, never a new core implementation.
Gate + commit: `phase 6.3: browser tab construction pattern (config block + adapter) or documented decision`.

**6.4 — close-out**:
- Re-run the full gate; record ref-to-ref metrics (`--ref` BEFORE vs
  AFTER — see §7 gotcha).
- Update `docs/design/Rewrite/ui-reuse-rewrite.md`: §2.4 table (tabs.rs/
  panes deltas + totals), phase-6 row status, a §5.5-style close-out note
  (real numbers + the §6.3 decision rationale), flip this doc's `status`
  to done.
- Update `HANDOFF.md` rewrite-status section (phase 6 row + next = 7
  close-out).
- Append a Phase 6 entry to `docs/design/Sessions/2026-08-11.md`.
- Commit: `phase 6 close-out: args expansion docs + metrics`.

## 6. Definition of done (6)

1. `cargo test --release` **1328/1328**; tests may MOVE with code, new
   tests may be added, but **existing assertions are never edited for the
   refactor** (new args tests are additions, not edits).
2. **Backward compatibility**: a config.ron in today's syntax (bare
   `Directories` etc.) parses with default args and behaves identically —
   pinned by a parse test; the round-23 config (all configs in
   `~/.config/s2udio/`) needs no edits (live-check).
3. Behavior parity: tree-width + info-cap tests unchanged and green;
   defaults = today's constants exactly (50/120/Some(15)).
4. Guardrail: same-named-fn pairs > 0.5 **≤ 60** excl-thin
   (`ui-metrics.py --ref HEAD`).
5. Warnings ≤ 3 baseline; no new warnings.
6. Docs updated (outline §2.4/phase row/§5.5, HANDOFF, this plan flipped
   done, session log).
7. Commits 6.1–6.4 on `rewrite`, each independently green. **No push.**

## 7. Risks & gotchas

- **Metrics script trap**: `ui-metrics.py` reads the file list from
  `git ls-tree HEAD` but contents from the worktree without `--ref` —
  ALWAYS compare committed refs for the close-out numbers.
- **Match churn is the main cost**: ~17 files match `PaneType::`; the
  compiler is the source of truth. Use `{ .. }` everywhere the fields are
  irrelevant (dispatch matches) and carry args only where a pane is
  constructed/configured. Never `#[allow(unreachable_patterns)]` a match.
- **serde defaults are load-bearing**: `#[serde(default)]` on the variant
  fields AND `impl Default for TreeBrowserArgs` (the derive Default would
  give 0/0/None — wrong). Pin the current-syntax parse test FIRST (6.1)
  before touching the panes.
- **`PaneType` is not Serialize** — keep the args struct plain-data so
  the SAME type works in both enums (like the existing `Volume` kind
  pattern); no conversion needed.
- **`tree_width` callers outside directories**: `playlists.rs:954` and
  `tree_browser.rs:150` call `directories::tree_width(area.width)` — the
  default path must keep working (keep the helper pub(crate), default
  args call it); per-pane args override at the pane's render.
- **settings.rs tab customization** may construct `PaneType` variants —
  find and update those construction sites (the compiler lists them).
- **Don't touch behavior**: pure args plumbing; if threading an arg
  forces a behavior-visible change, stop and re-scope (or follow the
  Phases 2.1/3 delta protocol).
- **rustfmt**: no nightly in the container; keep lines ≤ 100, match
  surrounding style; host formats.
- **No push**: local commits only.

## 8. Host live-check (after 6.4, before the phase closes)

`cargo build --release`, install `target/release/s2u` → `~/.local/bin/s2udio`,
restart, then:

- MPD / Playlists / Jellyfin / Radio tabs: kitty ~70 cols hides the left
  tree; a wide terminal shows the 50-col tree; MPD/Playlists info boxes
  ≈ 15 lines.
- Config override: set `tree_min_width: 60` (or `info_box_cap: None`) in
  `~/.config/s2udio/config.ron`, restart → the layout follows; revert →
  defaults return.
- The round-23 config needs NO edits (backward compat verified live).
- Queue tab + controls/lyrics/jellyfin (phases 4b/5 live checks) still
  good.

## 9. References

- Parent spec: `docs/design/Rewrite/ui-reuse-rewrite.md` (phases table,
  §3 rules, §6 DoD).
- Phase 5 close-out: `671bc91` (outline §5.4) — §3 decision rule reused
  for 6.3.
- Metrics tool: `scripts/dev/ui-metrics.py` (LOC + similarity guardrail).
