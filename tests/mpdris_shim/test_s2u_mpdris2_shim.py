#!/usr/bin/env python3
"""Unit tests for the s2u-mpdris2 shim patches (scripts/s2u-mpdris2).

Imports the working-tree script and drives _patch_mpdris2() against a
fake official mpDris2 module (no real /usr/bin/mpDris2, no D-Bus, no MPD
needed), asserting the s2udio extensions:

  - update_metadata() injects title/artist/duration from yt-info.json and
    arms the async-art retry; the retry lands mpris:artUrl once the art
    file exists and forces a Metadata rebuild.
  - seekid() catches CommandError as a no-op instead of letting the
    official __getattr__->call() reconnect path drop the D-Bus name.
  - the Seek/SetPosition D-Bus handlers no-op on CommandError and on a
    stream status with no 'time' (KeyError) instead of crashing.

Usage:  python3 tests/mpdris_shim/test_s2u_mpdris2_shim.py
"""
import importlib.machinery
import importlib.util
import json
import os
import sys
import tempfile
import types

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SHIM = os.path.join(REPO, "scripts", "s2u-mpdris2")

PASS, FAIL = [], []


def check(name, ok, detail=""):
    (PASS if ok else FAIL).append(name)
    if not ok:
        print(f"[FAIL] {name}: {detail}")


# ------------------------------------------------------------- fake GLib --

class FakeGLib(object):
    def __init__(self):
        self.sources = {}
        self.next_id = 1
        self.cancelled = set()

    def timeout_add(self, interval_ms, cb):
        src = self.next_id
        self.next_id += 1
        self.sources[src] = (interval_ms, cb)
        return src

    def source_remove(self, src):
        self.cancelled.add(src)
        self.sources.pop(src, None)

    def run(self, src):
        return self.sources.pop(src, None)[1]()


# ---------------------------------------------------------- fake official --

class FakeCommandError(Exception):
    pass


class FakeMPDError(Exception):
    pass


class FakeSocket(object):
    error = ConnectionError
    timeout = TimeoutError


class FakeLogger(object):
    def __init__(self):
        self.warnings = []
        self.debugs = []

    def warning(self, msg):
        self.warnings.append(msg)

    def debug(self, msg):
        self.debugs.append(msg)

    def error(self, msg):
        self.warnings.append(msg)


class FakeMpd(object):
    CommandError = FakeCommandError
    MPDError = FakeMPDError


class FakeClient(object):
    def __init__(self, fail=False):
        self.fail = fail
        self.seek_calls = []

    def seekid(self, songid, position):
        self.seek_calls.append((songid, position))
        if self.fail:
            raise FakeCommandError("Not seekable")
        return 1


class FakeWrapper(object):
    def __init__(self, client=None):
        self.client = client or FakeClient()
        self._metadata = {}
        self._dbus_service = None
        self.reconnected = 0

    def idle_leave(self):
        return False

    def idle_enter(self):
        pass

    def reconnect(self):
        self.reconnected += 1

    def find_cover(self, song_url):
        return None

    def update_metadata(self):
        self._metadata = {"xesam:url": "https://rr4.example/audio.m3u8"}

    # NOTE: mirrors the real official /usr/bin/mpDris2 MPDWrapper, which
    # has NO seekid in its class body (it only exists via __getattr__ ->
    # call()). The shim must install one; a guard on hasattr(wrapper,
    # "seekid") would silently skip the patch, so this fake intentionally
    # omits it to catch that regression.


class FakeMethod(object):
    def __init__(self, func):
        self.func = func


def fake_dbus_method(func):
    """Mirror dbus.service.method in this dbus-python version: it returns
    the plain function with _dbus_* attributes attached (there is no
    .func-wrapping Method object), and _method_lookup() reads the method
    out of the class __dict__ per call."""
    func._dbus_is_method = True
    func._dbus_interface = "org.mpris.MediaPlayer2.Player"
    func._dbus_in_signature = "x"
    func._dbus_out_signature = ""
    func._dbus_async_callbacks = None
    func._dbus_get_args_options = {}
    func._dbus_sender_keyword = None
    func._dbus_path_keyword = None
    func._dbus_rel_path_keyword = None
    func._dbus_destination_keyword = None
    func._dbus_message_keyword = None
    func._dbus_connection_keyword = None
    func._dbus_args = []
    return func


def _official_seek(self, offset):
    # An HLS stream status has no 'time': the official Seek dies with
    # KeyError before even reaching seekid(); the shim must no-op it.
    raise KeyError("time")


def _official_set_position(self, trackid, position):
    raise KeyError("time")


def fake_dbus_service(wrapper):
    class Svc(object):
        def __init__(self):
            self.updates = []

        def update_property(self, iface, prop):
            self.updates.append((iface, prop))

    wrapper._dbus_service = Svc()
    return wrapper._dbus_service


def make_mod():
    glib = FakeGLib()
    logger = FakeLogger()
    mod = types.SimpleNamespace(
        GLib=glib,
        logger=logger,
        socket=FakeSocket,
        mpd=FakeMpd(),
        MPDWrapper=FakeWrapper,
        MPRISInterface=type("MPRISInterface", (), {
            "Seek": fake_dbus_method(_official_seek),
            "SetPosition": fake_dbus_method(_official_set_position),
        }),
    )
    return mod


def load_shim():
    loader = importlib.machinery.SourceFileLoader("s2u_mpdris2_unit", SHIM)
    spec = importlib.util.spec_from_loader("s2u_mpdris2_unit", loader)
    assert spec is not None and spec.loader is not None
    shim = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(shim)
    return shim


def _test_patch_applies_and_metadata_injects():
    shim = load_shim()
    mod = make_mod()
    if not shim._patch_mpdris2(mod):
        print("[FAIL] _patch_mpdris2 returned False")
        return False
    ok = True

    # update_metadata(): inject title/artist/duration from yt-info.json.
    with tempfile.TemporaryDirectory(prefix="mpdris2-shim-") as tmp:
        cache = os.path.join(tmp, "yt-info.json")
        # Hermetic: point ART at a temp path with no file yet, or the
        # first tick finds a real ~/.cache/rmpc/mpris-art from live
        # playback and completes instead of re-arming (the container
        # passed for the same reason).
        art = os.path.join(tmp, "mpris-art")
        with open(cache, "w", encoding="utf-8") as f:
            json.dump({
                "https://rr4.example/audio.m3u8": {
                    "url": "https://rr4.example/audio.m3u8",
                    "original_url": "https://youtu.be/x",
                    "title": "Rick Astley - Never Gonna Give You Up",
                    "channel": "Rick Astley",
                    "duration": 213.0,
                    "thumbnail": "https://i.ytimg.com/vi/x/hqdefault.jpg",
                },
            }, f)
        old = shim.YT_INFO
        old_art = shim.ART
        setattr(shim, "YT_INFO", cache)
        setattr(shim, "ART", art)
        try:
            w = FakeWrapper()
            svc = fake_dbus_service(w)
            mod.MPDWrapper.update_metadata(w)
            meta = w._metadata
            if meta.get("xesam:title") != "Rick Astley - Never Gonna Give You Up":
                check("metadata title injected", False, repr(meta))
                ok = False
            if meta.get("xesam:artist") != ["Rick Astley"]:
                check("metadata artist injected", False, repr(meta))
                ok = False
            # dbus missing in container: plain int is the fallback.
            if meta.get("mpris:length") != 213000000:
                check("metadata duration injected", False, repr(meta))
                ok = False
            if "mpris:artUrl" in meta:
                check("no art before the file exists", False, repr(meta))
                ok = False
            if not getattr(w, "_s2u_art_retry_url", None):
                check("art retry armed", False)
                ok = False
            # Tick 1: still no art file -> re-arms.
            src = getattr(w, "_s2u_art_retry_src", None)
            if src is None or src not in mod.GLib.sources:
                check("art retry scheduled a GLib source", False)
                ok = False
            else:
                again = mod.GLib.run(src)
                if again:
                    check("tick without art re-arms", False)
                    ok = False
                if not getattr(w, "_s2u_art_retry_src", None):
                    check("tick without art re-scheduled", False)
                    ok = False
            # Write the art file; the next tick must land mpris:artUrl and
            # force a Metadata rebuild.
            with open(art, "wb") as f:
                f.write(b"IMG")
            src = getattr(w, "_s2u_art_retry_src", None)
            if src is None or src not in mod.GLib.sources:
                check("re-armed retry pending", False)
                ok = False
            else:
                mod.GLib.run(src)
                meta = w._metadata
                if "mpris:artUrl" not in meta:
                    check("artUrl landed after retry", False, repr(meta))
                    ok = False
                if ("org.mpris.MediaPlayer2.Player", "Metadata") not in svc.updates:
                    check("Metadata rebuild forced on art", False, repr(svc.updates))
                    ok = False
        finally:
            setattr(shim, "YT_INFO", old)
            setattr(shim, "ART", old_art)

    # seekid(): CommandError must be a no-op (no reconnect), success must
    # forward to the client.
    w = FakeWrapper(FakeClient(fail=True))
    result = mod.MPDWrapper.seekid(w, 5, 30.0)
    if result is not False:
        check("seekid catches CommandError", False, repr(result))
        ok = False
    if w.reconnected:
        check("seekid does not reconnect on CommandError", False, "reconnected")
        ok = False
    w2 = FakeWrapper(FakeClient(fail=False))
    result = mod.MPDWrapper.seekid(w2, 5, 30.0)
    if result is not True or w2.client.seek_calls != [(5, 30.0)]:
        check("seekid forwards successful seeks", False, repr(result))
        ok = False

    # Seek/SetPosition guards: the wrapped D-Bus handlers must no-op on
    # the KeyError an HLS stream status raises (no 'time' timeline) — and
    # still carry the _dbus_* contract so the bus recognizes them.
    seek_fn = mod.MPRISInterface.Seek
    setpos_fn = mod.MPRISInterface.SetPosition
    if not getattr(seek_fn, "_dbus_is_method", False):
        check("Seek wrapper kept the dbus contract", False)
        ok = False
    svc = type("Iface", (), {})()
    try:
        if seek_fn(svc, 30_000_000) is not None:
            check("guards no-op on stream KeyError", False, repr(seek_fn(svc, 0)))
            ok = False
        setpos_fn(svc, "/org/mpris/MediaPlayer2/Track/5", 0)
    except Exception as err:
        check("guards no-op on stream KeyError", False, repr(err))
        ok = False

    return ok


if __name__ == "__main__":
    results = [_test_patch_applies_and_metadata_injects()]
    print(f"\n{sum(results)}/{len(results)} mpdris2-shim tests passed")
    sys.exit(0 if all(results) else 1)
