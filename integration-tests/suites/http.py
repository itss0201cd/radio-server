"""Security headers, malformed requests, listeners, logout, and log checks."""

import json
import time

from support import *


def run(server, cookie, second_cookie):
    port = server.port
    suite.section("security headers and malformed requests")

    response = request(port, "GET", "/api/status", cookie=cookie)
    suite.check("API responses are nosniff", response.headers.get("x-content-type-options") == "nosniff")
    suite.check("API responses are no-store", response.headers.get("cache-control") == "no-store")
    suite.check("API responses are no-referrer", response.headers.get("referrer-policy") == "no-referrer")

    no_host = raw_exchange(port, b"GET / HTTP/1.1\r\n\r\n")
    suite.check("HTTP/1.1 without Host is rejected", b"400" in no_host, no_host[:80])

    put = raw_exchange(port, b"PUT / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n")
    suite.check("unsupported methods are 405", b"405" in put, put[:80])

    chunked = raw_exchange(
        port,
        b"POST /api/login HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    )
    suite.check("chunked bodies are rejected", b"400" in chunked, chunked[:80])

    no_length = raw_exchange(
        port,
        b"POST /api/login HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\r\n",
    )
    suite.check("POST without Content-Length is 411", b"411" in no_length, no_length[:80])

    big_header = (
        b"GET / HTTP/1.1\r\nHost: x\r\nX-Big: " + b"a" * 20000 + b"\r\n\r\n"
    )
    big = raw_exchange(port, big_header)
    suite.check("oversized headers are rejected", b"431" in big, big[:80])

    suite.section("listener counting")

    wait_until(
        lambda: status_json(port, cookie)["listeners"] == 0,
        timeout=10,
        message="listener count to return to zero",
    )
    suite.check("listener count starts at zero", True)

    stream_one, _ = open_stream(port)
    stream_two, _ = open_stream(port)
    try:
        wait_until(
            lambda: status_json(port, cookie)["listeners"] >= 2,
            timeout=10,
            message="two listeners to be counted",
        )
        suite.check("open streams are counted", True)
    finally:
        stream_one.close()
        stream_two.close()

    wait_until(
        lambda: status_json(port, cookie)["listeners"] == 0,
        timeout=10,
        message="listeners to be released",
    )
    suite.check("closed streams are removed from the count", True)

    suite.section("live listener details and disconnect")

    named, _ = open_stream(
        port,
        {"User-Agent": "RadioTest/1.0", "X-Forwarded-For": "203.0.113.5"},
    )
    try:
        entry = wait_until(
            lambda: (
                status_json(port, cookie).get("listener_list")
                or [None]
            )[0],
            timeout=10,
            message="the named listener to register",
        )
        suite.check(
            "listener details expose the user agent",
            entry.get("user_agent") == "RadioTest/1.0",
            entry,
        )
        suite.check(
            "listener details expose the forwarded client IP",
            entry.get("addr") == "203.0.113.5",
            entry,
        )
        suite.check(
            "listener details expose a connection time",
            isinstance(entry.get("since"), int),
            entry,
        )
        suite.check(
            "listener details expose a numeric id",
            isinstance(entry.get("id"), int),
            entry,
        )

        public = sse_payload(open_sse(port))
        suite.check(
            "public SSE does not leak listener details",
            "listener_list" not in public,
            public,
        )

        anon = request(
            port,
            "POST",
            "/api/listeners/disconnect",
            body=json.dumps({"id": entry["id"]}).encode(),
            headers={"Content-Type": "application/json"},
        )
        suite.check(
            "disconnect requires a session", anon.status == 401, anon.text()
        )

        missing = request(
            port,
            "POST",
            "/api/listeners/disconnect",
            body=json.dumps({"id": 999999}).encode(),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check(
            "disconnecting an unknown listener is 404",
            missing.status == 404,
            missing.text(),
        )

        dropped = request(
            port,
            "POST",
            "/api/listeners/disconnect",
            body=json.dumps({"id": entry["id"]}).encode(),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("disconnect succeeds", dropped.status == 200, dropped.text())

        wait_until(
            lambda: status_json(port, cookie)["listeners"] == 0,
            timeout=10,
            message="the disconnected listener to be released",
        )
        suite.check("the disconnected listener disappears from the count", True)

        named.settimeout(5)
        closed = False
        try:
            while named.recv(65536):
                pass
            closed = True
        except socket.timeout:
            closed = False
        suite.check("disconnect closes the stream socket", closed)
    finally:
        named.close()

    suite.section("logout")

    response = request(port, "POST", "/api/logout", cookie=cookie)
    suite.check("logout succeeds", response.status == 200)
    suite.check(
        "logout expires the cookie",
        "Max-Age=0" in response.headers.get("set-cookie", ""),
    )
    response = request(port, "GET", "/api/me", cookie=cookie)
    suite.check("the logged-out token no longer works", response.status == 401)
    response = request(port, "GET", "/api/status", cookie=second_cookie)
    suite.check("other sessions survive logout", response.status == 200)

    suite.section("server log sanity")
    log = server.tail()
    suite.check("no panic in the server log", "panicked at" not in log, log[-400:])
