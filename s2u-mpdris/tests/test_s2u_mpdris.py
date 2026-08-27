#!/usr/bin/env python3
"""Integration test for the s2u-mpdris MPRIS2 helper daemon.

Runs the real helper (scripts/s2u-mpdris) against a fake D-Bus session bus
and a fake mpv IPC server, then drives the mpv-mpris.json state file and
asserts the MPRIS properties, signals and mpv command forwarding.

The fake bus is an INDEPENDENT wire implementation (auth + marshalling)
so alignment/signature bugs in the helper are caught, not shared.

Usage:  python3 tests/test_s2u_mpris.py  [helper-path]
Env:    S2U_MPDRIS_BIN  helper path override (default: this repo's
                        ./s2u-mpdris, else ~/.local/bin/s2u-mpdris)
"""
import json
import os
import binascii
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
HELPER = os.environ.get("S2U_MPDRIS_BIN") or os.path.join(REPO, "s2u-mpdris")
if not os.path.exists(HELPER):
    HELPER = os.path.expanduser("~/.local/bin/s2u-mpdris")

IFACE = "org.mpris.MediaPlayer2"
IFACE_PLAYER = IFACE + ".Player"
IFACE_PROPS = "org.freedesktop.DBus.Properties"
OBJ_PATH = "/org/mpris/MediaPlayer2"
TRACK_ID = OBJ_PATH + "/track/current"

# ---------------------------------------------------------- fake bus wire --

_ALIGN = {"y": 1, "b": 4, "n": 2, "q": 2, "i": 4, "u": 4,
          "x": 8, "t": 8, "d": 8, "s": 4, "o": 4, "g": 1, "v": 1}


def _align_of(sig):
    t = sig[0]
    if t == "a":
        return 4  # array value starts with a uint32 length; elements pad after
    return 8 if t in "({" else _ALIGN.get(t, 1)


def _align(off, n):
    return (off + n - 1) & ~(n - 1)


def m_one(sig, value, out):
    """Independent marshal (bytearray out), no alignment, no trailing pad."""
    t = sig[0]
    if t == "y":
        out.append(value & 0xFF)
    elif t == "b":
        out.extend((1 if value else 0).to_bytes(4, "little"))
    elif t in "ni":
        out.extend(int(value).to_bytes(4, "little", signed=True))
    elif t in "qu":
        out.extend(int(value).to_bytes(4, "little"))
    elif t in "xt":
        out.extend(int(value).to_bytes(8, "little", signed=(t == "x")))
    elif t == "d":
        out.extend(struct.pack("<d", float(value)))
    elif t in "so":
        b = str(value).encode("utf-8")
        out.extend(len(b).to_bytes(4, "little"))  # length excludes the NUL
        out.extend(b)
        out.append(0)
    elif t == "g":
        sig = str(value).encode("ascii")
        out.append(len(sig))
        out.extend(sig)
        out.append(0)
    elif t == "v":
        vsig, vval = value
        out.append(len(vsig.encode("ascii")))
        out.extend(vsig.encode("ascii") + b"\0")
        out.extend(b"\0" * (_align(len(out), _align_of(vsig)) - len(out)))
        m_one(vsig, vval, out)
    elif t == "a":
        inner = sig[1:]
        payload = bytearray()
        if inner.startswith("{"):
            ksig = inner[1]
            for k, v in value.items():
                payload.extend(b"\0" * (_align(len(payload), 8) - len(payload)))
                m_one(ksig, k, payload)
                vsig, vval = v
                payload.append(len(vsig.encode("ascii")))
                payload.extend(vsig.encode("ascii") + b"\0")
                payload.extend(b"\0" * (_align(len(payload), _align_of(vsig))
                                        - len(payload)))
                m_one(vsig, vval, payload)
        else:
            for item in value:
                payload.extend(b"\0" * (_align(len(payload),
                                                _align_of(inner)) - len(payload)))
                m_one(inner, item, payload)
        out.extend(len(payload).to_bytes(4, "little"))
        # Spec: the array is UINT32 length, then padding to the element's
        # alignment (not counted in n), then the elements.
        out.extend(b"\0" * (_align(len(out), _align_of(inner)) - len(out)))
        out.extend(payload)
    else:
        raise ValueError(sig)


def u_one(sig, data, off):
    t = sig[0]
    if t == "y":
        return data[off], off + 1
    if t == "b":
        return bool(int.from_bytes(data[off:off + 4], "little")), off + 4
    if t in "ni":
        return int.from_bytes(data[off:off + 4], "little", signed=True), off + 4
    if t in "qu":
        return int.from_bytes(data[off:off + 4], "little"), off + 4
    if t in "xt":
        return int.from_bytes(data[off:off + 8], "little", signed=(t == "x")), \
            off + 8
    if t == "d":
        return struct.unpack("<d", data[off:off + 8])[0], off + 8
    if t in "so":
        n = int.from_bytes(data[off:off + 4], "little")
        return data[off + 4:off + 4 + n].decode(), off + 4 + n + 1
    if t == "g":
        n = data[off]
        return data[off + 1:off + 1 + n].decode(), off + n + 2
    if t == "v":
        vsig, off = u_one("g", data, off)
        off = _align(off, _align_of(vsig))
        value, off = u_one(vsig, data, off)
        return (vsig, value), off
    if t == "a":
        inner = sig[1:]
        n = int.from_bytes(data[off:off + 4], "little")
        end = off + 4 + n
        off += 4
        if inner.startswith("{"):
            off = _align(off, 8)
            ksig = inner[1]
            out = {}
            while off < end:
                # Each dict entry is an 8-aligned struct of (key, variant).
                off = _align(off, 8)
                k, off = u_one(ksig, data, off)
                vsig, off = u_one("g", data, off)
                off = _align(off, _align_of(vsig))
                v, off = u_one(vsig, data, off)
                out[k] = (vsig, v)
            return out, off
        off = _align(off, _align_of(inner))
        out = []
        while off < end:
            # Each array element is padded to its own alignment.
            off = _align(off, _align_of(inner))
            item, off = u_one(inner, data, off)
            out.append(item)
        return out, off
    raise ValueError(sig)


def make_msg(msg_type, serial, fields, body_sig="", body=()):
    """fields: list of (code, (vsig, vval))."""
    fp = bytearray()
    for code, (vsig, vval) in fields:
        fp.extend(b"\0" * (_align(len(fp), 8) - len(fp)))
        fp.append(code)
        fp.append(len(vsig.encode("ascii")))
        fp.extend(vsig.encode("ascii") + b"\0")
        fp.extend(b"\0" * (_align(len(fp), _align_of(vsig)) - len(fp)))
        m_one(vsig, vval, fp)
    body_bytes = bytearray()
    while body_sig:
        n = complete_len(body_sig)
        part = body_sig[:n]
        body_bytes.extend(b"\0" * (_align(len(body_bytes),
                                          _align_of(part)) - len(body_bytes)))
        m_one(part, body[0], body_bytes)
        body = body[1:]
        body_sig = body_sig[n:]
    body_bytes.extend(b"\0" * (_align(len(body_bytes), 8) - len(body_bytes)))
    hdr = bytearray(b"l" + bytes([msg_type, 0, 1]))
    hdr.extend(len(body_bytes).to_bytes(4, "little"))
    hdr.extend(serial.to_bytes(4, "little"))
    hdr.extend(len(fp).to_bytes(4, "little"))  # field-array length right after serial
    hdr.extend(b"\0" * (_align(len(hdr), 8) - len(hdr)))
    hdr.extend(fp)
    # The header (fixed part + field array) must be a multiple of 8 so the
    # body starts on an 8-byte boundary.
    hdr.extend(b"\0" * (_align(len(hdr), 8) - len(hdr)))
    return bytes(hdr) + bytes(body_bytes)


def complete_len(sig):
    """Chars of the first single complete type in `sig`."""
    if not sig:
        return 0
    t = sig[0]
    if t == "a":
        return 1 + complete_len(sig[1:])
    if t in "({":
        depth = 0
        for i, ch in enumerate(sig):
            if ch == t:
                depth += 1
            elif ch == (")" if t == "(" else "}"):
                depth -= 1
                if depth == 0:
                    return i + 1
        raise ValueError("unbalanced container in %r" % sig)
    return 1


def unmarshal_all(sig, data):
    out = []
    off = 0
    while sig:
        n = complete_len(sig)
        part = sig[:n]
        off = _align(off, _align_of(part))
        item, off = u_one(part, data, off)
        out.append(item)
        sig = sig[n:]
    return out


def parse_msg(buf):
    if len(buf) < 12:
        return None
    body_len = int.from_bytes(buf[4:8], "little")
    serial = int.from_bytes(buf[8:12], "little")
    n = int.from_bytes(buf[12:16], "little")  # field-array length right after serial
    off = _align(16, 8)
    fields = {}
    end = off + n
    while off < end:
        # Each header field is a struct (BYTE, VARIANT): 8-aligned.
        off = _align(off, 8)
        code = buf[off]
        off += 1
        vsig, off = u_one("g", buf, off)
        off = _align(off, _align_of(vsig))
        val, off = u_one(vsig, buf, off)
        fields[code] = val
    start = _align(off, 8)
    body = buf[start:start + body_len]
    body_sig = fields.get(8, "")   # header field 8 = SIGNATURE
    values = unmarshal_all(body_sig, body) if body_sig else None
    return {"type": buf[1], "serial": serial, "fields": fields,
            "body_sig": body_sig, "values": values}, buf[start + body_len:]


# ------------------------------------------------------------------- fakes --

class FakeBus:
    """A minimal session-bus daemon with two sockets, one per role: the
    helper dials `path`, the test client dials `path + ".client"`. This
    makes the role of each accepted connection deterministic (no accept
    ordering games). EXTERNAL auth, RequestName, and routing — client
    method calls are forwarded to the helper, replies and signals come
    back the other way. Each socket has exactly one reader thread."""

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self._serial = 0
        self.helper_conn = None
        self.helper_ready = threading.Event()
        self.helper_authed = threading.Event()
        self.client_conn = None
        self.client_ready = threading.Event()
        self.replies = {}      # (conn, serial) -> parsed reply message
        self.inbox = []        # signals the helper emitted
        self._pending = {}     # (client_conn, serial) -> True (awaiting reply)
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(4)
        threading.Thread(target=self._accept_helper, daemon=True).start()
        self.client_listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.client_listener.bind(path + ".client")
        self.client_listener.listen(4)
        threading.Thread(target=self._accept_client, daemon=True).start()

    def _accept_helper(self):
        conn, _ = self.listener.accept()
        self.helper_conn = conn
        self.helper_ready.set()
        threading.Thread(target=self._serve, args=(conn, True),
                         daemon=True).start()

    def _accept_client(self):
        conn, _ = self.client_listener.accept()
        self.client_conn = conn
        self.client_ready.set()
        threading.Thread(target=self._serve, args=(conn, False),
                         daemon=True).start()

    def _serve(self, conn, is_helper):
        """Serve one connection: EXTERNAL auth, then parse D-Bus messages.
        A single buffer is shared between the auth line reads and the
        message parser — if BEGIN and the first method call arrive in one
        read, nothing is lost."""
        try:
            conn.sendall(b"\0")
            conn.settimeout(5)
            buf = b""
            line, buf = self._read_line(conn, buf)
            if not line.lstrip(b"\0").startswith(b"AUTH EXTERNAL"):
                return
            conn.sendall(b"OK 0123456789abcdef0123456789abcdef\r\n")
            line, buf = self._read_line(conn, buf)
            if not line.startswith(b"BEGIN"):
                return
            if is_helper:
                self.helper_authed.set()
            conn.settimeout(None)
            # `buf` may already hold a method call that arrived in the
            # same read as BEGIN: parse it before reading more.
            while True:
                if buf:
                    parsed = parse_msg(buf)
                    if parsed is not None:
                        msg, buf = parsed
                        try:
                            self._route(conn, is_helper, msg)
                        except (BrokenPipeError, OSError):
                            return
                        continue
                try:
                    chunk = conn.recv(65536)
                except OSError:
                    return
                if not chunk:
                    return
                buf += chunk
                while True:
                    parsed = parse_msg(buf)
                    if parsed is None:
                        break
                    msg, buf = parsed
                    try:
                        self._route(conn, is_helper, msg)
                    except (BrokenPipeError, OSError):
                        return
        except (OSError, ValueError):
            return

    @staticmethod
    def _read_line(conn, buf):
        """Read one CRLF-terminated line, returning (line, leftover)."""
        while b"\r\n" not in buf:
            chunk = conn.recv(4096)
            if not chunk:
                return b"", buf
            buf += chunk
        line, buf = buf.split(b"\r\n", 1)
        return line, buf

    def _route(self, conn, is_helper, msg):
        fields = msg["fields"]
        iface = fields.get(2, "")
        member = fields.get(3, "")
        reply_serial = fields.get(5)
        if is_helper:
            if iface == "org.freedesktop.DBus" and member == "RequestName":
                self._reply(conn, msg, "u", [1])
                return
            if iface == "org.freedesktop.DBus" and member == "Hello":
                self._reply(conn, msg, "s", [":1.999"])
                return
            if reply_serial is not None:
                # Reply to a forwarded client call: it must arrive on the
                # socket the client serve thread reads (`client_conn`), so
                # write it on the DIALED socket whose peer that is.
                # Sending on `client_conn` writes into the client socket
                # nobody drains (self-deadlock).
                if self.client_sock is not None:
                    self.client_sock.sendall(parse_msg_back(msg))
                return
            # Signal / anything else from the helper.
            with self.lock:
                self.inbox.append(msg)
            return
        # Client side.
        if reply_serial is not None:
            # A reply to one of our calls: park it for `call()`.
            key = (conn, reply_serial)
            with self.lock:
                if key in self._pending:
                    del self._pending[key]
                self.replies[key] = msg
            return
        if iface == "org.freedesktop.DBus" and member == "RequestName":
            self._reply(conn, msg, "u", [1])
            return
        # Method call from the client: forward to the helper. The helper
        # must have finished AUTH/BEGIN first, or its _auth would read our
        # message instead of the OK line (a race the real daemon cannot hit
        # because it never routes before authentication completes).
        serial = msg["serial"]
        with self.lock:
            self._pending[(conn, serial)] = True
        if self.helper_conn is not None:
            self.helper_authed.wait(timeout=5)
            self.helper_conn.sendall(parse_msg_back(msg))

    def _reply(self, conn, req, body_sig="", body=()):
        fields = [(1, ("o", req["fields"].get(1, OBJ_PATH))),
                  (5, ("u", req["serial"])),
                  (6, ("s", req["fields"].get(6, "")))]
        if body_sig:
            fields.append((8, ("g", body_sig)))   # header field 8 = SIGNATURE
        conn.sendall(make_msg(2, self.next_serial(), fields, body_sig, body))

    def next_serial(self):
        with self.lock:
            self._serial += 1
            return self._serial

    # ---- client side: talk through the bus to the helper ----------------

    def client(self):
        """Open the test client's connection to the bus (its own
        listener, so the helper/client roles are deterministic). Retries
        while the accept thread is still starting up. Keeps the socket
        referenced."""
        self.helper_ready.wait(10)
        s = None
        deadline = time.time() + 10
        while s is None:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                s.connect(self.path + ".client")
            except OSError:
                s.close()
                if time.time() > deadline:
                    raise
                time.sleep(0.02)
                s = None
        s.recv(1)  # NUL greeting
        s.sendall(b"\0AUTH EXTERNAL " + binascii.hexlify(str(os.getuid()).encode()) + b"\r\n")
        assert s.recv(1024).startswith(b"OK")
        s.sendall(b"BEGIN\r\n")
        self.client_sock = s
        self.client_ready.set()
        return s
        s.recv(1)  # NUL greeting
        s.sendall(b"\0AUTH EXTERNAL " + binascii.hexlify(str(os.getuid()).encode()) + b"\r\n")
        assert s.recv(1024).startswith(b"OK")
        s.sendall(b"BEGIN\r\n")
        self.client_sock = s
        self.client_ready.set()
        return s

    def call(self, member, iface, body_sig="", body=(), obj=OBJ_PATH):
        self.client_ready.wait(10)
        deadline = time.time() + 10
        while self.client_conn is None:
            if time.time() > deadline:
                raise TimeoutError("no client connection")
            time.sleep(0.01)
        fields = [(1, ("o", obj)), (2, ("s", iface)), (3, ("s", member)),
                  (6, ("s", "org.mpris.MediaPlayer2.s2u-mpv")),
                  (7, ("s", ":1.100"))]
        if body_sig:
            fields.append((8, ("g", body_sig)))
        serial = self.next_serial()
        # Send on the DIALED socket: its peer is `client_conn`, which the
        # client serve thread reads. Sending on `client_conn` would write
        # into the client socket nobody drains (self-deadlock).
        self.client_sock.sendall(make_msg(1, serial, fields, body_sig, body))
        key = (self.client_conn, serial)
        return self._wait_reply(key)

    def _wait_reply(self, key, timeout=5):
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                if key in self.replies:
                    msg = self.replies.pop(key)
                    if msg["type"] == 3:
                        raise RuntimeError(msg["fields"].get(4, "error"))
                    return msg["values"]
            time.sleep(0.01)
        raise TimeoutError("no reply for %r" % (key,))

    def get(self, name, iface=IFACE_PLAYER):
        val = self.call("Get", IFACE_PROPS, "ss", [iface, name])
        return val[0]

    def get_all(self, iface=IFACE_PLAYER):
        return self.call("GetAll", IFACE_PROPS, "s", [iface])[0]

    def signal(self, timeout=5):
        """Pop the next message (signal) the helper emitted, or None."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self.lock:
                if self.inbox:
                    return self.inbox.pop(0)
            time.sleep(0.01)
        return None


# Header-field code -> signature (used when re-serializing parsed
# messages for forwarding).
_FIELD_SIG = {1: "o", 2: "s", 3: "s", 4: "s", 5: "u", 6: "s",
              7: "s", 8: "g", 9: "u"}


def parse_msg_back(msg):
    """Re-serialize a parsed message for forwarding on another socket."""
    fields = []
    for code, val in msg["fields"].items():
        if code in _FIELD_SIG:
            fields.append((code, (_FIELD_SIG[code], val)))
        else:
            raise ValueError("cannot re-serialize field %d" % code)
    return make_msg(msg["type"], msg["serial"], fields,
                    msg["body_sig"], msg["values"] or [])


class FakeMpv:
    """Records mpv IPC commands; answers get_property minimally."""

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.commands = []
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(4)
        threading.Thread(target=self._accept, daemon=True).start()

    def _accept(self):
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self._conn, args=(conn,),
                             daemon=True).start()

    def _conn(self, conn):
        f = conn.makefile("rb")
        while True:
            line = f.readline()
            if not line:
                break
            try:
                cmd = json.loads(line)["command"]
            except Exception:
                continue
            with self.lock:
                self.commands.append(cmd)
            try:
                conn.sendall((json.dumps({"error": "success", "data": None})
                              + "\n").encode())
            except OSError:
                break
        conn.close()

    def sent(self):
        with self.lock:
            return list(self.commands)


# ------------------------------------------------------------------- tests --

PASS, FAIL = [], []


def check(name, cond, detail=""):
    if cond:
        PASS.append(name)
        print("  ok  %s" % name)
    else:
        FAIL.append((name, detail))
        print("  FAIL %s  %s" % (name, detail))


class Harness:
    def __init__(self, tmp):
        self.tmp = tmp
        self.bus_path = os.path.join(tmp, "bus.sock")
        self.mpv_path = os.path.join(tmp, "mpv.sock")
        self.cache = os.path.join(tmp, "cache")
        os.makedirs(self.cache)
        self.bus = FakeBus(self.bus_path)
        self.mpv = FakeMpv(self.mpv_path)
        self.state = os.path.join(self.cache, "mpv-mpris.json")
        self.poster = os.path.join(self.cache, "mpris-mpv-art")
        # Seed a paused state file: the helper exits by design once the
        # state is stale for STALE_GRACE ticks, so it must start with
        # something to read.
        self.write_state(playing=False, title="", socket_=self.mpv_path)
        self.proc: subprocess.Popen = None  # type: ignore[assignment]
        self._start()

    def _start(self):
        env = dict(os.environ)
        env.update({
            "S2U_CACHE_DIR": self.cache,
            "S2U_MPRIS_BUS": "unix:path=" + self.bus_path,
            "S2U_MPRIS_POLL_S": "0.1",
            "S2U_MPRIS_STALE_S": "15",
            "S2U_MPRIS_PIDFILE": os.path.join(self.cache, "mpris.pid"),
        })
        self.proc = subprocess.Popen([sys.executable, HELPER],
                                     env=env, stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL)
        # Dial the test client's own connection to the bus.
        self.bus.client()
        # Wait for the name acquisition.
        deadline = time.time() + 10
        while time.time() < deadline:
            try:
                self.bus.get("Identity", IFACE)
                return
            except (TimeoutError, OSError, RuntimeError):
                if self.proc.poll() is not None:
                    raise RuntimeError("helper exited early: %r"
                                       % self.proc.returncode)
                time.sleep(0.05)
        raise TimeoutError("helper never acquired the name")

    def write_state(self, playing=True, title="Test Video", artist="",
                    art=None, position=12.0, duration=600.0, volume=71.0,
                    url="https://youtu.be/abc123", socket_=None):
        st = {
            "title": title, "artist": artist,
            "art": art if art is not None else self.poster,
            "playing": playing,
            "position": position, "duration": duration,
            "socket": socket_ or self.mpv_path,
            "item_id": "", "volume": volume,
            "playlist": [{"title": title, "url": url, "duration": duration}],
            "playlist_pos": 0,
        }
        with open(self.state, "w") as f:
            json.dump(st, f)

    def touch_poster(self):
        with open(self.poster, "wb") as f:
            f.write(b"\x89PNG")
        return os.path.getmtime(self.poster)

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        try:
            self.bus.listener.close()
            self.bus.client_listener.close()
        except OSError:
            pass
        try:
            self.mpv.listener.close()
        except OSError:
            pass


def wait_until(cond, timeout=5):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if cond():
                return True
        except (TimeoutError, OSError, RuntimeError):
            pass
        time.sleep(0.05)
    return False


def main():
    with tempfile.TemporaryDirectory(prefix="s2u-mpdris-test-") as tmp:
        h = Harness(tmp)
        try:
            # 1. initial state: Paused (the harness seeds a paused state
            # file so the daemon does not exit on stale state), correct
            # identity.
            print("[1] initial properties")
            allp = h.bus.get_all(IFACE)
            check("Identity", allp.get("Identity", ("", ""))[1] == "s2udio (mpv)",
                  repr(allp.get("Identity")))
            check("initial PlaybackStatus is Paused",
                  h.bus.get("PlaybackStatus")[1] == "Paused")
            check("CanControl true", h.bus.get("CanControl")[1] is True)

            # 2. playing state with metadata + poster.
            print("[2] playing metadata")
            h.write_state()
            mtime = h.touch_poster()
            ok = wait_until(
                lambda: h.bus.get("PlaybackStatus")[1] == "Playing")
            check("PlaybackStatus -> Playing", ok)
            meta = h.bus.get("Metadata")[1]
            check("xesam:title", meta.get("xesam:title") == ("s", "Test Video"),
                  repr(meta.get("xesam:title")))
            check("mpris:length", meta.get("mpris:length") ==
                  ("x", 600_000_000), repr(meta.get("mpris:length")))
            check("xesam:url", meta.get("xesam:url") ==
                  ("s", "https://youtu.be/abc123"), repr(meta.get("xesam:url")))
            check("mpris:trackid", meta.get("mpris:trackid") ==
                  ("o", TRACK_ID), repr(meta.get("mpris:trackid")))
            art_url = meta.get("mpris:artUrl", ("", ""))[1]
            check("mpris:artUrl cache-busted",
                  art_url.startswith("file://") and "?t=" in art_url
                  and str(int(mtime * 1e9)) in art_url, repr(art_url))
            check("Position in us", h.bus.get("Position")[1] == 12_000_000,
                  repr(h.bus.get("Position")))
            check("Volume 0..1", abs(h.bus.get("Volume")[1] - 0.71) < 1e-9,
                  repr(h.bus.get("Volume")))

            # PropertiesChanged fired on the Playing transition.
            sig = h.bus.signal(timeout=2)
            check("PropertiesChanged signal emitted",
                  sig is not None and sig["fields"].get(3) ==
                  "PropertiesChanged", repr(sig and sig["fields"].get(3)))

            # 3. transport forwarding.
            print("[3] mpv command forwarding")
            h.bus.call("Play", IFACE_PLAYER)
            h.bus.call("Pause", IFACE_PLAYER)
            h.bus.call("PlayPause", IFACE_PLAYER)
            h.bus.call("Stop", IFACE_PLAYER)
            h.bus.call("Next", IFACE_PLAYER)
            h.bus.call("Previous", IFACE_PLAYER)
            h.bus.call("Seek", IFACE_PLAYER, "x", [30_000_000])
            h.bus.call("SetPosition", IFACE_PLAYER, "ox", [TRACK_ID, 900_000_000])
            h.bus.call("OpenUri", IFACE_PLAYER, "s", ["https://vimeo.com/9"])
            ok = wait_until(lambda: len(h.mpv.sent()) >= 9)
            check("all commands forwarded", ok,
                  repr(h.mpv.sent()))
            sent = h.mpv.sent()
            expect = [["set_property", "pause", False],
                      ["set_property", "pause", True],
                      ["cycle", "pause"], ["stop"], ["playlist-next"],
                      ["playlist-previous"],
                      ["seek", 30.0, "relative"],
                      ["seek", 900.0, "absolute"],
                      ["loadfile", "https://vimeo.com/9", "replace"]]
            check("command sequence matches",
                  sent == expect, "got %r" % sent)

            # 4. Seeked signal + wrong-track SetPosition error.
            print("[4] Seeked / SetPosition validation")
            seen = []

            def _seeked():
                while True:
                    m = h.bus.signal(timeout=1)
                    if m is None:
                        return False
                    if m["fields"].get(3) == "Seeked":
                        seen.append(m)
                        return True

            check("Seeked signal emitted", wait_until(_seeked), "no Seeked")
            err = None
            try:
                h.bus.call("SetPosition", IFACE_PLAYER, "ox",
                           ["/org/other/track", 0])
            except RuntimeError as e:
                err = e
            check("SetPosition wrong track id errors",
                  err is not None and "InvalidArgs" in str(err),
                  repr(err))

            # 5. Volume set forwards and updates the property. mpv applies
            # it asynchronously: the helper's own poll overwrites Volume
            # from the state file, so the test simulates mpv applying the
            # change (state file updated) before asserting.
            print("[5] Volume set")
            h.bus.call("Set", IFACE_PROPS, "ssv",
                       [IFACE_PLAYER, "Volume", ("d", 0.5)])
            check("mpv volume command", wait_until(
                lambda: ["set_property", "volume", 50.0] in h.mpv.sent()))
            h.write_state(volume=50.0)
            check("Volume property updated",
                  wait_until(lambda: abs(h.bus.get("Volume")[1] - 0.5) < 1e-9))

            # 6. paused -> Paused.
            print("[6] paused state")
            h.write_state(playing=False, position=15.0)
            check("Paused reported", wait_until(
                lambda: h.bus.get("PlaybackStatus")[1] == "Paused"))

            # 7. state deleted -> Stopped and the daemon exits.
            print("[7] stale state exits")
            os.remove(h.state)
            check("Stopped before exit", wait_until(
                lambda: h.bus.get("PlaybackStatus")[1] == "Stopped"))
            check("daemon exits after stale state", wait_until(
                lambda: h.proc.poll() is not None))
        finally:
            h.stop()

    print("\n%d passed, %d failed" % (len(PASS), len(FAIL)))
    for name, detail in FAIL:
        print("  FAIL %s: %s" % (name, detail))
    sys.exit(1 if FAIL else 0)


if __name__ == "__main__":
    main()
