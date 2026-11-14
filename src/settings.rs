// settings.rs — runtime configuration for the public audio endpoints.
//
// The file is created with defaults on first start. Every update is validated
// before it is written, and the file is replaced atomically. The HTTP layer
// reads a snapshot per request; the station only needs the HLS enable flag,
// which is mirrored in Shared.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use rand::TryRng;
use rand::rngs::SysRng;
use serde::{Deserialize, Serialize};

const MAX_PATH_BYTES: usize = 200;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub stream_path: String,
    pub playlist_path: String,
    pub hls_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            stream_path: "/radio.opus".into(),
            playlist_path: "/radio.m3u8".into(),
            hls_enabled: true,
        }
    }
}

/// A full update. Missing fields are rejected, unlike a settings file, so a
/// partial JSON body cannot silently reset unrelated options.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsUpdate {
    pub stream_path: String,
    pub playlist_path: String,
    pub hls_enabled: bool,
}

impl SettingsUpdate {
    pub fn into_settings(self) -> Settings {
        Settings {
            stream_path: self.stream_path,
            playlist_path: self.playlist_path,
            hls_enabled: self.hls_enabled,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        validate_route("stream_path", &self.stream_path, ".opus")?;
        validate_route("playlist_path", &self.playlist_path, ".m3u8")?;
        Ok(())
    }

    /// `/radio.m3u8` -> `/radio-`; the segment route and playlist lines share it.
    pub fn segment_prefix(&self) -> String {
        let stem = self
            .playlist_path
            .strip_suffix(".m3u8")
            .unwrap_or(&self.playlist_path);
        format!("{stem}-")
    }
}

fn validate_route(field: &str, path: &str, suffix: &str) -> Result<(), String> {
    if path.len() < 2 || path.len() > MAX_PATH_BYTES {
        return Err(format!("{field} must be 2 to {MAX_PATH_BYTES} bytes"));
    }
    if !path.starts_with('/') || path.ends_with('/') {
        return Err(format!(
            "{field} must be an absolute path with no trailing slash"
        ));
    }
    if !path.ends_with(suffix) {
        return Err(format!("{field} must end with {suffix}"));
    }
    if path.contains("//")
        || path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
    {
        return Err(format!(
            "{field} must not contain empty, '.' or '..' segments"
        ));
    }
    if !path.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/')
    }) {
        return Err(format!(
            "{field} may contain only ASCII letters, digits, '-', '.', '_', '~', and '/'"
        ));
    }
    if path == "/events"
        || path == "/spectrum.wasm"
        || path == "/api"
        || path.starts_with("/api/")
        || path == "/admin"
        || path.starts_with("/admin/")
    {
        return Err(format!("{field} collides with a built-in route"));
    }
    Ok(())
}

pub struct SettingsStore {
    path: PathBuf,
    inner: RwLock<Settings>,
    /// Serialises file writes without holding the read/write lock across
    /// fsync, so request-time reads never wait on disk I/O.
    write: Mutex<()>,
}

impl SettingsStore {
    pub fn load_or_create(path: &Path) -> io::Result<SettingsStore> {
        let settings = if path.exists() {
            let raw = fs::read(path)?;
            let settings: Settings = serde_json::from_slice(&raw).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: invalid settings file: {error}", path.display()),
                )
            })?;
            settings.validate().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {error}", path.display()),
                )
            })?;
            settings
        } else {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            let settings = Settings::default();
            persist(path, &settings)?;
            eprintln!("[settings] created {} with defaults", path.display());
            settings
        };

        Ok(SettingsStore {
            path: path.to_path_buf(),
            inner: RwLock::new(settings),
            write: Mutex::new(()),
        })
    }

    pub fn get(&self) -> Settings {
        self.inner
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Validates, persists, then publishes. A failed write leaves memory and
    /// the file untouched. Call from a blocking context: `persist` fsyncs.
    pub fn update(&self, next: Settings) -> io::Result<Settings> {
        next.validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let _write = self.write.lock().unwrap_or_else(|error| error.into_inner());
        persist(&self.path, &next)?;
        *self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner()) = next.clone();
        Ok(next)
    }
}

fn persist(path: &Path, settings: &Settings) -> io::Result<()> {
    let mut body = serde_json::to_vec_pretty(settings)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    body.push(b'\n');
    write_atomic(path, &body)
}

fn write_atomic(path: &Path, body: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut rnd = [0u8; 8];
    SysRng
        .try_fill_bytes(&mut rnd)
        .map_err(|error| io::Error::other(format!("system RNG failed: {error:?}")))?;
    let tmp = parent.join(format!(
        ".settings-{}-{}.tmp",
        std::process::id(),
        hex::encode(rnd)
    ));

    let result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(body)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_root_paths() {
        let settings = Settings::default();
        assert_eq!(settings.stream_path, "/radio.opus");
        assert_eq!(settings.playlist_path, "/radio.m3u8");
        assert!(settings.hls_enabled);
        assert_eq!(settings.segment_prefix(), "/radio-");
    }

    #[test]
    fn validation_rejects_bad_routes() {
        let mut settings = Settings::default();
        for path in [
            "radio.opus",
            "/radio.opus/",
            "/api/radio.opus",
            "/admin/radio.opus",
            "/radio.opus?x",
            "/radio.opus#x",
            "/../radio.opus",
            "/r\u{00e9}adio.opus",
        ] {
            settings.stream_path = path.into();
            assert!(settings.validate().is_err(), "{path} should be rejected");
        }
        settings.stream_path = "/radio.opus".into();
        settings.playlist_path = "/radio.opus".into();
        assert!(settings.validate().is_err());
    }

    #[test]
    fn prefix_follows_the_playlist_directory() {
        let mut settings = Settings::default();
        settings.playlist_path = "/live/audio.m3u8".into();
        assert_eq!(settings.segment_prefix(), "/live/audio-");
    }

    #[test]
    fn update_persists_and_publishes() {
        let dir = std::env::temp_dir().join(format!(
            "radio-settings-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let store = SettingsStore::load_or_create(&path).unwrap();
        assert_eq!(store.get().stream_path, "/radio.opus");

        let mut next = store.get();
        next.stream_path = "/live/audio.opus".into();
        store.update(next).unwrap();
        assert_eq!(store.get().stream_path, "/live/audio.opus");

        let reread = SettingsStore::load_or_create(&path).unwrap();
        assert_eq!(reread.get().stream_path, "/live/audio.opus");
        let _ = fs::remove_dir_all(&dir);
    }
}
