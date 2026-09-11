#!/usr/bin/env python3
"""s2u-yt probe: exit 1 if the best video+audio URLs from a yt-dlp stdout payload
answer HTTP 403 to an open-range/follow request (i.e. mpv could not play them)."""
import json, re, subprocess, sys

def probe(url):
    if not url.startswith("http"):
        return None
    try:
        r = subprocess.run(
            ["curl", "-s", "-L", "-o", "/dev/null", "-w", "%{http_code}", "-m", "12", "-r", "0-1023", url],
            capture_output=True, text=True, timeout=25)
        return r.stdout.strip() or None
    except Exception:
        return None

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
