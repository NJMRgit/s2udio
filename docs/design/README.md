---
title: "s2udio Design Documents"
section: root
doc_type: index
id: "design/index"
description: >
  Entry point for the s2udio design documentation hierarchy: Backend flow
  references, shared Frontend design constraints and per-tab specs.
status: "current"
updated: "2026-08-11"
tags: [index, design, s2udio]
---

# s2udio Design Documents

The design of s2udio (the rmpc fork) is split into sections (Backend, Frontend, Tabs,
Validation, Rewrite, Sessions). Each
document carries YAML frontmatter (`section`, `doc_type`, `id`,
`description`, `status`, `updated`, `source_files`, `related`, `tags`) so
it can be indexed, filtered and cross-referenced.

[`HANDOFF.md`](../../HANDOFF.md) is the lean operational handoff (current
state, gotchas, housekeeping rules); the authoritative behavioral spec
lives in these documents.

## Hierarchy

```
docs/design/
├── README.md                     ← this index
├── Backend/                      ← APIs, scripts, functions, per-flow references
│   ├── mpd-playback.md           ← MPD queue/playback, temp entries, MPRIS tagging
│   ├── radio-directory.md        ← radio-browser.info directory, caching, lazy loads
│   ├── jellyfin-api.md           ← Jellyfin API: auth, endpoints, parsing, reporting
│   ├── ytdlp-resolution.md       ← yt-dlp stream resolution + cache lifecycle
│   ├── stream-downloads.md       ← s2udio-downloads save-as + playlist replace
│   ├── mpv-session.md            ← mpv launch, IPC poll, reattach, MPRIS tracker
│   ├── paste-pipeline.md         ← bracketed paste → parse → popup → resolve → play
│   ├── chapters.md               ← chapter sources, keying, seek routing
│   ├── blur-theme-watcher.md     ← blur mode schedule → accent color derivation
│   ├── image-overlays.md         ← terminal-side overlays (art, poster, cava, MPRIS)
│   ├── config-sidecars.md        ← config.ron + the never-rewritten sidecars
│   └── torrent-streaming.md      ← torrent/magnet streaming plan (rqbit engine, M1–M5)
├── Frontend/                     ← shared interactive design constraints
│   ├── colors-typography.md      ← text color, accent derivation, hover colors, placement rules
│   ├── glyphs.md                 ← one-cell glyph inventory
│   ├── layout-templates.md       ← reusable layout templates
│   └── interaction.md            ← mouse + keyboard rules, hover effects, contexts
├── Tabs/                         ← one spec per tab + settings
│   ├── queue-tab.md              ← Queue (Audio / Video / Chapters)
│   ├── playlists-tab.md          ← Playlists browser
│   ├── mpd-tab.md                ← MPD (Directories) browser
│   ├── jellyfin-tab.md           ← Jellyfin browser + video playback
│   ├── radio-tab.md              ← Radio browser
│   ├── search-tab.md             ← Search (MPD tab mode, round 28)
│   └── settings.md               ← Settings panel
├── Validation/                   ← validation plans (run per subsystem round)
│   ├── mpris-validation.md       ← MPRIS & art: timeline/art/track-info across every source
│   └── distro-support.md         ← distro support: podman test harness, target matrix, gates, roadmap
├── Rewrite/                      ← the UI reuse rewrite (branch `rewrite` in s2udio-working)
│   ├── ui-reuse-rewrite.md       ← project outline: audit, master-module architecture, phases, close-outs
│   ├── REVIEW.md                 ← branch-state review: phase table, review recipe, live-checks, caveats
│   ├── new-browser-tab.md        ← construction pattern: config block + adapter, no new pane file
│   ├── phase4b-queue-decomposition.md  ← phase-4b handoff plan (queue split; done)
│   ├── phase5-drawing-widgets.md ← phase-5 handoff plan (marquee/wrap widgets; done)
│   ├── phase6-args-expansion.md  ← phase-6 handoff plan (args expansion; done)
│   └── phase7-closeout.md        ← phase-7 close-out plan (docs + metrics; done)
└── Sessions/                     ← session work logs (one file per session)
    └── 2026-08-05.md             ← the session log (create/maintain per HANDOFF)
```

## Reading order

- **New to the codebase**: start with `Backend/mpd-playback.md`, then
  `Frontend/interaction.md`, then the tab you care about.
- **Changing the UI**: read `Frontend/*` first (constraints), then the tab
  spec, then the relevant backend flow.
- **Debugging a flow**: find the flow doc (Backend) and follow its
  `source_files`; each flow references its events, channels and threads.

## Conventions

- `source_files` lists the primary implementation files (paths relative to
  the repo root).
- `related` lists sibling document ids (`backend/…`, `frontend/…`,
  `tabs/…`) to follow.
- `status`: `current` (matches the code as of `updated`) or `draft`
  (planned / partially implemented).
- The maintenance workflow (when to update a doc, the session-end
  checklist, how HANDOFF.md stays lean) lives in
  [`HANDOFF.md`](../../HANDOFF.md) → "Documentation maintenance".
