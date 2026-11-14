// bands.rs — CELT band quantisation: stereo, folding, spreading, transients.
//
// Encoder side of `celt/bands.c` (float build) for the pinned 48 kHz / 20 ms
// stereo mode. `quant_all_bands` writes the residual section of the packet.
// The decoder half is not built: the station never decodes.

use crate::alloc::{
    CACHE_BITS, CACHE_INDEX, EBANDS, LOG_N, NB_EBANDS, bits2pulses, get_pulses, pulses2bits,
};
use crate::pvq::{Quant, alg_quant, renormalise_vector};
use crate::range_coder::RangeEncoder;

const BITRES: i32 = 3;
const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
const EPSILON: f32 = 1e-15;

/// The encoder is pinned to `LM = 3`; scratch buffers are sized for it.
const MAX_M: usize = 1 << 3;
const MAX_NORM_LEN: usize = MAX_M * EBANDS[NB_EBANDS - 1] as usize;

/// Allocation-vector depth in `quant_partition`.
///
/// `Leaf` is the reference's `lm == -1` row: splitting stops and the
/// allocation vector is row zero. `Level` is a normal depth in `0..=3`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lm {
    Leaf,
    Level(u8),
}

impl Lm {
    fn new(lm: i32) -> Self {
        if lm < 0 {
            Lm::Leaf
        } else {
            Lm::Level(lm as u8)
        }
    }

    fn split(self) -> Self {
        match self {
            Lm::Leaf | Lm::Level(0) => Lm::Leaf,
            Lm::Level(l) => Lm::Level(l - 1),
        }
    }

    fn level(self) -> i32 {
        match self {
            Lm::Leaf => -1,
            Lm::Level(l) => l as i32,
        }
    }

    fn cache_row(self) -> usize {
        match self {
            Lm::Leaf => 0,
            Lm::Level(l) => l as usize + 1,
        }
    }
}

pub const SPREAD_NONE: i32 = 0;
pub const SPREAD_LIGHT: i32 = 1;
pub const SPREAD_NORMAL: i32 = 2;
pub const SPREAD_AGGRESSIVE: i32 = 3;

const EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];

const ORDERY_TABLE: [i32; 30] = [
    1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

const BIT_INTERLEAVE_TABLE: [u32; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];

const BIT_DEINTERLEAVE_TABLE: [u32; 16] = [
    0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF,
];

/// Threshold with hysteresis, `hysteresis_decision` in `bands.c`.
pub fn hysteresis_decision(val: f32, thresholds: &[f32], hysteresis: &[f32], prev: i32) -> i32 {
    let mut i = 0usize;
    while i < thresholds.len() && !(val < thresholds[i]) {
        i += 1;
    }
    let mut i = i as i32;
    if i > prev && val < thresholds[prev as usize] + hysteresis[prev as usize] {
        i = prev;
    }
    if i < prev && val > thresholds[(prev - 1) as usize] - hysteresis[(prev - 1) as usize] {
        i = prev;
    }
    i
}

pub fn celt_lcg_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

fn ec_ilog(v: u32) -> i32 {
    debug_assert!(v > 0);
    32 - v.leading_zeros() as i32
}

/// `bitexact_cos`: bit-exact cosine approximation, Q14 in and Q15 out.
pub fn bitexact_cos(x: i16) -> i16 {
    let tmp = (4096 + (x as i32) * (x as i32)) >> 13;
    let mut x2 = tmp as i16;
    x2 = ((32767 - x2 as i32)
        + frac_mul16(
            x2 as i32,
            -7651 + frac_mul16(x2 as i32, 8277 + frac_mul16(-626, x2 as i32)),
        )) as i16;
    x2.wrapping_add(1)
}

pub fn bitexact_log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ec_ilog(icos as u32);
    let ls = ec_ilog(isin as u32);
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

fn celt_rsqrt(x: f32) -> f32 {
    1.0 / celt_sqrt(x)
}

const C_A: f32 = 0.43157974;
const C_B: f32 = 0.67848403;
const C_C: f32 = 0.08595542;
const C_E: f32 = 3.141592653 / 2.0;

fn fast_atan2f(y: f32, x: f32) -> f32 {
    let x2 = x * x;
    let y2 = y * y;
    if x2 + y2 < 1e-18 {
        return 0.0;
    }
    if x2 < y2 {
        let den = (y2 + C_B * x2) * (y2 + C_C * x2);
        -x * y * (y2 + C_A * x2) / den + if y < 0.0 { -C_E } else { C_E }
    } else {
        let den = (x2 + C_B * y2) * (x2 + C_C * y2);
        x * y * (x2 + C_A * y2) / den + if y < 0.0 { -C_E } else { C_E }
            - if x * y < 0.0 { -C_E } else { C_E }
    }
}

/// Two independent dot products in one pass. Each sum keeps its own `i` order
/// (both start at zero, as `inner_prod` did), so the result is bit-identical to
/// two separate calls, but the two dependency chains overlap.
fn pair_inner_prod(a: &[f32], b: &[f32], c: &[f32], d: &[f32], n: usize) -> (f32, f32) {
    let mut s1 = 0.0f32;
    let mut s2 = 0.0f32;
    for i in 0..n {
        s1 += a[i] * b[i];
        s2 += c[i] * d[i];
    }
    (s1, s2)
}

fn dual_inner_prod(x: &[f32], y01: &[f32], y02: &[f32], n: usize) -> (f32, f32) {
    let mut xy1 = 0.0f32;
    let mut xy2 = 0.0f32;
    for i in 0..n {
        xy1 += x[i] * y01[i];
        xy2 += x[i] * y02[i];
    }
    (xy1, xy2)
}

/// `stereo_itheta` from `vq.c`.
fn stereo_itheta(x: &[f32], y: &[f32], stereo: bool, n: usize) -> i32 {
    let mut emid = EPSILON;
    let mut eside = EPSILON;
    if stereo {
        for i in 0..n {
            let m = 0.5 * x[i] + 0.5 * y[i];
            let s = 0.5 * x[i] - 0.5 * y[i];
            emid += m * m;
            eside += s * s;
        }
    } else {
        let (sx, sy) = pair_inner_prod(x, x, y, y, n);
        emid += sx;
        eside += sy;
    }
    let mid = celt_sqrt(emid);
    let side = celt_sqrt(eside);
    (0.5 + 16384.0 * 0.63662 * fast_atan2f(side, mid)).floor() as i32
}

/// `normalise_bands`: scale each band to unit energy in place. Both channels
/// are scaled in one pass; the multiplications are independent, so the values
/// are bit-identical to two separate loops. Every caller is stereo.
pub fn normalise_bands(freq: &[f32], x: &mut [f32], band_e: &[f32], m: usize) {
    let n = m * 120;
    let (xl, xr) = x.split_at_mut(n);
    for i in 0..NB_EBANDS {
        let gl = 1.0 / (1e-27 + band_e[i]);
        let gr = 1.0 / (1e-27 + band_e[NB_EBANDS + i]);
        let start = m * EBANDS[i] as usize;
        let stop = m * EBANDS[i + 1] as usize;
        let fl = &freq[start..stop];
        let fr = &freq[n + start..n + stop];
        for ((a, b), (&fl, &fr)) in xl[start..stop]
            .iter_mut()
            .zip(xr[start..stop].iter_mut())
            .zip(fl.iter().zip(fr.iter()))
        {
            *a = fl * gl;
            *b = fr * gr;
        }
    }
}

fn compute_channel_weights(ex: f32, ey: f32) -> (f32, f32) {
    let min_e = ex.min(ey);
    (ex + min_e / 3.0, ey + min_e / 3.0)
}

fn intensity_stereo(ctx: &BandCtx, x: &mut [f32], y: &[f32], n: usize) {
    let i = ctx.i;
    let left = ctx.band_e[i];
    let right = ctx.band_e[i + NB_EBANDS];
    let norm = EPSILON + celt_sqrt(EPSILON + left * left + right * right);
    let a1 = left / norm;
    let a2 = right / norm;
    for j in 0..n {
        x[j] = a1 * x[j] + a2 * y[j];
    }
}

fn stereo_split(x: &mut [f32], y: &mut [f32], n: usize) {
    for j in 0..n {
        let l = 0.70710678 * x[j];
        let r = 0.70710678 * y[j];
        x[j] = l + r;
        y[j] = r - l;
    }
}

fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: usize) {
    let (xp, side) = dual_inner_prod(y, x, y, n);
    let xp = mid * xp;
    let el = mid * mid + side - 2.0 * xp;
    let er = mid * mid + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y[..n].copy_from_slice(&x[..n]);
        return;
    }
    let lgain = celt_rsqrt(el);
    let rgain = celt_rsqrt(er);
    for j in 0..n {
        let l = mid * x[j];
        let r = y[j];
        x[j] = lgain * (l - r);
        y[j] = rgain * (l + r);
    }
}

/// Hysteretic state carried between frames by the spreading decision.
pub struct SpreadState {
    pub average: i32,
    pub hf_average: i32,
    pub tapset_decision: i32,
}

/// Decide how aggressively to spread pulses, `spreading_decision` in `bands.c`.
pub fn spreading_decision(
    x: &[f32],
    state: &mut SpreadState,
    last_decision: i32,
    update_hf: bool,
    channels: usize,
    m: usize,
    spread_weight: &[i32],
) -> i32 {
    let SpreadState {
        average,
        hf_average,
        tapset_decision,
    } = state;
    let n0 = m * 120;
    if m * (EBANDS[NB_EBANDS] - EBANDS[NB_EBANDS - 1]) as usize <= 8 {
        return SPREAD_NONE;
    }
    let mut sum = 0i32;
    let mut nb_bands = 0i32;
    let mut hf_sum = 0i32;
    for c in 0..channels {
        for i in 0..NB_EBANDS {
            let n = m * (EBANDS[i + 1] - EBANDS[i]) as usize;
            if n <= 8 {
                continue;
            }
            let base = m * EBANDS[i] as usize + c * n0;
            let mut tcount = [0i32; 3];
            for j in 0..n {
                let x2n = x[base + j] * x[base + j] * n as f32;
                if x2n < 0.25 {
                    tcount[0] += 1;
                }
                if x2n < 0.0625 {
                    tcount[1] += 1;
                }
                if x2n < 0.015625 {
                    tcount[2] += 1;
                }
            }
            if i > NB_EBANDS - 4 {
                hf_sum += 32 * (tcount[1] + tcount[0]) / n as i32;
            }
            let tmp = (2 * tcount[2] >= n as i32) as i32
                + (2 * tcount[1] >= n as i32) as i32
                + (2 * tcount[0] >= n as i32) as i32;
            sum += tmp * spread_weight[i];
            nb_bands += spread_weight[i];
        }
    }

    if update_hf {
        if hf_sum != 0 {
            hf_sum = (hf_sum as u32 / (channels as i32 * 4) as u32) as i32;
        }
        *hf_average = (*hf_average + hf_sum) >> 1;
        hf_sum = *hf_average;
        if *tapset_decision == 2 {
            hf_sum += 4;
        } else if *tapset_decision == 0 {
            hf_sum -= 4;
        }
        *tapset_decision = if hf_sum > 22 {
            2
        } else if hf_sum > 18 {
            1
        } else {
            0
        };
    }
    debug_assert!(nb_bands > 0);
    sum = (((sum as u32) << 8) / nb_bands as u32) as i32;
    sum = (sum + *average) >> 1;
    *average = sum;
    sum = (3 * sum + (((3 - last_decision) << 7) + 64) + 2) >> 2;
    if sum < 80 {
        SPREAD_AGGRESSIVE
    } else if sum < 256 {
        SPREAD_NORMAL
    } else if sum < 384 {
        SPREAD_LIGHT
    } else {
        SPREAD_NONE
    }
}

fn deinterleave_hadamard(
    x: &mut [f32],
    n0: usize,
    stride: usize,
    hadamard: bool,
    tmp: &mut [f32; 176],
) {
    let n = n0 * stride;
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[ordery[i] as usize * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn interleave_hadamard(
    x: &mut [f32],
    n0: usize,
    stride: usize,
    hadamard: bool,
    tmp: &mut [f32; 176],
) {
    let n = n0 * stride;
    if hadamard {
        let ordery = &ORDERY_TABLE[stride - 2..];
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[ordery[i] as usize * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[i * n0 + j];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

pub(crate) fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    for i in 0..stride {
        for j in 0..n0 {
            let tmp1 = 0.70710678 * x[stride * 2 * j + i];
            let tmp2 = 0.70710678 * x[stride * (2 * j + 1) + i];
            x[stride * 2 * j + i] = tmp1 + tmp2;
            x[stride * (2 * j + 1) + i] = tmp1 - tmp2;
        }
    }
}

fn compute_qn(n: usize, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    let n2 = if stereo && n == 2 {
        2 * n as i32 - 2
    } else {
        2 * n as i32 - 1
    };
    let mut qb = (b + n2 * offset) / n2;
    qb = qb.min(b - pulse_cap - (4 << BITRES));
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES) >> 1 {
        1
    } else {
        let qn = EXP2_TABLE8[(qb & 0x7) as usize] >> (14 - (qb >> BITRES));
        (qn + 1) >> 1 << 1
    }
}

struct BandCtx<'a> {
    band_e: &'a [f32],
    i: usize,
    intensity: i32,
    spread: i32,
    tf_change: i32,
    remaining_bits: i32,
    seed: u32,
    theta_round: i32,
    avoid_split_noise: bool,
    resynth: bool,
}

struct SplitCtx {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

struct Theta {
    n: usize,
    b_size: usize,
    b0: usize,
    lm: i32,
    stereo: bool,
    b: i32,
    fill: u32,
    itheta: Option<i32>,
}

fn compute_theta(
    ctx: &mut BandCtx,
    enc: &mut RangeEncoder,
    sctx: &mut SplitCtx,
    x: &mut [f32],
    y: &mut [f32],
    t: &mut Theta,
) {
    let n = t.n;
    let b_size = t.b_size;
    let b0 = t.b0;
    let lm = t.lm;
    let stereo = t.stereo;
    let mut b = t.b;
    let mut fill = t.fill;
    let itheta_hint = t.itheta;
    let pulse_cap = LOG_N[ctx.i] + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1)
        - if stereo && n == 2 {
            QTHETA_OFFSET_TWOPHASE
        } else {
            QTHETA_OFFSET
        };
    let mut qn = compute_qn(n, b, offset, pulse_cap, stereo);
    if stereo && ctx.i as i32 >= ctx.intensity {
        qn = 1;
    }
    let mut itheta = match itheta_hint {
        Some(v) => v,
        None => stereo_itheta(x, y, stereo, n),
    };
    let tell = enc.tell_frac() as i32;
    let mut inv = false;

    if qn != 1 {
        if !stereo || ctx.theta_round == 0 {
            itheta = (itheta * qn + 8192) >> 14;
            if !stereo && ctx.avoid_split_noise && itheta > 0 && itheta < qn {
                let unquantized = itheta * 16384 / qn;
                let imid = bitexact_cos(unquantized as i16) as i32;
                let iside = bitexact_cos((16384 - unquantized) as i16) as i32;
                let delta = frac_mul16(((n as i32) - 1) << 7, bitexact_log2tan(iside, imid));
                if delta > b {
                    itheta = qn;
                } else if delta < -b {
                    itheta = 0;
                }
            }
        } else {
            let bias = if itheta > 8192 {
                32767 / qn
            } else {
                -32767 / qn
            };
            let down = (qn - 1).min(0.max((itheta * qn + bias) >> 14));
            itheta = if ctx.theta_round < 0 { down } else { down + 1 };
        }

        if stereo && n > 2 {
            let p0 = 3;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let fl = if itheta <= x0 {
                p0 * itheta
            } else {
                (itheta - 1 - x0) + (x0 + 1) * p0
            };
            let fh = if itheta <= x0 {
                p0 * (itheta + 1)
            } else {
                (itheta - x0) + (x0 + 1) * p0
            };
            enc.encode(fl as u32, fh as u32, ft as u32);
        } else if b0 > 1 || stereo {
            enc.encode_uint(itheta as u32, (qn + 1) as u32);
        } else {
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let (fl, fs) = if itheta <= qn >> 1 {
                (itheta * (itheta + 1) >> 1, itheta + 1)
            } else {
                (
                    ft - ((qn + 1 - itheta) * (qn + 2 - itheta) >> 1),
                    qn + 1 - itheta,
                )
            };
            enc.encode(fl as u32, (fl + fs) as u32, ft as u32);
        }
        debug_assert!(itheta >= 0);
        itheta = itheta * 16384 / qn;
        if stereo {
            if itheta == 0 {
                intensity_stereo(ctx, x, y, n);
            } else {
                stereo_split(x, y, n);
            }
        }
    } else if stereo {
        inv = itheta > 8192;
        if inv {
            for value in y.iter_mut().take(n) {
                *value = -*value;
            }
        }
        intensity_stereo(ctx, x, y, n);
        if b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            enc.encode_bit_logp(inv, 2);
        } else {
            inv = false;
        }
        itheta = 0;
    }
    let qalloc = enc.tell_frac() as i32 - tell;
    b -= qalloc;

    let (imid, iside, delta) = if itheta == 0 {
        fill &= (1u32 << b_size) - 1;
        (32767, 0, -16384)
    } else if itheta == 16384 {
        fill &= ((1u32 << b_size) - 1) << b_size;
        (0, 32767, 16384)
    } else {
        let imid = bitexact_cos(itheta as i16) as i32;
        let iside = bitexact_cos((16384 - itheta) as i16) as i32;
        let delta = frac_mul16(((n as i32) - 1) << 7, bitexact_log2tan(iside, imid));
        (imid, iside, delta)
    };

    sctx.inv = inv;
    sctx.imid = imid;
    sctx.iside = iside;
    sctx.delta = delta;
    sctx.itheta = itheta;
    sctx.qalloc = qalloc;
    t.b = b;
    t.fill = fill;
}

fn quant_band_n1(
    ctx: &mut BandCtx,
    enc: &mut RangeEncoder,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    lowband_out: Option<&mut [f32]>,
) -> u32 {
    let mut sign = false;
    if ctx.remaining_bits >= 1 << BITRES {
        sign = x[0] < 0.0;
        enc.encode_bits(sign as u32, 1);
        ctx.remaining_bits -= 1 << BITRES;
    }
    if ctx.resynth {
        x[0] = if sign { -1.0 } else { 1.0 };
    }
    if let Some(y) = y {
        let mut sign = false;
        if ctx.remaining_bits >= 1 << BITRES {
            sign = y[0] < 0.0;
            enc.encode_bits(sign as u32, 1);
            ctx.remaining_bits -= 1 << BITRES;
        }
        if ctx.resynth {
            y[0] = if sign { -1.0 } else { 1.0 };
        }
    }
    if let Some(out) = lowband_out {
        out[0] = x[0];
    }
    1
}

struct Partition<'a> {
    n: usize,
    b: i32,
    b_size: usize,
    lowband: Option<&'a [f32]>,
    lm: Lm,
    gain: f32,
    fill: u32,
}

fn quant_partition(
    ctx: &mut BandCtx,
    enc: &mut RangeEncoder,
    x: &mut [f32],
    p: Partition<'_>,
) -> u32 {
    let Partition {
        mut n,
        mut b,
        mut b_size,
        lowband,
        lm,
        gain,
        mut fill,
    } = p;
    let b0 = b_size;
    let mut split = false;
    if lm != Lm::Leaf && n > 2 {
        let base = CACHE_INDEX[lm.cache_row() * NB_EBANDS + ctx.i] as usize;
        let threshold = CACHE_BITS[base + CACHE_BITS[base] as usize] as i32;
        split = b > threshold + 12;
    }

    if split {
        n >>= 1;
        let (x, y) = x.split_at_mut(n);
        let lm = lm.split();
        let lm_level = lm.level();
        if b_size == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        b_size = (b_size + 1) >> 1;
        let mut sctx = SplitCtx {
            inv: false,
            imid: 0,
            iside: 0,
            delta: 0,
            itheta: 0,
            qalloc: 0,
        };
        let mut theta = Theta {
            n,
            b_size,
            b0,
            lm: lm_level,
            stereo: false,
            b,
            fill,
            itheta: None,
        };
        compute_theta(ctx, enc, &mut sctx, x, y, &mut theta);
        b = theta.b;
        fill = theta.fill;
        let mid = (1.0 / 32768.0) * sctx.imid as f32;
        let side = (1.0 / 32768.0) * sctx.iside as f32;
        let mut delta = sctx.delta;
        let itheta = sctx.itheta;

        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                delta -= delta >> (4 - lm_level);
            } else {
                delta = 0.min(delta + ((n as i32) << BITRES >> (5 - lm_level)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= sctx.qalloc;

        let (lowband1, lowband2) = match lowband {
            Some(lb) => (Some(&lb[..n]), Some(&lb[n..2 * n])),
            None => (None, None),
        };
        let mut rebalance = ctx.remaining_bits;
        let mut cm;
        if mbits >= sbits {
            cm = quant_partition(
                ctx,
                enc,
                x,
                Partition {
                    n,
                    b: mbits,
                    b_size,
                    lowband: lowband1,
                    lm,
                    gain: gain * mid,
                    fill,
                },
            );
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            cm |= quant_partition(
                ctx,
                enc,
                y,
                Partition {
                    n,
                    b: sbits,
                    b_size,
                    lowband: lowband2,
                    lm,
                    gain: gain * side,
                    fill: fill >> b_size,
                },
            ) << (b0 >> 1);
        } else {
            cm = quant_partition(
                ctx,
                enc,
                y,
                Partition {
                    n,
                    b: sbits,
                    b_size,
                    lowband: lowband2,
                    lm,
                    gain: gain * side,
                    fill: fill >> b_size,
                },
            ) << (b0 >> 1);
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_partition(
                ctx,
                enc,
                x,
                Partition {
                    n,
                    b: mbits,
                    b_size,
                    lowband: lowband1,
                    lm,
                    gain: gain * mid,
                    fill,
                },
            );
        }
        cm
    } else {
        let mut q = bits2pulses(ctx.i, lm.level(), b);
        let mut curr_bits = pulses2bits(ctx.i, lm.level(), q);
        ctx.remaining_bits -= curr_bits;
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(ctx.i, lm.level(), q);
            ctx.remaining_bits -= curr_bits;
        }
        if q != 0 {
            let k = get_pulses(q) as usize;
            alg_quant(
                enc,
                x,
                Quant {
                    n,
                    k,
                    spread: ctx.spread,
                    b: b_size,
                    gain,
                    resynth: ctx.resynth,
                },
            )
        } else {
            let mut cm = 0;
            if ctx.resynth {
                let cm_mask = (1u32 << b_size) - 1;
                fill &= cm_mask;
                if fill == 0 {
                    x[..n].fill(0.0);
                } else if let Some(lowband) = lowband {
                    for j in 0..n {
                        ctx.seed = celt_lcg_rand(ctx.seed);
                        let tmp = if ctx.seed & 0x8000 != 0 {
                            1.0 / 256.0
                        } else {
                            -1.0 / 256.0
                        };
                        x[j] = lowband[j] + tmp;
                    }
                    cm = fill;
                } else {
                    for value in x.iter_mut().take(n) {
                        ctx.seed = celt_lcg_rand(ctx.seed);
                        *value = ((ctx.seed as i32) >> 20) as f32;
                    }
                    cm = cm_mask;
                }
                if fill != 0 {
                    renormalise_vector(x, n, gain);
                }
            }
            cm
        }
    }
}

struct Band<'a, 'b> {
    n: usize,
    b: i32,
    b_size: usize,
    lowband: Option<&'a [f32]>,
    lm: i32,
    lowband_out: Option<&'b mut [f32]>,
    gain: f32,
    fill: u32,
}

fn quant_band(
    ctx: &mut BandCtx,
    enc: &mut RangeEncoder,
    x: &mut [f32],
    band: Band<'_, '_>,
    scratch: &mut [f32; 176],
    tmp: &mut [f32; 176],
) -> u32 {
    let Band {
        n,
        b,
        b_size: b_size_in,
        lowband,
        lm,
        lowband_out,
        gain,
        mut fill,
    } = band;
    let n0 = n;
    let mut n_b = n / b_size_in;
    let b_orig = b_size_in;
    let long_blocks = b_orig == 1;

    if n == 1 {
        return quant_band_n1(ctx, enc, x, None, lowband_out);
    }

    let mut tf_change = ctx.tf_change;
    let recombine = if tf_change > 0 { tf_change as usize } else { 0 };

    let owned = scratch;
    let copy = match lowband {
        Some(lb) if recombine > 0 || ((n_b & 1) == 0 && tf_change < 0) || b_orig > 1 => {
            owned[..n].copy_from_slice(&lb[..n]);
            true
        }
        _ => false,
    };

    for k in 0..recombine {
        haar1(x, n >> k, 1 << k);
        if copy {
            haar1(&mut owned[..n], n >> k, 1 << k);
        }
        fill = BIT_INTERLEAVE_TABLE[(fill & 0xF) as usize]
            | BIT_INTERLEAVE_TABLE[(fill >> 4) as usize] << 2;
    }
    let mut b_size = b_size_in >> recombine;
    n_b <<= recombine;

    let mut time_divide = 0usize;
    while (n_b & 1) == 0 && tf_change < 0 {
        haar1(x, n_b, b_size);
        if copy {
            haar1(&mut owned[..n], n_b, b_size);
        }
        fill |= fill << b_size;
        b_size <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change += 1;
    }
    let b0 = b_size;
    let n_b0 = n_b;

    if b0 > 1 {
        deinterleave_hadamard(x, n_b >> recombine, b0 << recombine, long_blocks, tmp);
        if copy {
            deinterleave_hadamard(
                &mut owned[..n],
                n_b >> recombine,
                b0 << recombine,
                long_blocks,
                tmp,
            );
        }
    }

    let lowband = if copy { Some(&owned[..n]) } else { lowband };
    let mut cm = quant_partition(
        ctx,
        enc,
        x,
        Partition {
            n,
            b,
            b_size,
            lowband,
            lm: Lm::new(lm),
            gain,
            fill,
        },
    );

    if ctx.resynth {
        if b0 > 1 {
            interleave_hadamard(x, n_b >> recombine, b0 << recombine, long_blocks, tmp);
        }
        n_b = n_b0;
        b_size = b0;
        for _ in 0..time_divide {
            b_size >>= 1;
            n_b <<= 1;
            cm |= cm >> b_size;
            haar1(x, n_b, b_size);
        }
        for k in 0..recombine {
            cm = BIT_DEINTERLEAVE_TABLE[cm as usize];
            haar1(x, n0 >> k, 1 << k);
        }
        b_size <<= recombine;

        if let Some(out) = lowband_out {
            let scale = celt_sqrt(n0 as f32);
            for j in 0..n0 {
                out[j] = scale * x[j];
            }
        }
        cm &= (1u32 << b_size) - 1;
    }
    cm
}

struct BandStereo<'a, 'b> {
    n: usize,
    b: i32,
    b_size: usize,
    lowband: Option<&'a [f32]>,
    lm: i32,
    lowband_out: Option<&'b mut [f32]>,
    fill: u32,
    itheta: Option<i32>,
}

fn quant_band_stereo(
    ctx: &mut BandCtx,
    enc: &mut RangeEncoder,
    x: &mut [f32],
    y: &mut [f32],
    band: BandStereo<'_, '_>,
    scratch: &mut [f32; 176],
    tmp: &mut [f32; 176],
) -> u32 {
    let BandStereo {
        n,
        b,
        b_size,
        lowband,
        lm,
        lowband_out,
        fill,
        itheta: itheta_hint,
    } = band;
    if n == 1 {
        return quant_band_n1(ctx, enc, x, Some(y), lowband_out);
    }

    let orig_fill = fill;
    let mut fill = fill;
    let mut b = b;
    let mut sctx = SplitCtx {
        inv: false,
        imid: 0,
        iside: 0,
        delta: 0,
        itheta: 0,
        qalloc: 0,
    };
    let mut theta = Theta {
        n,
        b_size,
        b0: b_size,
        lm,
        stereo: true,
        b,
        fill,
        itheta: itheta_hint,
    };
    compute_theta(ctx, enc, &mut sctx, x, y, &mut theta);
    b = theta.b;
    fill = theta.fill;
    let inv = sctx.inv;
    let mid = (1.0 / 32768.0) * sctx.imid as f32;
    let side = (1.0 / 32768.0) * sctx.iside as f32;
    let delta = sctx.delta;
    let itheta = sctx.itheta;
    let qalloc = sctx.qalloc;
    let mut cm;

    if n == 2 {
        let mut mbits = b;
        let mut sbits = 0;
        if itheta != 0 && itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let c = itheta > 8192;
        ctx.remaining_bits -= qalloc + sbits;

        let mut sign = false;
        if sbits != 0 {
            let (x2, y2) = if c { (&*y, &*x) } else { (&*x, &*y) };
            sign = x2[0] * y2[1] - x2[1] * y2[0] < 0.0;
            enc.encode_bits(sign as u32, 1);
        }
        let sign = 1 - 2 * (sign as i32);

        if c {
            cm = quant_band(
                ctx,
                enc,
                y,
                Band {
                    n,
                    b: mbits,
                    b_size,
                    lowband,
                    lm,
                    lowband_out,
                    gain: 1.0,
                    fill: orig_fill,
                },
                &mut *scratch,
                &mut *tmp,
            );
            x[0] = -(sign as f32) * y[1];
            x[1] = (sign as f32) * y[0];
        } else {
            cm = quant_band(
                ctx,
                enc,
                x,
                Band {
                    n,
                    b: mbits,
                    b_size,
                    lowband,
                    lm,
                    lowband_out,
                    gain: 1.0,
                    fill: orig_fill,
                },
                &mut *scratch,
                &mut *tmp,
            );
            y[0] = -(sign as f32) * x[1];
            y[1] = (sign as f32) * x[0];
        }
        if ctx.resynth {
            x[0] = mid * x[0];
            x[1] = mid * x[1];
            y[0] = side * y[0];
            y[1] = side * y[1];
            let tmp = x[0];
            x[0] = tmp - y[0];
            y[0] = tmp + y[0];
            let tmp = x[1];
            x[1] = tmp - y[1];
            y[1] = tmp + y[1];
        }
    } else {
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        let mut rebalance = ctx.remaining_bits;
        if mbits >= sbits {
            cm = quant_band(
                ctx,
                enc,
                x,
                Band {
                    n,
                    b: mbits,
                    b_size,
                    lowband,
                    lm,
                    lowband_out,
                    gain: 1.0,
                    fill,
                },
                &mut *scratch,
                &mut *tmp,
            );
            rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(
                ctx,
                enc,
                y,
                Band {
                    n,
                    b: sbits,
                    b_size,
                    lowband: None,
                    lm,
                    lowband_out: None,
                    gain: side,
                    fill: fill >> b_size,
                },
                &mut *scratch,
                &mut *tmp,
            );
        } else {
            cm = quant_band(
                ctx,
                enc,
                y,
                Band {
                    n,
                    b: sbits,
                    b_size,
                    lowband: None,
                    lm,
                    lowband_out: None,
                    gain: side,
                    fill: fill >> b_size,
                },
                &mut *scratch,
                &mut *tmp,
            );
            rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(
                ctx,
                enc,
                x,
                Band {
                    n,
                    b: mbits,
                    b_size,
                    lowband,
                    lm,
                    lowband_out,
                    gain: 1.0,
                    fill,
                },
                &mut *scratch,
                &mut *tmp,
            );
        }
        if ctx.resynth {
            stereo_merge(x, y, mid, n);
        }
    }

    if ctx.resynth && inv {
        for value in y.iter_mut().take(n) {
            *value = -*value;
        }
    }
    cm
}

/// Everything needed to quantise one frame's bands.
pub struct BandQuantizer<'a, 'b> {
    pub enc: &'a mut RangeEncoder<'b>,
    pub x: &'a mut [f32],
    pub collapse_masks: &'a mut [u8],
    pub band_e: &'a [f32],
    pub pulses: &'a [i32],
    pub short_blocks: bool,
    pub spread: i32,
    pub dual_stereo: bool,
    pub intensity: i32,
    pub tf_res: &'a [i32],
    pub total_bits: i32,
    pub balance: i32,
    pub lm: i32,
    pub coded_bands: i32,
    pub seed: &'a mut u32,
    pub complexity: i32,
}

/// Encode all bands, `quant_all_bands` in `bands.c`.
pub fn quant_all_bands(q: BandQuantizer<'_, '_>) {
    let BandQuantizer {
        enc,
        x,
        collapse_masks,
        band_e,
        pulses,
        short_blocks,
        spread,
        dual_stereo: dual_stereo_in,
        intensity,
        tf_res,
        total_bits,
        mut balance,
        lm,
        coded_bands,
        seed,
        complexity,
    } = q;
    let channels = 2;
    let m = 1usize << lm;
    let b = if short_blocks { m } else { 1 };
    let norm_len = m * EBANDS[NB_EBANDS - 1] as usize;
    let n_total = m * 120;
    debug_assert_eq!(x.len(), channels * n_total);

    let theta_rdo = !dual_stereo_in && complexity >= 8;
    let resynth = theta_rdo;
    let mut dual_stereo = dual_stereo_in;

    debug_assert!(norm_len <= MAX_NORM_LEN);
    let mut norm_buf = [0f32; MAX_NORM_LEN];
    let mut norm2_buf = [0f32; MAX_NORM_LEN];
    let norm = &mut norm_buf[..norm_len];
    let norm2 = &mut norm2_buf[..norm_len];
    let mut ctx = BandCtx {
        band_e,
        i: 0,
        intensity,
        spread,
        tf_change: 0,
        remaining_bits: 0,
        seed: *seed,
        theta_round: 0,
        avoid_split_noise: b > 1,
        resynth,
    };

    let (x0, x1) = x.split_at_mut(n_total);
    let mut lowband_offset = 0usize;
    let mut update_lowband = true;
    // Reused across bands: each buffer's active `[..n]` prefix is written
    // before it is read (proofs/Bands.lean: `init_irrelevant`), so hoisting
    // them out of the loop removes one clear per band.
    let mut lowband_buf = [0f32; 176];
    let mut x_save = [0f32; 176];
    let mut y_save = [0f32; 176];
    let mut x_save2 = [0f32; 176];
    let mut y_save2 = [0f32; 176];
    let mut norm_save2 = [0f32; 176];
    let mut band_scratch = [0f32; 176];
    let mut band_scratch2 = [0f32; 176];

    for i in 0..NB_EBANDS {
        let last = i == NB_EBANDS - 1;
        let x_start = m * EBANDS[i] as usize;
        let n = m * EBANDS[i + 1] as usize - x_start;
        ctx.i = i;

        let tell = enc.tell_frac() as i32;
        if i != 0 {
            balance -= tell;
        }
        ctx.remaining_bits = total_bits - tell - 1;
        let b_bits = if i as i32 <= coded_bands - 1 {
            let curr_balance = balance / 3.min(coded_bands - i as i32);
            0.max(16383.min((ctx.remaining_bits + 1).min(pulses[i] + curr_balance)))
        } else {
            0
        };

        if resynth
            && (m as i32 * EBANDS[i] >= n as i32 || i == 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }

        let tf_change = tf_res[i];
        ctx.tf_change = tf_change;
        // celt/bands.c calls special_hybrid_folding at i == 1. For CELT-only
        // (start == 0) it copies zero samples, so it is omitted. The
        // i >= effEBands aliasing branch is likewise unreachable: the pinned
        // mode has effEBands == nbEBands == end == 21.

        let mut x_cm;
        let mut y_cm;
        let mut effective_lowband: Option<usize> = None;
        if lowband_offset != 0 && (spread != SPREAD_AGGRESSIVE || b > 1 || tf_change < 0) {
            let eff = (m as i32 * EBANDS[lowband_offset] - n as i32).max(0);
            effective_lowband = Some(eff as usize);

            let mut fold_start = lowband_offset;
            loop {
                fold_start -= 1;
                if m as i32 * EBANDS[fold_start] <= eff {
                    break;
                }
            }
            let mut fold_end = lowband_offset - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i && (m as i32 * EBANDS[fold_end] < eff + n as i32)) {
                    break;
                }
            }
            x_cm = 0u32;
            y_cm = 0u32;
            let mut fold_i = fold_start;
            loop {
                x_cm |= collapse_masks[fold_i * channels] as u32;
                y_cm |= collapse_masks[fold_i * channels + channels - 1] as u32;
                fold_i += 1;
                if fold_i >= fold_end {
                    break;
                }
            }
        } else {
            x_cm = (1u32 << b) - 1;
            y_cm = x_cm;
        }

        if dual_stereo && i as i32 == intensity {
            dual_stereo = false;
            if resynth {
                let upto = x_start;
                for j in 0..upto {
                    norm[j] = 0.5 * (norm[j] + norm2[j]);
                }
            }
        }

        let lowband: Option<&[f32]> = match effective_lowband {
            Some(pos) => {
                lowband_buf[..n].copy_from_slice(&norm[pos..pos + n]);
                Some(&lowband_buf[..n])
            }
            None => None,
        };
        if dual_stereo {
            let (xb, yb) = (&mut x0[x_start..x_start + n], &mut x1[x_start..x_start + n]);
            x_cm = quant_band(
                &mut ctx,
                enc,
                xb,
                Band {
                    n,
                    b: b_bits / 2,
                    b_size: b,
                    lowband,
                    lm,
                    lowband_out: if last {
                        None
                    } else {
                        Some(&mut norm[x_start..x_start + n])
                    },
                    gain: 1.0,
                    fill: x_cm,
                },
                &mut band_scratch,
                &mut band_scratch2,
            );
            y_cm = quant_band(
                &mut ctx,
                enc,
                yb,
                Band {
                    n,
                    b: b_bits / 2,
                    b_size: b,
                    lowband,
                    lm,
                    lowband_out: if last {
                        None
                    } else {
                        Some(&mut norm2[x_start..x_start + n])
                    },
                    gain: 1.0,
                    fill: y_cm,
                },
                &mut band_scratch,
                &mut band_scratch2,
            );
        } else {
            let (xb, yb) = (&mut x0[x_start..x_start + n], &mut x1[x_start..x_start + n]);
            if theta_rdo && (i as i32) < intensity {
                let (w0, w1) = compute_channel_weights(band_e[i], band_e[i + NB_EBANDS]);
                let cm = x_cm | y_cm;
                // Both rate-distortion passes start from the same `xb`/`yb`
                // (the second restores them from `x_save`/`y_save`), so the
                // raw angle only needs computing once.
                let raw_itheta = stereo_itheta(xb, yb, true, n);
                let ec_save = enc.snapshot();
                let ctx_save_theta = ctx.theta_round;
                let ctx_save_remaining = ctx.remaining_bits;
                let ctx_save_seed = ctx.seed;
                x_save[..n].copy_from_slice(xb);
                y_save[..n].copy_from_slice(yb);

                ctx.theta_round = -1;
                x_cm = quant_band_stereo(
                    &mut ctx,
                    enc,
                    xb,
                    yb,
                    BandStereo {
                        n,
                        b: b_bits,
                        b_size: b,
                        lowband,
                        lm,
                        lowband_out: if last {
                            None
                        } else {
                            Some(&mut norm[x_start..x_start + n])
                        },
                        fill: cm,
                        itheta: Some(raw_itheta),
                    },
                    &mut band_scratch,
                    &mut band_scratch2,
                );
                let (d0x, d0y) = pair_inner_prod(&x_save[..n], xb, &y_save[..n], yb, n);
                let dist0 = w0 * d0x + w1 * d0y;

                let cm2 = x_cm;
                let ec_save2 = enc.snapshot();
                let ctx_save2_theta = ctx.theta_round;
                let ctx_save2_remaining = ctx.remaining_bits;
                let ctx_save2_seed = ctx.seed;
                x_save2[..n].copy_from_slice(xb);
                y_save2[..n].copy_from_slice(yb);
                if !last {
                    norm_save2[..n].copy_from_slice(&norm[x_start..x_start + n]);
                }

                enc.restore(&ec_save);
                ctx.theta_round = ctx_save_theta;
                ctx.remaining_bits = ctx_save_remaining;
                ctx.seed = ctx_save_seed;
                xb.copy_from_slice(&x_save[..n]);
                yb.copy_from_slice(&y_save[..n]);

                ctx.theta_round = 1;
                x_cm = quant_band_stereo(
                    &mut ctx,
                    enc,
                    xb,
                    yb,
                    BandStereo {
                        n,
                        b: b_bits,
                        b_size: b,
                        lowband,
                        lm,
                        lowband_out: if last {
                            None
                        } else {
                            Some(&mut norm[x_start..x_start + n])
                        },
                        fill: cm,
                        itheta: Some(raw_itheta),
                    },
                    &mut band_scratch,
                    &mut band_scratch2,
                );
                let (d1x, d1y) = pair_inner_prod(&x_save[..n], xb, &y_save[..n], yb, n);
                let dist1 = w0 * d1x + w1 * d1y;
                if dist0 >= dist1 {
                    x_cm = cm2;
                    enc.restore(&ec_save2);
                    ctx.theta_round = ctx_save2_theta;
                    ctx.remaining_bits = ctx_save2_remaining;
                    ctx.seed = ctx_save2_seed;
                    xb.copy_from_slice(&x_save2[..n]);
                    yb.copy_from_slice(&y_save2[..n]);
                    if !last {
                        norm[x_start..x_start + n].copy_from_slice(&norm_save2[..n]);
                    }
                }
            } else {
                ctx.theta_round = 0;
                x_cm = quant_band_stereo(
                    &mut ctx,
                    enc,
                    xb,
                    yb,
                    BandStereo {
                        n,
                        b: b_bits,
                        b_size: b,
                        lowband,
                        lm,
                        lowband_out: if last {
                            None
                        } else {
                            Some(&mut norm[x_start..x_start + n])
                        },
                        fill: x_cm | y_cm,
                        itheta: None,
                    },
                    &mut band_scratch,
                    &mut band_scratch2,
                );
            }
            y_cm = x_cm;
        }

        collapse_masks[i * channels] = x_cm as u8;
        collapse_masks[i * channels + channels - 1] = y_cm as u8;
        balance += pulses[i] + tell;
        update_lowband = b_bits > (n as i32) << BITRES;
        ctx.avoid_split_noise = false;
    }
    *seed = ctx.seed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::{Allocation, compute_allocation, init_caps};

    fn fill(x: &mut [f32], seed: u32) {
        let mut s = seed ^ 0x9E37_79B9;
        for v in x.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *v = ((s >> 16) as i32 % 2001 - 1000) as f32 * 0.001;
        }
    }

    fn fill_energies(band_e: &mut [f32; 42]) {
        for i in 0..21 {
            band_e[i] = 0.5 + 0.017 * i as f32;
            band_e[21 + i] = 0.4 + 0.023 * i as f32;
        }
    }

    fn fnv_bytes(p: &[u8]) -> u64 {
        let mut h = 14695981039346656037u64;
        for &b in p {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn fnv_words(p: &[u32]) -> u64 {
        let mut h = 14695981039346656037u64;
        for &w in p {
            h ^= w as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    struct Case {
        name: &'static str,
        nb: usize,
        x_kind: u8,
        short: bool,
        spread: i32,
        dual: bool,
        complexity: i32,
        intensity: i32,
        tf_kind: u8,
        off_mode: bool,
        seed: u32,
        pkt: u64,
        bytes: usize,
        end_seed: u32,
        x_hash: u64,
        mask_hash: u64,
    }

    const CASES: &[Case] = &[
        Case {
            name: "long-normal",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 2,
            dual: false,
            complexity: 10,
            intensity: 14,
            tf_kind: 0,
            off_mode: false,
            seed: 0x12345678,
            pkt: 0x2b2bd8d56fada09a,
            bytes: 314,
            end_seed: 0x12345678,
            x_hash: 0xfc855d36ad8c1d73,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "long-aggressive",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 3,
            dual: false,
            complexity: 10,
            intensity: 14,
            tf_kind: 0,
            off_mode: false,
            seed: 0x9abcdef0,
            pkt: 0xf68be53bb533a539,
            bytes: 320,
            end_seed: 0x9abcdef0,
            x_hash: 0x6af76b24812ac424,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "long-none",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 0,
            dual: false,
            complexity: 10,
            intensity: 21,
            tf_kind: 1,
            off_mode: false,
            seed: 0x0badc0de,
            pkt: 0xfe619df3b75cbb22,
            bytes: 338,
            end_seed: 0x0badc0de,
            x_hash: 0xa3237aaecc0595ac,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "long-light",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 1,
            dual: false,
            complexity: 10,
            intensity: 10,
            tf_kind: 3,
            off_mode: true,
            seed: 0xdeadbeef,
            pkt: 0x19a9c5dfa04d284f,
            bytes: 354,
            end_seed: 0xdeadbeef,
            x_hash: 0xd4e7368acb54a189,
            mask_hash: 0x27fcc064c89e16e9,
        },
        Case {
            name: "long-comp5",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 2,
            dual: false,
            complexity: 5,
            intensity: 14,
            tf_kind: 0,
            off_mode: false,
            seed: 0x31415926,
            pkt: 0xe141e83c85e8f8f6,
            bytes: 313,
            end_seed: 0x31415926,
            x_hash: 0xc3cb4147d25c572d,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "long-comp0",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 2,
            dual: false,
            complexity: 0,
            intensity: 21,
            tf_kind: 1,
            off_mode: false,
            seed: 0x27182818,
            pkt: 0x4adc2adb79a8b510,
            bytes: 342,
            end_seed: 0x27182818,
            x_hash: 0x24d164fb6d3608e4,
            mask_hash: 0x3c9ee95157370a9b,
        },
        Case {
            name: "long-dual",
            nb: 800,
            x_kind: 0,
            short: false,
            spread: 2,
            dual: true,
            complexity: 10,
            intensity: 14,
            tf_kind: 0,
            off_mode: false,
            seed: 0xfeedface,
            pkt: 0xdc2142804c769589,
            bytes: 318,
            end_seed: 0xfeedface,
            x_hash: 0xc6c11b8ac29f3782,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "short-normal",
            nb: 800,
            x_kind: 0,
            short: true,
            spread: 2,
            dual: false,
            complexity: 10,
            intensity: 14,
            tf_kind: 2,
            off_mode: false,
            seed: 0x00ddba11,
            pkt: 0x343cbad894ee40da,
            bytes: 317,
            end_seed: 0x00ddba11,
            x_hash: 0xed5d427542b844db,
            mask_hash: 0x41babe1c0fc1b19b,
        },
        Case {
            name: "short-none",
            nb: 800,
            x_kind: 0,
            short: true,
            spread: 0,
            dual: false,
            complexity: 10,
            intensity: 21,
            tf_kind: 3,
            off_mode: true,
            seed: 0xabad1dea,
            pkt: 0xc186cc0b2546a579,
            bytes: 343,
            end_seed: 0xabad1dea,
            x_hash: 0xa4a521edff32028b,
            mask_hash: 0x41babe1c0fc1b19b,
        },
        Case {
            name: "short-dual",
            nb: 800,
            x_kind: 0,
            short: true,
            spread: 3,
            dual: true,
            complexity: 10,
            intensity: 14,
            tf_kind: 2,
            off_mode: false,
            seed: 0xcafebabe,
            pkt: 0x6ddecc38b932a52e,
            bytes: 316,
            end_seed: 0xcafebabe,
            x_hash: 0xfdf81779f3beb2e4,
            mask_hash: 0x46086e6bd15d2ed5,
        },
        Case {
            name: "tiny-random",
            nb: 40,
            x_kind: 0,
            short: false,
            spread: 2,
            dual: false,
            complexity: 10,
            intensity: 14,
            tf_kind: 0,
            off_mode: false,
            seed: 0x1a2b3c4d,
            pkt: 0x6b132f11d328358a,
            bytes: 27,
            end_seed: 0xd696c4f5,
            x_hash: 0xd0f68425b56281e7,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "tiny-sparse",
            nb: 40,
            x_kind: 1,
            short: true,
            spread: 2,
            dual: false,
            complexity: 10,
            intensity: 14,
            tf_kind: 2,
            off_mode: true,
            seed: 0x55667788,
            pkt: 0xe522054de9f9ad3e,
            bytes: 22,
            end_seed: 0x3bd1b368,
            x_hash: 0xce3ddc17fed67b3d,
            mask_hash: 0x9bb01623832af6b9,
        },
        Case {
            name: "small-sparse",
            nb: 80,
            x_kind: 1,
            short: false,
            spread: 0,
            dual: false,
            complexity: 10,
            intensity: 21,
            tf_kind: 1,
            off_mode: false,
            seed: 0x99aabbcc,
            pkt: 0xd113e2f94e02c57c,
            bytes: 38,
            end_seed: 0xede2f32c,
            x_hash: 0xaeae9ecde996f22d,
            mask_hash: 0x05241571a0c23c7f,
        },
        Case {
            name: "small-dual",
            nb: 80,
            x_kind: 0,
            short: true,
            spread: 3,
            dual: true,
            complexity: 10,
            intensity: 14,
            tf_kind: 2,
            off_mode: false,
            seed: 0xddeeff00,
            pkt: 0xa7a4dd0d622f6ed3,
            bytes: 30,
            end_seed: 0xddeeff00,
            x_hash: 0xbd5a6521808167b0,
            mask_hash: 0x92b3dd2a0b82bd70,
        },
    ];

    fn run_case(case: &Case) {
        let mut x = vec![0f32; 2 * 960];
        fill(&mut x, case.seed);
        if case.x_kind == 1 {
            for c in 0..2 {
                for i in 8 * 16..960 {
                    x[c * 960 + i] = 0.0;
                }
            }
        }
        let mut band_e = [0f32; 42];
        fill_energies(&mut band_e);

        let mut tf_res = [0i32; 21];
        for (i, tf) in tf_res.iter_mut().enumerate() {
            *tf = match case.tf_kind {
                0 => 0,
                1 => -2,
                2 => {
                    if i % 2 == 1 {
                        0
                    } else {
                        3
                    }
                }
                _ => {
                    if i % 3 == 0 {
                        1
                    } else {
                        -1
                    }
                }
            };
        }

        let mut cap = [0i32; 21];
        init_caps(&mut cap, 3, 2);
        let mut offsets = [0i32; 21];
        if case.off_mode {
            offsets[2] = 180;
            offsets[9] = 300;
            offsets[18] = 500;
        }

        let mut pulses = [0i32; 21];
        let mut ebits = [0i32; 21];
        let mut fine_priority = [0i32; 21];
        let mut buf = vec![0u8; case.nb];
        let total_packet = (case.nb * 64) as i32;
        let mut collapse = [0u8; 42];

        let (pkt, bytes, end_seed) = {
            let mut enc = RangeEncoder::new(&mut buf);
            let bits = total_packet - enc.tell_frac() as i32 - 1;
            let mut intensity = case.intensity;
            let mut dual = case.dual;
            let mut balance = 0;
            let coded = compute_allocation(Allocation {
                ec: &mut enc,
                offsets: &offsets,
                cap: &cap,
                alloc_trim: 5,
                intensity: &mut intensity,
                dual_stereo: &mut dual,
                total_in: bits,
                balance: &mut balance,
                pulses: &mut pulses,
                ebits: &mut ebits,
                fine_priority: &mut fine_priority,
                c: 2,
                lm: 3,
                prev: 21,
                signal_bandwidth: 20,
            });
            let mut seed = case.seed;
            quant_all_bands(BandQuantizer {
                enc: &mut enc,
                x: &mut x,
                collapse_masks: &mut collapse,
                band_e: &band_e,
                pulses: &pulses,
                short_blocks: case.short,
                spread: case.spread,
                dual_stereo: dual,
                intensity,
                tf_res: &tf_res,
                total_bits: total_packet,
                balance,
                lm: 3,
                coded_bands: coded,
                seed: &mut seed,
                complexity: case.complexity,
            });
            enc.done();
            assert!(!enc.error(), "{}: range coder overflow", case.name);
            let bytes = enc.range_bytes();
            let end_seed = seed;
            drop(enc);
            (fnv_bytes(&buf), bytes, end_seed)
        };

        let words: Vec<u32> = x.iter().map(|v| v.to_bits()).collect();
        assert_eq!(pkt, case.pkt, "{}: packet", case.name);
        assert_eq!(bytes, case.bytes, "{}: length", case.name);
        assert_eq!(end_seed, case.end_seed, "{}: rng seed", case.name);
        assert_eq!(fnv_words(&words), case.x_hash, "{}: X", case.name);
        assert_eq!(
            fnv_bytes(&collapse),
            case.mask_hash,
            "{}: collapse masks",
            case.name
        );
    }

    #[test]
    fn quant_all_bands_matches_reference() {
        for case in CASES {
            run_case(case);
        }
    }

    #[test]
    fn normalise_bands_matches_reference() {
        let mut freq = vec![0f32; 2 * 960];
        let mut x = vec![0f32; 2 * 960];
        let mut band_e = [0f32; 42];
        fill(&mut freq, 0x11223344);
        fill_energies(&mut band_e);
        normalise_bands(&freq, &mut x, &band_e, 8);
        let words: Vec<u32> = x.iter().map(|v| v.to_bits()).collect();
        assert_eq!(fnv_words(&words), 0x5bd2b01a89169363);
    }

    #[test]
    fn bitexact_helpers_match_reference() {
        assert_eq!(bitexact_cos(0), -32768);
        assert_eq!(bitexact_cos(61), -32768);
        assert_eq!(bitexact_cos(122), 32766);
        assert_eq!(bitexact_cos(183), 32763);
        assert_eq!(bitexact_cos(305), 32754);
        assert_eq!(bitexact_cos(12139), 12971);

        assert_eq!(bitexact_log2tan(100, 16384 - 100), -15042);
        assert_eq!(bitexact_log2tan(200, 16384 - 200), -12977);
        assert_eq!(bitexact_log2tan(300, 16384 - 300), -11768);
        assert_eq!(bitexact_log2tan(400, 16384 - 400), -10894);
    }

    #[test]
    fn hysteresis_decision_matches_reference() {
        let thresholds = [0.5f32, 1.0, 2.0];
        let hysteresis = [0.1f32, 0.2, 0.3];
        let mut h = 14695981039346656037u64;
        for prev in [0, 1, 2, 3] {
            for k in 0..40 {
                let r = hysteresis_decision(0.01 * k as f32 * 7.0, &thresholds, &hysteresis, prev);
                h ^= r as u32 as u64;
                h = h.wrapping_mul(1099511628211);
            }
        }
        assert_eq!(h, 0xba5457456bf76bc0);
    }

    #[test]
    fn spreading_decision_matches_reference() {
        let mut spread_weight = [0i32; 21];
        for (i, w) in spread_weight.iter_mut().enumerate() {
            *w = 32 - i as i32;
        }
        let mut x = vec![0f32; 2 * 960];
        fill(&mut x, 0x5a5a5a5a);

        let want_decision = [3, 3, 2, 1, 0, 0, 0, 0];
        let want_average = [0, 30, 152, 341, 532, 650, 709, 738];
        let want_hf = [0, 4, 9, 17, 29, 38, 43, 45];
        let want_tap = [1, 0, 0, 0, 2, 2, 2, 2];

        let mut state = SpreadState {
            average: 0,
            hf_average: 0,
            tapset_decision: 1,
        };
        let mut last = 2;
        for frame in 0..8 {
            for (i, v) in x.iter_mut().enumerate() {
                *v *= 0.5;
                if (i as u32).wrapping_mul(2654435761) >> 28 == 0 {
                    *v = 0.0;
                }
            }
            last = spreading_decision(&x, &mut state, last, frame > 0, 2, 8, &spread_weight);
            assert_eq!(last, want_decision[frame], "frame {frame}");
            assert_eq!(state.average, want_average[frame], "frame {frame}");
            assert_eq!(state.hf_average, want_hf[frame], "frame {frame}");
            assert_eq!(state.tapset_decision, want_tap[frame], "frame {frame}");
        }
    }
}
