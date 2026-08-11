---
title: "Phase 4b — QueuePane Decomposition (handoff plan)"
section: rewrite
doc_type: plan
id: "rewrite/phase4b"
description: >
  Handoff plan for decomposing `src/ui/panes/queue.rs` (the largest file in
  the tree) into a module root + focused submodules. Prepared for a fresh
  agent to execute; parent spec is `docs/design/Rewrite/ui-reuse-rewrite.md`
  (phase-4 row, §4.4).
status: "done (2026-08-11)"
parent: "rewrite/ui-reuse"
updated: "2026-08-11"
---

# Phase 4b — QueuePane Decomposition (handoff plan)

> **Status: DONE (2026-08-11).** Executed as commits `5bf5a18` (4b1),
> `80b4844` (4b2), `c81cb2f` (4b3), and the close-out commit (4b4) on
> `rewrite`; `cargo test --release` **1328/1328**, warnings 3 baseline,
> zero test edits. queue.rs 4447 → **3485** (production −962). Host
> live-check (§8) still pending — see the outline §5.3 close-out.

## 1. Context — read this first

- **Repo**: `~/Projects/s2udio`, branch **`rewrite`** (private
  `NJMRgit/s2udio-working` only; distribution `master` untouched). Work in
  this branch; commit per sub-step below.
- **Environment**: Rust toolchain is at `~/.cargo/bin` — in the container run
  `export PATH="$HOME/.cargo/bin:$PATH"` first. No nightly toolchain (the
  repo's `rustfmt.toml` needs nightly) — format by hand to match style
  (max_width 100, imports grouped); the host runs `cargo fmt` (nightly).
- **Build/test loop**: `cargo check --release` for fast compile feedback,
  `cargo test --release` for the suite (baseline **1328/1328**, warnings
  **3** baseline: `config/mod.rs` unused-mut, `language.rs` unused ctx,
  `paste.rs` `AddAfterCurrent` dead variant — do not introduce more).
- **The rewrite**: UI-reuse consolidation — one master implementation per
  shape, panes become thin adapters/config. Full spec:
  `docs/design/Rewrite/ui-reuse-rewrite.md`. **User priority (2026-08-10):
  the aim is extensibility + predictable behavior; LOC is a proxy, not a
  gate** (outline §1, §6 DoD 4). A phase may land LOC-neutral/positive when
  the shared-core cost buys one-implementation-by-construction.
- **Phase 4 state**: audio list already adopts `SongListCore` (Phase 1,
  queue −208). 4a extracted the `● Audio ○ Video ○ Chapters` toggle row into
  `src/ui/widgets/sub_tab_bar.rs` (queue.rs `render_toggle_on_border` now
  delegates; public API unchanged). What remains is 4b (this plan) and the
  close-out.

## 2. Goal & scope

**Goal**: shrink the largest file in the tree (`src/ui/panes/queue.rs`,
**4447 LOC**: ~2674 production + 1773 test LOC in 5 `#[cfg(test)]` mods) by
splitting it into a module root + three focused submodules, with **zero
behavior change and zero test edits**.

**Target layout** (queue.rs stays a FILE — do NOT convert it to
`queue/mod.rs`, see §7):

```
src/ui/panes/queue.rs              module root: QueuePane struct, Pane impl,
                                   SongListCore impl, render / handle_action /
                                   handle_mouse_event dispatchers, toggle,
                                   shared helpers, ALL tests (unchanged)
src/ui/panes/queue/context_menus.rs   impl QueuePane { open_context_menu, … }
src/ui/panes/queue/video.rs           impl QueuePane { render_video, … }
src/ui/panes/queue/chapters.rs        impl QueuePane { render_chapters, … }
```

**In scope**: moving the function move-sets in §4 into submodules, per-file
imports, `mod` declarations, docs close-out.

**Out of scope**: behavior changes; test edits; audio-list rework (already on
SongListCore); SubTabBar rework; video/chapters unification (they stay
focused specs by design); anything outside `src/ui/panes/queue.rs` beyond the
new files + docs.

## 3. Why this shape (rules that make it safe)

- Submodules are **children** of the `queue` module, so they can access
  `queue.rs`'s private items (`Areas`, helpers, fields) without visibility
  changes — only `use super::QueuePane;` plus `use super::…` for anything
  else they need.
- Moved functions stay **inherent methods** on `QueuePane` (defined via
  `impl QueuePane { … }` in the submodule) → every existing call site
  (`render`, `handle_action`, `handle_mouse_event`, `tab_screen.rs`, tests)
  compiles unchanged.
- All struct fields stay declared in `queue.rs` → the test mods' direct
  field access (`pane.video_state`, `pane.chapters_state`,
  `pane.toggle_areas`, …) keeps working with zero edits.
- Tests stay in `queue.rs` → the 1328-test suite is the behavior-parity
  pin: **if a test needs editing to make the split compile, the split is
  wrong**.

## 4. Function move sets (exact inventory @ `9b46f54`)

Move whole functions (with their doc comments) into `impl QueuePane` blocks:

**4b1 — `context_menus.rs`** (~330 LOC): `open_context_menu`,
`open_audio_context_menu`, `open_video_context_menu`.
(Callers: `handle_action` lines ~2185/2188, `handle_mouse_event` ~1723,
`handle_video_action` ~2386 — all inherent-method calls, unchanged.)

**4b2 — `video.rs`** (~340 LOC): `render_video`, `handle_video_action`,
`video_load_entry`, `video_remove_entries`, `video_move`, `video_scroll_to`,
`video_page`, `video_jump`, `video_play_selected`, `follow_playing_video`.

**4b3 — `chapters.rs`** (~300 LOC): `render_chapters`,
`handle_chapters_action`, `chapters_move`, `chapters_page`, `chapters_jump`,
`chapters_select_current`, `chapters_play_selected`, `chapters_scroll_to`,
`seek_to`.

**Stays in `queue.rs`**: `resolved_stream_expired`, `play_queue_song`,
`clear_marked`, `local_queue`, `chapters_available`, `current_chapters`,
`set_tab`, `cycle_tab`, `new`, `init`, `enqueue_items`, `items`,
`render_toggle_on_border`, `render`, `calculate_areas`, `before_show`,
`resize`, `on_event`, `handle_mouse_event`, `on_query_finished`,
`handle_insert_mode`, `handle_action`, the whole `SongListCore` impl,
`scrollbar_area`, `truncate_to_width`, `stream_column_line`,
`is_box_corner_glyph`, the `QueueRow` impl, and every test mod.

(Line numbers above are as of `9b46f54` and shift as commits land; trust the
function names.)

## 5. Method — do it in 4 commits, each green

Per submodule (4b1 → 4b2 → 4b3):

1. Add `mod context_menus;` (etc.) to `queue.rs`'s module declarations.
2. Create the submodule file: `use super::QueuePane;` (+ anything else it
   needs), then `impl QueuePane { …moved fns… }`.
3. Delete the moved fns from `queue.rs`.
4. Resolve imports with `cargo check --release` (the compiler lists what
   each file is missing; pull from `queue.rs`'s import block — trim now-unused
   imports from `queue.rs` if the compiler reports them as unused).
5. Gate: `cargo check --release` (3 baseline warnings max) +
   `cargo test --release` (**1328/1328, zero test edits**) +
   `python3 scripts/dev/ui-metrics.py --ref HEAD` guardrail (see §6).
6. Commit: `phase 4b1: split queue context menus into queue/context_menus.rs`
   (then 4b2/4b3 similarly).

**4b4 — close-out** (last commit):
- Re-run the full gate; record ref-to-ref metrics (`--ref` BEFORE vs AFTER —
  see §7 gotcha).
- Update `docs/design/Rewrite/ui-reuse-rewrite.md`: §2.4 table (queue.rs row:
  `4686 → …`; add `src/ui/panes/queue/video.rs`, `…/chapters.rs`,
  `…/context_menus.rs` rows; update `src/ui` + tree totals), phase-4 row
  status, §5.2-style close-out note (this plan's real numbers), flip this
  doc's `status` to done.
- Update `HANDOFF.md` rewrite-status section (phase 4b row + next = 4c/5).
- Commit: `phase 4b close-out: …`.

## 6. Definition of done (4b)

1. `cargo test --release` **1328/1328** with **zero edits** to the 1773 test
   LOC; no new warnings (3 baseline).
2. Behavior parity: identical rendering/navigation/mouse/context-menu
   behavior (enforced by the untouched tests + host live check below).
3. Guardrail: same-named-fn pairs > 0.5 **≤ 60** excl-thin (run
   `python3 scripts/dev/ui-metrics.py --ref HEAD` after each commit; expect
   the count to drop or stay — new submodule fns have unique names).
4. queue.rs production code **−~950 LOC** (4447 → ~3450 total; tests stay).
   (The outline's "−1–1.5k in queue.rs" was written pre-Phase-1 when the
   audio list hadn't been migrated; per the user priority this is a target,
   not a gate.)
5. Docs updated (outline §2.4/phase row, HANDOFF, this plan flipped done).
6. Commits 4b1–4b4 on `rewrite`, each independently green.

## 7. Risks & gotchas

- **Keep `queue.rs` a file.** Do NOT rename to `queue/mod.rs`: the metrics
  script's `PANE_FILES` and `tab_screen.rs`/`panes/mod.rs` paths reference
  `src/ui/panes/queue.rs`; renaming churns them for no gain.
- **Metrics script trap**: `ui-metrics.py`'s LOC table reads the file list
  from `git ls-tree HEAD` but file contents from the worktree when run
  without `--ref` — new files are silently excluded. ALWAYS compare
  committed refs: `--ref <before>` vs `--ref <after>`.
- **Imports**: each submodule needs its own `use` block; the moved fns use
  ratatui widgets, `MenuModal`/`SelectModal`, `modal!`, `Song`, `Ctx`,
  `ActionEvent`, `MouseEvent`, `CommonAction`, `MarkState`,
  `VirtualizedTable`, etc. Copy what the compiler asks for.
- **`seek_to` is chapters-only** (callers: `handle_mouse_event` chapters
  double-click ~1421 and `chapters_play_selected` ~2577) — moves with
  chapters. `current_chapters`/`chapters_available` are used by the
  dispatchers/toggle too → **stay** in queue.rs (inherent calls still work
  if you disagree, but keeping them put minimizes diff).
- **`follow_playing_video`** is called from `set_tab` (queue.rs) — fine, it
  stays callable as an inherent method once moved.
- **rustfmt**: no nightly in the container; keep lines ≤ 100, match
  surrounding style; host formats.
- **Don't touch behavior**: this is a pure move. If moving a fn forces a
  behavior-visible change, stop and re-scope.

## 8. Host live-check (after 4b4, before the phase closes)

`cargo build --release`, install to `~/.local/bin/s2udio`, restart, then in
the **Queue tab**:

- Audio: song table, scrollbar, esc-deselect, marks (ctrl/alt-click +
  shift+↑/↓), context menu (Enter/right-click), `● Audio` active.
- Video: mpv playlist lists entries, current-entry highlight, scrollbar drag,
  marks, context menu, double-click loads.
- Chapters: single click highlights only, double click seeks, w/s/↑/↓ + page
  keys navigate, `● Chapters` only when the track has markers.
- Toggle row: `●/⭘` markers switch modes; click segments switch; `c` cycles
  Audio → Video → Chapters → Audio.
- Merged queue box + toggle on the row above it (unchanged from 4a).

## 9. References

- Parent spec: `docs/design/Rewrite/ui-reuse-rewrite.md` (phases table,
  §4.4 `SubTabBar`, §6 DoD).
- 4a commit: `9b46f54` (`src/ui/widgets/sub_tab_bar.rs`).
- Metrics tool: `scripts/dev/ui-metrics.py` (LOC + similarity guardrail).
