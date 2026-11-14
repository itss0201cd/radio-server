// pitch.rs — CELT pitch search, LPC preconditioning and the comb filter.
//
// Port of the float build of `celt/pitch.c`, the LPC helpers in
// `celt/celt_lpc.c` that `pitch_downsample` uses, and `celt.c::comb_filter`.
// This feeds the encoder's pitch prefilter, which is enabled at our rate.

const COMBFILTER_MINPERIOD: i32 = 15;

const SECOND_CHECK: [i32; 16] = [0, 0, 3, 2, 3, 2, 5, 2, 3, 2, 3, 2, 5, 2, 3, 2];

const GAINS: [[f32; 3]; 3] = [
    [0.3066406250, 0.2170410156, 0.1296386719],
    [0.4638671875, 0.2680664062, 0.0],
    [0.7998046875, 0.1000976562, 0.0],
];

pub fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

fn inner_prod(x: &[f32], y: &[f32], n: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..n {
        sum += x[i] * y[i];
    }
    sum
}

/// Reusable buffers for the pitch search. Kept by the encoder so the
/// per-frame path does not allocate.
#[derive(Default)]
pub struct PitchScratch {
    x_lp4: Vec<f32>,
    y_lp4: Vec<f32>,
    xcorr: Vec<f32>,
    yy_lookup: Vec<f32>,
}

/// `celt_pitch_xcorr`: `out[i] = sum_j x[j] * y[i + j]`.
///
/// The loops are ordered `j`-outer so the inner body is an AXPY that the
/// compiler can vectorise. Each `out[i]` still accumulates its terms in the
/// same `j` order (proofs/Pitch.lean: `xcorr_rowmajor`), so the float result
/// is unchanged bit for bit.
fn pitch_xcorr(x: &[f32], y: &[f32], out: &mut [f32], len: usize, max_pitch: usize) {
    out[..max_pitch].fill(0.0);
    for j in 0..len {
        let xj = x[j];
        let yrow = &y[j..j + max_pitch];
        for (o, &yv) in out[..max_pitch].iter_mut().zip(yrow) {
            *o += xj * yv;
        }
    }
}

fn find_best_pitch(
    xcorr: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    best_pitch: &mut [i32; 2],
) {
    // Both bounds hold for every pitch-search caller; stating them here lets
    // the compiler drop the per-iteration checks in the hot loop.
    debug_assert!(len + max_pitch <= y.len() && max_pitch <= xcorr.len());
    let y = &y[..len + max_pitch];
    let xcorr = &xcorr[..max_pitch];
    let mut syy = 1.0f32;
    let mut best_num = [-1.0f32; 2];
    let mut best_den = [0.0f32; 2];
    best_pitch[0] = 0;
    best_pitch[1] = 1;
    for j in 0..len {
        syy += y[j] * y[j];
    }
    for i in 0..max_pitch {
        if xcorr[i] > 0.0 {
            let xcorr16 = xcorr[i] * 1e-12;
            let num = xcorr16 * xcorr16;
            if num * best_den[1] > best_num[1] * syy {
                if num * best_den[0] > best_num[0] * syy {
                    best_num[1] = best_num[0];
                    best_den[1] = best_den[0];
                    best_pitch[1] = best_pitch[0];
                    best_num[0] = num;
                    best_den[0] = syy;
                    best_pitch[0] = i as i32;
                } else {
                    best_num[1] = num;
                    best_den[1] = syy;
                    best_pitch[1] = i as i32;
                }
            }
        }
        syy = syy + y[i + len] * y[i + len] - y[i] * y[i];
        if syy < 1.0 {
            syy = 1.0;
        }
    }
}

fn celt_fir5(x: &mut [f32], num: &[f32; 5], n: usize) {
    let mut mem = [0.0f32; 5];
    for i in 0..n {
        let mut sum = x[i];
        for k in 0..5 {
            sum += num[k] * mem[k];
        }
        mem[4] = mem[3];
        mem[3] = mem[2];
        mem[2] = mem[1];
        mem[1] = mem[0];
        mem[0] = x[i];
        x[i] = sum;
    }
}

fn celt_lpc(lpc: &mut [f32; 4], ac: &[f32; 5]) {
    let p = 4;
    lpc.iter_mut().for_each(|v| *v = 0.0);
    let mut error = ac[0];
    if ac[0] > 1e-10 {
        for i in 0..p {
            let mut rr = 0.0f32;
            for j in 0..i {
                rr += lpc[j] * ac[i - j];
            }
            rr += ac[i + 1];
            let r = -(rr / error);
            lpc[i] = r;
            for j in 0..(i + 1) >> 1 {
                let tmp1 = lpc[j];
                let tmp2 = lpc[i - 1 - j];
                lpc[j] = tmp1 + r * tmp2;
                lpc[i - 1 - j] = tmp2 + r * tmp1;
            }
            error -= (r * r) * error;
            if error <= 0.001 * ac[0] {
                break;
            }
        }
    }
}

fn autocorr(x: &[f32], ac: &mut [f32; 5], lag: usize) {
    let n = x.len();
    let fast_n = n - lag;
    pitch_xcorr(x, x, ac, fast_n, lag + 1);
    for k in 0..=lag {
        let mut d = 0.0f32;
        for i in k + fast_n..n {
            d += x[i] * x[i - k];
        }
        ac[k] += d;
    }
}

/// `pitch_downsample`: 2x decimation plus LPC whitening, mixing both channels.
pub fn pitch_downsample(x0: &[f32], x1: &[f32], x_lp: &mut [f32], len: usize) {
    let half = len >> 1;
    for i in 1..half {
        x_lp[i] = 0.25 * x0[2 * i - 1] + 0.25 * x0[2 * i + 1] + 0.5 * x0[2 * i];
    }
    x_lp[0] = 0.25 * x0[1] + 0.5 * x0[0];
    for i in 1..half {
        x_lp[i] += 0.25 * x1[2 * i - 1] + 0.25 * x1[2 * i + 1] + 0.5 * x1[2 * i];
    }
    x_lp[0] += 0.25 * x1[1] + 0.5 * x1[0];

    let mut ac = [0.0f32; 5];
    autocorr(&x_lp[..half], &mut ac, 4);
    ac[0] *= 1.0001;
    for i in 1..=4 {
        ac[i] -= ac[i] * (0.008 * i as f32) * (0.008 * i as f32);
    }

    let mut lpc = [0.0f32; 4];
    celt_lpc(&mut lpc, &ac);
    let mut tmp = 1.0f32;
    for value in lpc.iter_mut() {
        tmp *= 0.9;
        *value *= tmp;
    }
    let c1 = 0.8f32;
    let lpc2 = [
        lpc[0] + 0.8,
        lpc[1] + c1 * lpc[0],
        lpc[2] + c1 * lpc[1],
        lpc[3] + c1 * lpc[2],
        c1 * lpc[3],
    ];
    celt_fir5(x_lp, &lpc2, half);
}

/// `pitch_search`: returns a correlation lag (the caller converts it to a period).
pub fn pitch_search(
    x_lp: &[f32],
    y: &[f32],
    len: usize,
    max_pitch: usize,
    scratch: &mut PitchScratch,
) -> i32 {
    debug_assert!(len > 0 && max_pitch > 0);
    let lag = len + max_pitch;
    let len4 = len >> 2;
    let lag4 = lag >> 2;
    let max2 = max_pitch >> 2;
    let max4 = max_pitch >> 1;
    scratch.x_lp4.resize(len4, 0.0);
    scratch.y_lp4.resize(lag4, 0.0);
    scratch.xcorr.resize(max4, 0.0);
    for j in 0..len4 {
        scratch.x_lp4[j] = x_lp[2 * j];
    }
    for j in 0..lag4 {
        scratch.y_lp4[j] = y[2 * j];
    }

    pitch_xcorr(
        &scratch.x_lp4,
        &scratch.y_lp4,
        &mut scratch.xcorr,
        len4,
        max2,
    );
    let mut best_pitch = [0i32; 2];
    find_best_pitch(&scratch.xcorr, &scratch.y_lp4, len4, max2, &mut best_pitch);

    for i in 0..max4 {
        scratch.xcorr[i] = 0.0;
        if (i as i32 - 2 * best_pitch[0]).abs() > 2 && (i as i32 - 2 * best_pitch[1]).abs() > 2 {
            continue;
        }
        let sum = inner_prod(&x_lp[..len >> 1], &y[i..], len >> 1);
        scratch.xcorr[i] = sum.max(-1.0);
    }
    find_best_pitch(&scratch.xcorr, y, len >> 1, max_pitch >> 1, &mut best_pitch);

    let offset = if best_pitch[0] > 0 && best_pitch[0] < max4 as i32 - 1 {
        let bp = best_pitch[0] as usize;
        let a = scratch.xcorr[bp - 1];
        let b = scratch.xcorr[bp];
        let c = scratch.xcorr[bp + 1];
        if c - a > 0.7 * (b - a) {
            1
        } else if a - c > 0.7 * (b - c) {
            -1
        } else {
            0
        }
    } else {
        0
    };
    2 * best_pitch[0] - offset
}

fn compute_pitch_gain(xy: f32, xx: f32, yy: f32) -> f32 {
    xy / celt_sqrt(1.0 + xx * yy)
}

/// `remove_doubling`: resolve octave ambiguity and return the pitch gain.
pub struct RemoveDoubling<'a> {
    pub maxperiod: usize,
    pub minperiod: i32,
    pub n: usize,
    pub prev_period: i32,
    pub prev_gain: f32,
    pub scratch: &'a mut PitchScratch,
}

pub fn remove_doubling(x: &[f32], t0_in: &mut i32, p: RemoveDoubling<'_>) -> f32 {
    let RemoveDoubling {
        maxperiod: maxperiod_in,
        minperiod: minperiod_in,
        n: n_in,
        prev_period: prev_period_in,
        prev_gain,
        scratch,
    } = p;
    let minperiod0 = minperiod_in;
    let maxperiod = maxperiod_in / 2;
    let minperiod = minperiod_in / 2;
    *t0_in /= 2;
    let prev_period = prev_period_in / 2;
    let n = n_in / 2;
    let base = maxperiod;
    if *t0_in >= maxperiod as i32 {
        *t0_in = maxperiod as i32 - 1;
    }

    let mut t = *t0_in;
    let t0 = *t0_in;
    scratch.yy_lookup.resize(maxperiod + 1, 0.0);
    let mut xx = 0.0f32;
    let mut xy = 0.0f32;
    let cur = &x[base..base + n];
    let lag0 = &x[base - t0 as usize..base - t0 as usize + n];
    for i in 0..n {
        let xi = cur[i];
        xx += xi * xi;
        xy += xi * lag0[i];
    }
    scratch.yy_lookup[0] = xx;
    let mut yy = xx;
    for i in 1..=maxperiod {
        yy = yy + x[base - i] * x[base - i] - x[base + n - i] * x[base + n - i];
        scratch.yy_lookup[i] = yy.max(0.0);
    }
    yy = scratch.yy_lookup[t0 as usize];
    let mut best_xy = xy;
    let mut best_yy = yy;
    let mut g = compute_pitch_gain(xy, xx, yy);
    let g0 = g;

    fn periods(k: i32, t0: i32, maxperiod: i32) -> (i32, i32) {
        let t1 = (2 * t0 + k) / (2 * k);
        let t1b = if k == 2 {
            if t1 + t0 > maxperiod {
                t0
            } else {
                t0 + t1
            }
        } else {
            (2 * SECOND_CHECK[k as usize] * t0 + k) / (2 * k)
        };
        (t1, t1b)
    }

    let mut k = 2i32;
    while k <= 15 {
        let (t1, t1b) = periods(k, t0, maxperiod as i32);
        if t1 < minperiod {
            break;
        }
        let paired = if k < 15 {
            let (t1c, t1bc) = periods(k + 1, t0, maxperiod as i32);
            if t1c >= minperiod {
                Some((t1c, t1bc))
            } else {
                None
            }
        } else {
            None
        };
        // Two candidate periods share one pass so four independent
        // accumulation chains overlap. Each still sums over `i` in order, so
        // both correlations are bit-identical to separate loops; the
        // threshold decisions below still run in `k` order.
        let lag1 = &x[base - t1 as usize..base - t1 as usize + n];
        let lag2 = &x[base - t1b as usize..base - t1b as usize + n];
        let mut xy1 = 0.0f32;
        let mut xy2 = 0.0f32;
        let mut xy1c = 0.0f32;
        let mut xy2c = 0.0f32;
        if let Some((t1c, t1bc)) = paired {
            let lag3 = &x[base - t1c as usize..base - t1c as usize + n];
            let lag4 = &x[base - t1bc as usize..base - t1bc as usize + n];
            for i in 0..n {
                let xi = cur[i];
                xy1 += xi * lag1[i];
                xy2 += xi * lag2[i];
                xy1c += xi * lag3[i];
                xy2c += xi * lag4[i];
            }
        } else {
            for i in 0..n {
                let xi = cur[i];
                xy1 += xi * lag1[i];
                xy2 += xi * lag2[i];
            }
        }
        let mut apply = |kk: i32, t: i32, tb: i32, xy1: f32, xy2: f32, g: &mut f32| {
            let xy = 0.5 * (xy1 + xy2);
            let yy = 0.5 * (scratch.yy_lookup[t as usize] + scratch.yy_lookup[tb as usize]);
            let g1 = compute_pitch_gain(xy, xx, yy);
            let cont = if (t - prev_period).abs() <= 1 {
                prev_gain
            } else if (t - prev_period).abs() <= 2 && 5 * kk * kk < t0 {
                0.5 * prev_gain
            } else {
                0.0
            };
            let mut thresh = 0.3f32.max(0.7 * g0 - cont);
            if t < 3 * minperiod {
                thresh = 0.4f32.max(0.85 * g0 - cont);
            } else if t < 2 * minperiod {
                thresh = 0.5f32.max(0.9 * g0 - cont);
            }
            if g1 > thresh {
                best_xy = xy;
                best_yy = yy;
                *g = g1;
                true
            } else {
                false
            }
        };
        let gg = &mut g;
        if apply(k, t1, t1b, xy1, xy2, gg) {
            t = t1;
        }
        if let Some((t1c, t1bc)) = paired {
            if apply(k + 1, t1c, t1bc, xy1c, xy2c, gg) {
                t = t1c;
            }
        }
        k += if paired.is_some() { 2 } else { 1 };
    }
    best_xy = best_xy.max(0.0);
    let pg = if best_yy <= best_xy {
        1.0
    } else {
        best_xy / (best_yy + 1.0)
    };

    let mut xcorr = [0.0f32; 3];
    // One pass, three independent accumulators: each `xcorr[k]` still sums in
    // the same `i` order, so the results are bit-identical.
    let lagm = &x[base - (t - 1) as usize..base - (t - 1) as usize + n];
    let lag0 = &x[base - t as usize..base - t as usize + n];
    let lagp = &x[base - (t + 1) as usize..base - (t + 1) as usize + n];
    for i in 0..n {
        let xi = cur[i];
        xcorr[0] += xi * lagm[i];
        xcorr[1] += xi * lag0[i];
        xcorr[2] += xi * lagp[i];
    }
    let offset = if xcorr[2] - xcorr[0] > 0.7 * (xcorr[1] - xcorr[0]) {
        1
    } else if xcorr[0] - xcorr[2] > 0.7 * (xcorr[1] - xcorr[2]) {
        -1
    } else {
        0
    };
    let pg = if pg > g { g } else { pg };
    *t0_in = 2 * t + offset;
    if *t0_in < minperiod0 {
        *t0_in = minperiod0;
    }
    pg
}

struct CombConst {
    x_off: usize,
    t: i32,
    n: usize,
    g10: f32,
    g11: f32,
    g12: f32,
}

fn comb_filter_const(y: &mut [f32], x: &[f32], c: CombConst) {
    let CombConst {
        x_off,
        t,
        n,
        g10,
        g11,
        g12,
    } = c;
    let t = t as usize;
    let mut x4 = x[x_off - t - 2];
    let mut x3 = x[x_off - t - 1];
    let mut x2 = x[x_off - t];
    let mut x1 = x[x_off - t + 1];
    for i in 0..n {
        let x0 = x[x_off + i - t + 2];
        y[i] = x[x_off + i] + g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

/// Parameters for [`comb_filter`].
pub struct CombFilter<'a> {
    pub x_off: usize,
    pub t0: i32,
    pub t1: i32,
    pub n: usize,
    pub g0: f32,
    pub g1: f32,
    pub tapset0: usize,
    pub tapset1: usize,
    pub window: Option<&'a [f32]>,
    pub overlap: usize,
}

/// `comb_filter`: the pitch postfilter applied on the encoder side too.
pub fn comb_filter(y: &mut [f32], x: &[f32], p: CombFilter<'_>) {
    let CombFilter {
        x_off,
        t0,
        t1,
        n,
        g0,
        g1,
        tapset0,
        tapset1,
        window,
        mut overlap,
    } = p;
    if g0 == 0.0 && g1 == 0.0 {
        y[..n].copy_from_slice(&x[x_off..x_off + n]);
        return;
    }
    let t0 = t0.max(COMBFILTER_MINPERIOD);
    let t1 = t1.max(COMBFILTER_MINPERIOD);
    let g00 = g0 * GAINS[tapset0][0];
    let g01 = g0 * GAINS[tapset0][1];
    let g02 = g0 * GAINS[tapset0][2];
    let g10 = g1 * GAINS[tapset1][0];
    let g11 = g1 * GAINS[tapset1][1];
    let g12 = g1 * GAINS[tapset1][2];

    let t0 = t0 as usize;
    let t1 = t1 as usize;
    let mut x1 = x[x_off - t1 + 1];
    let mut x2 = x[x_off - t1];
    let mut x3 = x[x_off - t1 - 1];
    let mut x4 = x[x_off - t1 - 2];
    if g0 == g1 && t0 == t1 && tapset0 == tapset1 {
        overlap = 0;
    }
    if overlap > 0 {
        let window = window.unwrap();
        for i in 0..overlap {
            let x0 = x[x_off + i - t1 + 2];
            let f = window[i] * window[i];
            let omf = 1.0 - f;
            y[i] = x[x_off + i]
                + (omf * g00) * x[x_off + i - t0]
                + (omf * g01) * (x[x_off + i - t0 + 1] + x[x_off + i - t0 - 1])
                + (omf * g02) * (x[x_off + i - t0 + 2] + x[x_off + i - t0 - 2])
                + (f * g10) * x2
                + (f * g11) * (x1 + x3)
                + (f * g12) * (x0 + x4);
            x4 = x3;
            x3 = x2;
            x2 = x1;
            x1 = x0;
        }
    }
    if g1 == 0.0 {
        y[overlap..n].copy_from_slice(&x[x_off + overlap..x_off + n]);
        return;
    }
    comb_filter_const(
        &mut y[overlap..n],
        x,
        CombConst {
            x_off: x_off + overlap,
            t: t1 as i32,
            n: n - overlap,
            g10,
            g11,
            g12,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(x: &mut [f32], seed: u32) {
        let mut s = seed ^ 0x9E37_79B9;
        for v in x.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = ((s >> 16) as i32 % 2001 - 1000) as f32 * 0.0005;
        }
    }

    fn periodic(x: &mut [f32], period: usize, seed: u32) {
        fill(x, seed);
        for i in period..x.len() {
            x[i] += 0.8 * x[i - period];
        }
    }

    fn fnv_words(p: &[f32]) -> u64 {
        let mut h = 14695981039346656037u64;
        for v in p {
            h ^= v.to_bits() as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn window120() -> [f32; 120] {
        let mut w = [0.0f32; 120];
        for (i, v) in w.iter_mut().enumerate() {
            *v = (i + 1) as f32 * 0.008;
        }
        w
    }

    #[test]
    fn noise_corpus_matches_reference() {
        let mut x0 = vec![0f32; 1984];
        let mut x1 = vec![0f32; 1984];
        fill(&mut x0, 0x12345678);
        fill(&mut x1, 0x9abcdef0);
        let mut x_lp = vec![0f32; 992];
        pitch_downsample(&x0, &x1, &mut x_lp, 1984);
        assert_eq!(fnv_words(&x_lp), 0x2e798af9d381ebc0);

        let mut scratch = PitchScratch::default();
        let search = pitch_search(&x_lp[512..], &x_lp, 960, 979, &mut scratch);
        assert_eq!(search, 548);

        let mut pi = search;
        let gain = remove_doubling(
            &x_lp,
            &mut pi,
            RemoveDoubling {
                maxperiod: 1024,
                minperiod: 15,
                n: 960,
                prev_period: 0,
                prev_gain: 0.0,
                scratch: &mut scratch,
            },
        );
        assert_eq!(pi, 547);
        assert_eq!(gain.to_bits(), 0xbd237321);
    }

    #[test]
    fn periodic_corpus_matches_reference() {
        let mut x0 = vec![0f32; 1984];
        let mut x1 = vec![0f32; 1984];
        periodic(&mut x0, 140, 0x11112222);
        periodic(&mut x1, 70, 0x33334444);
        let mut x_lp = vec![0f32; 992];
        pitch_downsample(&x0, &x1, &mut x_lp, 1984);
        assert_eq!(fnv_words(&x_lp), 0xb828469b49d2562a);

        let mut scratch = PitchScratch::default();
        let search = pitch_search(&x_lp[512..], &x_lp, 960, 979, &mut scratch);
        assert_eq!(search, 884);

        let cases: &[(i32, f32, i32, u32)] = &[
            (0, 0.0, 140, 0x3f320540),
            (140, 0.6, 140, 0x3f320540),
            (70, 0.6, 70, 0x3e9fc3ba),
        ];
        for &(prev_period, prev_gain, want_period, want_gain) in cases {
            let mut pi = 1024 - search;
            let gain = remove_doubling(
                &x_lp,
                &mut pi,
                RemoveDoubling {
                    maxperiod: 1024,
                    minperiod: 15,
                    n: 960,
                    prev_period,
                    prev_gain,
                    scratch: &mut scratch,
                },
            );
            assert_eq!(pi, want_period, "prev_period {prev_period}");
            assert_eq!(gain.to_bits(), want_gain, "prev_period {prev_period}");
        }
    }

    #[test]
    fn comb_filter_matches_reference() {
        let w = window120();
        let mut x = vec![0f32; 2048];
        let mut y = vec![0f32; 960];

        fill(&mut x, 0xdeadbeef);
        comb_filter(
            &mut y,
            &x,
            CombFilter {
                x_off: 1024,
                t0: 100,
                t1: 140,
                n: 960,
                g0: 0.5,
                g1: 0.2,
                tapset0: 1,
                tapset1: 2,
                window: Some(&w),
                overlap: 120,
            },
        );
        assert_eq!(fnv_words(&y), 0x8db16c3aee529ff6);

        fill(&mut x, 0x0badc0de);
        comb_filter(
            &mut y,
            &x,
            CombFilter {
                x_off: 1024,
                t0: 160,
                t1: 160,
                n: 960,
                g0: 0.4,
                g1: 0.4,
                tapset0: 0,
                tapset1: 0,
                window: Some(&w),
                overlap: 120,
            },
        );
        assert_eq!(fnv_words(&y), 0xfd05e9a31b9692fa);

        fill(&mut x, 0xfeedface);
        comb_filter(
            &mut y,
            &x,
            CombFilter {
                x_off: 1024,
                t0: 120,
                t1: 0,
                n: 960,
                g0: 0.3,
                g1: 0.0,
                tapset0: 2,
                tapset1: 2,
                window: Some(&w),
                overlap: 120,
            },
        );
        assert_eq!(fnv_words(&y), 0xe79e8d7158535809);

        fill(&mut x, 0x0ddba11);
        comb_filter(
            &mut y,
            &x,
            CombFilter {
                x_off: 1024,
                t0: 100,
                t1: 100,
                n: 960,
                g0: 0.0,
                g1: 0.0,
                tapset0: 0,
                tapset1: 0,
                window: Some(&w),
                overlap: 120,
            },
        );
        assert_eq!(fnv_words(&y), 0x9f2750a22ad24bf9);
    }
}
