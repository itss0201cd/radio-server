# HTTP server

`src/http.rs` implements HTTP/1.1 directly on `TcpStream`. It does one request per connection and only handles `GET` and `POST`. There is no framework, keep-alive, chunked request body support, HEAD, range support, or compression. Every response closes the connection. HTML, JSON, downloads, and SSE set `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`. The stream uses `Cache-Control: no-cache` and `nosniff`. Each connection gets one Tokio task. Anything other than `GET` or `POST` returns 405.

The server binds to `127.0.0.1`. `client_addr` reads the first `X-Forwarded-For` entry, then `X-Real-IP`. The trust boundary is the loopback interface and a same-host reverse proxy.

## Routes

```text
GET  /                              player HTML
GET  /spectrum.wasm                 embedded Wasm visualiser
GET  /radio.opus                    default live Ogg Opus stream (configurable)
GET  /radio.m3u8                    default HLS media playlist (configurable)
GET  /radio-N.ts                    default HLS MPEG-TS segment
GET  /events                        public SSE status
GET  /admin                         admin shell
GET  /admin/files                   admin shell
GET  /admin/queue                   admin shell
GET  /admin/listeners               admin shell
GET  /admin/settings                admin shell
GET  /admin/events                  authenticated SSE status

POST /api/login                     create the account or log in
POST /api/logout                    destroy the session
GET  /api/me                        current session
GET  /api/status                    admin status JSON
GET  /api/settings                  current endpoint settings
POST /api/settings                  validate, persist, and apply settings
GET  /api/queue                     paginated queue
GET  /api/files                     paginated media listing
GET  /api/download                  download a media file
POST /api/upload                    multipart upload
POST /api/delete                    delete a media file
POST /api/rename                    rename a media file
POST /api/listeners/disconnect      drop one stream connection
```

The player, the wasm module, `/events`, the Ogg stream, and the HLS playlist and segments are public. The admin HTML shells are public too. They contain no server data and load the login form. Every route under `/api/` except `/api/login`, plus `/admin/events`, requires a valid `radio_session` cookie.

The HLS playlist lists up to two completed five-second MPEG-TS segments. It returns 404 until the first segment exists, and 404 again once a segment has fallen out of the ring. HLS clients are not listeners and cannot apply backpressure. The playlist is `no-store`. Segments are immutable by sequence number and use `Cache-Control: public, max-age=10`. HLS can be disabled in settings. While it is disabled the playlist and segment routes are not registered, and the station does not build segments.

All `POST` requests pass an origin check first. If `Origin` is present, its scheme must be `http` or `https` and its authority must equal `Host`, ignoring a trailing slash and case. A missing `Origin` is accepted. `SameSite=Strict` on the session cookie is the second layer.

## Request limits

The head must arrive within 10 seconds and is capped at 16 KiB. At most 100 headers are accepted, each line at most 8192 bytes. HTTP/1.1 requires `Host`. Duplicate `Content-Length` or `Host` headers are rejected. `Transfer-Encoding` is accepted only as `identity`, and only when `Content-Length` is absent. Every route that reads a body requires `Content-Length`. Without it the response is 411. `/api/logout` does not read its body. `Expect: 100-continue` is honoured before the body is read. Bodies time out after five minutes in total.

The global body cap is 512 MiB. Login, settings, delete, rename, and disconnect cap their small JSON bodies at 16 KiB. The query string is capped at 4 KiB and the `q` parameter at 200 characters. Pagination parameters are parsed as `usize`. Invalid values return 400.

## Status frames

`Status` is rendered once per change into an admin JSON object, an admin SSE frame, and a public SSE frame. The public frame is `{"listeners":N,"now":...}`. The admin object is:

```json
{
  "listeners": 0,
  "now": {"artist": "", "title": "", "file": "", "format": ""},
  "queue": [],
  "queue_total": 0,
  "listener_list": [
    {"id": 1, "addr": "203.0.113.9", "user_agent": "", "since": 0}
  ]
}
```

`now` is `null` when nothing is playing. `queue` previews the first 25 published entries; that list includes tracks already handed to the encoder but not yet announced. `queue_total` is the full published length. `since` is Unix milliseconds. The public frame carries the same `now` object and the listener count, so the player works without logging in.

`/api/status` returns the admin object without the SSE `data:` wrapper. `/api/queue` returns up to the published queue: 1000 remaining entries plus the in-flight tracks. Each entry gains a one-based `position` referring to its place in the filtered full queue, not its place on the page. `/api/files` walks the canonical music directory, skips symlinks, includes `jingles/` entries with kind `jingle`, and reports `path`, `name`, `size`, `modified` (Unix seconds), and `kind`. Both endpoints clamp the requested page to the page count, treat `per_page=0` as the default, and cap `per_page` at 200. Defaults are 50 for files and 25 for the queue.

## SSE

`serve_events` subscribes to the status watch channel, writes the current frame immediately, then writes the next frame whenever the status changes. While the connection is idle it emits `: keepalive` every 15 seconds. The task also reads from the socket and exits when the peer closes. The response sets `X-Accel-Buffering: no`. There is no event id and no replay. A reconnecting client gets the current value.

The admin UI does not poll. Queue and listener updates arrive through `/admin/events`. The paginated `/api/files` and `/api/queue` endpoints are fetched only when a view is opened or its controls change.

## Authentication

`Auth::load_or_create` creates `auth.json` containing `{}` when the file is missing, then applies mode `0600` on Unix. Parsing is strict: unknown fields are rejected, and username and password hash must both be present or both be absent. An invalid file stops startup. The username is trimmed and limited to 64 bytes; the password is limited to 1024 bytes and may not be empty.

The first successful login claims the account. It hashes the password with Argon2id and replaces the file atomically through a `create_new` temporary file, `fsync`, and rename. Later logins must match both username and password. Hashing, verification, and persistence run on a dedicated `auth` OS thread that owns the account state. HTTP tasks enqueue a job and wait for the result.

Success returns `{"ok":true,"user":...,"created":...}` and sets `radio_session` with `HttpOnly`, `SameSite=Strict`, `Path=/`, and `Max-Age=604800`. The token is 32 random bytes as hex. Sessions live only in memory. Expired sessions are pruned when a new one is created. When the table reaches 256 entries, the oldest by creation time is evicted. `validate` accepts only 64 hex characters. A failed password check sleeps 200 ms in addition to Argon2's cost. The application does not rate-limit. The reverse proxy owns that.

The cookie is not marked `Secure`. Terminate TLS at the proxy.

## Audio endpoint settings

`settings.json` supplies the public stream and HLS paths and the HLS enable flag. It is created with defaults on first start:

```json
{
  "stream_path": "/radio.opus",
  "playlist_path": "/radio.m3u8",
  "hls_enabled": true
}
```

`stream_path` must end in `.opus`; `playlist_path` must end in `.m3u8`. Both must be absolute URL paths without a trailing slash, use only ASCII letters, digits, `-`, `.`, `_`, `~`, and `/`, and avoid `/api`, `/admin`, `/events`, and `/spectrum.wasm`. The HLS segment route replaces the `.m3u8` suffix with `-`, so `/live/audio.m3u8` serves segments at `/live/audio-<n>.ts`. Playlist lines use the relative name `audio-<n>.ts`, which resolves in the playlist directory.

`GET /api/settings` returns the current values. `POST /api/settings` requires all three fields, validates them, writes the file atomically through a temporary file, `fsync`, and rename, and applies the HLS flag immediately. Failed validation returns 400 and leaves the file alone. Endpoint changes affect new requests; an open stream keeps the path it connected on. The admin settings page is at `/admin/settings`.

## Uploads and path handling

Uploads use `multipart/form-data` with exactly one `file` part and an optional `target` part whose value is `music` (the default) or `jingles`. The filename is reduced to its basename, trimmed, and rejected if it is empty, `.`, `..`, dot-prefixed, longer than 200 bytes, or contains control characters. The extension must be `.wav` or `.flac`. The body is parsed as it arrives. Only the file part is written, to a random `.upload-<pid>-<hex>.part` file in the music directory, using bounded read buffers and a 16 KiB part-header cap. The target field is capped at 1 KiB. The temporary file is `fsync`ed, validated by the same media parsers used for playback, and only then renamed into the target directory. An existing regular file is overwritten; an existing directory or symlink returns 409. Invalid media returns 415 and removes the temporary file. The station is notified only after the rename succeeds.

Delete, download, and rename accept a relative path. `safe_relative` rejects empty strings, backslashes, control characters, NUL, absolute paths, parent components, and prefix components. The resolved file must be under the canonical music directory, must not be a symlink, and must be a regular file. Delete removes the canonical path but notifies the prefetch thread with the non-canonical root join, matching paths from the initial scan. Rename refuses to change the extension and refuses to overwrite an existing entry. It updates the published now-playing and queue entries immediately, then sends `StationCmd::Rename`.

Download serves the file with `Content-Disposition: attachment`, `audio/wav` or `audio/flac`, and no range support.

## Errors

Every error is JSON `{"error":"..."}` with a matching status. Mapped statuses are 400, 401, 403, 404, 405, 408, 409, 411, 413, 414, 415, 431, and 500. Method and origin errors are returned before authentication. Authentication failures return 401.

## Proxy

Proxy requirements and a working nginx configuration are in [operations.md](operations.md#reverse-proxy).
