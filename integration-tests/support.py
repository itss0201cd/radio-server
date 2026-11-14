#!/usr/bin/env python3
"""Shared harness for the radio-server integration suites.

The suites import this module from ``run_tests.py``. It owns the process
lifecycle, HTTP/socket client, media fixtures, and the pass/fail counter.
"""

import http.client
import json
import math
import os
import socket
import struct
import subprocess
import time
import traceback
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = Path(os.environ.get("RADIO_BIN", ROOT / "target" / "release" / "radio-station"))
STREAM_PATH = "/radio.opus"
USER = "admin"
PASSWORD = "correct horse battery staple"

# --------------------------------------------------------------------------
# test bookkeeping
# --------------------------------------------------------------------------


class Suite:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.failures = []
        self.section_name = ""

    def section(self, name):
        self.section_name = name
        print(f"\n== {name} ==")

    def check(self, name, condition, detail=""):
        if condition:
            self.passed += 1
            print(f"  PASS  {name}")
        else:
            self.failed += 1
            label = f"{self.section_name} / {name}" if self.section_name else name
            self.failures.append((label, detail))
            print(f"  FAIL  {name}" + (f"  ({detail})" if detail else ""))

    def test(self, name, fn):
        try:
            fn()
            self.check(name, True)
        except Exception as exc:  # noqa: BLE001 - report, keep going
            self.check(name, False, f"{type(exc).__name__}: {exc}")
            if os.environ.get("RADIO_TEST_TRACE"):
                traceback.print_exc()

    def summary(self):
        print("\n" + "=" * 60)
        print(f"{self.passed} passed, {self.failed} failed")
        for label, detail in self.failures:
            print(f"  FAILED: {label}" + (f"  ({detail})" if detail else ""))
        print("=" * 60)
        return 0 if self.failed == 0 else 1


suite = Suite()


# --------------------------------------------------------------------------
# tiny HTTP client
# --------------------------------------------------------------------------


class Response:
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = {k.lower(): v for k, v in headers}
        self.body = body

    def json(self):
        return json.loads(self.body)

    def text(self):
        return self.body.decode("utf-8", "replace")


def request(port, method, path, body=None, headers=None, cookie=None, timeout=20):
    headers = dict(headers or {})
    if cookie:
        headers["Cookie"] = cookie
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    try:
        conn.request(method, path, body=body, headers=headers)
        raw = conn.getresponse()
        data = raw.read()
        return Response(raw.status, raw.getheaders(), data)
    finally:
        conn.close()


def cookie_from(response):
    raw = response.headers.get("set-cookie", "")
    return raw.split(";", 1)[0] if raw else ""


def login(port, username=USER, password=PASSWORD, extra_headers=None):
    body = json.dumps({"username": username, "password": password}).encode()
    headers = {"Content-Type": "application/json"}
    headers.update(extra_headers or {})
    return request(port, "POST", "/api/login", body=body, headers=headers)


def auth_cookie(port, username=USER, password=PASSWORD):
    response = login(port, username, password)
    assert response.status == 200, f"login failed: {response.status} {response.text()}"
    return cookie_from(response)


# --------------------------------------------------------------------------
# raw sockets
# --------------------------------------------------------------------------


def raw_exchange(port, payload, timeout=8):
    sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    try:
        sock.sendall(payload)
        sock.settimeout(timeout)
        out = b""
        while True:
            try:
                chunk = sock.recv(8192)
            except socket.timeout:
                break
            if not chunk:
                break
            out += chunk
        return out
    finally:
        sock.close()


def open_stream(port, headers=None, path=STREAM_PATH):
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)
    host = f"127.0.0.1:{port}"
    extra = "".join(f"{key}: {value}\r\n" for key, value in (headers or {}).items())
    sock.sendall(f"GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}\r\n".encode())
    sock.settimeout(10)
    first = sock.recv(4096)
    return sock, first


def open_sse(port, cookie=None, path="/events"):
    sock = socket.create_connection(("127.0.0.1", port), timeout=10)
    host = f"127.0.0.1:{port}"
    headers = f"GET {path} HTTP/1.1\r\nHost: {host}\r\n"
    if cookie:
        headers += f"Cookie: {cookie}\r\n"
    sock.sendall((headers + "\r\n").encode())
    sock.settimeout(10)
    data = b""
    while b"\n\n" not in data and len(data) < 65536:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    sock.close()
    return data


def sse_payload(data):
    line = [ln for ln in data.split(b"\n") if ln.startswith(b"data: ")][0]
    return json.loads(line[6:])


# --------------------------------------------------------------------------
# media fixtures
# --------------------------------------------------------------------------


def wav_bytes(seconds=2.0, freq=440.0):
    rate = 48000
    frames = int(rate * seconds)
    out = bytearray()
    for i in range(frames):
        value = int(math.sin(2 * math.pi * freq * i / rate) * 0.2 * 32767)
        out += struct.pack("<hh", value, value)
    buf = bytearray()
    data_len = len(out)
    buf += b"RIFF"
    buf += struct.pack("<I", 36 + data_len)
    buf += b"WAVEfmt "
    buf += struct.pack("<IHHIIHH", 16, 1, 2, rate, rate * 4, 4, 16)
    buf += b"data"
    buf += struct.pack("<I", data_len)
    buf += out
    return bytes(buf)


def write_wav(path, seconds=2.0, freq=440.0):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(wav_bytes(seconds, freq))


def silent_wav_bytes(seconds=0.25):
    rate = 48000
    data_len = int(rate * seconds) * 4
    buf = bytearray()
    buf += b"RIFF"
    buf += struct.pack("<I", 36 + data_len)
    buf += b"WAVEfmt "
    buf += struct.pack("<IHHIIHH", 16, 1, 2, rate, rate * 4, 4, 16)
    buf += b"data"
    buf += struct.pack("<I", data_len)
    buf += b"\x00" * data_len
    return bytes(buf)


def multipart(fields=None, files=None):
    boundary = "----radiotest" + os.urandom(8).hex()
    chunks = []
    for name, value in fields or []:
        chunks.append(
            f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()
        )
    for name, filename, content, content_type in files or []:
        chunks.append(
            (
                f'--{boundary}\r\n'
                f'Content-Disposition: form-data; name="{name}"; filename="{filename}"\r\n'
                f"Content-Type: {content_type}\r\n\r\n"
            ).encode()
        )
        chunks.append(content)
        chunks.append(b"\r\n")
    chunks.append(f"--{boundary}--\r\n".encode())
    return b"".join(chunks), f"multipart/form-data; boundary={boundary}"


# --------------------------------------------------------------------------
# server process
# --------------------------------------------------------------------------


def free_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


class Server:
    def __init__(self, tmp, music, auth, expect_start=True):
        self.tmp = Path(tmp)
        self.music = Path(music)
        self.auth = Path(auth)
        self.port = free_port()
        self.log_path = self.tmp / f"server-{self.port}.log"
        self.log = open(self.log_path, "wb")
        env = dict(os.environ)
        env["RADIO_AUTH_FILE"] = str(self.auth)
        self.proc = subprocess.Popen(
            [str(BIN), str(self.music), str(self.port)],
            cwd=str(self.tmp),
            env=env,
            stdout=self.log,
            stderr=subprocess.STDOUT,
        )
        if expect_start:
            self.wait_ready()

    def base(self):
        return f"http://127.0.0.1:{self.port}"

    def wait_ready(self, timeout=20):
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(
                    f"server exited early ({self.proc.returncode}):\n{self.tail()}"
                )
            try:
                response = request(self.port, "GET", "/", timeout=2)
                if response.status == 200:
                    return
                last = response.status
            except Exception as exc:  # noqa: BLE001
                last = exc
            time.sleep(0.1)
        raise RuntimeError(f"server did not become ready: {last}\n{self.tail()}")

    def tail(self, limit=8000):
        try:
            self.log.flush()
        except Exception:  # noqa: BLE001
            pass
        try:
            data = self.log_path.read_text(errors="replace")
        except Exception:  # noqa: BLE001
            return ""
        return data[-limit:]

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        try:
            self.log.close()
        except Exception:  # noqa: BLE001
            pass


def wait_until(predicate, timeout=20, interval=0.25, message="condition"):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            value = predicate()
            if value:
                return value
            last = value
        except Exception as exc:  # noqa: BLE001
            last = exc
        time.sleep(interval)
    raise AssertionError(f"timed out waiting for {message} (last={last!r})")


def status_json(port, cookie):
    response = request(port, "GET", "/api/status", cookie=cookie)
    assert response.status == 200, f"status {response.status}: {response.text()}"
    return response.json()


def playing_files(port, cookie):
    status = status_json(port, cookie)
    files = [item["file"] for item in status.get("queue", [])]
    if status.get("now"):
        files.append(status["now"]["file"])
    return files

def json_post(port, path, payload, cookie=None):
    return request(
        port,
        "POST",
        path,
        body=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )


def post_upload(port, cookie, filename, content, content_type="audio/wav", target=None):
    fields = [("target", target)] if target else []
    body, content_type_header = multipart(
        fields=fields, files=[("file", filename, content, content_type)]
    )
    return request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type_header},
        cookie=cookie,
    )


def post_delete(port, cookie, path):
    return json_post(port, "/api/delete", {"path": path}, cookie)


def post_rename(port, cookie, path, name):
    return json_post(port, "/api/rename", {"path": path, "name": name}, cookie)


def list_files(port, cookie, q="", kind="all", page=1, per_page=50):
    query = urllib.parse.urlencode(
        {"q": q, "kind": kind, "page": page, "per_page": per_page}
    )
    return request(port, "GET", f"/api/files?{query}", cookie=cookie)
