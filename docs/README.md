# radio-server

`radio-station` is a single-process internet radio server. It reads local audio files, encodes them into a continuous live stream, and serves both the web player and administrative controls from one binary.

## How it works

The server splits its work across three parts:

- **Prefetch thread:** Reads WAV and FLAC files from your music directory, resamples them to 48 kHz, trims silence, and crossfades between tracks.
- **Station thread:** Encodes the audio using a Rust port of the `libopus` floating-point CELT encoder, wraps each packet in its own Ogg page, and releases pages on a steady wall-clock schedule. (The production path only ever encodes Opus—decoding only happens in tests.)
- **Tokio runtime:** Handles all HTTP traffic, including the web player, admin API, two SSE status feeds, and the live Ogg stream.

HLS output is generated directly from the same page stream. See [audio-pipeline.md](audio-pipeline.md#hls-output) for details.

## System guarantees & invariants

### Audio encoding & verification
- **Fixed encoder settings:** Audio is always encoded as CELT at 48 kHz using 20 ms stereo frames, complexity 10, and a 320 kbps VBR target. Every TOC byte is `0xFC` (config 31, single frame).
- **Bitstream stability:** Stored hashes verify every reference packet; only regenerate them if you intentionally change the bitstream.
- **Formal proofs:** The `proofs/` directory contains Lean proofs covering the encoder, FLAC decoder, resampler, mixer, MDCT, Ogg framing, and account thread.

### Streaming & buffering
- **Zero-copy sharing:** Each Ogg page holds one Opus packet inside an immutable `Bytes` buffer shared across all connected listeners.
- **Pacing:** The station primes 500 pages before starting its clock, then releases one page every 20 ms.
- **Listener catch-up:** When a new listener connects, they receive the `OpusHead` and `OpusTags` headers, followed by a 10-second ring buffer of recent pages, before seamlessly transitioning to live pages.

### Networking & reverse proxying
- **Localhost binding:** The server only listens on `127.0.0.1`. To expose it publicly, place a reverse proxy on the same host.
- **Client IP logging:** For the stream listener logs, the server records the first address in `X-Forwarded-For` (falling back to `X-Real-IP`) rather than the proxy's socket peer address.

### Authentication
- **Single-account model:** There is only one account, which is created automatically upon the first successful login.
- **Storage & sessions:** The Argon2id password hash is written atomically to `auth.json` (with `0600` permissions on Unix). Active sessions live strictly in memory and are cleared on restart.

### Unsafe Rust
`unsafe` is restricted to exactly two places in the codebase:
1. **`src/pagecache.rs`:** A Linux `posix_fadvise` call providing best-effort OS caching hints on a live `File` descriptor.
2. **`wasm/spectrum.rs`:** Single-threaded static state for the WebAssembly spectrum analyzer, which JavaScript can only access via exported buffers.

## Building and running

Building requires **edition-2024 Rust** and the **`wasm32-unknown-unknown`** target. During the build, `build.rs` invokes `rustc` directly to compile `wasm/spectrum.rs` and embeds the resulting WebAssembly module into the server binary.

```sh
rustup target add wasm32-unknown-unknown
cargo build --release
./target/release/radio-station <music-dir> [port]
```

### Configuration & platform notes
- **Port:** Defaults to `8080` if omitted.
- **State files:** By default, `auth.json` and the endpoint settings file are stored in the current working directory. Override their paths with `RADIO_AUTH_FILE` and `RADIO_SETTINGS_FILE`.
- **CPU targets:** On `x86-64`, builds default to `target-cpu=x86-64-v3`, which requires AVX2 support (Intel Haswell / 2013 or newer). On `aarch64`, no extra CPU flags are set. See `.cargo/config.toml` for the benchmarks behind these defaults.

## Documentation

- [architecture.md](architecture.md) — Runtime structure and shared state
- [audio-pipeline.md](audio-pipeline.md) — Signal processing, encoding, and HLS output
- [web-server.md](web-server.md) — HTTP endpoints, SSE feeds, and web player
- [operations.md](operations.md) — Deployment and reverse-proxy configuration
- [development.md](development.md) — Tests, Lean proofs, benchmarks, and contribution guidelines
