"""Runtime endpoint-settings suite."""

import json

from support import *


def settings_suite(tmp):
    suite.section("runtime settings")
    music = Path(tmp) / "music"
    music.mkdir()
    write_wav(music / "Track.wav", 2.0, 440.0)
    settings_path = Path(tmp) / "settings.json"
    auth = Path(tmp) / "auth.json"

    server = Server(tmp, music, auth)
    try:
        suite.check("settings file is created on startup", settings_path.is_file())
        stored = json.loads(settings_path.read_text())
        suite.check(
            "default stream path is /radio.opus",
            stored.get("stream_path") == "/radio.opus",
            str(stored),
        )
        suite.check(
            "default playlist path is /radio.m3u8",
            stored.get("playlist_path") == "/radio.m3u8",
            str(stored),
        )
        suite.check("HLS defaults to enabled", stored.get("hls_enabled") is True, str(stored))

        anonymous = request(server.port, "GET", "/api/settings")
        suite.check("settings require a session", anonymous.status == 401)

        cookie = auth_cookie(server.port)
        current = request(server.port, "GET", "/api/settings", cookie=cookie)
        suite.check(
            "settings are readable with a session",
            current.status == 200 and current.json().get("stream_path") == "/radio.opus",
            current.text(),
        )

        bad = request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps(
                {
                    "stream_path": "/api/radio.opus",
                    "playlist_path": "/radio.m3u8",
                    "hls_enabled": True,
                }
            ),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("reserved endpoint paths are rejected", bad.status == 400, bad.text())

        bad_suffix = request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps(
                {
                    "stream_path": "/radio.opus",
                    "playlist_path": "/radio.m3u",
                    "hls_enabled": True,
                }
            ),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("endpoint extensions are enforced", bad_suffix.status == 400, bad_suffix.text())

        partial = request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps({"stream_path": "/radio.opus"}),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("partial settings updates are rejected", partial.status == 400, partial.text())

        custom = {
            "stream_path": "/live/audio.opus",
            "playlist_path": "/live/hls.m3u8",
            "hls_enabled": False,
        }
        updated = request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps(custom),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("settings update succeeds", updated.status == 200, updated.text())
        suite.check(
            "settings file was rewritten",
            json.loads(settings_path.read_text()) == custom,
            settings_path.read_text(),
        )
        suite.check(
            "the old stream path is gone",
            request(server.port, "GET", "/radio.opus").status == 404,
        )

        sock, first = open_stream(server.port, path="/live/audio.opus")
        sock.close()
        suite.check("the custom stream path serves", b"200 OK" in first, first[:200])

        player = request(server.port, "GET", "/")
        suite.check(
            "the player uses the custom stream path",
            "/live/audio.opus" in player.text(),
        )
        admin_page = request(server.port, "GET", "/admin/settings")
        suite.check(
            "the settings page is served",
            admin_page.status == 200 and "settings-form" in admin_page.text(),
        )
        suite.check(
            "a disabled HLS playlist is not routed",
            request(server.port, "GET", "/live/hls.m3u8").status == 404,
        )

        enabled = dict(custom, hls_enabled=True)
        response = request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps(enabled),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("HLS can be re-enabled", response.status == 200, response.text())

        def playlist_ready():
            response = request(server.port, "GET", "/live/hls.m3u8")
            return response if response.status == 200 else None

        playlist = wait_until(playlist_ready, timeout=35, message="the HLS playlist")
        text = playlist.text()
        suite.check("playlist has a media sequence", "#EXT-X-MEDIA-SEQUENCE:" in text, text)
        lines = [line for line in text.splitlines() if line and not line.startswith("#")]
        suite.check(
            "playlist segments are relative to the playlist directory",
            bool(lines) and all(line.startswith("hls-") for line in lines),
            text,
        )
        if lines:
            segment = request(server.port, "GET", "/live/" + lines[-1])
            suite.check(
                "configured HLS segments serve TS bytes",
                segment.status == 200 and segment.body[:1] == b"\x47",
                f"{segment.status} {segment.body[:16]!r}",
            )

        off = dict(enabled, hls_enabled=False)
        request(
            server.port,
            "POST",
            "/api/settings",
            body=json.dumps(off),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check(
            "HLS can be disabled again",
            request(server.port, "GET", "/live/hls.m3u8").status == 404,
        )

        server.stop()
        server = Server(tmp, music, auth)
        cookie = auth_cookie(server.port)
        persisted = request(server.port, "GET", "/api/settings", cookie=cookie)
        suite.check(
            "settings survive a restart",
            persisted.status == 200 and persisted.json() == off,
            persisted.text(),
        )
    finally:
        server.stop()
