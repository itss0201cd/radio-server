"""Orchestrates the populated-server suites."""

from support import Server

from .http import run as run_http
from .media import run as run_media
from .public import run as run_public


def main_server_suite(tmp, music, auth):
    server = Server(tmp, music, auth)
    try:
        cookie, second_cookie = run_public(server, auth)
        run_media(server, tmp, music, auth, cookie)
        run_http(server, cookie, second_cookie)
    finally:
        server.stop()
