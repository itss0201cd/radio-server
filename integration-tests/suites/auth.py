"""Concurrent account-creation suite."""

import json
import threading

from support import *


def concurrency_suite(tmp):
    suite.section("concurrent account creation")
    music = Path(tmp) / "music"
    music.mkdir()
    auth = Path(tmp) / "auth.json"

    server = Server(tmp, music, auth)
    try:
        results = {}

        def attempt(username, password):
            try:
                results[username] = login(server.port, username, password)
            except Exception as exc:  # noqa: BLE001
                results[username] = exc

        threads = [
            threading.Thread(target=attempt, args=("alice", "alice-password")),
            threading.Thread(target=attempt, args=("bob", "bob-password")),
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=20)

        statuses = {
            name: (value.status if isinstance(value, Response) else repr(value))
            for name, value in results.items()
        }
        ok = [name for name, value in results.items() if isinstance(value, Response) and value.status == 200]
        suite.check(
            "exactly one concurrent first login creates the account",
            len(ok) == 1,
            str(statuses),
        )
        suite.check(
            "the other concurrent first login is rejected",
            all(
                isinstance(value, Response) and value.status == 401
                for name, value in results.items()
                if name not in ok
            ),
            str(statuses),
        )

        stored = json.loads(auth.read_text())
        winner = ok[0] if ok else None
        suite.check(
            "auth.json contains only the winning user",
            winner is not None and stored.get("username") == winner,
            f"stored={stored.get('username')} winner={winner}",
        )
        if winner:
            password = "alice-password" if winner == "alice" else "bob-password"
            suite.check(
                "the winning credentials work",
                login(server.port, winner, password).status == 200,
            )
    finally:
        server.stop()
