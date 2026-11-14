# Audio pipeline

The path is fixed: decode and trim each file once, resample if needed, mix with crossfades, encode with CELT, mux into Ogg, and release at a paced rate. Internal audio is 48 kHz interleaved stereo `f32`. A block is 960 frames (`FRAME_FLOATS = 1920` floats), which is 20 ms.

## Library and naming

`scan_media` walks the music directory recursively, skips symlinks, and accepts only `.wav` or `.flac`, case-insensitively. `scan_library` leaves out paths under `<music-dir>/jingles`; `scan_jingles` walks that subtree. The prefetch thread scans once at startup. Files added later are invisible until a restart unless they come through the admin API, which turns uploads, deletes, and renames into `StationCmd` values.

`split_stem` strips a leading track number of one to three digits followed by `.`, then splits at the first `_-_` or `" - "`. The left side is the artist and the right side the title. `tidy` replaces underscores with spaces and collapses whitespace for display; `normalize_artist` uses the tidied lower-case form as the cooldown key. If there is no separator, the code logs a warning and uses the whole stem as the internal artist. The UI then shows an empty artist and the stem as the title. Jingles take their title from the file stem and display artist `Jingle`.

## Scheduling

`build_track` reads the container duration without decoding: `STREAMINFO` for FLAC, the `data` chunk for WAV. The duration is a projection. Trimming can make a track shorter, and the scheduler repairs the schedule when the mixer clock drifts. A header without a duration falls back to 180 s.

The playlist is a schedule, not scan order. `shuffled_tracks` creates the preference order at startup and whenever the queue wraps. `schedule_tracks` groups tracks by artist and uses two heaps: eligible artists ordered by preference rank, and artists in cooldown ordered by the time they become eligible. It gives each track a projected start and advances the clock by that track's duration. When several artists are eligible, the earliest-shuffled one wins, which preserves the shuffled order where possible. Cooldown is enforced during placement: a track starts only when `clock >= last_end[artist] + 3600`, where `last_end` is the actual end of the last track by that artist on the mixed audio timeline. If no artist is eligible, the one that becomes eligible soonest is scheduled anyway; a radio stream cannot insert silence. The scheduler logs how many slots it forced. The queue is rebuilt on startup, on wrap-around, after a library change, or when the actual mixer clock proves the head ineligible. Per-selection work is a head check, not a rotation.

Top-of-hour jingles use the same playout clock, not the prefetch thread's wall clock. Once pacing starts, the station publishes an audio epoch: playout wall time is `epoch + mixer.next_start_secs()`. A jingle is due at the first track boundary whose projected start is at or after `next_hour_after`. A song that starts before the hour and is still playing across it delays the jingle to the next boundary. That deadline is consumed and the next one is computed from the boundary. The prefetch lead cannot move a jingle behind a song selected before the hour. A missing jingle is logged and the next hour is armed.

`StationCmd` mutations are drained before each scheduling decision and again every five seconds while idle. `AddTrack` deduplicates by exact path, builds the track, and inserts it at a uniformly random position. The next scheduling pass reorders the queue. New uploads are marked fresh and held at the head for one second before playback. The encoder buffer hides the delay. `Remove` drops the path from the library, jingles, and playlist, and immediately removes it from the published queue and in-flight announcements. `Rename` rebuilds affected entries in place, preserving queue position and updating in-flight names. Paths under `jingles/` go to `AddJingle` regardless of which channel they arrive on.

## Analysis and trimming

The production path decodes a file once and records a peak envelope while streaming. Windows are 20 ms (`sample_rate / 50`), and each frame's peak is `max(|L|, |R|)`.

The start is the first frame above `-60 dBFS`; the end is one past the last such frame. A file with no sample above the threshold is rejected as entirely silent. The crossfade tail comes from scanning windows backwards for the last one at or above `-40 dBFS`; `ring_start` is the end of that window. Tail length is `min(end - ring_start, 8 s, (end - start) / 4)`, so a track never gives more than a quarter of its duration, or eight seconds, to a fade.

FLAC and WAV share the streaming loop. FLAC uses native `flac::Decoder` frames and WAV uses `WavStream`. The mixer keeps the most recent 15 seconds of resampled output outside itself. Older audio is fed normally. At EOF, the held tail is trimmed to the last audible frame and released with the actual ring. Fifteen seconds covers the eight-second maximum fade and makes the final partial read harmless. `pagecache::forget` runs after the decode, including after a failure.

## Decoding

FLAC uses the native decoder in `src/flac/`. It accepts 4 to 32 bits per sample, ignores channels beyond the first two, duplicates mono, and scales integers by `2^(bits-1)` into `f32`.

The WAV parser accepts RIFF/WAVE, a `fmt ` chunk of 16 to 4096 bytes, PCM at 8, 16, 24, or 32 bits, and IEEE 32-bit float. `WAVEFORMATEXTENSIBLE` is handled: a nonzero valid-bits field replaces the reported depth, and the first two bytes of the subformat GUID select PCM or float. Decoding still uses the container's sample width. Float samples are clamped to `[-1, 1]`, and non-finite values become zero. A `data` chunk of size zero means "to end of file". Only the first two channels are read; mono is duplicated.

Leading samples before `start` are skipped before resampling. After decoding, the retained tail is trimmed at `end`, and a resampler flush is appended before the final tail is released. Trim positions are computed on the source timeline and converted to output frames for the tail.

## Resampling

48 kHz sources skip the resampler. Supported input rates are 44.1, 64, 88.2, 96, 176.4, and 192 kHz. Anything else skips the track with a log line.

Every ratio is exact: `L / M = 48000 / fs`, and the prototype rate is `R = 48000 * M`. The coefficient table is a symmetric linear-phase Kaiser-windowed sinc designed at `R` with `N % (2*M) = 1`, giving `W = (N + L - 1) / L` taps per phase with zero padding. Output frame `k` sits at upsampled position `p = k*M`, so its input index advances by `M/L` and its phase by `M % L`; the initial index includes the group-delay offset. Group delay is `(N - 1) / (2*M) = 50` output frames. Discard the first 50 and flush 50 more at end of input, so the emitted stream starts at input sample zero and has exactly the rational length.

Arithmetic is fixed point: Q23 samples in `i32`, Q25 coefficients in `i32`, products accumulated in `i64`, then one scale. Samples are clamped to `±4` before conversion, which bounds the accumulator. The design targets a 20 kHz passband, a 24 kHz stopband, and 126 dB of attenuation. The shipped tables measure -126.8 dB worst case, above the -120 dB requirement.

`scaled_frames(fs, frames)` performs the corresponding integer conversion used by the mixer and trim logic. It stays exact where naive float scaling rounds differently. Tests cover those cases. `proofs/Resample.lean` models the structure and the fixed-point bounds.

The MAC loop dominates CPU time. On x86-64 the build targets `x86-64-v3` so LLVM can use AVX2's signed 32x32-to-64 multiply. Baseline SSE2 leaves the loop scalar. On aarch64 no flags are set: the loop already reaches the 64-bit integer multiply ceiling of 128-bit NEON on Neoverse-N1, and SVE is unavailable there. `.cargo/config.toml` records the measurements.

## Mixing and crossfades

`Mixer` holds interleaved samples, a write cursor, a hold count, a pushed-block count, and metadata for the next track. `begin_track(ring_frames, len_frames)` rewinds the cursor by `h = min(previous hold, buffered samples, new track length in samples)` and sets the new hold to the track's `ring_frames`. The rewound region is the previous track's fade tail; incoming samples are added into it, which is the crossfade. `add` sums the overlap and appends the remainder.

`commit` emits only whole 1920-float blocks and never drains into the current hold. Consumed samples stay at the front of the backing vector and are reclaimed in bulk once they reach half of it, so each block does not shift the whole held tail. The first emitted block takes the metadata attached by `begin_track`. The station publishes pages 500 blocks before their scheduled playback, so the now-playing announcement is queued for `frames_done + 500`, the moment the block reaches the live edge.

`end_secs` is `(pushed * 1920 + buffered) / 2 / 48000`, the end of the mixed timeline. `next_start_secs` subtracts the held tail and gives the start of the next track. `proofs/Station.lean` proves cursor and buffer bounds, whole-block emission, and conservation of the timeline total.

## Encoding

`Encoder::encode` consumes one 1920-float block and writes a packet with the TOC byte first. The configuration is fixed: two channels, `LM = 3`, `M = 8`, `N = 960`, complexity 10, a 320 kbps target, and TOC `0xFC`. The caller must provide at least 1276 bytes; the compressed payload is capped at 1275.

One frame follows the reference order: pre-emphasis into the overlap buffer, silence detection at `2^-24`, pitch downsampling and search, doubling removal, comb-filter prefilter state, transient analysis with optional short MDCTs, band energies and log2 conversion, band normalisation, dynamic allocation, time-frequency analysis, energy-error smoothing, two-pass coarse energy coding, TF encoding, spreading decision, bit allocation, fine energy, `quant_all_bands` with PVQ and stereo/intensity decisions, anti-collapse signalling, and final energy refinement. There is no SILK layer, no hybrid mode, no mode switching, and no delay buffer. The VBR target comes from the fixed bitrate; after allocation the packet can shrink, and a silent frame's compressed body is reduced to two bytes plus the TOC.

The encoder is meant to stay bit-exact with the reference float build. `encoder::tests::frames_match_reference_encoder` compares packet length and an FNV hash of every packet for a set of deterministic signals. `integration-tests/encoder_conformance.py` is the independent oracle: it writes a stream, decodes it with ffmpeg/libopus, checks the frame count after preskip removal, requires zero alignment lag, and requires at least 30 dB null-test SNR.

## Container framing

`PageWriter` writes RFC 7845 pages. The Ogg CRC uses polynomial `0x04c11db7`, MSB-first, initial value zero, and no final xor. The stream serial comes from `SystemTime` subsecond nanoseconds with the low bit forced to one. The header page is BOS, the tags page is ordinary, and each audio page carries one packet with granule `PRESKIP + frames_done * 960`.

`OpusHead` declares version 1, two channels, preskip 120 samples (2.5 ms of CELT lookahead), input rate 48 kHz, zero gain, and mapping family 0. `OpusTags` declares vendor `radio-station` and no comments. The 1275-byte packet bound keeps the lacing segment count below the 255-segment limit. CRC and lacing proofs are in `proofs/OggCrc.lean` and `proofs/OggLacing.lean`.

## Pacing and buffering

The station encodes 500 pages before starting its clock. After that, every page waits until `pace_start + (frames_done - 500) * 20 ms`. It never waits backwards: if encoding or decoding falls behind, pages are released immediately until the clock catches up. Prefetch may run sixty seconds ahead. The PCM channel holds 3000 blocks. When it fills, `Mixer::commit` blocks.

Pages are published as soon as they are encoded. The ring and the live edge represent audio ten seconds in the future relative to playback. A new listener gets the two header pages, all 500 ring pages, then live pages. A socket that falls behind the broadcast loses pages instead of stalling the station.

## HLS output

When HLS is enabled, each encoded page also goes into `hls::Hls`. Every 250 pages become one five-second MPEG-TS segment, assembled as pages arrive rather than batched at the boundary. The segment is built from the Opus packets: PAT/PMT, PES, 90 kHz PTS/PCR, and continuous TS continuity counters across segments. The page's Ogg framing is discarded because Ogg is not an HLS media segment format. The playlist is rendered on demand from the current segment ring using the configured path prefix.

Two completed five-second segments are kept in a ring that matches the ten-second page ring. The playlist exposes those two and increments `EXT-X-MEDIA-SEQUENCE` as the ring rotates. A request for a segment that has fallen out of the ring gets 404. HLS clients are not listeners and cannot apply backpressure.

The default public endpoints are `/radio.m3u8` and `/radio-<n>.ts`. The playlist path is configurable, and the segment prefix follows it. HLS can be disabled.

## Failure policy

Decode or trim failures skip the file, log the path and reason, and call `Mixer::abort_track`, which drops the hold and moves the cursor to the end so the failed track contributes no tail. Successful music tracks update `last_end[artist]`; failures do not. After `failures >= library.len()` consecutive skips, the schedule is cleared and the thread waits for a command, which prevents a spin over an unreadable library. Jingle failures are logged and aborted but do not count toward that threshold.

An encoder error or a disconnected PCM channel is fatal to the station thread. HTTP and SSE keep running, but no new pages are produced.
