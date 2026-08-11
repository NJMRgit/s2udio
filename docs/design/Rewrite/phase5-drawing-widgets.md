---
title: "Phase 5 — Shared Drawing Widgets (handoff plan)"
section: rewrite
doc_type: plan
id: "rewrite/phase5"
description: >
  Handoff plan for extracting shared drawing widgets from ControlsPane /
  LyricsPane into `src/ui/widgets/`: MarqueeLine (marquee + carousel cycle),
  wrap helpers (wrap_to_width / wrap_spans), button cluster, now-playing
  line templates. Prepared for a fresh agent to execute; parent spec is
  `docs/design/Rewrite/ui-reuse-rewrite.md` (phase-5 row, §4.5).
status: "active — awaiting implementer"
parent: "rewrite/ui-reuse"
updated: "2026-08-11"
---

# Phase 5 — Shared Drawing Widgets (handoff plan)

> **Status: PLAN.** 4b (queue decomposition) is DONE (`5bf5a18`+`80b4844`+
> `c81cb2f`+`9fee28e`, 1328/1328, close-out §5.3). This plan covers **5**
> only. A new agent completes it; the host reviews/live-checks.

## 1. Context — read this first

- **Repo**: `~/Projects/s2udio`, branch **`rewrite`** (private
  `NJMRgit/s2udio-working` only; distribution `master` untouched). Work in
  this branch; commit per sub-step below.
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
  gate** (outline §1, §6 DoD 4). A phase may land LOC-neutral/positive when
  the shared-core cost buys one-implementation-by-construction. **Never
  force two shapes that are genuinely different into one widget** — the
  decision rule in §3 exists for that.

## 2. Goal & scope

**Goal**: extract the shared drawing machinery from `ControlsPane`
(`src/ui/panes/controls.rs`, 1516 LOC) and `LyricsPane`
(`src/ui/panes/lyrics.rs`, 2543 LOC) into reusable widgets under
`src/ui/widgets/`, with **behavior parity** (the 1328-test suite is the
pin) and **≥2 call sites per extracted widget** (outline §4.5, phase-5
exit criteria).

**Target layout** (all NEW files; nothing renamed):

```
src/ui/widgets/marquee.rs      MarqueeLine + the carousel cycle (marquee_offset,
                               draw_panel_at, draw_marquee) + CAROUSEL_* constants
src/ui/widgets/wrap.rs         wrap_to_width + wrap_spans (+ relocated tests)
src/ui/widgets/button_cluster.rs  [if the decision rule passes — see §3]
src/ui/widgets/now_playing.rs  [if the decision rule passes — see §3]
```

**In scope**: the four candidate families below; per-file imports; `mod`
declarations in `src/ui/widgets/mod.rs`; docs close-out.

**Out of scope**: behavior changes; `queue.rs` (phase 4's territory);
modal/list/tree cores; anything outside the four families + the new files
+ docs.

## 3. Decision rule (the phase's safety valve)

Extract a shared widget only when the candidate shapes are **the same
shape** (args cover the differences without behavior-visible changes). If
unifying two call sites would change observable behavior (hover
mechanics, collapse thresholds, marquee timing, click zones), **do NOT
force it** — keep both, and document the divergence in the 5b5 close-out
(one paragraph: what differs, why unification would break behavior). The
user's priority is extensibility + predictable behavior; a documented
decision beats a forced merge. Minimum for the phase to close: **marquee**
and **wrap** land as real widgets with ≥2 call sites each (their shapes
are already shared — see the cross-pane calls in §4). `button_cluster`
and `now_playing` follow the decision rule.

## 4. Candidate inventory (measured @ `9fee28e`)

### 4a — marquee / carousel (MUST extract)

- `src/ui/widgets/scrolling_line.rs` — `ScrollingLine<'a>` (bon Builder:
  scroll_speed/align/line/progress; continuous wrap cycle with `|`
  separators, +3 cols). Single caller: `src/ui/panes/property.rs:53`.
- `src/ui/panes/controls.rs`:
  - `draw_marquee` (471, private) — manual buffer marquee; callers: render
    854/864, test 1402.
  - `marquee_offset` (509, **pub(crate)**) — the carousel cycle: hold
    `CAROUSEL_PAUSE_MS` at the start → scroll to tail at `ms_per_col`
    (10_000/`CAROUSEL_SPEED_X10`) → hold at the end → wrap 3× faster
    (`wrap_ms_per_col = ms_per_col/3`). Constants `CAROUSEL_SPEED_X10`,
    `CAROUSEL_WRAP_GAP`, `CAROUSEL_PAUSE_MS` live in controls.rs.
  - `draw_panel_at` (544, **pub(crate)**) — windowed strip draw with
    negative-offset centering during holds.
- **Cross-pane coupling (the smell this phase removes)**: `lyrics.rs`
  video-info title marquee calls
  `crate::ui::panes::controls::ControlsPane::marquee_offset` (596–601)
  and `ControlsPane::draw_panel_at` (54) — lyrics reaches into controls.
- **Tests pinning behavior**: controls
  `marquee_offset_holds_at_both_ends_and_wraps_forward` (1415),
  `marquee_wrap_shows_tail_gap_and_head_together` (1388),
  `artist_title_marquees_when_truncated` (1207),
  `mpv_long_title_never_overwrites_the_buttons` (1475); lyrics
  `title_marquee_holds_static_then_scrolls` (2241).
- **Target**: `src/ui/widgets/marquee.rs` — one `MarqueeLine`-style widget
  (or a widget + `marquee_offset` helper) parameterized by
  style/content/speed/pauses. Adopt in controls (2 render sites), lyrics
  (title marquee — flip the cross-pane calls), and decide for property.rs
  (`ScrollingLine`: adopt the unified cycle if its continuous-wrap shape
  is covered by args; otherwise keep `ScrollingLine` as a thin variant
  and say why in the close-out). ≥2 call sites guaranteed (controls +
  lyrics).
  **Do not change the cycle math or the constants** — the tests pin the
  exact timing.

### 4b — wrap helpers (MUST extract)

- `lyrics.rs::wrap_to_width` (1365, **pub(crate)**) — word-wrap +
  paragraph-keep; callers: `jellyfin.rs:1235` (imports it from lyrics!),
  `lyrics.rs:517`, `lyrics.rs:1345`. Test:
  `wrap_to_width_breaks_on_words_and_keeps_paragraphs` (2442).
- `lyrics.rs::wrap_spans` (1398, private) — span-aware wrap; caller:
  `lyrics.rs:927`.
- **Target**: `src/ui/widgets/wrap.rs` with both fns (+ the test moved
  with `wrap_to_width`); flip `jellyfin.rs`'s import to the new home.
  ≥2 call sites (lyrics + jellyfin). Do not drag `LINK_BLUE`,
  `scrub_emoji`, or `format_clock` along — they stay in lyrics.rs.

### 4c — button cluster (decision rule applies)

- `lyrics.rs`: `enum LyricsBtn` (77), `button_line` (207) — right-aligned
  text cluster (`● hide lyrics | ● fetch lyrics`), collapse labels on
  narrow panes, one-cell right margin, hover = label-text only, pressed
  markers. ~10 tests pin it (1547–1892).
- `controls.rs`: `mpv_button_layout` (312) + `audio_label`/`subtitle_label`
  (298/304) — row-0 `⤓ [Audio] [Sub]` cluster with click zones;
  `transport_zones` (183) + the transport row (4 buttons, pipes). Tests
  1290–1343, 1475.
- **Rule**: extract `ButtonCluster` ONLY if labels+separators+collapse+
  hover+alignment+click-zone args cover both without behavior-visible
  changes. Likely verdict: the lyrics cluster and the controls clusters
  are different shapes (text-collapse+hover-label vs
  click-zone+icon-cluster vs transport zones) — a documented decision
  NOT to merge is a valid outcome. Attempt it, measure, decide; put the
  rationale in the close-out either way.

### 4d — now-playing line templates (decision rule applies)

- `controls.rs::artist_title_line` (198), `channel_line` (254) — row-1
  content; tests: `now_playing_line_is_artist_dash_title` (1104),
  `channel_line_is_album_for_audio` (1115),
  `channel_line_is_show_for_mpv_video` (1123),
  `missing_artist_is_omitted_from_now_playing_line` (1162),
  `missing_title_is_omitted_from_now_playing_line` (1184),
  `artist_title_marquees_when_truncated` (1207).
- `lyrics.rs` info header (title prefix + marquee title + `Time:` + context
  row) — similar *semantics* (now-playing), different *layout* (fixed
  prefix + marquee vs centered carousel).
- **Rule**: extract a shared template/helper only if args cover the
  layout differences cleanly; otherwise document the divergence.

## 5. Method — commit per family, each green

Per family (5b1 → 5b2 → 5b3 → 5b4):

1. Create the widget file under `src/ui/widgets/`; add the `pub mod …;`
   row to `src/ui/widgets/mod.rs`.
2. Move the functions (+ their relocated tests when the code moves —
   moving a test WITH its code is allowed; **editing a test's assertions
   is not**, see DoD 1). Resolve imports per file (`cargo check --release`
   lists what each file is missing).
3. Update the callers (controls.rs / lyrics.rs / jellyfin.rs / property.rs
   as applicable) to use the widget; trim now-unused imports.
4. Gate: `cargo check --release` (3 baseline warnings max) +
   `cargo test --release` (**1328/1328**) +
   `python3 scripts/dev/ui-metrics.py --ref HEAD` guardrail (see §7).
5. Commit: `phase 5b1: extract MarqueeLine widget (marquee + carousel cycle)`
   (then 5b2 wrap, 5b3 button-cluster-or-decision, 5b4
   now-playing-or-decision).

**5b5 — close-out** (last commit):
- Re-run the full gate; record ref-to-ref metrics (`--ref` BEFORE vs AFTER
  — see §7 gotcha).
- Update `docs/design/Rewrite/ui-reuse-rewrite.md`: §2.4 table (new widget
  rows; update src/ui + tree totals), phase-5 row status, a §5.4-style
  close-out note (real numbers + the §3 decision rationales), flip this
  doc's `status` to done.
- Update `HANDOFF.md` rewrite-status section (phase 5 row + next = 6).
- Append a Phase 5 entry to `docs/design/Sessions/2026-08-11.md` (session
  log methodology).
- Commit: `phase 5 close-out: shared drawing widgets docs + metrics`.

## 6. Definition of done (5)

1. `cargo test --release` **1328/1328**. Tests may MOVE with their code;
   **assertions are never edited** for the move. If a behavior change
   becomes unavoidable, stop and re-scope (or follow the Phases 2.1/3
   delta protocol: implement, pin with a documented + tested delta, record
   it in the close-out — but the default is parity).
2. Behavior parity: identical rendering/navigation/mouse behavior
   (enforced by the untouched assertions + host live check §8).
3. Guardrail: same-named-fn pairs > 0.5 **≤ 60** excl-thin (run
   `python3 scripts/dev/ui-metrics.py --ref HEAD` after each commit).
4. **Marquee + wrap are real widgets with ≥2 call sites each**;
   button_cluster/now_playing per §3 (≥2 sites or a documented decision).
5. Docs updated (outline §2.4/phase row/§5.4, HANDOFF, this plan flipped
   done, session log).
6. Commits 5b1–5b5 on `rewrite`, each independently green.

## 7. Risks & gotchas

- **Metrics script trap**: `ui-metrics.py`'s LOC table reads the file
  list from `git ls-tree HEAD` but contents from the worktree when run
  without `--ref` — new files are silently excluded. ALWAYS compare
  committed refs: `--ref <before>` vs `--ref <after>`.
- **Marquee timing is pinned**: the exact cycle math (2×
  `CAROUSEL_PAUSE_MS` holds, 10_000/`CAROUSEL_SPEED_X10` ms/col, 3× faster
  wrap) is asserted by `marquee_offset_holds_at_both_ends_and_wraps_forward`
  and `title_marquee_holds_static_then_scrolls`. Move the constants with
  the code; change nothing.
- **Cross-pane calls must flip**: `lyrics.rs` → `ControlsPane::
  marquee_offset`/`draw_panel_at` become widget calls; `jellyfin.rs` →
  `lyrics::wrap_to_width` becomes a widget import. These flips ARE the
  phase's point — if a flip is impossible without behavior change, stop.
- **Don't drag unrelated helpers**: `LINK_BLUE`, `scrub_emoji`,
  `format_clock`, `InfoBodyLine` stay in lyrics.rs; `draw_line`/`draw_spans`/
  `draw_volume`/`put` stay in controls.rs unless the move set needs them
  (only `draw_marquee`/`marquee_offset`/`draw_panel_at` move).
- **property.rs `ScrollingLine`**: it has a different cycle (continuous
  wrap with `|`). Adopt-vs-keep is a §3 decision; if kept, `ScrollingLine`
  stays a thin widget and the close-out says why.
- **rustfmt**: no nightly in the container; keep lines ≤ 100, match
  surrounding style; host formats.
- **`pub(super)`/`pub(crate)`**: new widget fns need `pub(crate)` (or
  `pub`) visibility to be reachable from the panes; check the compiler.
- **Don't touch behavior**: this is a move + thin-adapter phase. If
  unifying forces a behavior-visible change, apply §3.

## 8. Host live-check (after 5b5, before the phase closes)

`cargo build --release`, install to `~/.local/bin/s2udio`, restart, then:

- **Controls bar**: row-0 channel/title carousel holds 2s at both ends and
  wraps; transport + mpv button clusters unchanged; long titles never
  overwrite the buttons.
- **Lyrics**: header button cluster unchanged (hover = label only,
  collapse on narrow, one-cell margin); video-info title marquee same
  cycle; description wraps to the box width; URL linkify unchanged.
- **Jellyfin**: episode overview still wraps to the body width.
- **Property pane**: the scrolling line still scrolls.
- **Queue tab** (phase-4b carry-over, plan §8 of the 4b doc): audio/video/
  chapters behavior, marks, esc-deselect, context menus, toggle row.

## 9. References

- Parent spec: `docs/design/Rewrite/ui-reuse-rewrite.md` (phases table,
  §4.5, §6 DoD).
- 4b commits: `5bf5a18`+`80b4844`+`c81cb2f`+`9fee28e` (close-out §5.3).
- Metrics tool: `scripts/dev/ui-metrics.py` (LOC + similarity guardrail).
