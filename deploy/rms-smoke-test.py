#!/usr/bin/env python3
"""Arena matchmaking smoke test — proves the rms feed works end-to-end on the live
server, with NO game client or device. Run ON the arena box (talks to localhost:8087).

    python3 deploy/rms-smoke-test.py

Flow: anon login -> open the rms WebSocket (server-push matchmaking feed) -> POST
matches/create -> read binary WS frames until "MatchmakingSucceeded" arrives (after
the ~15 s solo fallback). PASS means login -> session -> rms WS -> create -> Succeeded
all work — i.e. the session/rms half of a FIGHT is healthy (the rest is the client
dialing the ENet address in the Succeeded frame). Stdlib only (no websocket-client).

Why this exists: SessionStore used to be in-memory, so every arena-server rebuild
invalidated every token -> the rms WS 401'd -> matches/create returned 409 -> the
client showed "Server timed out." Sessions are now persisted (migration
2026-06-16_add_sessions); re-run this after any rebuild to confirm the feed survived.
"""
import base64
import json
import os
import socket
import sys
import time
import urllib.error
import urllib.request

BASE = os.environ.get("ARENA_BASE", "http://127.0.0.1:8087")
HOST, PORT = "127.0.0.1", int(os.environ.get("ARENA_PORT", "8087"))
WS_PATH = "/blades.bgs.services/api/rms/v1/public/"
LOGIN = "/blades.bgs.services/api/authentication/v1/public/auth/anon"
CREATE = "/blades.bgs.services/api/matchmaking/v1/public/matches/create"


def post(path, body, token=None):
    data = json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "t=" + token  # parser takes the part after the first '='
    req = urllib.request.Request(BASE + path, data=data, headers=headers, method="POST")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


def parse_frames(buf):
    """Parse server->client WS frames (unmasked). Returns (frames, leftover)."""
    out, i = [], 0
    while i + 2 <= len(buf):
        b0, b1 = buf[i], buf[i + 1]
        op, masked, ln, j = b0 & 0x0F, b1 & 0x80, b1 & 0x7F, i + 2
        if ln == 126:
            if j + 2 > len(buf):
                break
            ln = int.from_bytes(buf[j:j + 2], "big"); j += 2
        elif ln == 127:
            if j + 8 > len(buf):
                break
            ln = int.from_bytes(buf[j:j + 8], "big"); j += 8
        mask = b""
        if masked:
            if j + 4 > len(buf):
                break
            mask = buf[j:j + 4]; j += 4
        if j + ln > len(buf):
            break
        pl = buf[j:j + ln]
        if masked:
            pl = bytes(pl[k] ^ mask[k % 4] for k in range(ln))
        out.append((op, pl)); i = j + ln
    return out, buf[i:]


def main():
    st, body = post(LOGIN, {"platform": "gp", "deviceId": None, "userId": None})
    print("login HTTP", st)
    token = json.loads(body)["session"]["token"]
    print("session id", token.split("|")[0])

    s = socket.create_connection((HOST, PORT), timeout=10)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((
        f"GET {WS_PATH} HTTP/1.1\r\nHost: {HOST}:{PORT}\r\nUpgrade: websocket\r\n"
        f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Protocol: json\r\nAuthorization: t={token}\r\n\r\n"
    ).encode())
    buf = b""
    s.settimeout(10)
    while b"\r\n\r\n" not in buf:
        c = s.recv(4096)
        if not c:
            break
        buf += c
    hdr, _, rest = buf.partition(b"\r\n\r\n")
    line = hdr.split(b"\r\n")[0].decode("utf-8", "replace")
    print("ws handshake:", line)
    if "101" not in line:
        print("FAIL: rms WS did not upgrade"); sys.exit(1)

    st2, body2 = post(CREATE, {}, token=token)
    print("create HTTP", st2, body2[:160])
    if st2 != 200:
        print("FAIL: matches/create rejected (matchmaking_ws not set / session stale?)")
        sys.exit(1)

    # Wait comfortably past the solo fallback (ARENA_SOLO_FALLBACK_SECS, default 20s)
    # plus margin, so Succeeded reliably lands inside the window.
    deadline, buf2, ok, seen = time.monotonic() + 32, rest, None, 0
    while time.monotonic() < deadline and ok is None:
        s.settimeout(max(0.5, deadline - time.monotonic()))
        try:
            c = s.recv(8192)
        except socket.timeout:
            break
        if not c:
            print("ws closed by server"); break
        buf2 += c
        frames, buf2 = parse_frames(buf2)
        for op, pl in frames:
            if op == 0x8:
                print("ws close frame")
            elif op in (0x1, 0x2):
                t = pl.decode("utf-8", "replace"); seen += 1
                print("FRAME:", t[:280])
                if "MatchmakingSucceeded" in t:
                    ok = t

    print("=== RESULT ===")
    if ok:
        print("PASS: MatchmakingSucceeded delivered over the rms WS\n" + ok)
        sys.exit(0)
    print(f"FAIL/timeout: no Succeeded frame (frames seen={seen})")
    sys.exit(1)


if __name__ == "__main__":
    main()
