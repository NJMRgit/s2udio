#!/usr/bin/env python3
"""s2u-yt media-URL probe — checks a yt-dlp stdout payload before it is handed
to a consumer (mpv, MPD, s2udio).

Usage:
    s2u-yt-probe.py <file holding yt-dlp stdout>
    s2u-yt-probe.py --self-test

Exit 1 when a candidate media URL cannot be **opened** the way the consumers
open it; exit 0 when every candidate opens (or when there is nothing to check:
no googlevideo URL, unreadable payload).

Why the open shape and not a small bounded range
------------------------------------------------
mpv, MPD and ffmpeg all open a media URL with a **plain GET** (no Range header)
and only send `Range: bytes=N-` later, for seeks. googlevideo's range
enforcement answers 403 to exactly that open while a small bounded
`Range: bytes=0-1023` still answers 206. Measured again on this machine
2026-09-22 with three fresh itag=251 (VISIONOS) audio URLs:

    -r 0-1023     403 / 403 / 206      (the old probe's shape)
    -r 0-         206 / 206 / 206      (open-ended, the whole file)
    plain GET     200 / 200 / 200      (what the players send)
    abs tail      206 / 206 / 206      (the file's end, 1 KiB)
    ffprobe       0.1 s, exit 0        (libavformat, the players' own stack)

So a bounded-range probe can miss a URL that will 403 for the player, and can
even reject a healthy one. This probe opens each candidate the way the player
does: with `ffprobe` (libavformat — the codec stack mpv and MPD decode with)
when it is installed, and with a plain `curl` GET otherwise.

A failure is retried once after RETRY_DELAY_S: a freshly issued googlevideo URL
can answer 403 to its very first request and 206 to the identical one seconds
later (measured 2026-09-17), so a single immediate failure is not fatal.
"""
import json
import re
import shutil
import subprocess
import sys
import time

# A 403 on the very first request of a fresh URL is not final (2026-09-17).
RETRY_DELAY_S = 8.0
RETRIES = 1
# The curl fallback cancels its own transfer after this long: a stream that is
# still flowing is exactly what an open looks like, so exit 28 counts as ok.
GET_TIMEOUT_S = 3
# libavformat's own read timeout for the ffprobe open (microseconds).
FFPROBE_RW_TIMEOUT_US = 8_000_000


def _run(cmd, timeout):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except Exception:  # noqa: BLE001 - a probe must never raise into the wrapper
        return None


def _has_ffprobe():
    return shutil.which("ffprobe") is not None


def _ffprobe_opens(url, run=_run):
    """True when libavformat can open the URL (the codec stack mpv/MPD use)."""
    r = run(
        [
            "ffprobe", "-v", "error",
            "-rw_timeout", str(FFPROBE_RW_TIMEOUT_US),
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
            "-i", url,
        ],
        25,
    )
    return bool(r) and r.returncode == 0 and bool(r.stdout.strip())


def _plain_get_opens(url, run=_run):
    """True when a plain GET (no Range header) answers 200/206."""
    r = run(
        [
            "curl", "-s", "-L", "-o", "/dev/null", "-w", "%{http_code}",
            "-m", str(GET_TIMEOUT_S), url,
        ],
        GET_TIMEOUT_S + 5,
    )
    return bool(r) and r.stdout.strip() in ("200", "206")


def _opens(url, run=_run, has_ffprobe=None):
    if has_ffprobe is None:
        has_ffprobe = _has_ffprobe()
    if has_ffprobe:
        return _ffprobe_opens(url, run)
    return _plain_get_opens(url, run)


def url_opens(url, run=_run, has_ffprobe=None, sleep=time.sleep):
    """Whether the consumers can open `url`; a lone 403 is retried once."""
    if not url.startswith("http"):
        return True
    for attempt in range(RETRIES + 1):
        if _opens(url, run, has_ffprobe):
            return True
        if attempt < RETRIES:
            sleep(RETRY_DELAY_S)
    return False


def candidate_urls(data):
    """The media URLs in a yt-dlp payload: the selected format plus the best
    video and the best audio URL of a multi-format (mpv) payload."""
    urls = []
    s = data.lstrip()
    if s.startswith("{"):
        try:
            j = json.loads(s)
            top = j.get("url")
            if top and "googlevideo" in top:
                urls.append(top)
            best_v, best_a, best_vh, best_aabr = None, None, -1, -1.0
            for f in j.get("formats") or []:
                u = f.get("url")
                if not u or "googlevideo" not in u:
                    continue
                if (f.get("vcodec") or "none") != "none":
                    h = f.get("height") or 0
                    if h > best_vh:
                        best_v, best_vh = u, h
                else:
                    abr = f.get("abr") or 0.0
                    if abr > best_aabr:
                        best_a, best_aabr = u, abr
            if best_v:
                urls.append(best_v)
            if best_a:
                urls.append(best_a)
        except Exception:  # noqa: BLE001
            pass
    if not urls:
        m = re.search(r"https?://[^\s\"']*googlevideo[^\s\"']*", data)
        if m:
            urls.append(m.group(0))
    # Keep the order, drop duplicates.
    seen = set()
    return [u for u in urls if not (u in seen or seen.add(u))]


def check_payload(path, run=_run, has_ffprobe=None, sleep=time.sleep):
    """0 when every candidate URL opens, 1 when one of them does not."""
    try:
        data = open(path, "r", errors="replace").read()
    except Exception:  # noqa: BLE001
        return 0
    if "googlevideo" not in data:
        return 0
    urls = candidate_urls(data)
    if not urls:
        return 0
    for url in urls:
        if not url_opens(url, run, has_ffprobe, sleep):
            return 1
    return 0


def _self_test():
    """Exercise the verdicts with a stubbed runner (no network)."""
    class R:
        def __init__(self, code=0, out="", err=""):
            self.returncode, self.stdout, self.stderr = code, out, err

    def runner(results):
        calls = []

        def run(cmd, timeout):
            calls.append(cmd[0])
            return results.pop(0) if results else R(1)
        return run, calls

    failures = []

    # A URL that opens: one ffprobe call with a duration, no retry.
    run, calls = runner([R(0, "123.4\n")])
    if not url_opens("https://x.example/a", run, True, lambda _: None) or len(calls) != 1:
        failures.append("healthy URL should open on the first attempt")

    # Dead open: retried once, then reported dead.
    slept = []
    run, calls = runner([R(1), R(1)])
    if url_opens("https://x.example/b", run, True, slept.append) or len(calls) != 2 or slept != [RETRY_DELAY_S]:
        failures.append("dead URL should be retried once and then fail")

    # A first 403 that clears on the retry is not fatal.
    run, calls = runner([R(1), R(0, "12.0\n")])
    if not url_opens("https://x.example/c", run, True, lambda _: None) or len(calls) != 2:
        failures.append("a transient first failure should clear on the retry")

    # Without ffprobe the curl fallback decides.
    run, calls = runner([R(0, "200")])
    if not url_opens("https://x.example/d", run, False, lambda _: None) or calls != ["curl"]:
        failures.append("the curl fallback should carry the check without ffprobe")
    run, _ = runner([R(0, "403"), R(0, "403")])
    if url_opens("https://x.example/e", run, False, lambda _: None):
        failures.append("a plain GET answered 403 must be fatal")

    # Payload handling: no googlevideo means nothing to check.
    import tempfile
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as fh:
        fh.write('{"url": "https://cdn.soundcloud.example/x"}')
        plain = fh.name
    if check_payload(plain, runner([R(0, "1\n")])[0], True, lambda _: None) != 0:
        failures.append("a payload without googlevideo URLs must pass")

    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as fh:
        fh.write(json.dumps({"url": "https://rr1.googlevideo.example/videoplayback?itag=251"}))
        one = fh.name
    run, calls = runner([R(1), R(1)])
    if check_payload(one, run, True, lambda _: None) != 1 or len(calls) != 2:
        failures.append("a dead selected-format URL must fail the payload")

    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    print("s2u-yt-probe self-test: all checks passed")
    return 0


if __name__ == "__main__":
    if len(sys.argv) == 2 and sys.argv[1] == "--self-test":
        sys.exit(_self_test())
    if len(sys.argv) != 2:
        sys.exit(0)
    sys.exit(check_payload(sys.argv[1]))
