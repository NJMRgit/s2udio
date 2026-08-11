---
title: "Phase 7 — Rewrite Close-out (handoff plan)"
section: rewrite
doc_type: plan
id: "rewrite/phase7"
description: >
  Final phase of the UI-reuse rewrite: final LOC comparison vs the Phase-0
  baseline, docs/design source_files sweep, HANDOFF/notes update, and the
  rewrite branch-state writeup for review. Docs/metrics only — no behavior
  changes. Prepared for a fresh agent; parent spec is
  `docs/design/Rewrite/ui-reuse-rewrite.md` (phase-7 row, §6 DoD).
status: "done — executed 2026-08-11 (7.1 4512fbf, 7.2 77b820c, 7.3 close-out; see ui-reuse-rewrite.md §5.6)"
parent: "rewrite/ui-reuse"
updated: "2026-08-11"
---

# Phase 7 — Rewrite Close-out (handoff plan)

> **Status: DONE (2026-08-11).** Phases 0–6 were DONE on `rewrite` (HEAD `afd50c1`,
> 1337/1337, warnings 3, guardrail 60 excl-thin). This plan covers **7**
> only — the close-out. A new agent completes it; the host reviews.

## 1. Context — read this first

- **Repo**: `~/Projects/s2udio`, branch **`rewrite`** (private
  `NJMRgit/s2udio-working` only; distribution `master` untouched). Work in
  this branch; commit per sub-step. **NEVER push** — local commits and
  pulls only (user rule; the host pushes separately).
- **Environment**: Rust toolchain at `~/.cargo/bin` (only needed to re-run
  the gate — this phase changes no code).
- **Build/test loop**: `cargo test --release` — baseline **1337/1337**,
  warnings **3** (config/mod.rs unused-mut, language.rs unused ctx,
  paste.rs AddAfterCurrent). This phase must NOT change the count.
- **The rewrite**: UI-reuse consolidation (outline §1). User priority:
  extensibility + predictable behavior; LOC is a proxy, not a gate — the
  close-out records REAL ref-to-ref numbers per phase, no spin.
- **Phase-0 baseline commit**: `24bd883` (the outline's §2.4 table was
  measured there).

## 2. Goal & scope

**Goal**: close the rewrite — final numbers, truthful docs, and a
reviewable branch state.

**Deliverables** (each a commit, see §4):

1. **Final metrics** — `ui-metrics.py --ref 24bd883` vs `--ref HEAD`
   (committed refs both sides): src/ui + tree totals, per-file deltas for
   the panes/widgets/tabs.rs, and the same-named-pair guardrail count.
   Update the outline §2.4 table to its FINAL state + add a phase-by-phase
   LOC summary table (Phase 0 baseline → 1 → 2/2.1 → 3 → 4a/4b → 5 → 6 →
   HEAD) in the close-out section.
2. **docs/design `source_files` sweep** — every doc in
   `docs/design/**` whose `source_files` (or `related`) lists a path the
   rewrite moved/created: `src/ui/panes/queue.rs` (now also
   `queue/{context_menus,video,chapters}.rs`), `controls.rs`/`lyrics.rs`
   (marquee/wrap now live in `src/ui/widgets/{marquee,wrap}.rs`),
   `src/config/tabs.rs` (TreeBrowserArgs), `scripts/dev/ui-metrics.py`
   (THIN_ADAPTERS), new docs (`new-browser-tab.md`, phase plan docs).
   Grep `source_files`/`related` and fix every stale path; bump each
   doc's `updated:`.
3. **HANDOFF.md rewrite section** — final state: all phases ✅ with
   commits + real numbers, live-check status per phase (pending host
   items listed), the serde `__private228` caveat recorded, and "next" →
   nothing (rewrite complete; master untouched).
4. **notes.md** — a short "rewrite complete" block (phases, HEAD,
   live-check pending, no push rule) for the next container agent.
5. **Branch-state writeup** — `docs/design/Rewrite/REVIEW.md` (new):
   the full phase table with commits + numbers, how to review
   (`git log --oneline master..rewrite`, key diffs), what is UNPUSHED
   (origin/rewrite is behind), remaining host live-checks, and known
   caveats (serde `__private228` one-line bump; radio's arg-independent
   regions tree; `directories::tree_width` as test-only pin).
6. **Session log** — append the Phase 7 entry to
   `docs/design/Sessions/2026-08-11.md`.

**Out of scope**: code changes (any edit that changes test count or
behavior is out — stop and re-scope), feature work, distribution master,
pushes.

## 3. Rules

- **Truthfulness**: every number is measured on committed refs
  (`ui-metrics.py --ref <a>` vs `--ref <b>`) — the script reads the file
  list from `git ls-tree HEAD` and contents from the worktree without
  `--ref`; never mix.
- **No spin**: the rewrite is LOC-positive overall (+566 tree after
  Phase 6); the close-out reports that plainly with the per-phase table
  and the user-priority framing (one-implementation-per-shape bought at
  the price of args/plumbing LOC).
- **One home per fact** (HANDOFF methodology): numbers live in the
  outline §2.4/close-out; state lives in HANDOFF; narrative lives in the
  session log; REVIEW.md is the review entry point.
- **Docs win for behavior**: do not "fix" a doc by changing behavior —
  this phase changes docs only.

## 4. Method — commit per deliverable group, each green

1. **7.1** — metrics + outline final tables + close-out section.
   Commit: `phase 7.1: final LOC metrics and outline close-out tables`
2. **7.2** — source_files sweep + HANDOFF + notes + REVIEW.md.
   Commit: `phase 7.2: rewrite close-out docs, handoff, and branch-state review`
3. **7.3** — session log entry + final self-review (grep the sweep,
   re-run the gate). Commit: `phase 7.3: session log + close-out verification`

Gate after each: `cargo test --release` **1337/1337** unchanged +
warnings 3 + `ui-metrics.py --ref HEAD` guardrail 60.

## 5. Definition of done (7)

1. Test count UNCHANGED at 1337/1337 (no code edits); warnings 3.
2. Outline §2.4 final table + phase-by-phase LOC table with real
   ref-to-ref numbers; phase-7 row ✅.
3. `grep -rn "source_files" docs/design | grep -E "panes/(queue|controls|lyrics)|widgets"` shows no stale pre-rewrite paths.
4. HANDOFF rewrite section final; notes.md rewrite-complete block;
   REVIEW.md exists with the review recipe + unpushed state + caveats.
5. Session log Phase 7 entry written.
6. Commits 7.1–7.3 on `rewrite`, each independently green. **No push.**

## 6. Risks & gotchas

- **Metrics trap**: always `--ref` committed refs both sides; the
  baseline is `24bd883`, final is `HEAD`.
- **Do NOT touch code**: if the sweep suggests a doc describes behavior
  the code no longer has, record it in REVIEW.md's caveats — do not
  change the code to match an old doc in a close-out phase.
- **serde caveat**: `src/config/tabs.rs` uses `serde::__private228`
  (versioned hidden module; lockfile pins serde 1.0.228). A future
  `cargo update` past 1.0.228 needs the suffix bumped (one line) —
  record this in HANDOFF/REVIEW.md.
- **rustfmt**: docs only — no formatting concerns beyond the outline's
  tables.
- **No push**: local commits only.

## 7. Host review (after 7.3)

Read REVIEW.md; run the host live-checks for phases 4b/5/6 (queue tab;
controls/lyrics/jellyfin/property; browser-tab tree/info args + config
override + round-23 config parses) — then the rewrite is complete and
`rewrite` may be pushed by the host (user rule: agent never pushes).

## 8. References

- Parent spec: `docs/design/Rewrite/ui-reuse-rewrite.md` (phase-7 row,
  §2.4 baseline table, §6 DoD).
- Baseline commit: `24bd883`; phase close-outs: §5.1–§5.5.
- Metrics tool: `scripts/dev/ui-metrics.py`.
