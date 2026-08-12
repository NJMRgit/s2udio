#!/usr/bin/env python3
"""Integration test for the s2u-mpv-tracker mutual exclusion.

Runs the real tracker (installed at ~/.local/bin/s2u-mpv-tracker) in
caretaker mode against a fake mpv IPC server and a fake MPD server, then
drives playback-state transitions and asserts the pause commands.
"""
import json, os, socket, subprocess, sys, tempfile, threading, time

TRACKER = os.path.expanduser("~/.local/bin/s2u-mpv-tracker")

class FakeMpv:
    """Serves get_property commands; records pause commands."""
    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.paused = False
        self.pause_commands = []
        self.start()

    def _conn(self, s):
        f = s.makefile("rb")
        while True:
            line = f.readline()
            if not line:
                break
            try:
                cmd = json.loads(line)["command"]
            except Exception:
                continue
            if cmd[0] == "get_property":
                prop = cmd[1]
                with self.lock:
                    if prop == "pause":
                        data = self.paused
                    elif prop == "time-pos":
                        data = 12.0
                    elif prop == "duration":
                        data = 600.0
                    elif prop == "volume":
                        data = 71
                    elif prop == "playlist-pos":
                        data = 0
                    elif prop == "playlist-count":
                        data = 1
                    elif prop == "media-title":
                        data = "Test Video"
                    elif prop == "path":
                        data = "/tmp/test.mp4"
                    else:
                        data = []
                resp = json.dumps({"error": "success", "data": data}) + "\n"
            elif cmd[0] == "set_property" and cmd[1] == "pause":
                with self.lock:
                    self.paused = bool(cmd[2])
                    self.pause_commands.append(self.paused)
                continue  # fire-and-forget: no response expected
            else:
                continue
            try:
                s.sendall(resp.encode())
            except OSError:
                break
        s.close()

    def start(self):
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen(8)
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while True:
            try:
                s, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self._conn, args=(s,), daemon=True).start()

    def set_paused(self, paused):
        with self.lock:
            self.paused = paused
            self.pause_commands.clear()

class FakeMpd:
    """Serves `status`; records `pause 1` commands."""
    def __init__(self, host, port):
        self.host, self.port = host, port
        self.lock = threading.Lock()
        self.state = "pause"
        self.pause_cmds = []
        self.start()

    def start(self):
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind((self.host, self.port))
        self.listener.listen(8)
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while True:
            try:
                s, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self._conn, args=(s,), daemon=True).start()

    def _conn(self, s):
        f = s.makefile("rb")
        try:
            s.sendall(b"OK MPD 0.23.0\n")
        except OSError:
            return
        while True:
            line = f.readline()
            if not line:
                break
            if line == b"status\n":
                with self.lock:
                    body = f"state: {self.state}\n"
            elif line.startswith(b"pause "):
                with self.lock:
                    self.pause_cmds.append(line.decode().strip())
                body = ""
            else:
                body = ""
            try:
                s.sendall((body + "OK\n").encode())
            except OSError:
                break
        s.close()

    def set_state(self, state):
        with self.lock:
            self.state = state

    def pause_count(self):
        with self.lock:
            return len(self.pause_cmds)

_port_counter = [0]

def run_scenario(name, driver, timeout=12):
    global _port_counter
    _port_counter[0] += 1
    tmp = tempfile.mkdtemp(prefix="tracker-")
    mpv_sock = os.path.join(tmp, "mpv.sock")
    mpd_port = 17000 + (os.getpid() % 500) + _port_counter[0] * 10
    mpv = FakeMpv(mpv_sock)
    mpd = FakeMpd("127.0.0.1", mpd_port)
    env = dict(os.environ,
        S2U_FORCE_CARETAKER="1",
        S2U_MPV_SOCKET=mpv_sock,
        S2U_MPD_HOST="127.0.0.1",
        S2U_MPD_PORT=str(mpd_port),
        S2U_CACHE_DIR=tmp,
        S2U_JELLYFIN_CONFIG=os.path.join(tmp, "no-jellyfin.ron"),
        S2U_POLL_S="0.3",
    )
    proc = subprocess.Popen([sys.executable, TRACKER], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        ok, msg = driver(mpv, mpd)
    finally:
        proc.terminate()
        proc.wait(timeout=5)
        try:
            mpv.listener.close()
        except OSError:
            pass
        try:
            mpd.listener.close()
        except OSError:
            pass
        import shutil
        shutil.rmtree(tmp, ignore_errors=True)
    status = "PASS" if ok else "FAIL"
    print(f"[{status}] {name}: {msg}")
    return ok

def wait_for(cond, timeout=8):
    end = time.time() + timeout
    while time.time() < end:
        if cond():
            return True
        time.sleep(0.2)
    return False

results = []

# Scenario A: video playing + MPD paused -> MPD starts -> tracker pauses mpv
def a(mpv, mpd):
    mpv.set_paused(False); mpd.set_state("pause")
    if not wait_for(lambda: True, 1): pass
    time.sleep(1.0)  # let the tracker arm its latches
    mpd.set_state("play")
    if not wait_for(lambda: len(mpv.pause_commands) > 0, 8):
        return False, "mpv was not paused after MPD started"
    return True, f"mpv paused {len(mpv.pause_commands)}x after MPD started"
results.append(run_scenario("A: MPD start pauses the video", a))

# Scenario B: MPD playing + video paused -> video resumes -> tracker pauses MPD
def b(mpv, mpd):
    mpv.set_paused(True); mpd.set_state("play")
    time.sleep(1.0)  # let the tracker arm its latches
    mpv.set_paused(False)
    if not wait_for(lambda: mpd.pause_count() > 0, 8):
        return False, "MPD was not paused after the video resumed"
    return True, "MPD paused after the video resumed"
results.append(run_scenario("B: video resume pauses the music", b))

# Scenario C: both playing at takeover -> mpv paused once (restore)
def c(mpv, mpd):
    mpv.set_paused(False); mpd.set_state("play")
    # arming tick itself restores the invariant
    if not wait_for(lambda: len(mpv.pause_commands) > 0, 8):
        return False, "mpv was not paused at takeover restore"
    return True, "invariant restored at takeover (mpv paused)"
results.append(run_scenario("C: takeover restore pauses the video", c))

# Scenario D: video paused + MPD paused -> MPD starts -> mpv must NOT be paused
def d(mpv, mpd):
    mpv.set_paused(True); mpd.set_state("pause")
    time.sleep(1.0)
    mpd.set_state("play")
    time.sleep(1.5)
    if len(mpv.pause_commands) > 0:
        return False, "mpv was paused even though it was already paused"
    return True, "already-paused video left alone when music starts"
results.append(run_scenario("D: paused video untouched by MPD start", d))

# ---- regression: state file lifecycle -----------------------------------
def test_state_file_lifecycle():
    tmp = tempfile.mkdtemp(prefix="tracker-")
    mpv_sock = os.path.join(tmp, "mpv.sock")
    mpv = FakeMpv(mpv_sock)
    env = dict(os.environ, S2U_FORCE_CARETAKER="1", S2U_MPV_SOCKET=mpv_sock,
               S2U_MPD_HOST="127.0.0.1", S2U_MPD_PORT="1", S2U_CACHE_DIR=tmp,
               S2U_JELLYFIN_CONFIG=os.path.join(tmp, "none.ron"),
               S2U_POLL_S="0.3")
    proc = subprocess.Popen([sys.executable, TRACKER], env=env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ok = True
    try:
        state = os.path.join(tmp, "mpv-mpris.json")
        if not wait_for(lambda: os.path.exists(state), 8):
            print("[FAIL] state file never written"); ok = False
        else:
            doc = json.load(open(state))
            if doc["title"] != "Test Video" or doc["position"] != 12.0:
                print(f"[FAIL] state file content wrong: {doc}"); ok = False
            else:
                print("[PASS] state file written with mpv's live state")
        mpv.listener.close()
        try:
            socket.socket(socket.AF_UNIX, socket.SOCK_STREAM).connect(mpv_sock)
        except OSError:
            pass
        proc.wait(timeout=10)
        if proc.returncode != 0:
            print(f"[FAIL] tracker exited {proc.returncode}, expected 0"); ok = False
        elif os.path.exists(state):
            print("[FAIL] state file not deleted on mpv exit"); ok = False
        else:
            print("[PASS] tracker exited and deleted the state file")
    finally:
        proc.terminate(); proc.wait(timeout=5)
        import shutil
        shutil.rmtree(tmp, ignore_errors=True)
    return ok

def _test_unit_helpers():
    """Direct unit tests for the provisional-title / yt-info fallback
    helpers, importing the working-tree script (not the installed copy)."""
    ok = True
    import importlib.machinery
    import importlib.util

    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    tracker_path = os.path.join(repo, "scripts", "s2u-mpv-tracker")
    loader = importlib.machinery.SourceFileLoader("s2u_tracker_unit", tracker_path)
    spec = importlib.util.spec_from_loader("s2u_tracker_unit", loader)
    assert spec is not None and spec.loader is not None
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    # is_provisional: stream basenames and URL-ish titles are provisional.
    for provisional in ("index.m3u8", "watch?v=abc123",
                        "https://example/stream", "audio.m4a"):
        if not mod.is_provisional(provisional):
            print(f"[FAIL] is_provisional({provisional!r}) should be True")
            ok = False
    if mod.is_provisional("Rick Astley - Never Gonna Give You Up"):
        print("[FAIL] is_provisional() true for a real title")
        ok = False

    # load_yt_info: resolves by stream URL and by original_url.
    with tempfile.TemporaryDirectory(prefix="tracker-unit-") as tmp:
        cache = os.path.join(tmp, "yt-info.json")
        with open(cache, "w", encoding="utf-8") as f:
            json.dump({
                "https://rr4.example/audio.m3u8": {
                    "url": "https://rr4.example/audio.m3u8",
                    "original_url": "https://youtu.be/x",
                    "title": "Rick Astley - Never Gonna Give You Up",
                    "channel": "Rick Astley",
                    "duration": 213.0,
                },
            }, f)
        old = mod.CACHE
        setattr(mod, "CACHE", tmp)
        try:
            by_stream = mod.load_yt_info("https://rr4.example/audio.m3u8")
            by_orig = mod.load_yt_info("https://youtu.be/x")
            missing = mod.load_yt_info("https://nope.example/x")
        finally:
            setattr(mod, "CACHE", old)
        if not by_stream or by_stream.get("title") != "Rick Astley - Never Gonna Give You Up":
            print("[FAIL] load_yt_info() by stream URL")
            ok = False
        if not by_orig or by_orig.get("title") != "Rick Astley - Never Gonna Give You Up":
            print("[FAIL] load_yt_info() by original_url")
            ok = False
        if missing is not None:
            print("[FAIL] load_yt_info() should return None for unknown URLs")
            ok = False

    # _entry_title: the saved playlist entry for the playing URL.
    st = {"playlist": [
        {"title": "Rick Astley - Never Gonna Give You Up",
         "url": "https://rr4.example/audio.m3u8"},
    ]}
    t = mod._entry_title(st, "https://rr4.example/audio.m3u8")
    if t != "Rick Astley - Never Gonna Give You Up":
        print("[FAIL] _entry_title() did not find the entry title")
        ok = False
    if mod._entry_title(st, "https://other.example/x") is not None:
        print("[FAIL] _entry_title() matched the wrong URL")
        ok = False

    print("[PASS] unit helpers (is_provisional / load_yt_info / _entry_title)"
          if ok else "[FAIL] unit helpers")
    return ok


if __name__ == "__main__":
    results = results + [test_state_file_lifecycle(), _test_unit_helpers()]
    print(f"\n{sum(results)}/{len(results)} tracker tests passed")
    sys.exit(0 if all(results) else 1)
