#!/usr/bin/env python3
"""Encoder conformance: encode a deterministic tone, decode with ffmpeg, null it.

This is the mandated external oracle for the Rust CELT encoder. It fails if the
bitstream stops being decodable by libopus or if the reconstruction regresses.
"""

import math
import struct
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
STREAM = ROOT / "target" / "conformance.opus"
RAW = ROOT / "target" / "conformance.raw"

FRAMES = 250
RATE = 48000
PRESKIP = 120


def source(k: int, ch: int) -> float:
    t = k / RATE
    if ch == 0:
        return 0.25 * math.sin(2 * math.pi * 440.0 * t)
    click = 0.5 if k % RATE == 0 else 0.0
    return 0.25 * math.sin(2 * math.pi * 660.0 * t) + click


def main() -> int:
    subprocess.run(
        [
            "cargo",
            "test",
            "--quiet",
            "encoder::tests::write_conformance_stream",
            "--",
            "--ignored",
        ],
        cwd=ROOT,
        check=True,
    )
    if not STREAM.is_file():
        print(f"FAIL: {STREAM} was not written")
        return 1

    subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            str(STREAM),
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            str(RAW),
        ],
        check=True,
    )

    raw = RAW.read_bytes()
    decoded = struct.unpack(f"<{len(raw) // 4}f", raw)
    samples = len(decoded) // 2

    # The decoder removes PRESKIP samples of codec delay.
    expected = FRAMES * (RATE // 50) - PRESKIP
    if abs(samples - expected) > 2:
        print(f"FAIL: decoded {samples} frames, expected {expected}")
        return 1

    # Tone content must not be inverted or shifted: search a small lag window.
    best_lag, best_snr = 0, -1e9
    for lag in range(0, 240):
        num = den = 0.0
        for k in range(1000, samples - lag):
            err = decoded[2 * (k + lag)] - source(k, 0)
            num += err * err
            den += source(k, 0) ** 2
        snr = 10.0 * math.log10(den / num) if num > 0 else 999.0
        if snr > best_snr:
            best_lag, best_snr = lag, snr

    if best_lag != 0:
        print(f"FAIL: best alignment lag is {best_lag}, expected 0")
        return 1
    if best_snr < 30.0:
        print(f"FAIL: null-test SNR {best_snr:.2f} dB is below 30 dB")
        return 1

    print(f"PASS: {samples} frames, {best_snr:.2f} dB null SNR, preskip aligned")
    return 0


if __name__ == "__main__":
    sys.exit(main())
