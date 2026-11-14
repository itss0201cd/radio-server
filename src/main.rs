mod alloc;
mod auth;
mod bands;
mod celt_tables;
mod encoder;
mod energy;
mod fft;
mod flac;
mod hls;
mod http;
mod mdct;
mod ogg;
mod pagecache;
mod pitch;
mod pvq;
mod range_coder;
mod resample;
mod settings;
mod station;
mod wav;
mod window;

use std::path::PathBuf;
use std::sync::Arc;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| {
        eprintln!("usage: radio-station <music-dir> [port]");
        std::process::exit(2);
    });
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8080);

    let auth_path = std::env::var_os("RADIO_AUTH_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("auth.json"));
    let auth = Arc::new(auth::Auth::load_or_create(&auth_path)?);

    let settings_path = std::env::var_os("RADIO_SETTINGS_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("settings.json"));
    let settings = Arc::new(settings::SettingsStore::load_or_create(&settings_path)?);
    let initial = settings.get();

    // Returns immediately; the station thread primes the rolling cache in the
    // background, and the prefetch thread waits for UI upload notifications.
    let shared = station::start(PathBuf::from(dir), initial.hls_enabled)?;

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!(
        "[http] web ui at http://{addr}/  —  stream at http://{addr}{}",
        initial.stream_path
    );
    if initial.hls_enabled {
        eprintln!("[http] HLS playlist at http://{addr}{}", initial.playlist_path);
    } else {
        eprintln!("[http] HLS disabled");
    }
    loop {
        let (sock, _peer) = listener.accept().await?;
        tokio::spawn(http::serve(
            sock,
            shared.clone(),
            auth.clone(),
            settings.clone(),
        ));
    }
}
