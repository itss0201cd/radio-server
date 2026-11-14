# Operations

## Process and resources

The process has four execution contexts: the Tokio HTTP runtime, the prefetch thread, the station thread, and the auth thread. Argon2 adds about 19 MiB while a hash is running. The PCM channel between the audio threads holds 3000 blocks of 1920 floats, about 22 MiB of samples when full. The page ring holds 500 pages, well under a megabyte of shared `Bytes`. The rest of the footprint is the encoder scratch buffers, decoder buffers for the current file, a 15-second output tail (about 5.8 MiB at 48 kHz stereo `f32`), and the published queue: up to 1000 remaining entries plus the in-flight prefetch window.

The station publishes ten seconds ahead of playback. Prefetch can run up to sixty seconds ahead of the encoder. Neither buffer can be changed at runtime.

The process binds `127.0.0.1:<port>` and does not daemonise. Run it under a supervisor. There is no internal restart. If the station thread dies, HTTP and SSE stay up but the stream freezes. Treat a stale stream as a restart condition.

An empty music directory is fine. The HTTP server starts immediately, the station waits for PCM with a two-second receive timeout, and an admin upload can prime the stream. Until 500 blocks have been encoded the ten-second burst is incomplete and the paced clock has not started. A listener connecting during priming receives whatever pages exist at that moment.

## Files and permissions

The service account needs read access to the music directory. Upload, delete, and rename need write access there and to `jingles/` if jingles are managed through the UI.

The account file is `auth.json` by default. `RADIO_AUTH_FILE` overrides the path. Because the default is relative, set `WorkingDirectory` in the service unit or use an absolute path. The directory must be writable while no account exists. After that it is read at startup and rewritten only on first login. A missing file is created as `{}` with mode `0600`. A malformed file, a username without a hash, or an unparsable hash stops startup. Treat the file as a secret: do not commit or ship it.

Settings live in `settings.json`. `RADIO_SETTINGS_FILE` overrides the path. A missing file is created with the defaults in [web-server.md](web-server.md). The directory must be writable. A malformed file, an invalid path, or a bad extension stops startup. Hand edits require a restart. The admin page applies changes immediately.

Sessions live in memory. Restarting logs out every browser, but the account survives in `auth.json`. To reset a forgotten account, stop the process, delete `auth.json` or replace it with `{}`, restart, and log in once with the new credentials. That login claims the account. There is no in-band password change or recovery flow.

Files dropped into the music directory are noticed only at startup. Use the admin upload endpoint for anything added while the process runs. It validates the media and notifies the prefetch thread. Deleting or renaming through the API updates the in-memory library and, for renames, the published queue and now-playing entry. External renames or deletions leave stale entries in memory until restart. The affected tracks are skipped when decoding fails.

## Reverse proxy

The listener address shown in the admin UI comes from `X-Forwarded-For`, then `X-Real-IP`, then the socket peer. A proxy must overwrite these headers rather than append to them if untrusted users can reach the service. Preserve `Host`. The POST origin check compares it with `Origin`. `X-Forwarded-Proto` is ignored.

An nginx front end needs SSE buffering off, a body limit at least as large as the server's 512 MiB cap, request bodies with a `Content-Length` (leave request buffering on), and a read timeout that tolerates an unbounded stream. Rate limiting belongs at the proxy, including for `/api/login`. The application serialises account work but does not reject or throttle attempts.

```nginx
server {
    listen 443 ssl;
    server_name radio.example.com;

    client_max_body_size 512m;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header X-Forwarded-For $remote_addr;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 1h;
    }
}
```

TLS terminates at the proxy. The `radio_session` cookie is `HttpOnly` and `SameSite=Strict` but not `Secure`. Exposing the loopback port over plain HTTP exposes the token.

`proxy_buffering off` is required for the SSE feeds and also avoids buffering the endless stream. The server sends `Connection: close` on every response and never chunks a request body. A proxy that forces chunked upstream requests gets 400s. Disable response compression: the stream is already compressed, and SSE must not be delayed.

## Logging

Diagnostics go to stderr with a bracketed subsystem prefix. `[auth]` reports account creation, loading, and login failures. `[http]` prints the bound address and stream URL once. `[station]` reports cache priming, waiting for audio, and fatal encoder or channel errors. `[prefetch]` reports the library scan, now-playing track and format, trim and ring values, uploads and removals, skips with reasons, and playlist exhaustion. `[files]` reports directory listing failures.

There are no levels, timestamps, or structured fields. A supervisor or external collector handles rotation. The useful health signals are the `[station] cache primed` line at startup and the absence of repeated `[prefetch] skipping` lines. Repeated skips usually mean an unsupported sample rate, an unsupported bit depth, or a corrupt file. A `[prefetch] cooldown could not be met` line means the library has too few distinct artists to fill the one-hour rule. The station keeps playing and reports how many slots it had to force.

## Capacity and failure modes

Encoding is one thread at a fixed 320 kbps target, regardless of listener count. Fan-out is page writes on per-connection tasks. Extra listeners cost socket buffers and broadcast receivers, not encoding work. The broadcast buffer holds twelve seconds. A listener slower than that loses pages. New connections start from the page ring, ten seconds behind the live edge.

HLS keeps two five-second MPEG-TS segments, under a megabyte. HLS requests do not add broadcast receivers or listener records. A client that falls behind the segment ring gets 404 and has to restart from the current playlist. The station never waits for it.

Resampling is the main CPU cost that scales with source material. Sources at 48 kHz bypass it. Other supported rates use the fixed-point polyphase path. Unsupported rates skip the track instead of resampling approximately. Unreadable or entirely silent files are skipped too.

Uploads are validated before they become visible. A bad upload returns 415 and leaves the library untouched. The temporary file is removed. Deletes and downloads canonicalise the target and reject symlinks. A symlink planted in the music directory cannot read or remove files outside it. Upload names use only the basename. Directory components in a client filename cannot escape the target directory.

## Backups and upgrades

There is no backup feature. Back up `auth.json`, `settings.json`, and the music directory with whatever tooling you already use. In-place upgrades are not supported. Stop the process, replace the binary, and start it again.
