"""Public player, login, status, queue, and SSE coverage."""

import json
import stat

from support import *


def run(server, auth):
    port = server.port
    suite.section("static UI and public endpoints")

    response = request(port, "GET", "/")
    suite.check(
        "GET / is public HTML",
        response.status == 200
        and b'id="login-btn"' in response.body
        and b"Log in" in response.body,
        f"status={response.status}",
    )
    suite.check(
        "public player has now-playing and listener elements",
        b'id="title"' in response.body
        and b'id="artist"' in response.body
        and b'id="listeners"' in response.body
        and b"EventSource('/events')" in response.body,
    )
    suite.check(
        "login button links to the admin area",
        b'href="/admin"' in response.body,
    )
    suite.check(
        "public player does not contain the file manager",
        b'id="files"' not in response.body and b"Up next" not in response.body,
    )
    suite.check(
        "UI has the expected content type",
        "text/html" in response.headers.get("content-type", ""),
    )

    response = request(port, "GET", "/spectrum.wasm")
    suite.check(
        "spectrum wasm is embedded and served",
        response.status == 200
        and response.headers.get("content-type") == "application/wasm"
        and response.body.startswith(b"\0asm"),
        f"status={response.status}",
    )

    response = request(port, "GET", "/?anything=1")
    suite.check("query strings do not break routing", response.status == 200)

    for path in ["/admin", "/admin/", "/admin/files", "/admin/queue"]:
        response = request(port, "GET", path)
        suite.check(
            f"admin shell is served at {path}",
            response.status == 200
            and b'id="sidebar"' in response.body
            and b'data-nav="files"' in response.body
            and b'id="files-search"' in response.body
            and b'id="queue-search"' in response.body,
            f"status={response.status}",
        )

    response = request(port, "GET", "/auth.json")
    suite.check("auth.json is never served", response.status == 404, response.text())

    sock, first = open_stream(port)
    try:
        suite.check(
            "stream is public and is Ogg/Opus",
            b"200 OK" in first and b"audio/ogg" in first,
            first[:120],
        )
    finally:
        sock.close()

    public_sse = open_sse(port)
    suite.check("public SSE works without a session", b"data: " in public_sse)
    try:
        payload = sse_payload(public_sse)
        suite.check(
            "public SSE exposes listeners and now playing",
            "listeners" in payload and "now" in payload,
        )
        suite.check(
            "public SSE does not leak the queue",
            "queue" not in payload and "queue_total" not in payload,
            str(payload)[:200],
        )
    except Exception as exc:  # noqa: BLE001
        suite.check("public SSE payload is valid JSON", False, repr(exc))

    suite.section("unauthenticated API access")

    checks = [
        ("GET", "/admin/events"),
        ("GET", "/api/me"),
        ("GET", "/api/status"),
        ("GET", "/api/files"),
        ("GET", "/api/queue"),
    ]
    for method, path in checks:
        response = request(port, method, path)
        suite.check(f"{method} {path} is 401 without a session", response.status == 401)

    body, content_type = multipart(files=[("file", "x.wav", b"x", "audio/wav")])
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
    )
    suite.check("upload is 401 without a session", response.status == 401)

    response = request(
        port,
        "POST",
        "/api/delete",
        body=b'{"path":"x.wav"}',
        headers={"Content-Type": "application/json"},
    )
    suite.check("delete is 401 without a session", response.status == 401)

    response = request(port, "POST", "/api/logout")
    suite.check("logout is 401 without a session", response.status == 401)

    response = request(port, "GET", "/api/nope")
    suite.check("unknown API paths do not leak without a session", response.status == 401)

    suite.section("first login creates the only account")

    raw = auth.read_text()
    suite.check("auth.json is created on startup and empty", json.loads(raw) == {})

    response = login(port)
    suite.check(
        "first login succeeds and reports account creation",
        response.status == 200 and response.json().get("created") is True,
        response.text(),
    )
    set_cookie = response.headers.get("set-cookie", "")
    cookie = cookie_from(response)
    suite.check("session cookie is HttpOnly", "HttpOnly" in set_cookie, set_cookie)
    suite.check("session cookie is SameSite=Strict", "SameSite=Strict" in set_cookie)
    suite.check("session cookie is scoped to /", "Path=/" in set_cookie)
    suite.check("session cookie has a max age", "Max-Age=" in set_cookie)
    suite.check(
        "session token looks like 32 random bytes",
        len(cookie.split("=", 1)[1]) == 64,
        cookie,
    )

    raw = auth.read_text()
    stored = json.loads(raw)
    suite.check("auth.json contains exactly one username", stored.get("username") == USER)
    suite.check(
        "password is stored as an Argon2 hash",
        stored.get("password_hash", "").startswith("$argon2"),
    )
    suite.check("plaintext password is not in auth.json", PASSWORD not in raw)
    mode = stat.S_IMODE(os.stat(auth).st_mode)
    suite.check("auth.json is mode 0600", mode == 0o600, oct(mode))

    response = request(port, "GET", "/api/me", cookie=cookie)
    suite.check(
        "authenticated /api/me identifies the user",
        response.status == 200 and response.json().get("user") == USER,
    )

    response = login(port, USER, "wrong password")
    suite.check("wrong password is rejected", response.status == 401)

    response = login(port, "intruder", "another password")
    suite.check("a second account cannot be created", response.status == 401)
    stored_after = json.loads(auth.read_text())
    suite.check(
        "failed second-user login did not modify auth.json",
        stored_after.get("username") == USER,
    )

    response = login(port, USER, "")
    suite.check("empty password is rejected", response.status == 400)

    response = login(port, "", "x")
    suite.check("empty username is rejected", response.status == 400)

    response = request(
        port,
        "POST",
        "/api/login",
        body=b"{not json",
        headers={"Content-Type": "application/json"},
    )
    suite.check("malformed login JSON is rejected", response.status == 400)

    form = urllib.parse.urlencode({"username": USER, "password": PASSWORD}).encode()
    response = request(
        port,
        "POST",
        "/api/login",
        body=form,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    suite.check("form-encoded login also works", response.status == 200)
    second_cookie = cookie_from(response)

    response = login(port, USER, PASSWORD, {"Origin": "http://evil.example"})
    suite.check("cross-origin login is rejected", response.status == 403)

    forged = "radio_session=" + "a" * 64
    response = request(port, "GET", "/api/me", cookie=forged)
    suite.check("a forged session token is rejected", response.status == 401)

    response = request(port, "GET", "/api/me", cookie="radio_session=%2e%2e")
    suite.check("a malformed session token is rejected", response.status == 401)

    suite.section("status, queue and SSE")

    status = status_json(port, cookie)
    suite.check("status reports listeners as an integer", isinstance(status.get("listeners"), int))
    suite.check("status includes now and queue", "now" in status and isinstance(status.get("queue"), list))

    wait_until(
        lambda: status_json(port, cookie).get("now") is not None,
        timeout=25,
        message="the station to announce a track",
    )
    suite.check("currently playing track is announced", True)

    wait_until(
        lambda: len(status_json(port, cookie).get("queue", [])) > 0,
        timeout=25,
        message="the queue to fill",
    )
    suite.check("upcoming queue is exposed", True)

    sse = open_sse(port, cookie, "/admin/events")
    suite.check("authenticated admin SSE returns a data frame", b"data: " in sse, sse[:200])
    try:
        payload = sse_payload(sse)
        suite.check(
            "admin SSE payload has listeners/now/queue/total",
            "listeners" in payload
            and "now" in payload
            and "queue" in payload
            and "queue_total" in payload,
        )
        suite.check(
            "admin SSE hides the queue from unauthenticated clients",
            "queue_total" not in sse_payload(open_sse(port)),
        )
    except Exception as exc:  # noqa: BLE001
        suite.check("admin SSE payload is valid JSON", False, repr(exc))

    suite.section("queue pagination and search")

    queue_response = request(port, "GET", "/api/queue?per_page=2", cookie=cookie)
    suite.check("queue API is available", queue_response.status == 200, queue_response.text())
    queue_body = queue_response.json()
    suite.check(
        "queue API returns pagination metadata",
        all(key in queue_body for key in ("queue", "total", "page", "per_page", "pages")),
    )
    suite.check("queue page respects per_page", len(queue_body.get("queue", [])) <= 2)
    suite.check(
        "queue page count is correct",
        queue_body["pages"] == max(1, (queue_body["total"] + 1) // 2),
        str(queue_body)[:200],
    )

    empty_queue = request(port, "GET", "/api/queue?q=zzz-no-such-track", cookie=cookie).json()
    suite.check("queue search filters to nothing", empty_queue["total"] == 0 and empty_queue["queue"] == [])

    matching_queue = request(port, "GET", "/api/queue?q=Artist", cookie=cookie).json()
    suite.check(
        "queue search matches track text",
        matching_queue["total"] >= 1
        and matching_queue["total"] <= queue_body["total"],
        str(matching_queue)[:200],
    )

    last_queue = request(
        port, "GET", f"/api/queue?page={queue_body['pages']}&per_page=2", cookie=cookie
    )
    suite.check("last queue page is valid", last_queue.status == 200)
    return cookie, second_cookie
