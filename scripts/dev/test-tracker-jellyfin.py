#!/usr/bin/env python3
"""Test the caretaker tracker's Jellyfin playback reporting.

The tracker is embedded in `scripts/s2u-helper` (regenerate notes live in
`scripts/dev/build-helper.py`), so this test extracts the `tracker` source
from it, runs the real `main()` loop against a scripted fake mpv, and
records every HTTP call it would make.

It pins the episode-tracking behaviour:

- `Sessions/Playing` is reported once per item (the server needs the
  session start);
- progress is only reported once past the server's resume threshold (a
  report below it is saved as position 0 and wipes the saved position);
- when mpv advances to the next episode, the episode left behind gets its
  own `Sessions/Playing/Stopped` at its last position — a season watched
  through the caretaker marks every finished episode watched;
- the final stop uses the last position seen, never a bare 0;
- the resume position is read from the item DTO's `UserData` (Jellyfin has
  no `UserItems/{id}/UserData` route: that one 404s).

Usage: python3 scripts/dev/test-tracker-jellyfin.py
"""
import ast
import io
import json
import pathlib
import sys
import tokenize

REPO = pathlib.Path(__file__).resolve().parents[2]
HELPER = REPO / "scripts" / "s2u-helper"

ITEM_A = "a" * 32
ITEM_B = "b" * 32
URL_A = "http://jf:8096/Videos/%s/stream?static=true" % ITEM_A
URL_B = "http://jf:8096/Videos/%s/stream?static=true" % ITEM_B


class StopLoop(Exception):
    pass


def tracker_source() -> str:
    """The tracker program embedded in scripts/s2u-helper."""
    text = HELPER.read_text()
    tree = ast.parse(text)
    for node in tree.body:
        if isinstance(node, ast.Assign) and getattr(node.targets[0], "id", "") == "_SOURCES":
            return ast.literal_eval(node.value)["tracker"]
    raise SystemExit("scripts/s2u-helper has no _SOURCES dict")


def mpv_state(path, position, paused, duration, playlist_pos, count):
    return {
        "position": position,
        "paused": paused,
        "duration": duration,
        "volume": 80,
        "playlist_pos": playlist_pos,
        "playlist_count": count,
        "media_title": "Episode",
        "path": path,
        "playlist": [{"filename": path, "title": "Episode"}],
    }


def run_tracker(states):
    """Exec the embedded tracker with a scripted mpv; return (posts, gets)."""
    namespace = {"__name__": "tracker_under_test"}
    exec(compile(tracker_source(), "tracker", "exec"), namespace)

    posts = []
    gets = []
    ticks = {"n": 0}
    clock = {"t": 1000.0}

    def next_state(_socket):
        if ticks["n"] >= len(states):
            raise StopLoop()
        state = states[ticks["n"]]
        ticks["n"] += 1
        return state

    def fake_time():
        clock["t"] += 11.0
        return clock["t"]

    def fake_post(_jf, url, body):
        posts.append((url, body))

    def fake_get(_jf, url, timeout=6):
        gets.append(url)
        # The item DTO with UserData (the resume-position source).
        return json.dumps({
            "Id": ITEM_A,
            "Name": "Legendary Class",
            "Type": "Episode",
            "SeriesName": "Overgeared",
            "UserData": {"PlaybackPositionTicks": 5020 * 10_000_000},
        }).encode()

    namespace.update({
        "load_jellyfin": lambda: ("http://jf:8096", "token", "user1"),
        "claim_pid": lambda: True,
        "release_pid": lambda: None,
        "spawn_mpris_bridge": lambda: None,
        "mpdris2": lambda _action: None,
        "mpd_state": lambda: None,
        "s2u_running": lambda: False,
        # The socket disappears once the scripted ticks ran out, which is
        # how `main()` leaves its loop and reaches the final stop report.
        "mpv_socket": lambda: ("/nonexistent-mpv-socket"
                               if ticks["n"] < len(states) else None),
        "read_mpv_state": next_state,
        "mpv_command": lambda _socket, _command: None,
        "write_state": lambda _state: None,
        "delete_state": lambda: None,
        "load_yt_info": lambda _url: None,
        "display_title": lambda url: url,
        "jf_get": fake_get,
        "jf_post": fake_post,
        "log": lambda _message: None,
        "time": type("T", (), {"time": staticmethod(fake_time),
                               "sleep": staticmethod(lambda _s: None)}),
    })
    try:
        namespace["main"]()
    except StopLoop:
        pass
    return posts, gets


def main() -> int:
    # Tick 1: the first item barely played (10 s — below the report
    # threshold, no progress report, but the session start is sent).
    # Tick 2: mid-episode (700 s) — a progress report.
    # Tick 3: mpv advanced to the next episode: item A is finalized at 700 s.
    # Tick 4: the new episode plays to 800 s, then the session ends.
    states = [
        mpv_state(URL_A, 10.0, False, 1000.0, 0, 2),
        mpv_state(URL_A, 700.0, False, 1000.0, 0, 2),
        mpv_state(URL_B, 5.0, False, 900.0, 1, 2),
        mpv_state(URL_B, 800.0, False, 900.0, 1, 2),
    ]
    posts, gets = run_tracker(states)

    def by_kind(item):
        return [body for url, body in posts if url.endswith(item)]

    starts = by_kind("/Sessions/Playing")
    progress = by_kind("/Sessions/Playing/Progress")
    stops = by_kind("/Sessions/Playing/Stopped")

    failures = []

    def check(condition, message):
        if not condition:
            failures.append(message)

    check(any(isinstance(g, str) and "/Users/user1/Items/" + ITEM_A in g for g in gets),
          "the resume position is read from the item DTO: %r" % (gets,))
    check(all("UserItems/" not in (g or "") for g in gets),
          "no request goes to the 404ing UserItems/.../UserData route: %r" % (gets,))
    check(len(starts) == 2, "one session start per item: %r" % (starts,))
    check(ITEM_A in [s.get("ItemId") for s in starts], "item A reported started")
    check(ITEM_B in [s.get("ItemId") for s in starts], "item B reported started")
    check(all(s.get("MediaSourceId") == s.get("ItemId") for s in starts),
          "starts carry MediaSourceId")
    check(all(s.get("PlaySessionId") for s in starts),
          "starts carry a PlaySessionId")
    check(any(p["PositionTicks"] == 700 * 10_000_000 for p in progress),
          "the mid-episode tick reports its position: %r" % (progress,))
    check(any(p["PositionTicks"] == 800 * 10_000_000 for p in progress),
          "the second episode's tick reports its position: %r" % (progress,))
    # The first tick (10 s) is below the server's resume threshold, so the
    # only positions reported are the resumed/advanced ones.
    check(all(p.get("PositionTicks", 0) >= 60 * 10_000_000 for p in progress),
          "no progress report below the resume threshold: %r" % (progress,))
    a_stops = [s for s in stops if s.get("ItemId") == ITEM_A]
    b_stops = [s for s in stops if s.get("ItemId") == ITEM_B]
    check(len(a_stops) == 1, "the finished item A got its own stop: %r" % (stops,))
    check(a_stops and a_stops[0]["PositionTicks"] == 700 * 10_000_000,
          "item A's stop carries its last position: %r" % (a_stops,))
    check(len(b_stops) == 1, "item B's exit stop: %r" % (stops,))
    check(b_stops and b_stops[0]["PositionTicks"] == 800 * 10_000_000,
          "item B's stop carries its last position, not 0: %r" % (b_stops,))
    check(all(s.get("Failed") is False for s in stops), "stops say Failed=false")

    if failures:
        print("FAIL")
        for failure in failures:
            print(" -", failure)
        return 1
    print("PASS: tracker Jellyfin reporting")
    return 0


if __name__ == "__main__":
    sys.exit(main())
