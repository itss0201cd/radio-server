#!/usr/bin/env python3
"""End-to-end and security tests for radio-server.

These live outside the Rust source on purpose: the server gets exercised the
same way a browser or an attacker would, over a real TCP socket.

Run:
    python3 integration-tests/run_tests.py

Environment:
    RADIO_BIN=/path/to/radio-station   skip the cargo build and use this binary
    RADIO_SKIP_BUILD=1                 use the existing release binary
"""

import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from support import BIN, ROOT, suite, write_wav
from suites.auth import concurrency_suite
from suites.lifecycle import empty_library_suite, invalid_auth_suite, restart_suite
from suites.main import main_server_suite
from suites.settings import settings_suite


def build():
    if os.environ.get("RADIO_SKIP_BUILD") or os.environ.get("RADIO_BIN"):
        if not BIN.is_file():
            raise SystemExit(f"binary not found: {BIN}")
        return
    print(f"building {BIN} ...")
    result = subprocess.run(
        ["cargo", "build", "--release"],
        cwd=str(ROOT),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if result.returncode != 0:
        print(result.stdout)
        raise SystemExit("cargo build failed")
    if not BIN.is_file():
        raise SystemExit(f"binary not found after build: {BIN}")


def main():
    build()
    base = Path(tempfile.mkdtemp(prefix="radio-tests-"))
    print(f"test root: {base}")

    main_tmp = base / "main"
    music = main_tmp / "music"
    music.mkdir(parents=True)
    auth = main_tmp / "auth.json"
    for i in range(6):
        write_wav(music / f"Artist{i % 3} - Song {i}.wav", 2.0, 220 + i * 35)

    try:
        main_server_suite(main_tmp, music, auth)
        restart_suite(main_tmp, music, auth)

        empty_tmp = base / "empty"
        empty_tmp.mkdir()
        empty_library_suite(empty_tmp)

        bad_tmp = base / "bad-auth"
        bad_tmp.mkdir()
        invalid_auth_suite(bad_tmp)

        concurrent_tmp = base / "concurrent"
        concurrent_tmp.mkdir()
        concurrency_suite(concurrent_tmp)

        settings_tmp = base / "settings"
        settings_tmp.mkdir()
        settings_suite(settings_tmp)
    finally:
        if not os.environ.get("RADIO_KEEP_TESTS"):
            shutil.rmtree(base, ignore_errors=True)
        else:
            print(f"kept test root: {base}")

    sys.exit(suite.summary())


if __name__ == "__main__":
    main()
