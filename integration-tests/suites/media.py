"""File listing, uploads, downloads, renames, and deletes."""

import subprocess

from support import *


def run(server, tmp, music, auth, cookie):
    port = server.port
    suite.section("file listing and uploads")

    response = request(port, "GET", "/api/files", cookie=cookie)
    listing = response.json()
    files = listing.get("files", [])
    suite.check("file list is available", response.status == 200)
    suite.check("startup media is listed", len(files) >= 6, f"count={len(files)}")
    suite.check("all startup files are music", all(f.get("kind") == "music" for f in files))
    suite.check(
        "file entries contain path/size",
        all("path" in f and f.get("size", 0) > 0 for f in files),
    )
    suite.check(
        "file API returns pagination metadata",
        all(key in listing for key in ("total", "page", "per_page", "pages")),
    )

    paged = request(port, "GET", "/api/files?per_page=2", cookie=cookie).json()
    suite.check("file pagination limits the page", len(paged["files"]) <= 2)
    suite.check(
        "file page count is correct",
        paged["pages"] == max(1, (paged["total"] + 1) // 2),
        str(paged)[:200],
    )

    search = request(port, "GET", "/api/files?q=Artist0", cookie=cookie).json()
    suite.check(
        "file search matches paths",
        search["total"] == 2 and all("Artist0" in f["path"] for f in search["files"]),
        str(search)[:200],
    )

    jingles = request(port, "GET", "/api/files?kind=jingle", cookie=cookie).json()
    suite.check("kind filter can select an empty jingle set", jingles["total"] == 0)

    bad_kind = request(port, "GET", "/api/files?kind=video", cookie=cookie)
    suite.check("invalid kind filters are rejected", bad_kind.status == 400)

    clamped = request(port, "GET", "/api/files?page=999&per_page=2", cookie=cookie).json()
    suite.check("out-of-range file pages clamp to the last page", clamped["page"] == clamped["pages"])

    uploaded = wav_bytes(2.0, 330.0)
    body, content_type = multipart(
        fields=[("target", "music")],
        files=[("file", "uploaded - song.wav", uploaded, "audio/wav")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check(
        "valid upload succeeds",
        response.status == 201 and response.json().get("path") == "uploaded - song.wav",
        response.text(),
    )
    suite.check(
        "new upload is not reported as an overwrite",
        response.json().get("overwritten") is False,
        response.text(),
    )
    suite.check("uploaded file is on disk", (music / "uploaded - song.wav").is_file())

    names = [f["path"] for f in request(port, "GET", "/api/files", cookie=cookie).json()["files"]]
    suite.check("uploaded file appears in the listing", "uploaded - song.wav" in names)

    wait_until(
        lambda: "uploaded - song.wav" in playing_files(port, cookie),
        timeout=25,
        message="the uploaded file to enter the playlist",
    )
    suite.check("upload notifies the station and changes the queue", True)

    replacement = wav_bytes(2.0, 550.0)
    replacement_body, replacement_type = multipart(
        fields=[("target", "music")],
        files=[("file", "uploaded - song.wav", replacement, "audio/wav")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=replacement_body,
        headers={"Content-Type": replacement_type},
        cookie=cookie,
    )
    suite.check("duplicate upload succeeds", response.status == 201, response.text())
    suite.check(
        "duplicate upload reports the overwrite",
        response.json().get("overwritten") is True,
        response.text(),
    )
    suite.check(
        "duplicate upload overwrote the file on disk",
        (music / "uploaded - song.wav").read_bytes() == replacement,
    )

    body, content_type = multipart(
        files=[("file", "../../evil.wav", uploaded, "audio/wav")]
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("path traversal in the upload name is neutralised", response.status == 201)
    suite.check("traversal file lands in the music directory", (music / "evil.wav").is_file())
    suite.check(
        "traversal file does not escape the music directory",
        not (Path(tmp) / "evil.wav").exists() and not (Path(tmp).parent / "evil.wav").exists(),
    )

    body, content_type = multipart(
        files=[("file", r"C:\fakepath\windows.wav", uploaded, "audio/wav")]
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("Windows-style upload path is reduced to a basename", response.status == 201)
    suite.check("basename was written into the music directory", (music / "windows.wav").is_file())

    body, content_type = multipart(files=[("file", "notes.txt", b"hello", "text/plain")])
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("non-media upload is rejected", response.status == 415)
    suite.check("rejected upload did not create a file", not (music / "notes.txt").exists())

    body, content_type = multipart(
        files=[("file", "broken.wav", b"RIFFnot really a wave file", "audio/wav")]
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("invalid media content is rejected", response.status == 415)
    suite.check("invalid upload left no file behind", not (music / "broken.wav").exists())
    suite.check(
        "invalid upload left no temporary file behind",
        not list(music.glob(".upload-*.part")),
        str(list(music.glob(".upload-*.part"))),
    )

    body, content_type = multipart(
        fields=[("target", "jingles")],
        files=[("file", "station id.wav", uploaded, "audio/wav")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("jingle upload succeeds", response.status == 201, response.text())
    suite.check("jingle is stored in the jingles directory", (music / "jingles" / "station id.wav").is_file())
    listing = request(port, "GET", "/api/files", cookie=cookie).json()["files"]
    suite.check(
        "jingle is listed with kind=jingle",
        any(f["path"] == "jingles/station id.wav" and f["kind"] == "jingle" for f in listing),
    )

    flac_wav = Path(tmp) / "flac-source.wav"
    flac_file = Path(tmp) / "flac-source.flac"
    write_wav(flac_wav, 1.0, 440.0)
    subprocess.run(
        ["flac", "--silent", "--force", "-o", str(flac_file), str(flac_wav)],
        check=True,
    )
    body, content_type = multipart(
        fields=[("target", "music")],
        files=[("file", "uploaded - lossless.flac", flac_file.read_bytes(), "audio/flac")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("FLAC upload succeeds", response.status == 201, response.text())
    suite.check("FLAC file is on disk", (music / "uploaded - lossless.flac").is_file())
    listing = request(port, "GET", "/api/files", cookie=cookie).json()["files"]
    suite.check(
        "FLAC file appears in the listing",
        any(f["path"] == "uploaded - lossless.flac" for f in listing),
    )

    body, content_type = multipart(
        fields=[("target", "music")],
        files=[("file", "silent - file.wav", silent_wav_bytes(), "audio/wav")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("silent WAV upload succeeds", response.status == 201, response.text())
    suite.check("silent WAV is on disk", (music / "silent - file.wav").is_file())

    body, content_type = multipart(
        files=[("file", "broken - lossless.flac", b"fLaCnot really", "audio/flac")]
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("invalid FLAC content is rejected", response.status == 415)
    suite.check("invalid FLAC left no file behind", not (music / "broken - lossless.flac").exists())

    for name in ["uploaded - lossless.flac", "silent - file.wav"]:
        request(
            port,
            "POST",
            "/api/delete",
            body=json.dumps({"path": name}).encode(),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )

    body, content_type = multipart(
        fields=[("target", "../../escape")],
        files=[("file", "bad.wav", uploaded, "audio/wav")],
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("invalid upload target is rejected", response.status == 400)

    body, content_type = multipart(
        files=[
            ("file", "one.wav", uploaded, "audio/wav"),
            ("file", "two.wav", uploaded, "audio/wav"),
        ]
    )
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("multiple files in one upload are rejected", response.status == 400)

    body, content_type = multipart(fields=[("target", "music")])
    response = request(
        port,
        "POST",
        "/api/upload",
        body=body,
        headers={"Content-Type": content_type},
        cookie=cookie,
    )
    suite.check("upload without a file is rejected", response.status == 400)

    huge = (
        f"POST /api/upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        f"Cookie: {cookie}\r\nContent-Length: 999999999\r\n"
        "Content-Type: multipart/form-data; boundary=x\r\n\r\n"
    ).encode()
    response = raw_exchange(port, huge)
    suite.check("oversized bodies are rejected with 413", b"413" in response, response[:100])

    huge_login = (
        f"POST /api/login HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        "Content-Length: 100000\r\nContent-Type: application/json\r\n\r\n"
    ).encode()
    response = raw_exchange(port, huge_login)
    suite.check(
        "oversized login bodies are rejected without being read",
        b"413" in response,
        response[:100],
    )

    huge_delete = (
        f"POST /api/delete HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        f"Cookie: {cookie}\r\nContent-Length: 100000\r\n"
        "Content-Type: application/json\r\n\r\n"
    ).encode()
    response = raw_exchange(port, huge_delete)
    suite.check(
        "oversized delete bodies are rejected without being read",
        b"413" in response,
        response[:100],
    )

    suite.section("downloads and renames")

    download_path = "/api/download?path=" + urllib.parse.quote("windows.wav")
    response = request(port, "GET", download_path)
    suite.check("download requires a session", response.status == 401)

    response = request(port, "GET", download_path, cookie=cookie)
    suite.check("download succeeds", response.status == 200, response.text()[:200])
    suite.check("download body matches the uploaded file", response.body == uploaded)
    suite.check(
        "download is served as an attachment",
        response.headers.get("content-disposition", "").startswith("attachment;"),
        response.headers.get("content-disposition", ""),
    )
    suite.check(
        "download uses the media content type",
        response.headers.get("content-type") == "audio/wav",
        response.headers.get("content-type", ""),
    )

    response = request(port, "GET", "/api/download?path=missing.wav", cookie=cookie)
    suite.check("downloading a missing file is 404", response.status == 404)
    response = request(
        port,
        "GET",
        "/api/download?path=" + urllib.parse.quote("../auth.json"),
        cookie=cookie,
    )
    suite.check("download path traversal is rejected", response.status == 400)
    suite.check("auth.json survived the download traversal attempt", auth.is_file())

    def rename_file(path, name):
        return post_rename(port, cookie, path, name)

    # Rename a track that is already published. Waiting for a fresh upload
    # depends on when the prefetch thread drains its command channel, which
    # can be far behind the encoder.
    status = status_json(port, cookie)
    tracked = [
        item["file"]
        for item in status.get("queue", [])
        if item["file"] != "evil.wav"
        and item["file"].endswith(".wav")
        and (music / item["file"]).is_file()
    ]
    if not tracked and status.get("now"):
        current = status["now"]["file"]
        if current != "evil.wav" and current.endswith(".wav") and (music / current).is_file():
            tracked = [current]
    assert tracked, "rename test needs a published music file"
    rename_old = tracked[-1]

    response = rename_file(rename_old, "renamed - song.wav")
    suite.check(
        "rename succeeds",
        response.status == 200 and response.json().get("path") == "renamed - song.wav",
        response.text(),
    )
    suite.check("renamed file is on disk", (music / "renamed - song.wav").is_file())
    suite.check("old file name is gone from disk", not (music / rename_old).exists())
    names = [f["path"] for f in request(port, "GET", "/api/files", cookie=cookie).json()["files"]]
    suite.check("renamed file appears in the listing", "renamed - song.wav" in names)
    suite.check("old name disappears from the listing", rename_old not in names)

    def renamed_visible():
        files = playing_files(port, cookie)
        return "renamed - song.wav" in files and rename_old not in files

    wait_until(
        renamed_visible,
        timeout=10,
        message="the renamed file to replace the old name in the queue",
    )
    suite.check("rename notifies the station and updates the queue", True)

    response = rename_file("renamed - song.wav", "evil.wav")
    suite.check("renaming onto an existing file is rejected", response.status == 409)
    response = rename_file("renamed - song.wav", "renamed - song.flac")
    suite.check("rename cannot change the file extension", response.status == 400)
    response = rename_file("does-not-exist.wav", "other.wav")
    suite.check("renaming a missing file is 404", response.status == 404)
    response = rename_file("../auth.json", "other.json")
    suite.check("rename path traversal is rejected", response.status == 400)

    (music / "rename-me.txt").write_text("not music")
    response = rename_file("rename-me.txt", "rename-me.wav")
    suite.check("renaming non-media is rejected", response.status == 415)
    suite.check("non-media file was not renamed", (music / "rename-me.txt").exists())
    (music / "rename-me.txt").unlink()

    response = rename_file("renamed - song.wav", rename_old)
    suite.check("renaming back succeeds", response.status == 200, response.text())
    suite.check("restored file is on disk", (music / rename_old).is_file())

    suite.section("deletes")

    response = request(
        port,
        "POST",
        "/api/delete",
        body=json.dumps({"path": "uploaded - song.wav"}).encode(),
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )
    suite.check("delete succeeds", response.status == 200, response.text())
    suite.check("file is gone from disk", not (music / "uploaded - song.wav").exists())
    names = [f["path"] for f in request(port, "GET", "/api/files", cookie=cookie).json()["files"]]
    suite.check("deleted file disappears from the listing", "uploaded - song.wav" not in names)

    wait_until(
        lambda: "uploaded - song.wav" not in playing_files(port, cookie),
        timeout=25,
        message="the deleted file to leave the playlist",
    )
    suite.check("delete notifies the station and removes it from the queue", True)

    response = request(
        port,
        "POST",
        "/api/delete",
        body=b'{"path":"does-not-exist.wav"}',
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )
    suite.check("deleting a missing file is 404", response.status == 404)

    response = request(
        port,
        "POST",
        "/api/delete",
        body=b'{"path":"../auth.json"}',
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )
    suite.check("delete path traversal is rejected", response.status == 400)
    suite.check("auth.json survived the traversal attempt", auth.is_file())

    response = request(
        port,
        "POST",
        "/api/delete",
        body=b'{"path":"/etc/passwd"}',
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )
    suite.check("absolute delete paths are rejected", response.status == 400)

    (music / "notes.txt").write_text("not music")
    response = request(
        port,
        "POST",
        "/api/delete",
        body=json.dumps({"path": "notes.txt"}).encode(),
        headers={"Content-Type": "application/json"},
        cookie=cookie,
    )
    suite.check("deleting non-media is rejected", response.status == 415)
    suite.check("non-media file was not deleted", (music / "notes.txt").exists())
    (music / "notes.txt").unlink()

    symlink = music / "sneaky.wav"
    try:
        os.symlink(auth, symlink)
    except OSError:
        symlink = None
    if symlink is not None:
        response = request(
            port,
            "POST",
            "/api/delete",
            body=b'{"path":"sneaky.wav"}',
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("symlink deletion is refused", response.status == 403)
        suite.check("symlink target is untouched", auth.is_file())
        symlink.unlink()

    outside = Path(tmp) / "outside"
    outside.mkdir(exist_ok=True)
    write_wav(outside / "outside.wav", 0.2, 500.0)
    link_dir = music / "outlink"
    try:
        os.symlink(outside, link_dir)
    except OSError:
        link_dir = None
    if link_dir is not None:
        response = request(
            port,
            "POST",
            "/api/delete",
            body=b'{"path":"outlink/outside.wav"}',
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
        suite.check("deleting through a symlinked directory is refused", response.status == 403)
        suite.check("outside file is untouched", (outside / "outside.wav").exists())
        listing = request(port, "GET", "/api/files", cookie=cookie).json()["files"]
        suite.check(
            "symlinked directories are not listed",
            not any(f["path"].startswith("outlink/") for f in listing),
        )
        link_dir.unlink()

    # Clean up the remaining upload fixtures.
    for name in ["evil.wav", "windows.wav", "jingles/station id.wav"]:
        request(
            port,
            "POST",
            "/api/delete",
            body=json.dumps({"path": name}).encode(),
            headers={"Content-Type": "application/json"},
            cookie=cookie,
        )
