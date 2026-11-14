// http.rs — minimal HTTP/1.1, no framework.
//
//   GET  /                         public player (now playing works logged out)
//   GET  /events                   public SSE: listeners + now playing
//   GET  /admin                    admin shell (login or overview)
//   GET  /admin/files              admin shell (files view)
//   GET  /admin/queue              admin shell (queue view)
//   GET  /admin/listeners          admin shell (live listeners view)
//   GET  /admin/settings           admin shell (settings view)
//   GET  /admin/events             authenticated SSE: adds queue + totals
//   GET  /radio.opus               default live stream (configurable)
//   GET  /radio.m3u8               default HLS playlist (configurable, toggleable)
//   GET  /radio-N.ts               default HLS MPEG-TS segment
//
// authenticated JSON API used by the admin UI:
//   POST /api/login                log in, or create the single account
//   POST /api/logout               destroy the session
//   GET  /api/me                   current session
//   GET  /api/status               listeners + now playing + queue preview
//   GET  /api/settings             current audio endpoint settings
//   POST /api/settings             validate, persist, and apply settings
//   GET  /api/queue                paginated/searchable full queue
//   GET  /api/files                paginated/searchable media files
//   GET  /api/download             download a media file
//   POST /api/upload               multipart upload into music/ or jingles/
//   POST /api/delete               delete a media file
//   POST /api/rename               rename a media file on disk
//   POST /api/listeners/disconnect drop one live stream connection
//
// Everything under /api/ except /api/login, plus /admin/events, requires the
// HttpOnly session cookie. The player and stream are public.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rand::TryRng;
use rand::rngs::SysRng;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast::error::RecvError;

use crate::auth::{Auth, LoginError};
use crate::settings::{SettingsStore, SettingsUpdate};
use crate::station::{NowPlaying, Shared, StationCmd};

const UI: &str = include_str!("ui.html");
const ADMIN: &str = include_str!("admin.html");
const SPECTRUM_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/spectrum.wasm"));
const SESSION_COOKIE: &str = "radio_session";
const SESSION_MAX_AGE: u64 = 7 * 24 * 60 * 60;
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: u64 = 512 * 1024 * 1024;
const SMALL_BODY: usize = 16 * 1024;
const MAX_PART_HEADERS: usize = 16 * 1024;
const MAX_TARGET_BYTES: usize = 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const BODY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

struct HttpError {
    status: u16,
    message: String,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        HttpError {
            status,
            message: message.into(),
        }
    }
}

pub async fn serve(
    mut sock: TcpStream,
    shared: Arc<Shared>,
    auth: Arc<Auth>,
    settings: Arc<SettingsStore>,
) {
    let _ = sock.set_nodelay(true);

    let head = match read_head(&mut sock).await {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            let _ = error_response(&mut sock, &e).await;
            return;
        }
    };

    let peer = sock
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    if let Err(e) = handle(&mut sock, head, shared, auth, &settings, &peer).await {
        let _ = error_response(&mut sock, &e).await;
    }
}

async fn handle(
    sock: &mut TcpStream,
    head: Head,
    shared: Arc<Shared>,
    auth: Arc<Auth>,
    settings: &Arc<SettingsStore>,
    peer: &str,
) -> Result<(), HttpError> {
    let method = head.method.as_str();
    let path = head.path.as_str();
    let config = settings.get();
    let segment_prefix = config.segment_prefix();

    if method != "GET" && method != "POST" {
        return Err(HttpError::new(405, "method not allowed"));
    }
    if method == "POST" && !origin_ok(&head.headers) {
        return Err(HttpError::new(403, "cross-origin request rejected"));
    }

    let protected = path == "/admin/events" || (path.starts_with("/api/") && path != "/api/login");
    let session = if protected {
        match session_user(&head.headers, &auth) {
            Some(s) => Some(s),
            None => return Err(HttpError::new(401, "authentication required")),
        }
    } else {
        None
    };

    match (method, path) {
        ("GET", "/") => {
            let player = UI.replace("__STREAM_PATH__", &config.stream_path);
            let _ =
                write_response(sock, 200, "text/html; charset=utf-8", player.as_bytes(), &[]).await;
            Ok(())
        }
        ("GET", "/spectrum.wasm") => {
            let _ = write_response(sock, 200, "application/wasm", SPECTRUM_WASM, &[]).await;
            Ok(())
        }
        ("GET", p) if p == config.stream_path.as_str() => {
            serve_stream(sock, &shared, peer, &head).await;
            Ok(())
        }
        ("GET", p) if config.hls_enabled && p == config.playlist_path.as_str() => {
            serve_hls_playlist(sock, &shared, &segment_prefix).await?;
            Ok(())
        }
        ("GET", p) if config.hls_enabled && p.starts_with(segment_prefix.as_str()) => {
            serve_hls_segment(sock, &shared, p, &segment_prefix).await?;
            Ok(())
        }
        ("GET", "/events") => {
            serve_events(sock, &shared, false).await;
            Ok(())
        }
        ("GET", "/admin/events") => {
            serve_events(sock, &shared, true).await;
            Ok(())
        }
        ("GET", p) if is_admin_page(p) => {
            let _ =
                write_response(sock, 200, "text/html; charset=utf-8", ADMIN.as_bytes(), &[]).await;
            Ok(())
        }
        ("POST", "/api/login") => {
            let body = read_body(sock, &head, SMALL_BODY).await?;
            let (username, password) = parse_credentials(&head.headers, &body)?;

            match auth.login(username, password).await {
                Ok(ok) => {
                    let cookie = format!(
                        "{SESSION_COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE}",
                        ok.token
                    );
                    let body = json!({
                        "ok": true,
                        "user": ok.user,
                        "created": ok.created,
                    });
                    respond_json(sock, 200, &body, &[("Set-Cookie", &cookie)]).await;
                    Ok(())
                }
                Err(LoginError::Invalid) => {
                    // Argon2 already makes guessing expensive; the extra pause
                    // also slows a scripted brute-force loop.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    respond_json(
                        sock,
                        401,
                        &json!({"error": "invalid username or password"}),
                        &[],
                    )
                    .await;
                    Ok(())
                }
                Err(LoginError::BadRequest(msg)) => Err(HttpError::new(400, msg)),
                Err(LoginError::Internal(msg)) => {
                    eprintln!("[auth] login failed: {msg}");
                    Err(HttpError::new(500, "could not save the account"))
                }
            }
        }
        ("POST", "/api/logout") => {
            if let Some((_, token)) = &session {
                auth.logout(token);
            }
            let cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
            respond_json(sock, 200, &json!({"ok": true}), &[("Set-Cookie", &cookie)]).await;
            Ok(())
        }
        ("GET", "/api/me") => {
            let (user, _) = session.as_ref().expect("protected route");
            respond_json(
                sock,
                200,
                &json!({"authenticated": true, "user": user}),
                &[],
            )
            .await;
            Ok(())
        }
        ("GET", "/api/settings") => {
            respond_json(
                sock,
                200,
                &json!({
                    "stream_path": &config.stream_path,
                    "playlist_path": &config.playlist_path,
                    "hls_enabled": config.hls_enabled,
                }),
                &[],
            )
            .await;
            Ok(())
        }
        ("POST", "/api/settings") => {
            let body = read_body(sock, &head, SMALL_BODY).await?;
            let update: SettingsUpdate = serde_json::from_slice(&body)
                .map_err(|error| HttpError::new(400, format!("invalid settings JSON: {error}")))?;
            let next = update.into_settings();
            next.validate()
                .map_err(|error| HttpError::new(400, error))?;

            let store = settings.clone();
            let updated = tokio::task::spawn_blocking(move || store.update(next))
                .await
                .map_err(|_| HttpError::new(500, "settings task failed"))?
                .map_err(|error| {
                    eprintln!("[settings] save failed: {error}");
                    HttpError::new(500, "could not save settings")
                })?;
            shared.set_hls_enabled(updated.hls_enabled);

            respond_json(
                sock,
                200,
                &json!({
                    "stream_path": &updated.stream_path,
                    "playlist_path": &updated.playlist_path,
                    "hls_enabled": updated.hls_enabled,
                }),
                &[],
            )
            .await;
            Ok(())
        }
        ("GET", "/api/status") => {
            let body = shared.status_json();
            let _ = write_response(sock, 200, "application/json; charset=utf-8", &body, &[]).await;
            Ok(())
        }
        ("GET", "/api/files") => {
            let files = match list_files(&shared.dir) {
                Ok(files) => files,
                Err(e) => {
                    eprintln!("[files] listing {}: {e}", shared.dir.display());
                    return Err(HttpError::new(500, "could not list the music directory"));
                }
            };

            let q = query_text(&head, "q")?.trim().to_lowercase();
            let kind = head.query.get("kind").map(String::as_str).unwrap_or("");
            if !matches!(kind, "" | "all" | "music" | "jingle") {
                return Err(HttpError::new(400, "invalid kind filter"));
            }
            let (page, per_page) = page_params(&head, 50, 200)?;

            let filtered: Vec<FileEntry> = files
                .into_iter()
                .filter(|f| {
                    (kind.is_empty() || kind == "all" || f.kind == kind)
                        && (q.is_empty()
                            || f.path.to_lowercase().contains(&q)
                            || f.name.to_lowercase().contains(&q))
                })
                .collect();
            let total = filtered.len();
            let pages = page_count(total, per_page);
            let page = page.min(pages);

            let files: Vec<Value> = filtered
                .into_iter()
                .skip((page - 1) * per_page)
                .take(per_page)
                .map(|f| {
                    json!({
                        "path": f.path,
                        "name": f.name,
                        "size": f.size,
                        "modified": f.modified,
                        "kind": f.kind,
                    })
                })
                .collect();

            respond_json(
                sock,
                200,
                &json!({
                    "files": files,
                    "total": total,
                    "page": page,
                    "per_page": per_page,
                    "pages": pages,
                }),
                &[],
            )
            .await;
            Ok(())
        }
        ("GET", "/api/download") => {
            let raw = query_text(&head, "path")?;
            let (_, name, path) = resolve_media_file(&shared, &raw)?;
            serve_download(sock, &path, &name).await?;
            Ok(())
        }
        ("GET", "/api/queue") => {
            let q = query_text(&head, "q")?.trim().to_lowercase();
            let (page, per_page) = page_params(&head, 25, 200)?;

            let filtered = filter_queue(shared.queue_snapshot(), &q);
            let total = filtered.len();
            let pages = page_count(total, per_page);
            let page = page.min(pages);

            let queue: Vec<Value> = filtered
                .iter()
                .skip((page - 1) * per_page)
                .take(per_page)
                .map(|(position, n)| np_value(n, position + 1))
                .collect();

            respond_json(
                sock,
                200,
                &json!({
                    "queue": queue,
                    "total": total,
                    "page": page,
                    "per_page": per_page,
                    "pages": pages,
                }),
                &[],
            )
            .await;
            Ok(())
        }
        ("POST", "/api/upload") => {
            let upload = stream_upload(sock, &head, &shared.dir).await?;
            let name = match sanitize_filename(&upload.filename) {
                Ok(name) => name,
                Err(error) => {
                    remove_upload_temp(&upload.temp).await;
                    return Err(error);
                }
            };
            let kind = match media_kind(&name) {
                Some(kind) => kind,
                None => {
                    remove_upload_temp(&upload.temp).await;
                    return Err(HttpError::new(
                        415,
                        "only .wav and .flac files are supported",
                    ));
                }
            };

            let dir = shared.dir.clone();
            let temp = upload.temp.clone();
            let cleanup = upload.temp;
            let target = upload.target;
            let task_name = name.clone();
            let task_target = target.clone();
            let finalized = tokio::task::spawn_blocking(move || {
                finalize_upload(&dir, &temp, &task_name, &task_target, kind)
            })
            .await
            .unwrap_or_else(|_| Err(HttpError::new(500, "upload task failed")));
            let (final_path, overwritten) = match finalized {
                Ok(result) => result,
                Err(error) => {
                    remove_upload_temp(&cleanup).await;
                    return Err(error);
                }
            };
            let cmd = if target == "jingles" {
                StationCmd::AddJingle(final_path.clone())
            } else {
                StationCmd::AddTrack(final_path.clone())
            };
            shared.notify(cmd);

            respond_json(
                sock,
                201,
                &json!({
                    "ok": true,
                    "path": final_path
                        .strip_prefix(&shared.dir)
                        .unwrap_or(&final_path)
                        .to_string_lossy()
                        .replace('\\', "/"),
                    "name": name,
                    "size": upload.size,
                    "kind": if target == "jingles" { "jingle" } else { "music" },
                    "overwritten": overwritten,
                }),
                &[],
            )
            .await;
            Ok(())
        }
        ("POST", "/api/delete") => {
            let body = read_body(sock, &head, SMALL_BODY).await?;
            let rel = parse_delete_path(&body)?;
            delete_file(&shared, &rel)?;
            respond_json(sock, 200, &json!({"ok": true}), &[]).await;
            Ok(())
        }
        ("POST", "/api/rename") => {
            let body = read_body(sock, &head, SMALL_BODY).await?;
            let (path, name) = parse_rename(&body)?;
            let (path, name) = rename_file(&shared, &path, &name)?;
            respond_json(
                sock,
                200,
                &json!({"ok": true, "path": path, "name": name}),
                &[],
            )
            .await;
            Ok(())
        }
        ("POST", "/api/listeners/disconnect") => {
            let body = read_body(sock, &head, SMALL_BODY).await?;
            let id = parse_listener_id(&body)?;
            if shared.disconnect_listener(id) {
                respond_json(sock, 200, &json!({"ok": true}), &[]).await;
            } else {
                respond_json(sock, 404, &json!({"error": "listener not found"}), &[]).await;
            }
            Ok(())
        }
        _ => Err(HttpError::new(404, "not found")),
    }
}

struct Head {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    leftover: Vec<u8>,
}

async fn read_head(sock: &mut TcpStream) -> Result<Option<Head>, HttpError> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let header = buf[..pos].to_vec();
            let leftover = buf[pos + 4..].to_vec();
            return parse_head(&header, leftover).map(Some);
        }
        if buf.len() > MAX_HEAD {
            return Err(HttpError::new(431, "request header too large"));
        }

        let n = match tokio::time::timeout(HEAD_TIMEOUT, sock.read(&mut tmp)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err(HttpError::new(400, "could not read the request")),
            Err(_) => return Err(HttpError::new(408, "request timed out")),
        };
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Err(HttpError::new(400, "incomplete request"))
            };
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

fn parse_head(raw: &[u8], leftover: Vec<u8>) -> Result<Head, HttpError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| HttpError::new(400, "request head must be valid UTF-8"))?;
    let mut lines = text.split("\r\n");

    let first = lines
        .next()
        .ok_or_else(|| HttpError::new(400, "missing request line"))?;
    let mut parts = first.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing method"))?;
    let target = parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing request target"))?;
    let version = parts
        .next()
        .ok_or_else(|| HttpError::new(400, "missing HTTP version"))?;
    if parts.next().is_some() {
        return Err(HttpError::new(400, "malformed request line"));
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::new(400, "unsupported HTTP version"));
    }
    if !target.starts_with('/') {
        return Err(HttpError::new(400, "unsupported request target"));
    }
    let path = target.split('?').next().unwrap_or("/").to_string();
    let query_raw = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let query = parse_query(query_raw)?;

    let mut headers = HashMap::new();
    let mut count = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > 100 || line.len() > 8192 {
            return Err(HttpError::new(431, "too many or oversized headers"));
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::new(400, "malformed header"))?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
            return Err(HttpError::new(400, "malformed header name"));
        }
        if value.bytes().any(|b| b < 0x20 && b != b'\t') {
            return Err(HttpError::new(400, "malformed header value"));
        }

        let name = name.to_ascii_lowercase();
        let value = value.trim().to_string();
        if headers.contains_key(&name) && (name == "content-length" || name == "host") {
            return Err(HttpError::new(400, "duplicate header"));
        }
        headers.insert(name, value);
    }

    if version == "HTTP/1.1" && !headers.contains_key("host") {
        return Err(HttpError::new(400, "missing Host header"));
    }
    if let Some(te) = headers.get("transfer-encoding") {
        if !te.eq_ignore_ascii_case("identity") {
            return Err(HttpError::new(
                400,
                "chunked request bodies are not supported",
            ));
        }
        if headers.contains_key("content-length") {
            return Err(HttpError::new(400, "conflicting body framing"));
        }
    }

    Ok(Head {
        method: method.to_string(),
        path,
        query,
        headers,
        leftover,
    })
}

fn parse_query(raw: &str) -> Result<HashMap<String, String>, HttpError> {
    let mut out = HashMap::new();
    if raw.is_empty() {
        return Ok(out);
    }
    if raw.len() > 4096 {
        return Err(HttpError::new(414, "query string too long"));
    }
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key)?;
        if key.is_empty() {
            continue;
        }
        out.insert(key, percent_decode(value)?);
    }
    Ok(out)
}

fn query_text(head: &Head, key: &str) -> Result<String, HttpError> {
    let value = head.query.get(key).cloned().unwrap_or_default();
    if value.chars().count() > 200 {
        return Err(HttpError::new(400, "query parameter too long"));
    }
    Ok(value)
}

fn page_params(
    head: &Head,
    default_per: usize,
    max_per: usize,
) -> Result<(usize, usize), HttpError> {
    let page = query_usize(head.query.get("page"), 1)?.max(1);
    let mut per_page = query_usize(head.query.get("per_page"), default_per)?;
    if per_page == 0 {
        per_page = default_per;
    }
    Ok((page, per_page.min(max_per)))
}

fn query_usize(value: Option<&String>, default: usize) -> Result<usize, HttpError> {
    match value.map(String::as_str) {
        None | Some("") => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .map_err(|_| HttpError::new(400, "invalid pagination parameter")),
    }
}

fn page_count(total: usize, per_page: usize) -> usize {
    if total == 0 {
        1
    } else {
        total.div_ceil(per_page)
    }
}

fn is_admin_page(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "/admin"
            | "/admin/files"
            | "/admin/queue"
            | "/admin/listeners"
            | "/admin/settings"
    )
}

fn filter_queue(queue: Vec<Arc<NowPlaying>>, q: &str) -> Vec<(usize, Arc<NowPlaying>)> {
    queue
        .into_iter()
        .enumerate()
        .filter(|(_, n)| {
            q.is_empty()
                || n.artist.to_lowercase().contains(q)
                || n.title.to_lowercase().contains(q)
                || n.file.to_lowercase().contains(q)
        })
        .collect()
}

fn np_value(n: &NowPlaying, position: usize) -> Value {
    json!({
        "artist": n.artist,
        "title": n.title,
        "file": n.file,
        "format": n.format,
        "position": position,
    })
}

fn content_length(head: &Head) -> Result<usize, HttpError> {
    let value = head
        .headers
        .get("content-length")
        .ok_or_else(|| HttpError::new(411, "content-length required"))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(HttpError::new(400, "invalid content-length"));
    }
    let n: u64 = value
        .parse()
        .map_err(|_| HttpError::new(400, "invalid content-length"))?;
    if n > MAX_BODY {
        return Err(HttpError::new(413, "request body too large"));
    }
    Ok(n as usize)
}

async fn read_body(sock: &mut TcpStream, head: &Head, limit: usize) -> Result<Vec<u8>, HttpError> {
    let len = content_length(head)?;
    if len > limit {
        return Err(HttpError::new(413, "request body too large"));
    }
    if len == 0 {
        return Ok(Vec::new());
    }

    if head
        .headers
        .get("expect")
        .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"))
    {
        sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| HttpError::new(400, "could not send 100 Continue"))?;
    }

    let deadline = Instant::now() + BODY_TIMEOUT;
    let mut body = Vec::with_capacity(len.min(8 * 1024 * 1024));
    let take = len.min(head.leftover.len());
    body.extend_from_slice(&head.leftover[..take]);

    let mut tmp = [0u8; 64 * 1024];
    while body.len() < len {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpError::new(408, "request body timed out"));
        }
        let n = match tokio::time::timeout(remaining, sock.read(&mut tmp)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return Err(HttpError::new(400, "could not read the request body")),
            Err(_) => return Err(HttpError::new(408, "request body timed out")),
        };
        if n == 0 {
            return Err(HttpError::new(400, "incomplete request body"));
        }
        let need = len - body.len();
        body.extend_from_slice(&tmp[..n.min(need)]);
    }
    Ok(body)
}

fn origin_ok(headers: &HashMap<String, String>) -> bool {
    let Some(origin) = headers.get("origin") else {
        return true; // non-browser client
    };
    let Some(host) = headers.get("host") else {
        return false;
    };
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    rest.trim_end_matches('/').eq_ignore_ascii_case(host)
}

fn session_user(headers: &HashMap<String, String>, auth: &Auth) -> Option<(String, String)> {
    let token = cookie_token(headers.get("cookie")?)?;
    let user = auth.validate(&token)?;
    Some((user, token))
}

fn cookie_token(cookie: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("radio_session=") {
            let value = value.trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn parse_credentials(
    headers: &HashMap<String, String>,
    body: &[u8],
) -> Result<(String, String), HttpError> {
    let content_type = headers
        .get("content-type")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    if content_type.starts_with("application/json") {
        let text = std::str::from_utf8(body)
            .map_err(|_| HttpError::new(400, "request body must be UTF-8"))?;
        let value: Value = serde_json::from_str(text)
            .map_err(|e| HttpError::new(400, format!("invalid JSON: {e}")))?;
        let username = value
            .get("username")
            .and_then(Value::as_str)
            .ok_or_else(|| HttpError::new(400, "username is required"))?;
        let password = value
            .get("password")
            .and_then(Value::as_str)
            .ok_or_else(|| HttpError::new(400, "password is required"))?;
        return Ok((username.to_string(), password.to_string()));
    }

    if content_type.starts_with("application/x-www-form-urlencoded") {
        let text = std::str::from_utf8(body)
            .map_err(|_| HttpError::new(400, "request body must be UTF-8"))?;
        let mut username = None;
        let mut password = None;
        for pair in text.split('&') {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = percent_decode(key)?;
            let value = percent_decode(value)?;
            match key.as_str() {
                "username" => username = Some(value),
                "password" => password = Some(value),
                _ => {}
            }
        }
        return Ok((
            username.ok_or_else(|| HttpError::new(400, "username is required"))?,
            password.ok_or_else(|| HttpError::new(400, "password is required"))?,
        ));
    }

    Err(HttpError::new(
        415,
        "content-type must be application/json or application/x-www-form-urlencoded",
    ))
}

fn percent_decode(s: &str) -> Result<String, HttpError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(HttpError::new(400, "invalid percent encoding"));
                }
                let hi = hex_val(bytes[i + 1])
                    .ok_or_else(|| HttpError::new(400, "invalid percent encoding"))?;
                let lo = hex_val(bytes[i + 2])
                    .ok_or_else(|| HttpError::new(400, "invalid percent encoding"))?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::new(400, "invalid percent encoding"))
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_delete_path(body: &[u8]) -> Result<String, HttpError> {
    let text =
        std::str::from_utf8(body).map_err(|_| HttpError::new(400, "request body must be UTF-8"))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|e| HttpError::new(400, format!("invalid JSON: {e}")))?;
    value
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| HttpError::new(400, "path is required"))
}

fn parse_listener_id(body: &[u8]) -> Result<u64, HttpError> {
    let text =
        std::str::from_utf8(body).map_err(|_| HttpError::new(400, "request body must be UTF-8"))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|e| HttpError::new(400, format!("invalid JSON: {e}")))?;
    value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| HttpError::new(400, "id is required"))
}

fn parse_rename(body: &[u8]) -> Result<(String, String), HttpError> {
    let text =
        std::str::from_utf8(body).map_err(|_| HttpError::new(400, "request body must be UTF-8"))?;
    let value: Value = serde_json::from_str(text)
        .map_err(|e| HttpError::new(400, format!("invalid JSON: {e}")))?;
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::new(400, "path is required"))?;
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::new(400, "name is required"))?;
    Ok((path.to_string(), name.to_string()))
}

fn safe_relative(raw: &str) -> Result<PathBuf, HttpError> {
    if raw.is_empty()
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_control())
        || raw.as_bytes().contains(&0)
    {
        return Err(HttpError::new(400, "invalid path"));
    }

    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return Err(HttpError::new(400, "invalid path")),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(HttpError::new(400, "invalid path"));
    }
    Ok(out)
}

fn sanitize_filename(raw: &str) -> Result<String, HttpError> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty()
        || base == "."
        || base == ".."
        || base.starts_with('.')
        || base.len() > 200
        || base.chars().any(char::is_control)
    {
        return Err(HttpError::new(400, "invalid file name"));
    }
    Ok(base.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Wav,
    Flac,
}

fn media_kind(name: &str) -> Option<MediaKind> {
    let ext = Path::new(name).extension()?.to_str()?;
    if ext.eq_ignore_ascii_case("wav") {
        Some(MediaKind::Wav)
    } else if ext.eq_ignore_ascii_case("flac") {
        Some(MediaKind::Flac)
    } else {
        None
    }
}

fn validate_media(path: &Path, kind: MediaKind) -> Result<(), String> {
    match kind {
        MediaKind::Wav => crate::wav::wav_format(path).map(|_| ()),
        MediaKind::Flac => {
            let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            crate::flac::Decoder::new(file)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }
}

struct Uploaded {
    filename: String,
    target: String,
    temp: PathBuf,
    size: u64,
}

enum PartSink {
    File {
        file: tokio::fs::File,
        path: PathBuf,
        size: u64,
    },
    Field(Vec<u8>),
    Discard,
}

impl PartSink {
    async fn write(&mut self, data: &[u8]) -> Result<(), HttpError> {
        match self {
            PartSink::File { file, size, .. } => {
                file.write_all(data)
                    .await
                    .map_err(|e| HttpError::new(500, format!("could not write the upload: {e}")))?;
                *size += data.len() as u64;
            }
            PartSink::Field(field) => {
                if field.len() + data.len() > MAX_TARGET_BYTES {
                    return Err(HttpError::new(400, "invalid target"));
                }
                field.extend_from_slice(data);
            }
            PartSink::Discard => {}
        }
        Ok(())
    }
}

struct UploadBody<'a> {
    sock: &'a mut TcpStream,
    leftover: &'a [u8],
    leftover_pos: usize,
    remaining: usize,
    deadline: Instant,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl<'a> UploadBody<'a> {
    fn new(sock: &'a mut TcpStream, leftover: &'a [u8], len: usize) -> Self {
        let take = len.min(leftover.len());
        UploadBody {
            sock,
            leftover: &leftover[..take],
            leftover_pos: 0,
            remaining: len - take,
            deadline: Instant::now() + BODY_TIMEOUT,
            buf: Vec::with_capacity(64 * 1024),
            start: 0,
            end: 0,
        }
    }

    fn available(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    fn consume(&mut self, count: usize) {
        self.start += count;
        debug_assert!(self.start <= self.end);
    }

    fn compact(&mut self) {
        if self.start > 0 {
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
            self.buf.truncate(self.end);
        }
    }

    async fn fill(&mut self) -> Result<(), HttpError> {
        self.compact();
        if self.leftover_pos < self.leftover.len() {
            let take = (self.leftover.len() - self.leftover_pos).min(64 * 1024);
            self.buf
                .extend_from_slice(&self.leftover[self.leftover_pos..self.leftover_pos + take]);
            self.leftover_pos += take;
            self.end = self.buf.len();
            return Ok(());
        }
        if self.remaining == 0 {
            return Err(HttpError::new(400, "incomplete request body"));
        }

        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpError::new(408, "request body timed out"));
        }
        let mut tmp = [0u8; 64 * 1024];
        let want = self.remaining.min(tmp.len());
        let n = match tokio::time::timeout(remaining, self.sock.read(&mut tmp[..want])).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) => {
                return Err(HttpError::new(400, "could not read the request body"));
            }
            Err(_) => return Err(HttpError::new(408, "request body timed out")),
        };
        if n == 0 {
            return Err(HttpError::new(400, "incomplete request body"));
        }
        self.remaining -= n;
        self.buf.extend_from_slice(&tmp[..n]);
        self.end = self.buf.len();
        Ok(())
    }

    async fn ensure(&mut self, count: usize) -> Result<(), HttpError> {
        while self.available().len() < count {
            self.fill().await?;
        }
        Ok(())
    }

    async fn drain(&mut self) -> Result<(), HttpError> {
        loop {
            if !self.available().is_empty() {
                self.start = self.end;
            } else if self.leftover_pos < self.leftover.len() || self.remaining > 0 {
                self.fill().await?;
            } else {
                return Ok(());
            }
        }
    }
}

async fn stream_upload(
    sock: &mut TcpStream,
    head: &Head,
    dir: &Path,
) -> Result<Uploaded, HttpError> {
    let len = content_length(head)?;
    if len == 0 {
        return Err(HttpError::new(400, "malformed multipart body"));
    }

    if head
        .headers
        .get("expect")
        .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"))
    {
        sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| HttpError::new(400, "could not send 100 Continue"))?;
    }

    let content_type = head
        .headers
        .get("content-type")
        .map(String::as_str)
        .unwrap_or("");
    let boundary = multipart_boundary(content_type)?;
    let mut body = UploadBody::new(sock, &head.leftover, len);
    let mut temp = None;
    let result = parse_upload_body(&mut body, &boundary, dir, &mut temp).await;
    if result.is_err()
        && let Some(path) = temp.take()
    {
        remove_upload_temp(&path).await;
    }
    result
}

async fn parse_upload_body(
    body: &mut UploadBody<'_>,
    boundary: &str,
    dir: &Path,
    temp: &mut Option<PathBuf>,
) -> Result<Uploaded, HttpError> {
    let delimiter = format!("--{boundary}").into_bytes();
    let mut marker = b"\r\n".to_vec();
    marker.extend_from_slice(&delimiter);

    body.ensure(delimiter.len()).await?;
    if !body.available().starts_with(&delimiter) {
        return Err(HttpError::new(400, "malformed multipart body"));
    }
    body.consume(delimiter.len());

    let mut filename: Option<String> = None;
    let mut target = "music".to_string();
    let mut file_path: Option<PathBuf> = None;
    let mut file_size = 0u64;

    loop {
        body.ensure(2).await?;
        if body.available().starts_with(b"--") {
            body.consume(2);
            break;
        }
        if !body.available().starts_with(b"\r\n") {
            return Err(HttpError::new(400, "malformed multipart boundary"));
        }
        body.consume(2);

        let header_end = loop {
            if let Some(pos) = find(body.available(), b"\r\n\r\n") {
                break pos;
            }
            if body.available().len() >= MAX_PART_HEADERS {
                return Err(HttpError::new(400, "multipart headers too large"));
            }
            let want = body.available().len() + 1;
            body.ensure(want).await?;
        };
        let (field, part_filename) = {
            let headers = std::str::from_utf8(&body.available()[..header_end])
                .map_err(|_| HttpError::new(400, "multipart headers must be UTF-8"))?;
            match header_value(headers, "content-disposition") {
                Some(disposition) => (
                    disposition_param(disposition, "name"),
                    disposition_param(disposition, "filename"),
                ),
                None => (None, None),
            }
        };
        body.consume(header_end + 4);

        let mut sink = match (field.as_deref(), part_filename) {
            (Some("file"), Some(file_name)) => {
                if filename.is_some() {
                    return Err(HttpError::new(400, "only one file per upload is supported"));
                }
                let (path, file) = create_upload_temp(dir).await?;
                *temp = Some(path.clone());
                filename = Some(file_name);
                PartSink::File {
                    file,
                    path,
                    size: 0,
                }
            }
            (Some("target"), None) => PartSink::Field(Vec::new()),
            _ => PartSink::Discard,
        };

        loop {
            if let Some(pos) = find(body.available(), &marker) {
                if pos > 0 {
                    sink.write(&body.available()[..pos]).await?;
                }
                body.consume(pos + marker.len());
                break;
            }

            // Keep the longest suffix that can still become the marker, so a
            // boundary split across reads is never written into the file.
            let keep = marker.len().saturating_sub(1);
            let available = body.available().len();
            if available > keep {
                let write_len = available - keep;
                sink.write(&body.available()[..write_len]).await?;
                body.consume(write_len);
            }
            body.ensure(keep + 1).await?;
        }

        match sink {
            PartSink::File {
                mut file,
                path,
                size,
            } => {
                file.flush()
                    .await
                    .map_err(|e| HttpError::new(500, format!("could not write the upload: {e}")))?;
                file.sync_all()
                    .await
                    .map_err(|e| HttpError::new(500, format!("could not write the upload: {e}")))?;
                file_path = Some(path);
                file_size = size;
            }
            PartSink::Field(data) => target = parse_target(&data)?,
            PartSink::Discard => {}
        }

        body.ensure(2).await?;
        if body.available().starts_with(b"--") {
            body.consume(2);
            break;
        }
        if !body.available().starts_with(b"\r\n") {
            return Err(HttpError::new(400, "malformed multipart boundary"));
        }
    }

    body.drain().await?;
    let filename = filename.ok_or_else(|| HttpError::new(400, "missing file field"))?;
    let temp = file_path.ok_or_else(|| HttpError::new(400, "missing file field"))?;
    if file_size == 0 {
        return Err(HttpError::new(400, "empty file"));
    }
    Ok(Uploaded {
        filename,
        target,
        temp,
        size: file_size,
    })
}

fn parse_target(data: &[u8]) -> Result<String, HttpError> {
    let value = std::str::from_utf8(data)
        .map_err(|_| HttpError::new(400, "invalid target field"))?
        .trim();
    if value != "music" && value != "jingles" {
        return Err(HttpError::new(400, "invalid target"));
    }
    Ok(value.to_string())
}

async fn create_upload_temp(dir: &Path) -> Result<(PathBuf, tokio::fs::File), HttpError> {
    for _ in 0..8 {
        let mut rnd = [0u8; 8];
        SysRng.try_fill_bytes(&mut rnd).expect("system RNG failed");
        let path = dir.join(format!(
            ".upload-{}-{}.part",
            std::process::id(),
            hex::encode(rnd)
        ));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(HttpError::new(
                    500,
                    format!("could not write the upload: {e}"),
                ));
            }
        }
    }
    Err(HttpError::new(
        500,
        "could not allocate a temporary upload file",
    ))
}

async fn remove_upload_temp(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

fn multipart_boundary(content_type: &str) -> Result<String, HttpError> {
    if !content_type
        .to_ascii_lowercase()
        .starts_with("multipart/form-data")
    {
        return Err(HttpError::new(
            415,
            "content-type must be multipart/form-data",
        ));
    }
    for part in content_type.split(';').skip(1) {
        if let Some((key, value)) = part.trim().split_once('=')
            && key.trim().eq_ignore_ascii_case("boundary")
        {
            let value = value.trim().trim_matches('"');
            if !value.is_empty() && value.len() <= 70 && value.bytes().all(|b| b.is_ascii_graphic())
            {
                return Ok(value.to_string());
            }
        }
    }
    Err(HttpError::new(400, "missing multipart boundary"))
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim().eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn disposition_param(value: &str, key: &str) -> Option<String> {
    for part in value.split(';').skip(1) {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(key) {
            let value = value.trim().trim_matches('"');
            return Some(value.to_string());
        }
    }
    None
}

fn finalize_upload(
    dir: &Path,
    temp: &Path,
    name: &str,
    target: &str,
    kind: MediaKind,
) -> Result<(PathBuf, bool), HttpError> {
    let result = (|| {
        let target_dir = match target {
            "music" => dir.to_path_buf(),
            "jingles" => dir.join("jingles"),
            _ => return Err(HttpError::new(400, "invalid target")),
        };
        fs::create_dir_all(&target_dir).map_err(|e| {
            HttpError::new(500, format!("could not create the target directory: {e}"))
        })?;

        let final_path = target_dir.join(name);
        let overwritten = match fs::symlink_metadata(&final_path) {
            Ok(meta) if meta.is_file() => true,
            Ok(_) => {
                return Err(HttpError::new(
                    409,
                    "an entry with that name already exists",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(_) => return Err(HttpError::new(500, "could not inspect the target file")),
        };

        validate_media(temp, kind)
            .map_err(|e| HttpError::new(415, format!("not a valid media file: {e}")))?;

        fs::rename(temp, &final_path)
            .map_err(|e| HttpError::new(500, format!("could not finalise the upload: {e}")))?;
        Ok((final_path, overwritten))
    })();

    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn delete_file(shared: &Shared, raw: &str) -> Result<(), HttpError> {
    let rel = safe_relative(raw)?;
    if media_kind(&rel.to_string_lossy()).is_none() {
        return Err(HttpError::new(
            415,
            "only .wav and .flac files can be deleted",
        ));
    }

    let root = fs::canonicalize(&shared.dir)
        .map_err(|_| HttpError::new(500, "music directory is unavailable"))?;
    let target = root.join(&rel);

    let meta = fs::symlink_metadata(&target).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            HttpError::new(404, "file not found")
        } else {
            HttpError::new(500, "could not inspect the file")
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(HttpError::new(403, "refusing to delete a symlink"));
    }
    if !meta.is_file() {
        return Err(HttpError::new(400, "not a file"));
    }

    let canonical =
        fs::canonicalize(&target).map_err(|_| HttpError::new(500, "could not resolve the file"))?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(HttpError::new(403, "path escapes the music directory"));
    }

    fs::remove_file(&canonical).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            HttpError::new(404, "file not found")
        } else {
            HttpError::new(500, "could not delete the file")
        }
    })?;
    // Remove it from the upcoming/in-flight view immediately; the prefetch
    // thread will drop it from the library on its next command drain.
    shared.forget_file(&canonical);
    // Notify with the same path shape the station scanned (which may be a
    // relative or symlinked root), while deletion itself used the canonical
    // path only for the containment check.
    shared.notify(StationCmd::Remove(shared.dir.join(&rel)));
    Ok(())
}

fn resolve_media_file(shared: &Shared, raw: &str) -> Result<(PathBuf, String, PathBuf), HttpError> {
    let rel = safe_relative(raw)?;
    if media_kind(&rel.to_string_lossy()).is_none() {
        return Err(HttpError::new(
            415,
            "only .wav and .flac files are supported",
        ));
    }

    let root = fs::canonicalize(&shared.dir)
        .map_err(|_| HttpError::new(500, "music directory is unavailable"))?;
    let target = root.join(&rel);
    let meta = fs::symlink_metadata(&target).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            HttpError::new(404, "file not found")
        } else {
            HttpError::new(500, "could not inspect the file")
        }
    })?;
    if meta.file_type().is_symlink() {
        return Err(HttpError::new(403, "refusing to access a symlink"));
    }
    if !meta.is_file() {
        return Err(HttpError::new(400, "not a file"));
    }

    let canonical =
        fs::canonicalize(&target).map_err(|_| HttpError::new(500, "could not resolve the file"))?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(HttpError::new(403, "path escapes the music directory"));
    }

    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((rel, name, canonical))
}

fn rename_file(shared: &Shared, raw: &str, raw_name: &str) -> Result<(String, String), HttpError> {
    let (rel, _, canonical) = resolve_media_file(shared, raw)?;
    let old_kind =
        media_kind(&rel.to_string_lossy()).expect("resolve_media_file checks the extension");

    let name = sanitize_filename(raw_name)?;
    match media_kind(&name) {
        Some(kind) if kind == old_kind => {}
        Some(_) => return Err(HttpError::new(400, "the file extension cannot change")),
        None => {
            return Err(HttpError::new(
                415,
                "the new name must end in .wav or .flac",
            ));
        }
    }

    let new_rel = match rel.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(&name),
        _ => PathBuf::from(&name),
    };
    if new_rel == rel {
        return Ok((rel.to_string_lossy().replace('\\', "/"), name));
    }

    let parent = canonical
        .parent()
        .ok_or_else(|| HttpError::new(500, "could not resolve the file"))?;
    let target = parent.join(&name);
    if fs::symlink_metadata(&target).is_ok() {
        return Err(HttpError::new(409, "a file with that name already exists"));
    }

    fs::rename(&canonical, &target).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            HttpError::new(404, "file not found")
        } else {
            HttpError::new(500, "could not rename the file")
        }
    })?;

    shared.file_renamed(&shared.dir.join(&rel), &shared.dir.join(&new_rel));
    Ok((new_rel.to_string_lossy().replace('\\', "/"), name))
}

async fn serve_download(sock: &mut TcpStream, path: &Path, name: &str) -> Result<(), HttpError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|_| HttpError::new(404, "file not found"))?;
    let len = file
        .metadata()
        .await
        .map_err(|_| HttpError::new(500, "could not inspect the file"))?
        .len();
    let content_type = match media_kind(name) {
        Some(MediaKind::Wav) => "audio/wav",
        Some(MediaKind::Flac) => "audio/flac",
        None => "application/octet-stream",
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Content-Disposition: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\r\n",
        content_disposition(name)
    );
    if sock.write_all(head.as_bytes()).await.is_err() {
        return Ok(());
    }
    let _ = tokio::io::copy(&mut file, sock).await;
    Ok(())
}

fn content_disposition(name: &str) -> String {
    format!(
        "attachment; filename=\"{}\"",
        name.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

struct FileEntry {
    path: String,
    name: String,
    size: u64,
    modified: u64,
    kind: &'static str,
}

fn list_files(root: &Path) -> io::Result<Vec<FileEntry>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let root = fs::canonicalize(root)?;
    let mut out = Vec::new();
    walk(&root, &root, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<FileEntry>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk(&path, root, out);
            continue;
        }
        if !file_type.is_file() || !crate::wav::is_media(&path) {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(FileEntry {
            kind: if rel.starts_with("jingles/") {
                "jingle"
            } else {
                "music"
            },
            path: rel,
            name,
            size: meta.len(),
            modified,
        });
    }
}

async fn serve_events(sock: &mut TcpStream, shared: &Shared, admin: bool) {
    let (mut rd, mut wr) = sock.split();
    let resp = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream\r\n\
                Cache-Control: no-store\r\n\
                X-Accel-Buffering: no\r\n\
                X-Content-Type-Options: nosniff\r\n\
                Referrer-Policy: no-referrer\r\n\
                Connection: close\r\n\r\n";
    if wr.write_all(resp.as_bytes()).await.is_err() {
        return;
    }

    let mut rx = shared.watch_status();
    let mut scratch = [0u8; 64];
    let mut keepalive = tokio::time::interval(SSE_KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    loop {
        let frame = {
            let status = rx.borrow_and_update();
            if admin {
                status.frame.clone()
            } else {
                status.public_frame.clone()
            }
        };
        if wr.write_all(&frame).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                _ = keepalive.tick() => {
                    if wr.write_all(b": keepalive\n\n").await.is_err() {
                        return;
                    }
                }
                got = rd.read(&mut scratch) => {
                    if matches!(got, Ok(0) | Err(_)) {
                        return;
                    }
                }
            }
        }
    }
}

/// The server only accepts loopback connections, so a reverse proxy on the
/// same host is normally the peer. Prefer the client address it forwards.
fn client_addr(headers: &HashMap<String, String>, peer: &str) -> String {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(first) = headers.get(name).and_then(|v| v.split(',').next()) {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    peer.to_string()
}

async fn serve_hls_playlist(
    sock: &mut TcpStream,
    shared: &Shared,
    segment_prefix: &str,
) -> Result<(), HttpError> {
    let Some(playlist) = shared.hls.playlist(segment_prefix) else {
        return Err(HttpError::new(404, "HLS is not ready"));
    };
    let _ = write_response(
        sock,
        200,
        "application/vnd.apple.mpegurl",
        &playlist,
        &[],
    )
    .await;
    Ok(())
}

async fn serve_hls_segment(
    sock: &mut TcpStream,
    shared: &Shared,
    path: &str,
    segment_prefix: &str,
) -> Result<(), HttpError> {
    let seq = hls_segment_seq(path, segment_prefix)
        .ok_or_else(|| HttpError::new(404, "HLS segment not found"))?;
    let Some(segment) = shared.hls.segment(seq) else {
        return Err(HttpError::new(404, "HLS segment has expired"));
    };
    let _ = write_response_with_cache(
        sock,
        200,
        "video/mp2t",
        &segment,
        "public, max-age=10",
        &[],
    )
    .await;
    Ok(())
}

fn hls_segment_seq(path: &str, segment_prefix: &str) -> Option<u64> {
    let name = path.strip_prefix(segment_prefix)?.strip_suffix(".ts")?;
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

async fn serve_stream(sock: &mut TcpStream, shared: &Arc<Shared>, peer: &str, head: &Head) {
    let user_agent = head.headers.get("user-agent").cloned().unwrap_or_default();
    let listener = shared.register_listener(client_addr(&head.headers, peer), user_agent);
    let mut disconnect = listener.disconnect_signal();
    if *disconnect.borrow() {
        return;
    }

    let resp = "HTTP/1.1 200 OK\r\n\
                Content-Type: audio/ogg; codecs=opus\r\n\
                Cache-Control: no-cache\r\n\
                X-Content-Type-Options: nosniff\r\n\
                Connection: close\r\n\r\n";
    if sock.write_all(resp.as_bytes()).await.is_err() {
        return;
    }
    if sock.write_all(&shared.head).await.is_err() {
        return;
    }

    let mut rx = shared.subscribe();
    let mut last_seq: u64 = 0;
    for (seq, page) in shared.burst() {
        if *disconnect.borrow() {
            return;
        }
        last_seq = seq;
        if sock.write_all(&page).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            _ = disconnect.changed() => return,
            page = rx.recv() => match page {
                Ok((seq, page)) => {
                    if seq <= last_seq {
                        continue;
                    }
                    last_seq = seq;
                    if sock.write_all(&page).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
        }
    }
}

async fn error_response(sock: &mut TcpStream, error: &HttpError) -> io::Result<()> {
    let body = json!({"error": error.message}).to_string();
    write_response(
        sock,
        error.status,
        "application/json; charset=utf-8",
        body.as_bytes(),
        &[],
    )
    .await
}

async fn respond_json(sock: &mut TcpStream, status: u16, value: &Value, extra: &[(&str, &str)]) {
    let body = value.to_string();
    let _ = write_response(
        sock,
        status,
        "application/json; charset=utf-8",
        body.as_bytes(),
        extra,
    )
    .await;
}

async fn write_response(
    sock: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> io::Result<()> {
    write_response_with_cache(sock, status, content_type, body, "no-store", extra).await
}

async fn write_response_with_cache(
    sock: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    cache: &str,
    extra: &[(&str, &str)],
) -> io::Result<()> {
    let mut head = String::with_capacity(256);
    head.push_str("HTTP/1.1 ");
    head.push_str(&status.to_string());
    head.push(' ');
    head.push_str(status_text(status));
    head.push_str("\r\n");
    head.push_str(&format!(
        "Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: {cache}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n",
        body.len()
    ));
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");

    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let first = needle[0];
    let last = haystack.len().checked_sub(needle.len())?;
    let mut start = 0;
    while start <= last {
        let offset = haystack[start..=last].iter().position(|&b| b == first)?;
        let pos = start + offset;
        if haystack[pos..pos + needle.len()] == *needle {
            return Some(pos);
        }
        start = pos + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_reports_the_first_match() {
        assert_eq!(find(b"abcabc", b"abc"), Some(0));
        assert_eq!(find(b"xabcabc", b"abc"), Some(1));
        assert_eq!(find(b"abcab", b"cab"), Some(2));
        assert_eq!(find(b"abcXYZ", b"XYZ"), Some(3));
        assert_eq!(find(b"aaaaab", b"aab"), Some(3));
        assert_eq!(find(b"abc", b"abcd"), None);
        assert_eq!(find(b"", b"a"), None);
        assert_eq!(find(b"abc", b""), Some(0));
    }

    #[test]
    fn hls_segment_paths_accept_only_numeric_ts_names() {
        assert_eq!(
            hls_segment_seq("/radio-12.ts", "/radio-"),
            Some(12)
        );
        assert_eq!(hls_segment_seq("/radio-.ts", "/radio-"), None);
        assert_eq!(hls_segment_seq("/radio-12.m3u8", "/radio-"), None);
        assert_eq!(hls_segment_seq("/radio-12.ts/extra", "/radio-"), None);
        assert_eq!(hls_segment_seq("/live/audio-7.ts", "/live/audio-"), Some(7));
    }

    #[test]
    fn admin_ui_does_not_poll_the_queue() {
        assert!(ADMIN.contains("loadQueue"), "admin.html must be embedded");
        assert!(
            !ADMIN.contains("setInterval"),
            "the admin UI must not poll; queue updates come from the SSE status"
        );
    }

    #[test]
    fn admin_ui_keeps_no_refresh_timer() {
        assert!(
            !ADMIN.contains("refreshTimer"),
            "the admin UI must not keep a polling timer handle"
        );
    }

    #[test]
    fn admin_ui_exposes_live_listeners_and_a_disconnect_control() {
        assert!(is_admin_page("/admin/listeners"));
        assert!(
            ADMIN.contains("view-listeners"),
            "admin.html must embed the listeners view"
        );
        assert!(
            ADMIN.contains("listener_list"),
            "the view must read the live list from the SSE status frame"
        );
        assert!(
            ADMIN.contains("/api/listeners/disconnect"),
            "the view must offer a disconnect control"
        );
    }

    #[test]
    fn client_addr_prefers_the_forwarded_client() {
        let headers = HashMap::from([(
            "x-forwarded-for".to_string(),
            "203.0.113.9, 127.0.0.1".to_string(),
        )]);
        assert_eq!(client_addr(&headers, "127.0.0.1"), "203.0.113.9");

        let headers = HashMap::from([("x-real-ip".to_string(), "198.51.100.4".to_string())]);
        assert_eq!(client_addr(&headers, "127.0.0.1"), "198.51.100.4");

        assert_eq!(client_addr(&HashMap::new(), "127.0.0.1"), "127.0.0.1");
        let blank = HashMap::from([("x-forwarded-for".to_string(), " , ".to_string())]);
        assert_eq!(client_addr(&blank, "127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn parse_listener_id_accepts_only_integer_ids() {
        assert_eq!(
            parse_listener_id(br#"{"id":42}"#).map_err(|e| e.message),
            Ok(42)
        );
        assert!(parse_listener_id(br#"{"id":"42"}"#).is_err());
        assert!(parse_listener_id(br#"{"id":-1}"#).is_err());
        assert!(parse_listener_id(br#"{}"#).is_err());
        assert!(parse_listener_id(b"not json").is_err());
    }

    #[test]
    fn queue_search_keeps_the_full_queue_position() {
        let track = |title: &str| {
            Arc::new(NowPlaying {
                artist: String::new(),
                title: title.to_string(),
                file: format!("{title}.flac"),
                format: String::new(),
            })
        };
        let queue = vec![
            track("alpha"),
            track("beta"),
            track("alpha two"),
            track("gamma"),
        ];

        let all: Vec<usize> = filter_queue(queue.clone(), "")
            .iter()
            .map(|(position, _)| position + 1)
            .collect();
        assert_eq!(all, vec![1, 2, 3, 4]);

        let matched: Vec<usize> = filter_queue(queue, "alpha")
            .iter()
            .map(|(position, _)| position + 1)
            .collect();
        assert_eq!(matched, vec![1, 3]);
    }
}
