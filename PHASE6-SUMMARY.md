# Phase 6 — Args Expansion: Summary

Branch `rewrite` (private NJMRgit/s2udio-working). **No push** (user rule).
Working tree clean; all four commits local on `rewrite`.

## Commits (each independently green)

| Commit | Phase | Contents |
| --- | --- | --- |
| `4a5b054` | 6.1 | `TreeBrowserArgs` (serde + explicit Default: `tree_min_width: 50`, `tree_hide_below: 120`, `info_box_cap: Some(15)`; +`Hash` because `PaneType` derives Hash) and the four variant fields on BOTH `PaneTypeFile` and `PaneType`; manual `Deserialize` for `PaneTypeFile` (backward compat — see below); TryFrom conversion + every exhaustive match the compiler flagged (`{ .. }`; args carried at construction); tests: bare-syntax parse, explicit-args round-trip, defaults = today's constants |
| `a1caf6b` | 6.2 | `TreeBrowserArgs::tree_width`/`info_box_height` (defaults = today's `directories::tree_width()` / `.min(15)`), `Config::tree_browser_args` (first config occurrence drives the singleton pane; absent → defaults), `tree_args` hook on `TreeBrowserCore` feeding the shared `split_tree`; directories/playlists/jellyfin/radio read the args (playlists in its inline render; radio keeps its always-visible 30% regions tree — its shape); tree-width + info-cap parity tests green UNCHANGED; one new test per pane |
| `9abb201` | 6.3 | Construction pattern — **documented decision**: recipe `docs/design/Rewrite/new-browser-tab.md` (config block + thin adapter, never a new core) + config→args bridge test; `tree_args` added to the metrics script's thin-adapter list |
| `1e2aa1d` | 6.4 | Close-out: docs + metrics (outline §2.4/phase-row/§5.5, HANDOFF, plan flipped done, session log, design index) |

## Gate (final, and after each commit)

- **Tests: 1337/1337** (baseline 1328 + 9 new: 3 in 6.1, 5 in 6.2, 1 in 6.3). Zero existing assertions edited; the tree-width + `info_box_height_cap` parity tests are byte-unchanged.
- **Warnings: 3 baseline unchanged** (`config/mod.rs` unused-mut, `language.rs` unused ctx, `paste.rs` `AddAfterCurrent`).
- **Similarity guardrail: 60 excl-thin pairs before and after** (identical pair set; thin-adapter count 140 → 143 — the three `tree_args` overrides are thin accessors, the same category as the Phase-1/2 hook list).

## LOC (ref-to-ref, `ui-metrics.py --ref 5a2ce9d` vs `--ref HEAD`)

- `src/config/tabs.rs` 1277 → 1672 (+395: args struct + manual Deserialize machinery + derived mirror + tests)
- `src/config/mod.rs` 1050 → 1076 (+26: `Config::tree_browser_args`)
- `src/ui/tree_browser.rs` 602 → 610 (+8: `tree_args` hook)
- `directories.rs` 2121 → 2155, `jellyfin.rs` 2703 → 2736, `radio.rs` 2431 → 2467, `playlists.rs` 1755 → 1759, `playlists/tests.rs` 1524 → 1554
- Match-churn files (panes/mod.rs, tab_help.rs, ui/mod.rs, paste.rs, event_loop.rs, work.rs): line-count-neutral
- **src/ui +145** (57,173 → 57,318); **tree total .rs +566** (96,231 → 96,797) — the phase front-loads the args/plumbing side; the serde machinery is the price of the load-bearing backward-compat guarantee. Consumer paydown is the phase-7 comparison.

## Backward compatibility (load-bearing — PINNED FIRST in 6.1)

The plan's §3 claim that `#[serde(default)]` + `Default` makes bare
`Directories` parse "for free" is **not true for RON**: a struct variant
cannot be deserialized from its unit form (verified with a minimal repro —
`ExpectedStructLike`). Since the HARD RULES make bare-syntax parsing
mandatory, `PaneTypeFile`'s `Deserialize` is manual:

1. capture the value with serde's `Content` (`serde::__private228`, the
   versioned hidden module serde's own derive uses; the lockfile pins
   1.0.228 — see deviations);
2. bare unit names (`Directories` …) dispatch to the variant with
   `TreeBrowserArgs::default()` (round-23 syntax);
3. `{Variant: ()}` → `{Variant: Seq([])}` rewrite (RON's value capture
   encodes every parenthesized variant as a map, while serde's
   tuple-variant replay needs a sequence — `Empty()`, `InputBuffer()` …);
4. replay into a derived mirror enum (`PaneTypeFileArgs`, same shapes with
   `#[serde(default)] tree`) and convert.

**Backward-compat test names**: `bare_browser_panes_parse_with_default_tree_args`
(all four panes), `explicit_tree_args_round_trip`,
`default_args_are_today_s_constants`, plus `config::tests::*`
(example config/theme with `Pane(Radio)`, `Pane(Empty())`,
`Property(Status(InputBuffer()))` … parse unchanged).

## 6.3 decision + rationale (Phase-5 §3 rule)

**Documented decision — do NOT unify the four panes into one config-driven
backend enum.** The exit criterion "new browser tab = config block +
adapter, no new pane file" is met by construction (recipe in
`docs/design/Rewrite/new-browser-tab.md`): a new backend implements the
~15 `TreeBrowserCore` hooks / 4 `BrowserPane` hooks + data fetch, adds a
`PaneType` block with `tree: TreeBrowserArgs`, and the shared core
provides render/mouse/actions. Unifying the four panes was attempted and
rejected because it would change observable behavior in four pinned
places — radio's focus-aware back-out + always-visible 30% regions tree,
jellyfin's shared tree/items selection + poster overlay + season
expansion, playlists' list-shaped left pane (`BrowserPane`, ♪/▶ kind
prefixes), directories' disk-backed never-cached Downloads folder — a
fork, not an arg.

## Doc files updated (6.4)

- `docs/design/Rewrite/ui-reuse-rewrite.md` — §2.4 table + totals, phase-6 row ✅, §5.5 close-out (real numbers + 6.3 rationale + the backward-compat mechanism), `phase_status`/`updated`
- `docs/design/Rewrite/phase6-args-expansion.md` — status flipped to done
- `docs/design/Rewrite/new-browser-tab.md` — new recipe doc (6.3)
- `docs/design/README.md` — Rewrite/ index entries
- `HANDOFF.md` — phase-6 status bullet; next = 7 close-out (+ pending host live-checks)
- `docs/design/Sessions/2026-08-11.md` — Phase 6 entry appended

## Deviations / notes

1. **Backward-compat mechanism** (above): the plan's "free" serde-defaults
   claim was wrong; the manual `Deserialize` is the fix that keeps BOTH
   the §3 config shape AND the hard backward-compat rule. Documented in
   outline §5.5 + session log.
2. **`serde::__private228` hidden-module dependency**: the versioned path
   includes serde's patch version (228). The Cargo.toml requirement is
   caret (`1.0.228`) but the lockfile pins 1.0.228; a future
   `cargo update` to serde 1.0.229+ would need the suffix bumped to
   `__private229` (one line in tabs.rs).
3. **Radio's per-pane test**: the plan's example ("a non-default arg
   changes the layout") does not apply to radio by design — radio's
   `split_tree` is deliberately arg-independent (always-visible 30%
   regions tree, pinned by existing behavior). The radio test documents
   that the args plumb in while the regions tree keeps its shape.
4. **Metrics script**: `tree_args` added to `THIN_ADAPTERS` (same
   category as the existing hook accessors); without it the guardrail
   would have shown 63 — now 60, identical pair set.
5. `directories::tree_width` (the free fn) is now a test-only parity pin
   (`#[cfg_attr(not(test), allow(dead_code))]`); production callers read
   the args directly.
6. The `{Variant: ()}` → `{Variant: Seq([])}` rewrite (step 3 above) is a
   generic rule: in RON's Content capture, a single-entry map whose value
   is the unit can only arise from a zero-length tuple variant.

## Remaining

Phase-6 host live-check (plan §8 — the four browser tabs at ~70 vs wide
widths, info boxes ≈ 15 rows, a config override `tree_min_width: 60` /
`info_box_cap: None` + restart, round-23 config needing NO edits), then
Phase 7 (close-out).
