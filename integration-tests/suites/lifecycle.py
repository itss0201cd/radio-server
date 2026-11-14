"""Restart, empty-library, and invalid-auth suites."""

import json
import stat

from support import *


def restart_suite(tmp, music, auth):
    suite.section("restart and persistence")
    before = auth.read_bytes()

    server = Server(tmp, music, auth)
    try:
        response = login(server.port)
        suite.check(
            "the existing user can log in after restart",
            response.status == 200 and response.json().get("created") is False,
            response.text(),
        )
        cookie = cookie_from(response)
        suite.check("auth.json is unchanged by a login", auth.read_bytes() == before)

        response = login(server.port, "new-user", "new-password")
        suite.check("a new account still cannot be created after restart", response.status == 401)
        suite.check("auth.json still has one user", json.loads(auth.read_text()).get("username") == USER)

        status = status_json(server.port, cookie)
        suite.check("the restarted station still works", "now" in status and "queue" in status)
    finally:
        server.stop()

def empty_library_suite(tmp):
    suite.section("empty library boots and accepts the first upload")
    music = Path(tmp) / "music"
    music.mkdir()
    auth = Path(tmp) / "auth.json"

    server = Server(tmp, music, auth)
    try:
        suite.check("the UI is reachable with no media", request(server.port, "GET", "/").status == 200)
        cookie = auth_cookie(server.port)

        status = status_json(server.port, cookie)
        suite.check("an empty library has no now-playing track", status.get("now") is None)
        suite.check("an empty library has an empty queue", status.get("queue") == [])

        queue = request(server.port, "GET", "/api/queue", cookie=cookie).json()
        suite.check("the queue API works with an empty library", queue["total"] == 0 and queue["queue"] == [])

        public = sse_payload(open_sse(server.port))
        suite.check(
            "the public player works with an empty library",
            public.get("listeners") == 0 and public.get("now") is None and "queue" not in public,
        )

        stream = socket.create_connection(("127.0.0.1", server.port), timeout=10)
        stream.sendall(
            f"GET /events HTTP/1.1\r\nHost: 127.0.0.1:{server.port}\r\n\r\n".encode()
        )
        stream.settimeout(25)
        idle = b""
        try:
            while b": keepalive" not in idle and len(idle) < 65536:
                chunk = stream.recv(4096)
                if not chunk:
                    break
                idle += chunk
        finally:
            stream.close()
        suite.check(
            "the SSE stream sends keepalives while idle",
            idle.startswith(b"HTTP/1.1 200") and b": keepalive\n\n" in idle,
            idle[:200],
        )

        files = request(server.port, "GET", "/api/files", cookie=cookie).json()["files"]
        suite.check("an empty library lists no files", files == [])

        content = wav_bytes(2.0, 260.0)
        body, content_type = multipart(files=[("file", "first - upload.wav", content, "audio/wav")])
        response = request(
            server.port,
            "POST",
            "/api/upload",
            body=body,
            headers={"Content-Type": content_type},
            cookie=cookie,
        )
        suite.check("the first upload succeeds", response.status == 201, response.text())

        wait_until(
            lambda: status_json(server.port, cookie).get("now") is not None,
            timeout=30,
            message="the first upload to prime the stream",
        )
        suite.check("the uploaded file starts the station", True)

        sock, first = open_stream(server.port)
        try:
            sock.settimeout(5)
            data = first
            while b"OpusHead" not in data and len(data) < 65536:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                data += chunk
            suite.check("the stream serves Opus headers after upload", b"OpusHead" in data)
        finally:
            sock.close()

        log = server.tail()
        suite.check("no panic in the empty-library server log", "panicked at" not in log, log[-400:])
    finally:
        server.stop()

def invalid_auth_suite(tmp):
    suite.section("invalid auth files fail closed")

    cases = [
        ("broken json", b"{not json"),
        ("username without a hash", b'{"username":"x"}'),
        ("hash without a username", b'{"password_hash":"$argon2id$v=19$m=1,t=1,p=1$eA$eA"}'),
        ("non-argon2 hash", b'{"username":"x","password_hash":"not-a-hash"}'),
        ("unknown fields", b'{"username":"x","password_hash":"$argon2id$v=19$m=1,t=1,p=1$eA$eA","extra":1}'),
    ]

    for index, (label, content) in enumerate(cases):
        case_dir = Path(tmp) / f"bad-auth-{index}"
        music = case_dir / "music"
        music.mkdir(parents=True)
        auth = case_dir / "auth.json"
        auth.write_bytes(content)
        server = Server(case_dir, music, auth, expect_start=False)
        try:
            try:
                code = server.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                code = None
            suite.check(
                f"server refuses to start with {label}",
                code is not None and code != 0,
                f"exit={code}",
            )
            if code is None:
                server.stop()
        finally:
            if server.proc.poll() is None:
                server.stop()
