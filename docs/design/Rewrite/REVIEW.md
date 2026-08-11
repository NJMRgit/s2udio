---
title: "UI Reuse Rewrite — Branch-State Review"
section: rewrite
doc_type: review
id: "rewrite/review"
description: >
  Review entry point for the completed UI-reuse rewrite on branch
  `rewrite`: the full phase table with commits and real ref-to-ref
  numbers, how to review the branch (diff ranges, key diffs), the
  UNPUSHED state, the remaining host live-checks, and the known caveats.
status: "current"
updated: "2026-08-11"
related:
  - rewrite/ui-reuse
  - rewrite/phase7
tags: [rewrite, review, handoff]
---

# UI Reuse Rewrite — Branch-State Review

The rewrite is **COMPLETE** (Phases 0–7, close-out 2026-08-11). This
file is the review entry point: what landed, how to look at it, what is
still waiting on the host, and what to know before touching it.

## Branch state (UNPUSHED)

- Branch **`rewrite`** (private `NJMRgit/s2udio-working`), HEAD
  `1a163d7` + the phase-7 commits (7.1 `4512fbf`, 7.2, 7.3).
- **`origin/rewrite` is BEHIND — it still sits at `24bd883`** (the
  Phase-0 baseline commit); local `rewrite` is **ahead 38**. The whole
  rewrite is **UNPUSHED by design** (user rule: the agent never pushes;
  the host pushes separately).
- Distribution `master` is **untouched** (the rewrite lives only on
  `rewrite` of the private repo).
- Working tree clean; gate at HEAD: **1337/1337**, warnings 3 baseline,
  similarity guardrail **60 excl-thin pairs**.

## Phase table (commits + real numbers)

All LOC numbers are ref-to-ref (`ui-metrics.py --ref <boundary>`),
measured on committed refs; the boundary ref is the phase's last commit.

| Phase | Commits (on `rewrite`) | Work | Tests | src/ui | tree .rs |
| --- | --- | --- | --- | --- | --- |
| 0 | `24bd883` + `d0d3a56` | Baseline: outline, metrics script, LOC + similarity guardrail | 1312/1312 | 56,704 | 95,811 |
| 1 | `cd103ac` `cd10c75` `113d7e7` `6af6f07` | `SongListCore<T,S>` extracted; BrowserPane thin; queue audio + search adopt it | 1312/1312 | 56,525 (−179) | 95,632 (−179) |
| 2 | `f5c2ac4` `948c85c` `26e9834` `eb401ce` | `TreeBrowserCore` unifies directories/jellyfin/radio | 1312/1312 | 56,755 (+230) | 95,862 (+230) |
| 2.1 | `5b73b39` (plan) `3e667ed` | Title-spacing parity + temp-play pins | 1318/1318 | 56,923 (+168) | 96,030 (+168) |
| 3 | `a5aac04` `53b9e90` + close-outs `d3e34a4` `3ea0bba` `48004f1` `15f9707` | `ListModal`/`InfoListModal` masters; select_section merged; select/torrent/decoders thin | 1326/1326 | 56,868 (−55) | 95,926 (−104) |
| 4a | `9b46f54` | `SubTabBar` widget (queue toggle row) | 1328/1328 | 56,987 (+119) | 96,045 (+119) |
| 4b | `1867964` (plan) `5bf5a18` `80b4844` `c81cb2f` `9fee28e` | queue.rs split → `queue/{context_menus,video,chapters}.rs` (4447 → 3485, prod −962) | 1328/1328 | 57,070 (+83) | 96,128 (+83) |
| 5 | `64be282` (plan) `483a73c` `490c62e` `2fcb10c` `ef4863f` `671bc91` `ec5d553` | marquee/wrap widgets from controls/lyrics; two documented NOT-to-merge decisions | 1328/1328 | 57,173 (+103) | 96,231 (+103) |
| 6 | `5a2ce9d` (plan) `4a5b054` `a1caf6b` `9abb201` `1e2aa1d` `afd50c1` | `TreeBrowserArgs` config args (serde-defaults, backward compatible) | 1337/1337 | 57,318 (+145) | 96,797 (+566) |
| 7 | `1a163d7` (plan) + 7.1 `4512fbf` + 7.2 + 7.3 | Close-out: final metrics, docs sweep, HANDOFF/notes/REVIEW, session log | 1337/1337 | 57,318 (+0) | 96,797 (+0) |

**Totals vs baseline `24bd883`:** src/ui **+614**, tree **+986** —
the rewrite is LOC-positive overall and that is reported plainly: the
master-module pattern front-loads the shared cores (song_list +955,
tree_browser +610, list_modal +765, marquee +282, sub_tab_bar +149, the
Phase-6 serde machinery +395 in tabs.rs) while the consumers shrank
(browser 1041 → 232, queue 4686 → 3485, controls 1516 → 1319, search
−236, select/torrent/decoders −227/−360/−181). The user priority is
**extensibility + predictable behavior** (one implementation per shape);
LOC is a proxy, not a gate.

Per-phase detail and the per-file table: `ui-reuse-rewrite.md` §2.4 +
§5.1–§5.6. Guardrail history: baseline 42 excl-thin pairs (with the
final thin-adapter list; the historical 51 used the Phase-1-era list),
**60 since Phase 2, unchanged through HEAD**.

## How to review

1. **The range.** `git log --oneline master..rewrite` (78 commits — the
   full dev lineage since the repo split; the rewrite proper is the
   38-commit chain `24bd883..HEAD`; `master` is the distribution branch
   and shares no commit objects with `rewrite`, so the range includes the
   pre-rewrite dev history too). The phase-by-phase view:
   `git log --oneline --reverse 24bd883..rewrite` (38 commits, phases 0–7
   in order, each close-out commit ends its phase).
2. **The key diffs** (one implementation per shape — verify the shared
   cores exist once and the panes are thin specs):
   - `src/ui/song_list.rs` (Phase 1 core) vs the old `src/ui/browser.rs`
     (`git diff 24bd883:src/ui/browser.rs HEAD:src/ui/song_list.rs`);
   - `src/ui/tree_browser.rs` (Phase 2 core) vs the three panes'
     deleted tree/items functions (`git diff 24bd883 HEAD -- src/ui/panes/directories.rs src/ui/panes/jellyfin.rs src/ui/panes/radio.rs`);
   - `src/ui/modals/list_modal.rs` + `info_list_modal.rs` (Phase 3
     masters) vs `select_modal.rs`/`torrent_file_picker.rs`/`decoders.rs`
     thin adapters;
   - `src/ui/widgets/sub_tab_bar.rs`, `marquee.rs`, `wrap.rs` (4a/5
     widgets) and the shrunk `controls.rs`/`lyrics.rs`;
   - `src/config/tabs.rs` (Phase 6 `TreeBrowserArgs` + the manual
     `Deserialize` — the backward-compat machinery) and the four panes'
     `tree_args` hook.
3. **The close-out numbers.** Re-run the gate:
   `export PATH="$HOME/.cargo/bin:$PATH"` (RUSTUP_HOME/CARGO_HOME as in
   HANDOFF), then `cargo test --release` (**1337/1337**, warnings 3) and
   `python3 scripts/dev/ui-metrics.py --ref 24bd883` vs
   `--ref HEAD` (src/ui 56,704 → 57,318; tree 95,811 → 96,797; guardrail
   60 excl-thin at HEAD).

## Remaining host live-checks (per phase plan §8)

- **Phase 4b (queue tab)**: Audio/Video/Chapters sub-tabs, merged box,
  marks, context menus, the toggle row, scrollbars.
- **Phase 5**: controls carousel cycle (marquee holds 2s, scrolls, wraps
  with a 5-col gap), lyrics header cluster + info marquee + description
  wrap, jellyfin overview wrap, property `ScrollingLine` unchanged.
- **Phase 6**: the four browser tabs at ~70 cols (tree hidden / radio's
  30% regions tree) vs wide widths (min 50), info boxes ≈ 15 rows, a
  config override (`tree_min_width: 60` / `info_box_cap: None`) followed
  by a restart, and the round-23 config parsing with **NO edits**.

After the live-checks the host pushes `rewrite` (the agent never pushes).

## Known caveats

1. **serde `__private228` (one-line bump on serde > 1.0.228).**
   `src/config/tabs.rs` imports `serde::__private228::de::{Content, …}`
   for the manual `PaneTypeFile` Deserialize (the versioned hidden module
   serde's derive itself uses). Cargo.lock pins **serde 1.0.228**; a
   future `cargo update` past 1.0.228 must bump the suffix
   (`__private228` → the new version) — one line. Pinned by
   `bare_browser_panes_parse_with_default_tree_args` + the config/theme
   parse tests.
2. **Radio's arg-independent 30% regions tree.** RadioPane keeps its own
   always-visible 30% regions tree — its `split_tree` override never
   applies `TreeBrowserArgs::tree_min_width`/`tree_hide_below`
   (deliberate: radio's shape; the args plumb in for uniformity only).
   Pinned by `tree_args_plumb_in_but_keep_the_regions_tree_shape`.
3. **`directories::tree_width` is a test-only parity pin.**
   `directories.rs::tree_width(total)` is now a one-line wrapper over
   `TreeBrowserArgs::default().tree_width(total)`; it exists so the
   pre-Phase-6 tree-width tests (the round-8/9 width-regime pins, e.g.
   `tree_width(120) == 0`, `tree_width(200) == 60`) keep calling the
   same-named helper and pin the args default = today's constants. Do not
   delete it while those tests call it.
4. **Docs win for behavior.** The close-out is docs/metrics only — where
   a doc's wording and the code diverge, REVIEW.md/§5.x records it;
   behavior was not changed to match old docs.
