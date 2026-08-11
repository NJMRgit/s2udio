#!/usr/bin/env python3
"""ui-metrics.py — LOC + same-named-function similarity metrics for s2udio's UI.

Phase-0 guardrail tool for the UI reuse rewrite (docs/design/Rewrite/ui-reuse-rewrite.md).

Metrics
-------
1. LOC: physical source lines per .rs file / directory (blanks and comments counted,
   matching the outline's §2 numbers).
2. Same-named-function similarity: for every pair of files under src/ui that both
   define a function with the same name, extract the function bodies (comment-
   stripped, balanced-brace delimited), tokenize (identifiers / numbers / single
   punctuation chars), and compute difflib.SequenceMatcher.ratio() on the token
   sequences. Reports pairs with ratio > 0.5 (the per-phase DoD guardrail).

Note: numbers in the Phase-0 baseline table were measured with this script at
commit 24bd883 (branch rewrite). Earlier ad-hoc numbers in the outline §2.2 were
measured on a pre-rounds-24-27 tree with a slightly different method; the script
is now the single source of truth for the guardrail.

Usage
-----
  python3 scripts/dev/ui-metrics.py            # LOC table + similarity pairs > 0.5
  python3 scripts/dev/ui-metrics.py --json     # machine-readable (baseline diffing)
  python3 scripts/dev/ui-metrics.py --pairs    # every same-named pair, any ratio
  python3 scripts/dev/ui-metrics.py --ref HEAD # read files from a git ref instead
                                               #   of the working tree
"""

import argparse, difflib, json, os, re, subprocess, sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# Methods of the shared SongListCore / BrowserPane / TreeBrowserCore
# traits: same-named pairs of these are the intended thin adapters /
# shared call sites, not duplication (Phase 1+ consolidation). Excluded
# from the > 0.5 guardrail report; --pairs still shows them.
THIN_ADAPTERS = {
    "list", "list_mut", "open", "leave", "enqueue", "fetch_data",
    "stack", "stack_mut", "browser_areas",
    "fetch_data_internal", "initial_playlist_name", "scrollbar_area",
    "list_area", "list_songs_in_item", "song_format", "show_info",
    "delete", "can_rename", "rename", "move_selected", "items",
    "delete_items", "enqueue_items", "handle_common_action",
    "handle_claimed_common_action", "handle_global_action",
    "handle_insert_mode", "handle_scrollbar_interaction",
    "handle_list_mouse_action", "handle_mouse_action",
    "handle_stack_mouse_action", "open_context_menu",
    # Phase-2 TreeBrowserCore hooks + accessors (directories/jellyfin/radio
    # tree+items browsers): the shared tree/items/mouse/action machinery and
    # the per-pane hooks that implement it.
    "tree_rows", "tree_selected", "tree_list", "tree_list_mut",
    "tree_area", "set_tree_area", "highlight_tree_node", "set_expanded",
    "set_expanded_idx", "items_len", "items_list", "items_list_mut",
    "items_area", "set_items_area", "item_at", "item_row",
    "item_row_height", "selected_item", "select_items_item",
    "sync_tree_to_items_cursor", "select_parent", "activate_selected",
    "render_info", "temp_play_id", "set_temp_play_id", "tree_title",
    "tree_arrow", "items_title", "items_highlight", "tree_highlight",
    "tips_lines", "tips_area", "split_tree", "layout_vertical",
    "on_items_cursor_moved", "on_tree_focus", "on_items_focus",
    "on_reconnected", "on_tree_hidden", "tree_context_menu",
    "on_tree_double_click", "handle_items_left_click", "on_confirm",
    "on_select_range", "on_close", "move_tree", "move_items",
    "render_tree", "render_items", "render_tips", "cleanup_temp_play",
    # Phase-6 TreeBrowserCore hook: the panes' tree-browser layout args
    # (thin accessors over the pane's configured TreeBrowserArgs).
    "tree_args",
}

PANE_FILES = [
    "src/ui/browser.rs",
    "src/ui/panes/mod.rs",
    "src/ui/panes/queue.rs",
    "src/ui/panes/directories.rs",
    "src/ui/panes/jellyfin.rs",
    "src/ui/panes/radio.rs",
    "src/ui/panes/search/mod.rs",
    "src/ui/panes/playlists.rs",
    "src/ui/panes/tag_browser.rs",
    "src/ui/panes/albums.rs",
    "src/ui/panes/controls.rs",
    "src/ui/panes/lyrics.rs",
]


def read_file(path, ref=None):
    if ref:
        out = subprocess.run(["git", "-C", REPO, "show", f"{ref}:{path}"],
                             capture_output=True, text=True)
        if out.returncode != 0:
            return None
        return out.stdout
    p = os.path.join(REPO, path)
    if not os.path.exists(p):
        return None
    with open(p, encoding="utf-8") as fh:
        return fh.read()


def strip_comments(src):
    src = re.sub(r"/\*.*?\*/", "", src, flags=re.S)
    src = re.sub(r"//[^\n]*", "", src)
    return src


def extract_fns(src):
    """name -> list of bodies (comment-stripped, balanced-brace delimited)."""
    src = strip_comments(src)
    fns = {}
    for m in re.finditer(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(", src):
        name = m.group(1)
        i = src.find("{", m.start())
        if i == -1:
            continue
        depth, j = 0, i
        while j < len(src):
            if src[j] == "{":
                depth += 1
            elif src[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        fns.setdefault(name, []).append(src[i : j + 1])
    return fns


def tokens(body):
    return re.findall(r"[A-Za-z_][A-Za-z0-9_]*|[0-9]+|.", body)


def similarity(a, b):
    return difflib.SequenceMatcher(None, tokens(a), tokens(b)).ratio()


def loc_table(ref=None):
    out = subprocess.run(["git", "-C", REPO, "ls-tree", "-r", "--name-only",
                          ref or "HEAD", "--", "src"], capture_output=True, text=True)
    rows = []
    for path in out.stdout.splitlines():
        if not path.endswith(".rs"):
            continue
        src = read_file(path, ref)
        if src is None:
            continue
        rows.append((path, len(src.splitlines())))
    return rows


def pane_similarity(ref=None):
    fns = {}
    for path in PANE_FILES:
        src = read_file(path, ref)
        if src is None:
            continue
        fns[path] = extract_fns(src)
    pairs = []
    files = list(fns)
    for i, fa in enumerate(files):
        for fb in files[i + 1 :]:
            for name, bodies_a in fns[fa].items():
                if name not in fns[fb]:
                    continue
                for ba in bodies_a:
                    for bb in fns[fb][name]:
                        r = similarity(ba, bb)
                        pairs.append((r, name, fa, fb, len(ba), len(bb)))
    pairs.sort(reverse=True)
    return pairs


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--pairs", action="store_true")
    ap.add_argument("--ref", default=None)
    args = ap.parse_args()

    rows = loc_table(args.ref)
    total = sum(n for _, n in rows)
    ui_rows = [(p, n) for p, n in rows if p.startswith("src/ui/")]
    ui_total = sum(n for _, n in ui_rows)
    pairs = pane_similarity(args.ref)
    over_all = [(r, n, a, b, la, lb) for r, n, a, b, la, lb in pairs if r > 0.5]
    over = [p for p in over_all if p[1] not in THIN_ADAPTERS]

    if args.json:
        print(json.dumps({
            "ref": args.ref or "worktree",
            "total_rs_loc": total,
            "ui_rs_loc": ui_total,
            "loc": {p: n for p, n in rows},
            "same_named_pairs": [{"ratio": round(r, 4), "fn": n, "a": a, "b": b,
                                  "a_lines": la, "b_lines": lb}
                                 for r, n, a, b, la, lb in pairs],
            "over_0_5": [{"ratio": round(r, 4), "fn": n, "a": a, "b": b}
                         for r, n, a, b, _, _ in over],
            "over_0_5_incl_thin": len(over_all),
            "over_0_5_excl_thin": len(over),
        }, indent=2))
        return

    print(f"LOC  (ref={args.ref or 'worktree'}):  src/ui {ui_total}  of  total {total} .rs")
    for p, n in sorted(ui_rows):
        print(f"  {n:6d}  {p}")
    if args.pairs:
        print("\nSame-named function pairs (all):")
        for r, n, a, b, la, lb in pairs:
            flag = "  <-- OVER 0.5" if r > 0.5 else ""
            print(f"  {r:.2f}  {n:24s} {a} <-> {b}  ({la}/{lb} lines){flag}")
    else:
        print(f"\nSame-named function pairs with ratio > 0.5 ({len(over)} "
              f"excluding {len(over_all) - len(over)} thin-adapter pairs):")
        for r, n, a, b, la, lb in over:
            print(f"  {r:.2f}  {n:24s} {a} <-> {b}  ({la}/{lb} lines)")


if __name__ == "__main__":
    main()
