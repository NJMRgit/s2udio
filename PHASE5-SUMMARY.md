# Phase 5 — Shared Drawing Widgets: summary (2026-08-11)

Executed on branch `rewrite` per `docs/design/Rewrite/phase5-drawing-widgets.md`
(handoff plan, now flipped to done). Working tree was clean at `64be282`.
Five commits, each independently green; **1328/1328** after every commit,
warnings at the 3 baseline (`config/mod.rs` unused-mut, `language.rs` unused
ctx, `paste.rs` `AddAfterCurrent`), similarity guardrail **60 excl-thin pairs,
identical pair set** after every commit.

## Commits

| Commit | Step | Content |
| --- | --- | --- |
| `483a73c` | 5b1 | `src/ui/widgets/marquee.rs` (282 LOC) — `draw_marquee`, `marquee_offset`, `draw_panel_at` + `CAROUSEL_PAUSE_MS`/`CAROUSEL_SPEED_X10`/`CAROUSEL_WRAP_GAP` moved from controls.rs; the 2 marquee timing tests moved with the code (assertions byte-identical); lyrics (601/609) **and** jellyfin (1474/1482) cross-pane `ControlsPane::marquee_offset`/`draw_panel_at` calls flipped to the widget; controls' 2 render sites call the widget's `draw_marquee`. Widget carries its own minimal private `draw_line`/`draw_spans` (left-aligned / skip+clip). |
| `490c62e` | 5b2 | `src/ui/widgets/wrap.rs` (84 LOC) — `wrap_to_width` (pub(crate)) + `wrap_spans` (pub(crate)) + the wrap test moved with its code; jellyfin's `crate::ui::panes::lyrics::wrap_to_width` import flipped to the widget (`scrub_emoji` import stays). |
| `2fcb10c` | 5b3 | **Documented decision NOT to merge** the button cluster (plan §3a): lyrics header cluster vs controls mpv row-0 cluster vs transport row are three different shapes (separator, collapse tiers, label-text-only vs whole-label hover, pressed glyph, zone shape, write primitive). |
| `ef4863f` | 5b4 | **Documented decision NOT to merge** the now-playing line templates (plan §3a): controls `artist_title_line`/`channel_line` vs lyrics info header differ in data resolution, styles and layout; the shared marquee cycle is already the 5b1 widget. |
| `671bc91` | 5b5 | Close-out: full gate re-run; ref-to-ref metrics; outline §2.4 table + phase-5 row + new §5.4 close-out; HANDOFF (next = 6); plan doc flipped to done; session log entry. |

## Gate (final HEAD)

- `cargo test --release`: **1328/1328** (tests moved with their code, assertions untouched).
- `cargo check --release`: warnings **3** baseline (no new).
- Guardrail `python3 scripts/dev/ui-metrics.py --ref HEAD`: **60 excl-thin
  pairs** (identical pair set vs the `64be282` baseline; incl-thin 200 = 200).
- No leftover cross-pane coupling: grep for `ControlsPane::marquee_offset` /
  `ControlsPane::draw_panel_at` / `panes::lyrics::wrap_to_width` → empty.

## Metrics (committed refs: `--ref 64be282` vs `--ref HEAD`)

| File | Before | After | Δ |
| --- | --- | --- | --- |
| `src/ui/panes/controls.rs` | 1516 | 1319 | −197 (prod −112, tests −85: 2 marquee tests moved) |
| `src/ui/panes/lyrics.rs` | 2543 | 2475 | −68 (prod −61, tests −7: wrap test moved) |
| `src/ui/widgets/marquee.rs` | — | 282 | +282 (193 prod + 89 test) |
| `src/ui/widgets/wrap.rs` | — | 84 | +84 (72 prod + 12 test) |
| `src/ui/widgets/mod.rs` | 21 | 23 | +2 |
| **src/ui total** | **57,070** | **57,173** | **+103** |
| **tree total .rs** | **96,128** | **96,231** | **+103** |

A pure move plus the widget header/doc cost (marquee.rs carries the full
cycle documentation with the constants); per the user priority the phase
closes on extensibility + predictable behavior (one implementation per
shape), LOC being a proxy, not a gate.

## Adopted call sites

- **marquee**: controls (2 render sites) + lyrics (video-info title
  marquee) + jellyfin (header title marquee) — 3 panes, ≥2 ✓.
- **wrap**: lyrics (`wrap_to_width` ×2 at 517/1345 + `wrap_spans` ×1 at
  927) + jellyfin (episode overview, 1235) — 2 panes, ≥2 ✓.

## §3 decisions (rationale)

1. **Button cluster — NOT merged**: the lyrics header cluster
   (`LyricsBtn`/`button_line`), the controls mpv row-0 cluster
   (`mpv_button_layout`) and the transport row (`transport_zones`) are
   three different shapes: space-padded ` | ` separator + 2-tier width
   collapse + label-text-only hover (glyph/space keep base style, pinned
   cell-by-cell by `hover_highlights_only_the_label_text`) + per-button
   `●`/`⭘` pressed marker + `Rect` zones vs plain labels + 1-col gap + no
   collapse + whole-label hover + `(x1,x2)` zones vs a fixed 25-col
   centered slot row with literal pipes. Any single hover mode changes one
   side's visible behavior (whole-label hover would highlight the lyrics
   `●`; label-only hover would drop the mpv hover entirely — `⤓`/
   `[Audio]` have no space to split). Unifying needs a hover-scope enum +
   separator enum + collapse tiers + pressed-glyph callback + zone-output
   shape — a fork, not an arg.
2. **Now-playing templates — NOT merged**: controls `artist_title_line`/
   `channel_line` and the lyrics info header share only the semantics.
   Data resolution (mpv/yt/mpd tag strategy + fallbacks vs Jellyfin
   item/year-prefix/episode + yt-stream parts), styles (theme-derived
   blur-following palette vs explicit ANSI white + yellow keys + bold
   `Time:`) and layout (one centered marquee line vs fixed prefix +
   marquee window + context rows + description + credits) all differ. The
   one shared piece — the marquee cycle — is already the 5b1 widget.
3. **`ScrollingLine` (property.rs) KEPT**: its cycle is a different shape
   — continuous `|`-separated modulo repeat, whole-second progress, no
   holds, no 3× wrap — vs the carousel's hold-2s → scroll → hold-2s → 3×
   wrap with a 5-col gap. Unifying would change the property pane's
   visible scroll behavior; it stays a thin widget (single caller).

## Doc files updated

- `docs/design/Rewrite/ui-reuse-rewrite.md` — §2.4 table (marquee/wrap
  widget rows + new totals), phase-5 table row ✅, new §5.4 close-out
  (real numbers + the §3 rationales).
- `docs/design/Rewrite/phase5-drawing-widgets.md` — status flipped to
  done; new §3a decision record.
- `HANDOFF.md` — phase 5 ✅ bullet; next = 6 (args expansion).
- `docs/design/Sessions/2026-08-11.md` — Phase 5 session entry + updated
  front-matter.

## Deviations / notes

- **jellyfin was a third marquee call site** not enumerated in the plan
  text (only lyrics was listed at ~596-601/54); it is the same cross-pane
  coupling, so it was flipped to the widget too (same cycle, zero
  behavior change).
- The metrics trap hit mid-phase as predicted: `--ref HEAD` before
  committing 5b1 showed `marquee.rs: None` (file list from `git ls-tree
  HEAD`); all final numbers are on committed refs.
- marquee.rs carries private `draw_line`/`draw_spans` equivalents
  (skip+clip) because the plan's don't-drag rule keeps controls.rs's
  copies in the pane; both are outside the pane-pair analysis so the
  guardrail is unaffected.
- Host live-check (plan §8) is still pending, as in phases 1–4b.
