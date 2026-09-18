#!/usr/bin/env python3
"""s2u-yt probe: exit 1 if the best video+audio URLs from a yt-dlp stdout payload
answer HTTP 403 to an open-range/follow request (i.e. mpv could not play them)."""
import json, re, subprocess, sys, time

# A freshly issued googlevideo URL can answer 403 to the FIRST range request and
# 206 to the identical request seconds later (measured 2026-09-17: 403 at t~0,
# 206 at t~+8..10 s, same URL, same client). A single immediate probe therefore
# reports a false 403 and the wrapper wrongly falls back to the P3 web_safari
# HLS floor, whose ceiling is 1080p. A 403 is only fatal when a retry confirms it.
RETRY_DELAY_S = 8.0
RETRIES = 1

def probe_once(url):
    try:
        r = subprocess.run(
            ["curl", "-s", "-L", "-o", "/dev/null", "-w", "%{http_code}", "-m", "12", "-r", "0-1023", url],
            capture_output=True, text=True, timeout=25)
        return r.stdout.strip() or None
    except Exception:
        return None

def probe(url):
    if not url.startswith("http"):
        return None
    code = probe_once(url)
    for _ in range(RETRIES):
        if code != "403":
            return code
        time.sleep(RETRY_DELAY_S)
        code = probe_once(url)
    return code

def main(path):
    try:
        data = open(path, "r", errors="replace").read()
    except Exception:
        sys.exit(0)
    if "googlevideo" not in data:
        sys.exit(0)
    urls = []
    s = data.lstrip()
    if s.startswith("{"):
        try:
            j = json.loads(s)
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
        except Exception:
            pass
    if not urls:
        m = re.search(r"https?://[^\s\"']*googlevideo[^\s\"']*", data)
        if m:
            urls.append(m.group(0))
    if not urls:
        sys.exit(0)
    codes = []
    for u in urls:
        c = probe(u)
        if c:
            codes.append(c)
    if any(c == "403" for c in codes):
        sys.exit(1)
    sys.exit(0)

if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(0)
    main(sys.argv[1])
