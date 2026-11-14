// resample.rs — exact rational polyphase resampler to 48 kHz.
//
// Only six input rates are supported (see `SUPPORTED`); 48 kHz source audio is
// passed through by the caller and never reaches this module. Every ratio is
// exact, so all timing state is integer arithmetic: no f64 ratio, no rounding,
// nothing to drift. `proofs/Resample.lean` proves the structure, and this file
// is its literal transcription:
//
//   h[0..N)  symmetric Kaiser-windowed sinc at the prototype rate R = L * fs
//            = 48000 * M, linear phase, N % (2*M) = 1
//   W        = (N + L - 1) / L, uniform taps per phase (zero padded)
//   table    per phase r, taps stored reversed so the inner loop reads forward
//   frame i  exists iff i * M < n * L, for n input frames seen
//   delay    = (N - 1) / (2*M) = 50 output frames, discarded from the start
//
// Discarding the first `delay` frames and flushing `delay` more is what makes
// the emitted stream start exactly at input sample 0 and end exactly at the
// rational length. Without it every resampled track would open with a ~1 ms
// ramp and be `delay` frames long relative to a 48 kHz track, which would
// shift the crossfade in `Mixer::begin_track`.
//
// Fixed point: samples Q23 in i32, coefficients Q25 in i32, products in i64.
// Q25 keeps the coefficient-quantisation floor near -136 dB, under the -120 dB
// design stopband. Samples are clamped to +-4 before conversion, so the
// accumulator is bounded by W * 2^50 (proved: `acc_fits`).
//
// The MAC loop is the station's hottest arithmetic, and baseline x86-64 cannot
// vectorise a signed 32x32->64 multiply, so `.cargo/config.toml` targets
// x86-64-v3 (Haswell, 2013). That alone took a minute of 44.1 kHz stereo from
// 204 ms to 99 ms; the carry below is division-free for the same reason -- it
// is 9% once the loop is vectorised and invisible before.

/// Target rate. Sources already at this rate are never passed in.
pub const OUT_RATE: usize = 48_000;

/// Input rates handled by this module.
pub const SUPPORTED: [u32; 6] = [44_100, 64_000, 88_200, 96_000, 176_400, 192_000];

const COEFF_BITS: u32 = 25;
const SAMPLE_BITS: u32 = 23;
/// Q(COEFF_BITS + SAMPLE_BITS) accumulator to real sample value.
const INV_ACC_SCALE: f64 = 1.0 / (1u64 << (COEFF_BITS + SAMPLE_BITS)) as f64;
const SAMPLE_ONE: f32 = (1u64 << SAMPLE_BITS) as f32;
const SAMPLE_LIMIT: f32 = 4.0;

/// Passband edge and stopband start. Opus fullband tops out at 20 kHz, so
/// matching anything above that would be discarded on the next hop anyway.
const PASS_HZ: f64 = 20_000.0;
const STOP_HZ: f64 = 24_000.0;

/// Kaiser design target. Measured worst case of the resulting tables is
/// -126.8 dB, which leaves margin over the -120 dB requirement.
const DESIGN_DB: f64 = 126.0;

/// Batched front-trimming keeps the inner loop contiguous. Below this many
/// dead samples a memmove is not worth the branch.
const DRAIN_MIN: usize = 4096;

/// Exact polyphase parameters for one input rate.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub fs: usize,
    pub l: usize,
    pub m: usize,
    pub n: usize,
}

impl Shape {
    pub const fn for_rate(fs: u32) -> Option<Shape> {
        match fs {
            44_100 => Some(Shape {
                fs: 44_100,
                l: 160,
                m: 147,
                n: 14_701,
            }),
            64_000 => Some(Shape {
                fs: 64_000,
                l: 3,
                m: 4,
                n: 401,
            }),
            88_200 => Some(Shape {
                fs: 88_200,
                l: 80,
                m: 147,
                n: 14_701,
            }),
            96_000 => Some(Shape {
                fs: 96_000,
                l: 1,
                m: 2,
                n: 201,
            }),
            176_400 => Some(Shape {
                fs: 176_400,
                l: 40,
                m: 147,
                n: 14_701,
            }),
            192_000 => Some(Shape {
                fs: 192_000,
                l: 1,
                m: 4,
                n: 401,
            }),
            _ => None,
        }
    }

    /// Prototype rate: both the filter's sample rate and `48000 * M`.
    pub const fn rate(&self) -> usize {
        self.l * self.fs
    }

    /// Uniform taps per phase, zero padded to a whole number of branches.
    pub const fn width(&self) -> usize {
        (self.n + self.l - 1) / self.l
    }

    /// Group delay in output frames; a whole number because `N % (2*M) = 1`.
    pub const fn delay(&self) -> usize {
        (self.n - 1) / (2 * self.m)
    }
}

/// Reduced exact ratio `L / M` for an input rate: `L * fs = OUT_RATE * M`.
/// `OUT_RATE` itself reduces to `1 / 1`, so the pass-through rate needs no
/// special case.
fn ratio(fs: usize) -> (usize, usize) {
    let g = gcd(OUT_RATE, fs);
    (OUT_RATE / g, fs / g)
}

const fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Exact number of output frames the resampler emits for `frames` input frames
/// at `fs`, rounded up. This is the same count `process` produces, so callers
/// that scale a trim or a crossfade overlap use it instead of an f64 ratio:
/// `ring * L / M` is an integer for tracks whose `ring` is a multiple of `M`,
/// and rounding that in floating point lands one frame high most of the time.
pub fn scaled_frames(fs: usize, frames: usize) -> usize {
    let (l, m) = ratio(fs);
    (frames * l).div_ceil(m)
}

/// Modified Bessel function of the first kind, order zero, by its series.
fn bessel_i0(x: f64) -> f64 {
    let q = 0.25 * x * x;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while term > 1e-17 * sum {
        term *= q / (k * k);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Symmetric Q25 coefficients, length `n`, summing to exactly `l * 2^25`.
///
/// The residual of the float-to-integer quantisation is placed on the centre
/// tap, which is its own mirror, so symmetry — and therefore exact linear
/// phase and an integral delay — survives the quantisation.
fn coefficients(shape: Shape) -> Vec<i32> {
    let (l, n) = (shape.l, shape.n);
    let rate = shape.rate() as f64;
    let cutoff = 0.5 * (PASS_HZ + STOP_HZ) / rate;
    let centre = (n - 1) as f64 * 0.5;
    let beta = 0.1102 * (DESIGN_DB - 8.7);
    let denom = bessel_i0(beta);

    let mut h = Vec::with_capacity(n);
    for j in 0..n {
        let t = 2.0 * j as f64 / (n - 1) as f64 - 1.0;
        let window = bessel_i0(beta * (1.0 - t * t).max(0.0).sqrt()) / denom;
        let x = j as f64 - centre;
        let y = std::f64::consts::PI * 2.0 * cutoff * x;
        let sinc = if x == 0.0 { 1.0 } else { y.sin() / y };
        h.push(2.0 * cutoff * sinc * window);
    }

    let gain = l as f64 / h.iter().sum::<f64>();
    let mut c: Vec<i32> = h
        .iter()
        .map(|v| (v * gain * (1u64 << COEFF_BITS) as f64).round() as i32)
        .collect();
    let total: i64 = c.iter().map(|&v| v as i64).sum();
    let residual = l as i64 * (1i64 << COEFF_BITS) - total;
    c[(n - 1) / 2] += residual as i32;
    c
}

/// Per-phase table, `width * l` entries, taps reversed within each phase.
fn polyphase(shape: Shape, c: &[i32]) -> Vec<i32> {
    let (l, w) = (shape.l, shape.width());
    let mut table = vec![0i32; w * l];
    for r in 0..l {
        for j in 0..w {
            let idx = r + (w - 1 - j) * l;
            if idx < shape.n {
                table[r * w + j] = c[idx];
            }
        }
    }
    table
}

fn to_q23(x: f32) -> i32 {
    // Q23 scaling is an exact power-of-two multiply in f32; the clamp bounds
    // the result well inside both f32 and i32. This avoids the f64 round trip
    // on every input sample.
    (x.clamp(-SAMPLE_LIMIT, SAMPLE_LIMIT) * SAMPLE_ONE).round() as i32
}

/// Streaming resampler for one track. Input is interleaved stereo f32.
pub struct Resampler {
    shape: Shape,
    w: usize,
    table: Vec<i32>,
    left: Vec<i32>,
    right: Vec<i32>,
    /// Upsampled position of the next output frame: `i * M`.
    p: usize,
    /// Newest tap index, in input frames, of the next output frame.
    q: usize,
    /// Polyphase branch of the next output frame.
    r: usize,
    /// `M / L` and `M % L`: the carry is one conditional subtract, not a
    /// division. See `step_fast` in proofs/Resample.lean.
    q_step: usize,
    r_step: usize,
    /// Frames dropped from the front of `left`/`right`.
    trimmed: usize,
    /// Real input frames appended so far.
    fed: usize,
}

impl Resampler {
    /// `None` for a rate this module does not handle (including 48 kHz).
    pub fn new(fs: u32) -> Option<Resampler> {
        debug_assert!(SUPPORTED.contains(&fs), "{fs} is not a supported rate");
        let shape = Shape::for_rate(fs)?;
        debug_assert_eq!(shape.rate(), OUT_RATE * shape.m, "exact ratio");
        let w = shape.width();
        let table = polyphase(shape, &coefficients(shape));
        let centre = shape.delay() * shape.m;
        Some(Resampler {
            shape,
            w,
            table,
            // Zero history: taps below input sample 0 are silence, which is
            // exactly what precedes a track.
            left: vec![0; w - 1],
            right: vec![0; w - 1],
            p: 0,
            q: centre / shape.l,
            r: centre % shape.l,
            q_step: shape.m / shape.l,
            r_step: shape.m % shape.l,
            trimmed: 0,
            fed: 0,
        })
    }

    /// Append interleaved stereo input, then emit every frame it makes
    /// available.
    pub fn resample(&mut self, input: &[f32], out: &mut Vec<f32>) {
        debug_assert_eq!(input.len() % 2, 0, "interleaved stereo input");
        self.left.reserve(input.len() / 2);
        self.right.reserve(input.len() / 2);
        for pair in input.chunks_exact(2) {
            self.left.push(to_q23(pair[0]));
            self.right.push(to_q23(pair[1]));
        }
        self.fed += input.len() / 2;
        self.process(out);
    }

    /// Emit the delayed tail. Appends `w` zeros of padding without counting
    /// them as input, so the emitted count stays at the exact rational length.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        self.left.resize(self.left.len() + self.w, 0);
        self.right.resize(self.right.len() + self.w, 0);
        self.process(out);
    }

    fn process(&mut self, out: &mut Vec<f32>) {
        let limit = self.fed * self.shape.l;
        let (l, m, w) = (self.shape.l, self.shape.m, self.w);
        let newest = self.trimmed + self.left.len();
        while self.p < limit {
            // `start_abs = q` because the zero prefix is exactly `w - 1`.
            if w - 1 + self.q >= newest {
                break;
            }
            let base = self.q - self.trimmed;
            let c = &self.table[self.r * w..self.r * w + w];
            let hl = &self.left[base..base + w];
            let hr = &self.right[base..base + w];

            let mut acc_l: i64 = 0;
            let mut acc_r: i64 = 0;
            for j in 0..w {
                let k = c[j] as i64;
                acc_l += k * hl[j] as i64;
                acc_r += k * hr[j] as i64;
            }
            // One rounding: i64 -> f64 is exact to well under f32's 24 bits.
            out.push((acc_l as f64 * INV_ACC_SCALE) as f32);
            out.push((acc_r as f64 * INV_ACC_SCALE) as f32);

            // `r_step < l`, so `r + r_step < 2l` and the carry is one
            // conditional subtract (proofs/Resample.lean: step_fast_eq).
            self.p += m;
            let t = self.r + self.r_step;
            let carry = (t >= l) as usize;
            self.r = t - carry * l;
            self.q += self.q_step + carry;
        }
        self.drain();
    }

    fn drain(&mut self) {
        let drop = self.q.saturating_sub(self.trimmed);
        if drop >= DRAIN_MIN {
            self.left.drain(..drop);
            self.right.drain(..drop);
            self.trimmed += drop;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_shapes() -> Vec<Shape> {
        SUPPORTED
            .iter()
            .filter_map(|&fs| Shape::for_rate(fs))
            .collect()
    }

    /// Interleaved stereo sine at `f` Hz.
    fn tone(shape: Shape, frames: usize, f: f64, amp: f32) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = (std::f64::consts::TAU * f * i as f64 / shape.fs as f64).sin() as f32 * amp;
            v.push(s);
            v.push(s);
        }
        v
    }

    fn run(shape: Shape, input: &[f32]) -> Vec<f32> {
        let mut r = Resampler::new(shape.fs as u32).expect("supported rate");
        let mut out = Vec::new();
        // Push in awkward chunk sizes to exercise the streaming path.
        for chunk in input.chunks(1_022) {
            r.resample(chunk, &mut out);
        }
        r.flush(&mut out);
        out
    }

    /// Manual benchmark, deliberately not in the suite:
    ///
    ///     cargo test --release bench_resample -- --ignored --nocapture
    ///
    /// Reported in milliseconds per second of audio, so numbers from different
    /// machines and different CPU flags are directly comparable. Run it on the
    /// aarch64 server, not just here.
    #[test]
    #[ignore]
    fn bench_resample() {
        let seconds = 20usize;
        for s in all_shapes() {
            let frames = s.fs * seconds;
            let input: Vec<f32> = (0..frames)
                .flat_map(|i| {
                    let v = (i as f64 * 0.03).sin() as f32 * 0.5;
                    [v, -v]
                })
                .collect();
            let expect = scaled_frames(s.fs, frames);
            let mut best = f64::MAX;
            for _ in 0..3 {
                let mut r = Resampler::new(s.fs as u32).expect("supported rate");
                let mut out = Vec::new();
                let t = std::time::Instant::now();
                for chunk in input.chunks(960 * 2 * 20) {
                    r.resample(chunk, &mut out);
                }
                r.flush(&mut out);
                assert_eq!(out.len() / 2, expect);
                best = best.min(t.elapsed().as_secs_f64());
            }
            println!(
                "{:>6} Hz -> 48 kHz: {:6.2} ms per second of audio ({:6.1} ms for {seconds} s)",
                s.fs,
                best * 1e3 / seconds as f64,
                best * 1e3
            );
        }
    }

    /// The sample conversion must not alter anything a real source can hold.
    /// Q23 holds every 24-bit integer sample exactly, so 16- and 24-bit PCM is
    /// fed to the filter with zero quantisation error; the clamp at +-4 never
    /// engages for audio. Checked exhaustively over the whole 24-bit range.
    #[test]
    fn q23_conversion_is_lossless_for_24_bit_audio() {
        for n in -8_388_608i32..=8_388_607 {
            let x = n as f32 / 8_388_608.0;
            assert_eq!(to_q23(x), n, "24-bit sample {n} changed");
        }
    }

    #[test]
    fn shape_table_matches_the_proofs() {
        for s in all_shapes() {
            assert_eq!(s.l * s.fs, OUT_RATE * s.m, "exact ratio for {} Hz", s.fs);
            assert_eq!((s.n - 1) % (2 * s.m), 0, "integral delay for {} Hz", s.fs);
            assert_eq!(s.delay(), 50, "delay for {} Hz", s.fs);
            assert!(
                s.width() * s.l >= s.n,
                "padding covers the table for {} Hz",
                s.fs
            );
        }
        assert_eq!(Shape::for_rate(48_000).is_none(), true);
        assert_eq!(Shape::for_rate(22_050).is_none(), true);
    }

    #[test]
    fn coefficients_are_symmetric_and_sum_exactly() {
        for s in all_shapes() {
            let c = coefficients(s);
            assert_eq!(c.len(), s.n);
            for j in 0..s.n / 2 {
                assert_eq!(c[j], c[s.n - 1 - j], "symmetry at {j} for {} Hz", s.fs);
            }
            let sum: i64 = c.iter().map(|&v| v as i64).sum();
            assert_eq!(
                sum,
                s.l as i64 * (1i64 << COEFF_BITS),
                "unit DC gain for {} Hz",
                s.fs
            );
            let peak = c.iter().map(|v| v.unsigned_abs()).max().unwrap();
            assert!(
                peak as i64 <= 1i64 << COEFF_BITS,
                "|h| <= 1 for {} Hz",
                s.fs
            );
        }
    }

    #[test]
    fn every_phase_sums_to_one() {
        for s in all_shapes() {
            let c = coefficients(s);
            let table = polyphase(s, &c);
            let w = s.width();
            for r in 0..s.l {
                let sum: i64 = table[r * w..r * w + w].iter().map(|&v| v as i64).sum();
                let err = (sum - (1i64 << COEFF_BITS)).abs();
                assert!(
                    err <= w as i64,
                    "phase {r} of {} Hz sums to {sum}, off by {err} LSB",
                    s.fs
                );
            }
        }
    }

    /// Magnitude response of the prototype at `f` Hz, normalised so that DC is
    /// unity (the raw coefficient sum is `l * 2^25`).
    fn response(c: &[i32], rate: f64, f: f64) -> f64 {
        let omega = std::f64::consts::TAU * f / rate;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (j, &v) in c.iter().enumerate() {
            let ph = omega * j as f64;
            re += v as f64 * ph.cos();
            im += v as f64 * ph.sin();
        }
        let sum: i64 = c.iter().map(|&v| v as i64).sum();
        (re * re + im * im).sqrt() / sum as f64
    }

    #[test]
    fn passband_is_flat_to_20_khz() {
        for s in all_shapes() {
            let c = coefficients(s);
            let rate = s.rate() as f64;
            for step in 0..=20 {
                let f = PASS_HZ * step as f64 / 20.0;
                let db = 20.0 * response(&c, rate, f).log10();
                assert!(db.abs() < 0.001, "{} Hz at {:.0} Hz: {db:.4} dB", s.fs, f);
            }
        }
    }

    #[test]
    fn stopband_is_at_least_120_db() {
        let floor = 1e-6; // -120 dB
        for s in all_shapes() {
            let c = coefficients(s);
            let rate = s.rate() as f64;
            let top = rate * 0.5;
            let steps = 200;
            let mut worst: f64 = 0.0;
            for i in 0..steps {
                let f = STOP_HZ * (top / STOP_HZ).powf(i as f64 / (steps - 1) as f64);
                let mag = response(&c, rate, f);
                worst = worst.max(mag);
                assert!(
                    mag < floor,
                    "{} Hz at {f:.0} Hz: {:.1} dB",
                    s.fs,
                    20.0 * mag.log10()
                );
            }
            assert!(
                worst < floor,
                "worst {:.1} dB for {} Hz",
                20.0 * worst.log10(),
                s.fs
            );
        }
    }

    #[test]
    fn length_is_exactly_the_rational_length() {
        for s in all_shapes() {
            for &n in &[1usize, 97, s.m, s.m + 1, s.fs, s.fs * 3 + 17] {
                let input = vec![0.0f32; n * 2];
                let out = run(s, &input);
                let frames = out.len() / 2;
                let expect = (n * s.l + s.m - 1) / s.m;
                assert_eq!(frames, expect, "{} Hz, {n} in frames", s.fs);
            }
        }
    }

    /// `frames * L / M` is an integer whenever `frames` is a multiple of `M`,
    /// which happens for roughly one track in `M`. Computing it as
    /// `frames * (48000.0 / fs)` and taking the ceiling can land one frame on
    /// the wrong side, which shifts the crossfade by a frame; these values are
    /// exact integers.
    #[test]
    fn scaled_frames_is_exact_for_values_float_gets_wrong() {
        assert_eq!(scaled_frames(44_100, 147 * 7), 160 * 7);
        assert_eq!(scaled_frames(44_100, 44_100), 48_000);
        assert_eq!(scaled_frames(88_200, 147 * 3), 80 * 3);
        assert_eq!(scaled_frames(176_400, 147), 40);
        assert_eq!(scaled_frames(96_000, 2), 1);
        assert_eq!(scaled_frames(96_000, 3), 2);
        assert_eq!(scaled_frames(192_000, 5), 2);
        for n in 0..10_000 {
            assert_eq!(scaled_frames(48_000, n), n, "pass-through at {n}");
        }

        // Why this is integer math and not a style preference: with the old
        // f64 formula, 1260 of the first 1999 exact multiples at 44.1 kHz
        // came out one frame too high (90 of 19999 arbitrary values).
        let float_scaled =
            |fs: usize, frames: usize| (frames as f64 * (48_000.0 / fs as f64)).ceil() as usize;
        let overshoot = (1..2000)
            .filter(|&k| float_scaled(44_100, 147 * k) != 160 * k)
            .count();
        assert!(
            overshoot > 1_000,
            "the f64 formula should still overshoot on most exact multiples"
        );
    }

    /// The mixer clamps its crossfade rewind to `scaled_frames(fs, track_len)`;
    /// that must be exactly what the resampler emits, or the clamp can let the
    /// cursor rewind past what the new track will actually supply.
    #[test]
    fn scaled_frames_matches_what_the_resampler_emits() {
        for s in all_shapes() {
            for &n in &[0usize, 1, 5, s.m, s.m + 1, s.m * 3, 4_410] {
                let mut r = Resampler::new(s.fs as u32).expect("supported rate");
                let mut out = Vec::new();
                r.resample(&vec![0.0f32; n * 2], &mut out);
                r.flush(&mut out);
                assert_eq!(
                    out.len() / 2,
                    scaled_frames(s.fs, n),
                    "{} Hz with {n} input frames",
                    s.fs
                );
            }
        }
    }

    #[test]
    fn sixty_seconds_does_not_drift() {
        let s = Shape::for_rate(44_100).unwrap();
        let out = run(s, &vec![0.0f32; s.fs * 60 * 2]);
        assert_eq!(out.len() / 2, 2_880_000);
    }

    #[test]
    fn impulse_lands_at_the_compensated_position() {
        let s = Shape::for_rate(44_100).unwrap();
        // A multiple of `m` maps to an integer output frame, so the peak lands
        // exactly on the filter's centre tap (~0.9977) instead of between two
        // samples.
        let at = 7 * s.m;
        let mut input = vec![0.0f32; s.fs * 2 * 2];
        input[at * 2] = 1.0;
        input[at * 2 + 1] = 1.0;
        let out = run(s, &input);

        let peak = out
            .chunks_exact(2)
            .enumerate()
            .max_by(|a, b| a.1[0].abs().partial_cmp(&b.1[0].abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let expect = (at * s.l) / s.m;
        assert!(
            peak.abs_diff(expect) <= 1,
            "impulse at frame {peak}, expected {expect}"
        );
        assert!(out[peak * 2] > 0.99, "impulse amplitude {}", out[peak * 2]);
    }

    #[test]
    fn channels_do_not_leak() {
        let s = Shape::for_rate(96_000).unwrap();
        let input: Vec<f32> = (0..s.fs * 2)
            .flat_map(|i| {
                let x = (i as f64 * 0.01).sin() as f32;
                [x, 0.0]
            })
            .collect();
        let out = run(s, &input);
        assert!(out.chunks_exact(2).all(|p| p[1] == 0.0));
        assert!(out.chunks_exact(2).any(|p| p[0].abs() > 0.01));
    }

    #[test]
    fn silence_stays_silent() {
        for s in all_shapes() {
            let out = run(s, &vec![0.0f32; s.fs * 2]);
            assert!(out.iter().all(|&v| v == 0.0), "{} Hz", s.fs);
        }
    }

    #[test]
    fn steady_state_tone_keeps_its_amplitude() {
        for s in all_shapes() {
            let frames = s.fs * 2;
            let out = run(s, &tone(s, frames, 1_000.0, 0.5));
            let total = out.len() / 2;
            let skip = (s.delay() + s.width() * 4) as usize;
            let tail = total - skip;
            let peak = out[skip * 2..tail * 2]
                .iter()
                .fold(0.0f32, |a, &v| a.max(v.abs()));
            assert!((peak - 0.5).abs() < 1e-3, "{} Hz peak {peak}", s.fs);
        }
    }
}
