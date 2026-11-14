# Development

## Build

Build and invocation are covered in [README.md](README.md#build-and-invocation). `build.rs` runs `rustc` directly for `wasm/spectrum.rs` with `--target wasm32-unknown-unknown`, `--crate-type cdylib`, `opt-level=3`, `panic=abort`, `codegen-units=1`, fat LTO, and stripped symbols. There is no separate packaging step; `include_bytes!` embeds the artifact.

Dependencies: Tokio for the runtime and sockets, `bytes` for shared pages, `rand` for playlist and session randomness, `argon2` for the account, `hex`, `serde`/`serde_json` for framing, and `libc` for the page-cache hint. FLAC decoding is native and self-contained; libFLAC's raw decoder is the differential oracle. Do not add dependencies to the encoder or resampler. Both must stay self-contained and bit-exact.

The release profile uses `opt-level = 3` with fat LTO. `.cargo/config.toml` sets `target-cpu=x86-64-v3` on x86-64 and nothing on aarch64; the comments there record why. Justify any change to those flags with the ignored resampler benchmark, and keep the output identical.

## Unit tests

```sh
cargo test
cargo test --release
cargo test encoder::tests::frames_match_reference_encoder
cargo test resample::tests
```

`src/flac/` is both the native FLAC decoder core and the application decoder. Its tests decode generated files and compare every sample, frame size, rate, depth, and channel count against libFLAC. Coverage includes 8/16/24/32-bit audio, mono through 8 channels, fixed and LPC prediction, wasted bits, mid-side, uncommon block sizes and sample rates, Rice/Rice2/escape partitions, and truncated or corrupt input.

The rest of the suite covers range-coder round trips and reference streams, PVQ round trips and reference matches, allocation and energy reference matches, FFT correctness, MDCT reference matches with TDAC, window power complementarity, Ogg lacing, WAV and FLAC decode identity, the native FLAC decoder matrix, mixer accounting, resampler length and frequency response, HLS MPEG-TS framing and playlist rotation, and HTTP parsing. `auth.rs` has no Rust tests. The Python suite exercises login, persistence, and malformed files.

`frames_match_reference_encoder` is the encoder contract. It compares the length and an FNV hash of every packet for several deterministic signals against stored values.

The `RangeDecoder`, the decode halves of `pvq.rs`, `alloc.rs`, and `energy.rs`, and `Mdct::backward` exist only under `cfg(test)`.

## Conformance oracle

```sh
python3 integration-tests/encoder_conformance.py
```

The script runs the ignored `encoder::tests::write_conformance_stream`, which writes `target/conformance.opus`. It then decodes the file with ffmpeg's libopus decoder, checks the frame count after preskip removal to within two frames, searches for alignment lag in the first 240 samples, and requires zero lag plus at least 30 dB null-test SNR. It needs an ffmpeg build with Opus support. The hash tests catch bitstream changes; this script checks that libopus can decode the result.

## Integration tests

```sh
python3 integration-tests/run_tests.py
```

The runner builds the release binary unless `RADIO_BIN` points at one or `RADIO_SKIP_BUILD=1` is set. It creates isolated temporary roots, starts real server processes, and talks to them over TCP.

The suite is split by area. `support.py` holds the process wrapper, HTTP client, media fixtures, and pass/fail counter. `suites/` contains `public.py` (player, login, status, SSE, queue pagination), `media.py` (uploads, downloads, renames, deletes), `http.py` (malformed requests, listeners, logout, log checks), `lifecycle.py` (restart, empty library, invalid account files), `auth.py` (concurrent account creation), and `settings.py` (runtime endpoints). `run_tests.py` only builds, creates roots, and calls them in order.

Coverage includes login and cookie handling, origin checks, malformed requests, pagination, uploads and overwrites, delete and rename, path traversal and symlink rejection, SSE, stream fan-out and disconnect, listener accounting, restart persistence, an empty library, invalid account files, concurrent first login, and endpoint settings. `RADIO_KEEP_TESTS=1` keeps the temporary roots for inspection.

The tests use a fixed username and password defined in `support.py`.

## Proofs

```sh
lake build
```

`lakefile.lean` defines the `radio_proofs` package against Lean 4.35.0 and Mathlib 4.35.0. The proofs are separate from the Rust build and are not required to compile the binary. They justify specific optimisations and index assumptions: range-coder interval arithmetic and decoder mirroring, resampler structure and fixed-point bounds, mixer buffer accounting, MDCT scratch coverage, window perfect reconstruction, pitch-loop reordering, PVQ invariants, bands scratch reuse, energy branch elimination, the pinned encoder configuration, the FLAC decoder's table CRCs, Rice residual range, LPC accumulator bound, stereo inverses, and the account thread's panic containment and single-account invariants. The account model lets the Argon2, `serde_json`, and filesystem oracles panic. Its only runtime premise is that `catch_unwind` catches unwinding panics, which the release profile makes explicit.

When you change an invariant that a proof covers, update the proof in the same change. A stale proof is misleading. `proofs/Soxr.lean` still models a libsoxr-based `src/sox.rs` that is not in the tree. The active resampler is `src/resample.rs`, proved in `proofs/Resample.lean`. The stale file remains in the Lake glob.

## Benchmarks

```sh
cargo test --release bench_resample -- --ignored --nocapture
cargo test --release bench_encoder -- --ignored --nocapture
cargo test --release bench_mixer -- --ignored --nocapture
cargo test --release bench_analyze -- --ignored --nocapture
cargo test --release bench_stream -- --ignored --nocapture
cargo test --release bench_schedule -- --ignored --nocapture
cargo test --release bench_probe -- --ignored --nocapture
cargo test --release bench_segment_mux -- --ignored --nocapture
```

Most report milliseconds per second of audio. Results compare across machines and CPU flags. `bench_segment_mux` reports microseconds per page and milliseconds per segment. The resampler benchmark exercises every supported input rate. `bench_mixer` isolates the crossfade buffer at an eight-second hold, `bench_analyze` covers the test-only WAV and FLAC analysis pass, `bench_stream` covers the whole one-pass prefetch path (decode, resampling, 15-second tail, and mixing), `bench_schedule` compares schedule construction with the old per-selection rotation for sparse and cooldown-dense libraries, `bench_probe` measures the header-only duration probe, and `bench_segment_mux` compares the Ogg-page and direct-packet HLS inputs. Run performance changes on the target architecture as well as the development machine. The x86-64 AVX2 path and the aarch64 NEON path have different ceilings.

## Change discipline

The encoder and resampler are reference implementations. Their structure follows libopus and the documented proof model. Refactoring for style is usually wrong. Preserve operation order where floating-point results depend on it. When a proof justifies an optimisation, keep the Rust and the Lean model in lockstep.

HTTP changes must preserve the security properties in [web-server.md](web-server.md): authentication before routing for protected prefixes, the origin check on every POST, path containment and symlink rejection for filesystem access, and pre-publication validation for uploads. When adding a route, update the routing comment at the top of `src/http.rs` and the route list in the documentation.

CI (`.github/workflows/rust.yml`) installs the wasm target and `flac`, then runs `cargo build` and `cargo test` on pushes and pull requests to `main`. The integration suite, conformance oracle, `lake build`, and benchmarks remain manual: run the integration suite for server changes, the conformance script for encoder changes, `lake build` for proof changes, and the relevant benchmark for performance changes.
