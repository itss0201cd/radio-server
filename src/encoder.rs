// encoder.rs — CELT-only Opus encoder for the pinned 48 kHz stereo 20 ms mode.
//
// Direct port of the float build of `celt/celt_encoder.c` with the Opus-layer
// wrapper removed: no SILK, no hybrid, no mode switching, no delay buffer. The
// packet is one CELT frame behind the config-31 TOC byte. Every optimisation
// later must keep the byte-for-byte reference tests in `tests` green.

use crate::alloc::{Allocation, EBANDS, LOG_N, NB_EBANDS, compute_allocation, init_caps};
use crate::bands::{
    BandQuantizer, SPREAD_NORMAL, SpreadState, hysteresis_decision, normalise_bands,
    quant_all_bands, spreading_decision,
};
use crate::energy::{
    CoarseRequest, E_MEANS, amp2_log2, compute_band_energies, quant_coarse_energy,
    quant_energy_finalise, quant_fine_energy,
};
use crate::mdct::Mdct;
use crate::pitch::{
    CombFilter, PitchScratch, RemoveDoubling, comb_filter, pitch_downsample, pitch_search,
    remove_doubling,
};
use crate::range_coder::RangeEncoder;
use crate::window::{OVERLAP, window};

const CHANNELS: usize = 2;
const LM: i32 = 3;
const M: usize = 1 << LM;
const N: usize = 960;
const SHORT_MDCT_SIZE: usize = 120;
const COMBFILTER_MAXPERIOD: usize = 1024;
const COMBFILTER_MINPERIOD: i32 = 15;
const BITRES: i32 = 3;
const SIG_SCALE: f32 = 32768.0;
const COMPLEXITY: i32 = 10;
pub const BITRATE: i32 = 320_000;
const LSB_DEPTH: i32 = 24;

/// Config 31: CELT fullband, 20 ms, stereo, single frame.
pub const TOC: u8 = 0b1111_1100;

const TAPSET_ICDF: [u8; 3] = [2, 1, 0];
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];

const TF_SELECT_TABLE: [[i32; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1],
    [0, -1, 0, -2, 1, 0, 1, -1],
    [0, -2, 0, -3, 2, 0, 1, -1],
    [0, -2, 0, -3, 3, 0, 1, -1],
];

const INTENSITY_THRESHOLDS: [f32; 21] = [
    1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 16.0, 24.0, 36.0, 44.0, 50.0, 56.0, 62.0, 67.0, 72.0,
    79.0, 88.0, 106.0, 134.0,
];
const INTENSITY_HYSTERESIS: [f32; 21] = [
    1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 2.0, 3.0, 3.0, 4.0, 5.0, 6.0,
    8.0, 8.0,
];

const INV_TABLE: [u8; 128] = [
    255, 255, 156, 110, 86, 70, 59, 51, 45, 40, 37, 33, 31, 28, 26, 25, 23, 22, 21, 20, 19, 18, 17,
    16, 16, 15, 15, 14, 13, 13, 12, 12, 12, 12, 11, 11, 11, 10, 10, 10, 9, 9, 9, 9, 9, 9, 8, 8, 8,
    8, 8, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2,
];

fn ilog(v: u32) -> i32 {
    debug_assert!(v > 0);
    32 - v.leading_zeros() as i32
}

fn celt_log2(x: f32) -> f32 {
    (1.442695040888963387f64 * (x as f64).ln()) as f32
}

fn celt_exp2(x: f32) -> f32 {
    (0.6931471805599453094f64 * (x as f64)).exp() as f32
}

fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

fn maxabs(x: &[f32]) -> f32 {
    x.iter().fold(0.0f32, |m, &v| m.max(v.abs()))
}

fn inner_prod(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(&x, &y)| x * y).sum()
}

fn median_of_3(x: &[f32]) -> f32 {
    let (t0, t1) = if x[0] > x[1] {
        (x[1], x[0])
    } else {
        (x[0], x[1])
    };
    let t2 = x[2];
    if t1 < t2 {
        t1
    } else if t0 < t2 {
        t2
    } else {
        t0
    }
}

fn median_of_5(x: &[f32]) -> f32 {
    let t2 = x[2];
    let (mut t0, mut t1) = if x[0] > x[1] {
        (x[1], x[0])
    } else {
        (x[0], x[1])
    };
    let (mut t3, mut t4) = if x[3] > x[4] {
        (x[4], x[3])
    } else {
        (x[3], x[4])
    };
    if t0 > t3 {
        std::mem::swap(&mut t0, &mut t3);
        std::mem::swap(&mut t1, &mut t4);
    }
    if t2 > t1 {
        if t1 < t3 { t2.min(t3) } else { t4.min(t1) }
    } else if t2 < t3 {
        t1.min(t3)
    } else {
        t2.min(t4)
    }
}

fn l1_metric(tmp: &[f32], n: usize, lm: i32, bias: f32) -> f32 {
    let l1: f32 = tmp[..n].iter().map(|v| v.abs()).sum();
    l1 + (lm as f32 * bias) * l1
}

/// `celt_preemphasis`, fast path (`coef[1] == 0`, no upsampling, no clipping).
fn preemphasis(pcm: &[f32], channel: usize, out: &mut [f32], mem: &mut f32) {
    let coef0 = 0.85000610f32;
    let mut m = *mem;
    for i in 0..N {
        let x = pcm[channel + CHANNELS * i] * SIG_SCALE;
        out[i] = x - m;
        m = coef0 * x;
    }
    *mem = m;
}

fn compute_mdcts(mdct: &mut Mdct, short_blocks: bool, input: &[f32], out: &mut [f32]) {
    let (blocks, block_n, shift) = if short_blocks {
        (M, SHORT_MDCT_SIZE, LM as u32)
    } else {
        (1, N, 0u32)
    };
    for c in 0..CHANNELS {
        for b in 0..blocks {
            let start = c * (blocks * block_n + OVERLAP) + b * block_n;
            let o = b + c * block_n * blocks;
            mdct.forward(&input[start..], &mut out[o..], shift, blocks);
        }
    }
}

fn transient_analysis(input: &[f32], tf_estimate: &mut f32, tf_chan: &mut usize) -> bool {
    let len = N + OVERLAP;
    let len2 = len / 2;
    let mut tmp = [0.0f32; N + OVERLAP];
    let mut mask_metric = 0i32;

    for c in 0..CHANNELS {
        let data = &input[c * len..(c + 1) * len];
        let mut mem0 = 0.0f32;
        let mut mem1 = 0.0f32;
        for i in 0..len {
            let x = data[i];
            let y = mem0 + x;
            mem0 = mem1 + y - 2.0 * x;
            mem1 = x - 0.5 * y;
            tmp[i] = y;
        }
        tmp[..12].fill(0.0);

        let mut mean = 0.0f32;
        let mut mem0 = 0.0f32;
        for i in 0..len2 {
            let x2 = tmp[2 * i] * tmp[2 * i] + tmp[2 * i + 1] * tmp[2 * i + 1];
            mean += x2;
            tmp[i] = mem0 + 0.0625 * (x2 - mem0);
            mem0 = tmp[i];
        }

        mem0 = 0.0;
        let mut max_e = 0.0f32;
        for i in (0..len2).rev() {
            tmp[i] = mem0 + 0.125 * (tmp[i] - mem0);
            mem0 = tmp[i];
            if mem0 > max_e {
                max_e = mem0;
            }
        }

        mean = celt_sqrt(mean * max_e * 0.5 * len2 as f32);
        let norm = len2 as f32 / (1e-15 + mean);
        let mut unmask = 0i32;
        for i in (12..len2 - 5).step_by(4) {
            let id = (64.0 * norm * (tmp[i] + 1e-15)).floor().clamp(0.0, 127.0) as usize;
            unmask += INV_TABLE[id] as i32;
        }
        unmask = 64 * unmask * 4 / (6 * (len2 as i32 - 17));
        if unmask > mask_metric {
            *tf_chan = c;
            mask_metric = unmask;
        }
    }

    let is_transient = mask_metric > 200;
    let tf_max = 0.0f32.max(celt_sqrt(27.0 * mask_metric as f32) - 42.0);
    *tf_estimate = celt_sqrt(0.0f32.max(0.0069 * 163.0f32.min(tf_max) - 0.139));
    is_transient
}

fn patch_transient_decision(new_e: &[f32], old_e: &[f32]) -> bool {
    let mut spread_old = [0.0f32; 26];
    spread_old[0] = old_e[0].max(old_e[NB_EBANDS]);
    for i in 1..NB_EBANDS {
        spread_old[i] = (spread_old[i - 1] - 1.0).max(old_e[i].max(old_e[i + NB_EBANDS]));
    }
    for i in (0..NB_EBANDS - 1).rev() {
        spread_old[i] = spread_old[i].max(spread_old[i + 1] - 1.0);
    }
    let mut mean_diff = 0.0f32;
    for c in 0..CHANNELS {
        for i in 2..NB_EBANDS - 1 {
            let x1 = new_e[i + c * NB_EBANDS].max(0.0);
            let x2 = spread_old[i].max(0.0);
            mean_diff += 0.0f32.max(x1 - x2);
        }
    }
    mean_diff /= (CHANNELS * (NB_EBANDS - 3)) as f32;
    mean_diff > 1.0
}

fn dynalloc_analysis(
    band_log_e: &[f32],
    band_log_e2: &[f32],
    offsets: &mut [i32; NB_EBANDS],
    importance: &mut [i32; NB_EBANDS],
    spread_weight: &mut [i32; NB_EBANDS],
    effective_bytes: i32,
) -> (f32, i32) {
    let mut noise_floor = [0.0f32; NB_EBANDS];
    let mut max_depth = -31.9f32;
    for i in 0..NB_EBANDS {
        noise_floor[i] = 0.0625 * LOG_N[i] as f32 + 0.5 + (9 - LSB_DEPTH) as f32 - E_MEANS[i]
            + 0.0062 * ((i + 5) * (i + 5)) as f32;
    }
    for c in 0..CHANNELS {
        for i in 0..NB_EBANDS {
            max_depth = max_depth.max(band_log_e[c * NB_EBANDS + i] - noise_floor[i]);
        }
    }

    let mut mask = [0.0f32; NB_EBANDS];
    for i in 0..NB_EBANDS {
        mask[i] = (band_log_e[i] - noise_floor[i]).max(band_log_e[NB_EBANDS + i] - noise_floor[i]);
    }
    let sig = mask;
    for i in 1..NB_EBANDS {
        mask[i] = mask[i].max(mask[i - 1] - 2.0);
    }
    for i in (0..NB_EBANDS - 1).rev() {
        mask[i] = mask[i].max(mask[i + 1] - 3.0);
    }
    for i in 0..NB_EBANDS {
        let smr = sig[i] - (0.0f32.max(max_depth - 12.0)).max(mask[i]);
        let shift = (-(0.5 + smr).floor() as i32).clamp(0, 5);
        spread_weight[i] = 32 >> shift;
    }

    let mut tot_boost = 0i32;
    if effective_bytes > 50 {
        let mut follower = [0.0f32; CHANNELS * NB_EBANDS];
        for c in 0..CHANNELS {
            let base = c * NB_EBANDS;
            let mut last = 0usize;
            follower[base] = band_log_e2[base];
            for i in 1..NB_EBANDS {
                if band_log_e2[base + i] > band_log_e2[base + i - 1] + 0.5 {
                    last = i;
                }
                follower[base + i] = (follower[base + i - 1] + 1.5).min(band_log_e2[base + i]);
            }
            for i in (0..last).rev() {
                follower[base + i] = follower[base + i]
                    .min((follower[base + i + 1] + 2.0).min(band_log_e2[base + i]));
            }
            for i in 2..NB_EBANDS - 2 {
                follower[base + i] = follower[base + i]
                    .max(median_of_5(&band_log_e2[base + i - 2..base + i + 3]) - 1.0);
            }
            let tmp = median_of_3(&band_log_e2[base..base + 3]) - 1.0;
            follower[base] = follower[base].max(tmp);
            follower[base + 1] = follower[base + 1].max(tmp);
            let tmp = median_of_3(&band_log_e2[base + NB_EBANDS - 3..base + NB_EBANDS]) - 1.0;
            follower[base + NB_EBANDS - 2] = follower[base + NB_EBANDS - 2].max(tmp);
            follower[base + NB_EBANDS - 1] = follower[base + NB_EBANDS - 1].max(tmp);
            for i in 0..NB_EBANDS {
                follower[base + i] = follower[base + i].max(noise_floor[i]);
            }
        }
        for i in 0..NB_EBANDS {
            follower[NB_EBANDS + i] = follower[NB_EBANDS + i].max(follower[i] - 4.0);
            follower[i] = follower[i].max(follower[NB_EBANDS + i] - 4.0);
            follower[i] = 0.5
                * (0.0f32.max(band_log_e[i] - follower[i])
                    + 0.0f32.max(band_log_e[NB_EBANDS + i] - follower[NB_EBANDS + i]));
        }
        for i in 0..NB_EBANDS {
            importance[i] = (0.5 + 13.0 * celt_exp2(follower[i].min(4.0))).floor() as i32;
        }
        for i in 0..NB_EBANDS {
            if i < 8 {
                follower[i] *= 2.0;
            }
            if i >= 12 {
                follower[i] *= 0.5;
            }
        }
        for i in 0..NB_EBANDS {
            follower[i] = follower[i].min(4.0);
            let width = (CHANNELS * (EBANDS[i + 1] - EBANDS[i]) as usize) << LM;
            let (boost, boost_bits) = if width < 6 {
                let boost = follower[i] as i32;
                (boost, (boost * (width as i32)) << BITRES)
            } else if width > 48 {
                let boost = (follower[i] * 8.0) as i32;
                (boost, (boost * (width as i32) << BITRES) / 8)
            } else {
                let boost = (follower[i] * width as f32 / 6.0) as i32;
                (boost, boost * 6 << BITRES)
            };
            offsets[i] = boost;
            tot_boost += boost_bits;
        }
    } else {
        importance.fill(13);
    }
    (max_depth, tot_boost)
}

fn tf_analysis(
    x: &[f32],
    is_transient: bool,
    tf_res: &mut [i32; NB_EBANDS],
    lambda: i32,
    tf_estimate: f32,
    tf_chan: usize,
    importance: &[i32],
) -> i32 {
    let bias = 0.04 * (-0.25f32).max(0.5 - tf_estimate);
    let mut metric = [0i32; NB_EBANDS];
    let mut tmp = [0.0f32; 176];
    let mut tmp_1 = [0.0f32; 176];

    for i in 0..NB_EBANDS {
        let n = ((EBANDS[i + 1] - EBANDS[i]) as usize) << LM;
        let narrow = EBANDS[i + 1] - EBANDS[i] == 1;
        let base = tf_chan * N + (EBANDS[i] as usize) * M;
        tmp[..n].copy_from_slice(&x[base..base + n]);
        let mut best_l1 = l1_metric(&tmp, n, if is_transient { LM } else { 0 }, bias);
        let mut best_level = 0i32;
        if is_transient && !narrow {
            tmp_1[..n].copy_from_slice(&tmp[..n]);
            crate::bands::haar1(&mut tmp_1[..n], n >> LM, 1 << LM);
            let l1 = l1_metric(&tmp_1, n, LM + 1, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = -1;
            }
        }
        let upper = LM + if !(is_transient || narrow) { 1 } else { 0 };
        for k in 0..upper {
            let b = if is_transient { LM - k - 1 } else { k + 1 };
            crate::bands::haar1(&mut tmp[..n], n >> k as usize, 1 << k as usize);
            let l1 = l1_metric(&tmp, n, b, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = k + 1;
            }
        }
        metric[i] = if is_transient {
            2 * best_level
        } else {
            -2 * best_level
        };
        if narrow && (metric[i] == 0 || metric[i] == -2 * LM) {
            metric[i] -= 1;
        }
    }

    let table = &TF_SELECT_TABLE[LM as usize];
    let mut selcost = [0i32; 2];
    for sel in 0..2i32 {
        let mut cost0 = importance[0]
            * (metric[0] - 2 * table[(4 * is_transient as usize + 2 * sel as usize) as usize])
                .abs();
        let mut cost1 = importance[0]
            * (metric[0] - 2 * table[4 * is_transient as usize + 2 * sel as usize + 1]).abs()
            + if is_transient { 0 } else { lambda };
        for i in 1..NB_EBANDS {
            let curr0 = cost0.min(cost1 + lambda);
            let curr1 = (cost0 + lambda).min(cost1);
            cost0 = curr0
                + importance[i]
                    * (metric[i] - 2 * table[4 * is_transient as usize + 2 * sel as usize]).abs();
            cost1 = curr1
                + importance[i]
                    * (metric[i] - 2 * table[4 * is_transient as usize + 2 * sel as usize + 1])
                        .abs();
        }
        selcost[sel as usize] = cost0.min(cost1);
    }
    let tf_select = if selcost[1] < selcost[0] && is_transient {
        1
    } else {
        0
    };

    let mut cost0 = importance[0]
        * (metric[0] - 2 * table[4 * is_transient as usize + 2 * tf_select as usize]).abs();
    let mut cost1 = importance[0]
        * (metric[0] - 2 * table[4 * is_transient as usize + 2 * tf_select as usize + 1]).abs()
        + if is_transient { 0 } else { lambda };
    let mut path0 = [0i32; NB_EBANDS];
    let mut path1 = [0i32; NB_EBANDS];
    for i in 1..NB_EBANDS {
        let from0 = cost0;
        let from1 = cost1 + lambda;
        let curr0;
        if from0 < from1 {
            curr0 = from0;
            path0[i] = 0;
        } else {
            curr0 = from1;
            path0[i] = 1;
        }
        let from0 = cost0 + lambda;
        let from1 = cost1;
        let curr1;
        if from0 < from1 {
            curr1 = from0;
            path1[i] = 0;
        } else {
            curr1 = from1;
            path1[i] = 1;
        }
        cost0 = curr0
            + importance[i]
                * (metric[i] - 2 * table[4 * is_transient as usize + 2 * tf_select as usize]).abs();
        cost1 = curr1
            + importance[i]
                * (metric[i] - 2 * table[4 * is_transient as usize + 2 * tf_select as usize + 1])
                    .abs();
    }
    tf_res[NB_EBANDS - 1] = if cost0 < cost1 { 0 } else { 1 };
    for i in (0..NB_EBANDS - 1).rev() {
        tf_res[i] = if tf_res[i + 1] == 1 {
            path1[i + 1]
        } else {
            path0[i + 1]
        };
    }
    tf_select
}

fn tf_encode(
    enc: &mut RangeEncoder,
    is_transient: bool,
    tf_res: &mut [i32; NB_EBANDS],
    tf_select_in: i32,
) {
    let mut tf_select = tf_select_in;
    let mut budget = enc.storage() as i32 * 8;
    let mut tell = enc.tell();
    let mut logp = if is_transient { 2 } else { 4 };
    let tf_select_rsv = tell + logp + 1 <= budget;
    budget -= tf_select_rsv as i32;
    let mut curr = 0i32;
    let mut tf_changed = 0i32;
    for i in 0..NB_EBANDS {
        if tell + logp <= budget {
            enc.encode_bit_logp((tf_res[i] ^ curr) != 0, logp as u32);
            tell = enc.tell();
            curr = tf_res[i];
            tf_changed |= curr;
        } else {
            tf_res[i] = curr;
        }
        logp = if is_transient { 4 } else { 5 };
    }
    let table = &TF_SELECT_TABLE[LM as usize];
    let index = 4 * is_transient as usize;
    if tf_select_rsv && table[index + tf_changed as usize] != table[index + 2 + tf_changed as usize]
    {
        enc.encode_bit_logp(tf_select != 0, 1);
    } else {
        tf_select = 0;
    }
    for i in 0..NB_EBANDS {
        tf_res[i] = table[index + 2 * tf_select as usize + tf_res[i] as usize];
    }
}

fn stereo_analysis(x: &[f32]) -> bool {
    let mut sum_lr = 1e-15f32;
    let mut sum_ms = 1e-15f32;
    for i in 0..13 {
        let start = (EBANDS[i] as usize) << LM;
        let stop = (EBANDS[i + 1] as usize) << LM;
        for j in start..stop {
            let l = x[j];
            let r = x[N + j];
            sum_lr += l.abs() + r.abs();
            sum_ms += (l + r).abs() + (l - r).abs();
        }
    }
    sum_ms *= 0.707107f32;
    let thetas = 13i32;
    ((EBANDS[13] << (LM + 1)) + thetas) as f32 * sum_ms > (EBANDS[13] << (LM + 1)) as f32 * sum_lr
}

fn alloc_trim_analysis(
    x: &[f32],
    band_log_e: &[f32],
    intensity: i32,
    stereo_saving: &mut f32,
    tf_estimate: f32,
    equiv_rate: i32,
) -> i32 {
    let mut trim = 5.0f32;
    if equiv_rate < 64000 {
        trim = 4.0;
    } else if equiv_rate < 80000 {
        let frac = (equiv_rate - 64000) >> 10;
        trim = 4.0 + (1.0 / 16.0) * frac as f32;
    }
    let mut sum = 0.0f32;
    for i in 0..8 {
        let start = (EBANDS[i] as usize) << LM;
        let stop = (EBANDS[i + 1] as usize) << LM;
        sum += inner_prod(&x[start..stop], &x[N + start..N + stop]);
    }
    sum *= 1.0 / 8.0;
    sum = sum.abs().min(1.0);
    let mut min_xc = sum;
    for i in 8..intensity as usize {
        let start = (EBANDS[i] as usize) << LM;
        let stop = (EBANDS[i + 1] as usize) << LM;
        let partial = inner_prod(&x[start..stop], &x[N + start..N + stop]);
        min_xc = min_xc.min(partial.abs());
    }
    min_xc = min_xc.abs().min(1.0);
    let log_xc = celt_log2(1.001 - sum * sum);
    let log_xc2 = (0.5 * log_xc).max(celt_log2(1.001 - min_xc * min_xc));
    trim += (-4.0f32).max(0.75 * log_xc);
    *stereo_saving = (*stereo_saving + 0.25).min(-0.5 * log_xc2);

    let mut diff = 0.0f32;
    for c in 0..CHANNELS {
        for i in 0..NB_EBANDS - 1 {
            diff += band_log_e[i + c * NB_EBANDS] * (2 + 2 * i as i32 - NB_EBANDS as i32) as f32;
        }
    }
    diff /= (CHANNELS * (NB_EBANDS - 1)) as f32;
    trim -= ((diff + 1.0) / 6.0).clamp(-2.0, 2.0);
    trim -= 2.0 * tf_estimate;
    ((0.5 + trim).floor() as i32).clamp(0, 10)
}

fn compute_vbr(
    base_target: i32,
    last_coded_bands: i32,
    intensity: i32,
    stereo_saving: f32,
    tot_boost: i32,
    tf_estimate: f32,
    max_depth: f32,
) -> i32 {
    let coded_bands = if last_coded_bands != 0 {
        last_coded_bands
    } else {
        NB_EBANDS as i32
    };
    let mut coded_bins = (EBANDS[coded_bands as usize] << LM) as i32;
    coded_bins += (EBANDS[intensity.min(coded_bands) as usize] << LM) as i32;

    let mut target = base_target;
    let coded_stereo_bands = intensity.min(coded_bands);
    let coded_stereo_dof = (EBANDS[coded_stereo_bands as usize] << LM) - coded_stereo_bands;
    let max_frac = 0.8 * coded_stereo_dof as f32 / coded_bins as f32;
    let saving = stereo_saving.min(1.0);
    let term = (saving - 0.1) * ((coded_stereo_dof << BITRES) as f32);
    target -= (max_frac * target as f32).min(term) as i32;

    target += tot_boost - (19 << LM);
    target += ((tf_estimate - 0.044) * target as f32) as i32;

    let bins = (EBANDS[NB_EBANDS - 2] << LM) as i32;
    let mut floor_depth = ((CHANNELS as i32 * bins << BITRES) as f32 * max_depth) as i32;
    floor_depth = floor_depth.max(target >> 2);
    target = target.min(floor_depth);
    (2 * base_target).min(target)
}

struct Prefilter<'a> {
    mem: &'a mut [f32],
    in_mem: &'a mut [f32],
    period: &'a mut i32,
    gain: f32,
    tapset: i32,
    enabled: bool,
    nb_available_bytes: i32,
    scratch: &'a mut PitchScratch,
}

fn run_prefilter(in_buf: &mut [f32], params: Prefilter<'_>) -> (bool, i32, f32, i32) {
    let Prefilter {
        mem,
        in_mem,
        period,
        gain: prefilter_gain,
        tapset: prefilter_tapset,
        enabled,
        nb_available_bytes,
        scratch,
    } = params;
    let mut pre = [0.0f32; CHANNELS * (N + COMBFILTER_MAXPERIOD)];
    for c in 0..CHANNELS {
        let p = c * (N + COMBFILTER_MAXPERIOD);
        pre[p..p + COMBFILTER_MAXPERIOD]
            .copy_from_slice(&mem[c * COMBFILTER_MAXPERIOD..(c + 1) * COMBFILTER_MAXPERIOD]);
        let s = c * (N + OVERLAP);
        pre[p + COMBFILTER_MAXPERIOD..p + COMBFILTER_MAXPERIOD + N]
            .copy_from_slice(&in_buf[s + OVERLAP..s + OVERLAP + N]);
    }

    let mut pitch_buf = [0.0f32; (COMBFILTER_MAXPERIOD + N) >> 1];
    let mut pitch_index = COMBFILTER_MINPERIOD;
    let mut gain1 = 0.0f32;
    if enabled {
        pitch_downsample(
            &pre[..N + COMBFILTER_MAXPERIOD],
            &pre[N + COMBFILTER_MAXPERIOD..],
            &mut pitch_buf,
            COMBFILTER_MAXPERIOD + N,
        );
        let search = pitch_search(
            &pitch_buf[COMBFILTER_MAXPERIOD >> 1..],
            &pitch_buf,
            N,
            COMBFILTER_MAXPERIOD - 3 * COMBFILTER_MINPERIOD as usize,
            scratch,
        );
        pitch_index = COMBFILTER_MAXPERIOD as i32 - search;
        gain1 = remove_doubling(
            &pitch_buf,
            &mut pitch_index,
            RemoveDoubling {
                maxperiod: COMBFILTER_MAXPERIOD,
                minperiod: COMBFILTER_MINPERIOD,
                n: N,
                prev_period: *period,
                prev_gain: prefilter_gain,
                scratch,
            },
        );
        if pitch_index > COMBFILTER_MAXPERIOD as i32 - 2 {
            pitch_index = COMBFILTER_MAXPERIOD as i32 - 2;
        }
        gain1 *= 0.7;
    }

    let mut pf_threshold = 0.2f32;
    if (pitch_index - *period).abs() * 10 > pitch_index {
        pf_threshold += 0.2;
    }
    if nb_available_bytes < 25 {
        pf_threshold += 0.1;
    }
    if nb_available_bytes < 35 {
        pf_threshold += 0.1;
    }
    if prefilter_gain > 0.4 {
        pf_threshold -= 0.1;
    }
    if prefilter_gain > 0.55 {
        pf_threshold -= 0.1;
    }
    pf_threshold = pf_threshold.max(0.2);

    let (pf_on, qg);
    if gain1 < pf_threshold {
        gain1 = 0.0;
        pf_on = false;
        qg = 0;
    } else {
        if (gain1 - prefilter_gain).abs() < 0.1 {
            gain1 = prefilter_gain;
        }
        qg = ((0.5 + gain1 * 32.0 / 3.0).floor() as i32 - 1).clamp(0, 7);
        gain1 = 0.09375 * (qg + 1) as f32;
        pf_on = true;
    }

    for c in 0..CHANNELS {
        *period = (*period).max(COMBFILTER_MINPERIOD);
        let s = c * (N + OVERLAP);
        in_buf[s..s + OVERLAP].copy_from_slice(&in_mem[c * OVERLAP..(c + 1) * OVERLAP]);
        let x_off = c * (N + COMBFILTER_MAXPERIOD) + COMBFILTER_MAXPERIOD;
        comb_filter(
            &mut in_buf[s + OVERLAP..s + OVERLAP + N],
            &pre,
            CombFilter {
                x_off,
                t0: *period,
                t1: pitch_index,
                n: N,
                g0: -prefilter_gain,
                g1: -gain1,
                tapset0: prefilter_tapset as usize,
                tapset1: prefilter_tapset as usize,
                window: Some(window()),
                overlap: OVERLAP,
            },
        );
        in_mem[c * OVERLAP..(c + 1) * OVERLAP].copy_from_slice(&in_buf[s + N..s + N + OVERLAP]);
        let keep = COMBFILTER_MAXPERIOD - N;
        let pm = c * COMBFILTER_MAXPERIOD;
        mem.copy_within(pm + N..pm + COMBFILTER_MAXPERIOD, pm);
        mem[pm + keep..pm + COMBFILTER_MAXPERIOD].copy_from_slice(&pre[x_off..x_off + N]);
    }

    (pf_on, pitch_index, gain1, qg)
}

pub struct Encoder {
    mdct: Mdct,
    rng: u32,
    spread_decision: i32,
    delayed_intra: f32,
    spread_state: SpreadState,
    last_coded_bands: i32,
    prefilter_period: i32,
    prefilter_gain: f32,
    prefilter_tapset: i32,
    consec_transient: i32,
    preemph_mem_e: [f32; CHANNELS],
    overlap_max: f32,
    stereo_saving: f32,
    intensity: i32,
    in_mem: [f32; CHANNELS * OVERLAP],
    prefilter_mem: [f32; CHANNELS * COMBFILTER_MAXPERIOD],
    old_band_e: [f32; CHANNELS * NB_EBANDS],
    old_log_e: [f32; CHANNELS * NB_EBANDS],
    old_log_e2: [f32; CHANNELS * NB_EBANDS],
    energy_error: [f32; CHANNELS * NB_EBANDS],
    pitch_scratch: PitchScratch,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            mdct: Mdct::new(),
            rng: 0,
            spread_decision: SPREAD_NORMAL,
            delayed_intra: 1.0,
            spread_state: SpreadState {
                average: 256,
                hf_average: 0,
                tapset_decision: 0,
            },
            last_coded_bands: 0,
            prefilter_period: 0,
            prefilter_gain: 0.0,
            prefilter_tapset: 0,
            consec_transient: 0,
            preemph_mem_e: [0.0; CHANNELS],
            overlap_max: 0.0,
            stereo_saving: 0.0,
            intensity: 0,
            in_mem: [0.0; CHANNELS * OVERLAP],
            prefilter_mem: [0.0; CHANNELS * COMBFILTER_MAXPERIOD],
            old_band_e: [0.0; CHANNELS * NB_EBANDS],
            old_log_e: [-28.0; CHANNELS * NB_EBANDS],
            old_log_e2: [-28.0; CHANNELS * NB_EBANDS],
            energy_error: [0.0; CHANNELS * NB_EBANDS],
            pitch_scratch: PitchScratch::default(),
        }
    }

    /// Encode one 20 ms stereo frame. Returns the packet length, TOC included.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Option<usize> {
        debug_assert_eq!(pcm.len(), CHANNELS * N);
        debug_assert!(out.len() >= 1276);
        let frame = self.encode_frame(pcm, &mut out[1..])?;
        out[0] = TOC;
        Some(frame + 1)
    }

    fn encode_frame(&mut self, pcm: &[f32], compressed: &mut [u8]) -> Option<usize> {
        let mut nb_compressed_bytes = compressed.len().min(1275);
        let buf = &mut compressed[..nb_compressed_bytes];
        let mut enc = RangeEncoder::new(buf);
        let mut tell = 1i32;
        let mut nb_available_bytes = nb_compressed_bytes as i32;

        let den = 48000 >> BITRES;
        let vbr_rate = (BITRATE * N as i32 + (den >> 1)) / den;
        let effective_bytes = vbr_rate >> (3 + BITRES);
        let mut equiv_rate = nb_compressed_bytes as i32 * 8 * 50 << (3 - LM);
        equiv_rate -= (40 * CHANNELS as i32 + 20) * ((400 >> LM) - 50);
        equiv_rate = equiv_rate.min(BITRATE - (40 * CHANNELS as i32 + 20) * ((400 >> LM) - 50));

        let mut total_bits = nb_compressed_bytes as i32 * 8;

        let mut in_buf = [0.0f32; CHANNELS * (N + OVERLAP)];
        let tail_max = maxabs(&pcm[CHANNELS * (N - OVERLAP)..]);
        let sample_max = self
            .overlap_max
            .max(maxabs(&pcm[..CHANNELS * (N - OVERLAP)]))
            .max(tail_max);
        self.overlap_max = tail_max;
        let silence = sample_max <= 1.0 / (1i32 << LSB_DEPTH) as f32;
        enc.encode_bit_logp(silence, 15);
        if silence {
            nb_compressed_bytes = nb_compressed_bytes.min(2);
            total_bits = nb_compressed_bytes as i32 * 8;
            nb_available_bytes = 2;
            enc.shrink(nb_compressed_bytes);
            tell = nb_compressed_bytes as i32 * 8;
            enc.force_tell(tell);
        }

        for c in 0..CHANNELS {
            preemphasis(
                pcm,
                c,
                &mut in_buf[c * (N + OVERLAP) + OVERLAP..],
                &mut self.preemph_mem_e[c],
            );
        }

        let enabled = nb_available_bytes > 12 * CHANNELS as i32 && !silence;
        let prefilter_tapset = self.spread_state.tapset_decision;
        let (pf_on, pitch_index, gain1, qg) = run_prefilter(
            &mut in_buf,
            Prefilter {
                mem: &mut self.prefilter_mem,
                in_mem: &mut self.in_mem,
                period: &mut self.prefilter_period,
                gain: self.prefilter_gain,
                tapset: prefilter_tapset,
                enabled,
                nb_available_bytes,
                scratch: &mut self.pitch_scratch,
            },
        );
        let mut pitch_index = pitch_index;
        if pf_on {
            enc.encode_bit_logp(true, 1);
            pitch_index += 1;
            let octave = ilog(pitch_index as u32) - 5;
            enc.encode_uint(octave as u32, 6);
            enc.encode_bits((pitch_index - (16 << octave)) as u32, (4 + octave) as u32);
            pitch_index -= 1;
            enc.encode_bits(qg as u32, 3);
            enc.encode_icdf(prefilter_tapset as usize, &TAPSET_ICDF, 2);
        } else if tell + 16 <= total_bits {
            enc.encode_bit_logp(false, 1);
        }

        let mut short_blocks = 0usize;
        let mut tf_estimate = 0.0f32;
        let mut tf_chan = 0usize;
        let mut transient_got_disabled = false;
        let mut is_transient = transient_analysis(&in_buf, &mut tf_estimate, &mut tf_chan);
        if enc.tell() + 3 <= total_bits {
            if is_transient {
                short_blocks = M;
            }
        } else {
            is_transient = false;
            transient_got_disabled = true;
        }

        let mut freq = [0.0f32; CHANNELS * N];
        let mut band_e = [0.0f32; CHANNELS * NB_EBANDS];
        let mut band_log_e = [0.0f32; CHANNELS * NB_EBANDS];
        let mut band_log_e2 = [0.0f32; CHANNELS * NB_EBANDS];

        let second_mdct = short_blocks != 0;
        if second_mdct {
            compute_mdcts(&mut self.mdct, false, &in_buf, &mut freq);
            compute_band_energies(&freq, &mut band_e, LM);
            amp2_log2(&band_e, &mut band_log_e2, CHANNELS);
            for c in 0..CHANNELS {
                for i in 0..NB_EBANDS {
                    band_log_e2[NB_EBANDS * c + i] += 1.5;
                }
            }
        }

        compute_mdcts(&mut self.mdct, short_blocks != 0, &in_buf, &mut freq);
        compute_band_energies(&freq, &mut band_e, LM);
        amp2_log2(&band_e, &mut band_log_e, CHANNELS);

        if !second_mdct {
            band_log_e2.copy_from_slice(&band_log_e);
        }

        if enc.tell() + 3 <= total_bits && !is_transient {
            if patch_transient_decision(&band_log_e, &self.old_band_e) {
                is_transient = true;
                short_blocks = M;
                compute_mdcts(&mut self.mdct, true, &in_buf, &mut freq);
                compute_band_energies(&freq, &mut band_e, LM);
                amp2_log2(&band_e, &mut band_log_e, CHANNELS);
                for c in 0..CHANNELS {
                    for i in 0..NB_EBANDS {
                        band_log_e2[NB_EBANDS * c + i] += 1.5;
                    }
                }
                tf_estimate = 0.2;
            }
        }

        if enc.tell() + 3 <= total_bits {
            enc.encode_bit_logp(is_transient, 3);
        }

        let mut x = [0.0f32; CHANNELS * N];
        normalise_bands(&freq, &mut x, &band_e, M);

        let enable_tf_analysis = effective_bytes >= 15 * CHANNELS as i32;
        let mut offsets = [0i32; NB_EBANDS];
        let mut importance = [0i32; NB_EBANDS];
        let mut spread_weight = [0i32; NB_EBANDS];
        let (max_depth, tot_boost) = dynalloc_analysis(
            &band_log_e,
            &band_log_e2,
            &mut offsets,
            &mut importance,
            &mut spread_weight,
            effective_bytes,
        );

        let mut tf_res = [0i32; NB_EBANDS];
        let tf_select;
        if enable_tf_analysis {
            let lambda = 80.max(20480 / effective_bytes + 2);
            tf_select = tf_analysis(
                &x,
                is_transient,
                &mut tf_res,
                lambda,
                tf_estimate,
                tf_chan,
                &importance,
            );
        } else {
            for value in tf_res.iter_mut() {
                *value = is_transient as i32;
            }
            tf_select = 0;
        }

        let mut error = [0.0f32; CHANNELS * NB_EBANDS];
        for c in 0..CHANNELS {
            for i in 0..NB_EBANDS {
                let idx = i + c * NB_EBANDS;
                if (band_log_e[idx] - self.old_band_e[idx]).abs() < 2.0 {
                    band_log_e[idx] -= self.energy_error[idx] * 0.25;
                }
            }
        }

        quant_coarse_energy(
            &mut enc,
            &mut self.old_band_e,
            &mut error,
            &mut self.delayed_intra,
            CoarseRequest {
                e_bands: &band_log_e,
                budget: total_bits,
                c: CHANNELS,
                lm: LM,
                nb_available_bytes,
            },
        );

        tf_encode(&mut enc, is_transient, &mut tf_res, tf_select);

        if enc.tell() + 4 <= total_bits {
            if short_blocks != 0 || nb_available_bytes < 10 * CHANNELS as i32 {
                self.spread_decision = SPREAD_NORMAL;
            } else {
                self.spread_decision = spreading_decision(
                    &x,
                    &mut self.spread_state,
                    self.spread_decision,
                    pf_on && short_blocks == 0,
                    CHANNELS,
                    M,
                    &spread_weight,
                );
            }
            enc.encode_icdf(self.spread_decision as usize, &SPREAD_ICDF, 5);
        }

        let mut cap = [0i32; NB_EBANDS];
        init_caps(&mut cap, LM, CHANNELS as i32);

        let mut dynalloc_logp = 6;
        total_bits <<= BITRES;
        let mut total_boost = 0i32;
        let mut tell_frac = enc.tell_frac() as i32;
        for i in 0..NB_EBANDS {
            let width = (CHANNELS * (EBANDS[i + 1] - EBANDS[i]) as usize) << LM;
            let quanta = ((width << BITRES) as i32).min((6 << BITRES).max(width as i32));
            let mut dynalloc_loop_logp = dynalloc_logp;
            let mut boost = 0i32;
            let mut j = 0i32;
            while tell_frac + (dynalloc_loop_logp << BITRES) < total_bits - total_boost
                && boost < cap[i]
            {
                let flag = j < offsets[i];
                enc.encode_bit_logp(flag, dynalloc_loop_logp as u32);
                tell_frac = enc.tell_frac() as i32;
                if !flag {
                    break;
                }
                boost += quanta;
                total_boost += quanta;
                dynalloc_loop_logp = 1;
                j += 1;
            }
            if j != 0 {
                dynalloc_logp = (dynalloc_logp - 1).max(2);
            }
            offsets[i] = boost;
        }

        let dual_stereo = stereo_analysis(&x);
        self.intensity = hysteresis_decision(
            (equiv_rate / 1000) as f32,
            &INTENSITY_THRESHOLDS,
            &INTENSITY_HYSTERESIS,
            self.intensity,
        );
        self.intensity = self.intensity.min(NB_EBANDS as i32).max(0);

        let mut alloc_trim = 5;
        if tell_frac + (6 << BITRES) <= total_bits - total_boost {
            alloc_trim = alloc_trim_analysis(
                &x,
                &band_log_e,
                self.intensity,
                &mut self.stereo_saving,
                tf_estimate,
                equiv_rate,
            );
            enc.encode_icdf(alloc_trim as usize, &TRIM_ICDF, 7);
            tell_frac = enc.tell_frac() as i32;
        }

        let base_target = vbr_rate - ((40 * CHANNELS as i32 + 20) << BITRES);
        let mut target = compute_vbr(
            base_target,
            self.last_coded_bands,
            self.intensity,
            self.stereo_saving,
            tot_boost,
            tf_estimate,
            max_depth,
        );
        target += tell_frac;
        let min_allowed = ((tell_frac + total_boost + (1 << (BITRES + 3)) - 1) >> (BITRES + 3)) + 2;
        let mut avail = (target + (1 << (BITRES + 2))) >> (BITRES + 3);
        avail = avail.max(min_allowed).min(nb_compressed_bytes as i32);
        if silence {
            avail = 2;
        }
        nb_compressed_bytes = nb_compressed_bytes.min(avail as usize);
        enc.shrink(nb_compressed_bytes);

        let mut fine_quant = [0i32; NB_EBANDS];
        let mut pulses = [0i32; NB_EBANDS];
        let mut fine_priority = [0i32; NB_EBANDS];
        let mut bits = ((nb_compressed_bytes as i32 * 8) << BITRES) - enc.tell_frac() as i32 - 1;
        let anti_collapse_rsv = if is_transient && bits >= ((LM + 2) << BITRES) {
            1 << BITRES
        } else {
            0
        };
        bits -= anti_collapse_rsv;
        let mut intensity = self.intensity;
        let mut dual = dual_stereo;
        let mut balance = 0i32;
        let coded_bands = compute_allocation(Allocation {
            ec: &mut enc,
            offsets: &offsets,
            cap: &cap,
            alloc_trim,
            intensity: &mut intensity,
            dual_stereo: &mut dual,
            total_in: bits,
            balance: &mut balance,
            pulses: &mut pulses,
            ebits: &mut fine_quant,
            fine_priority: &mut fine_priority,
            c: CHANNELS as i32,
            lm: LM,
            prev: self.last_coded_bands,
            signal_bandwidth: (NB_EBANDS - 1) as i32,
        });
        self.intensity = intensity;
        if self.last_coded_bands != 0 {
            self.last_coded_bands =
                (self.last_coded_bands + 1).min((self.last_coded_bands - 1).max(coded_bands));
        } else {
            self.last_coded_bands = coded_bands;
        }

        quant_fine_energy(
            &mut enc,
            &mut self.old_band_e,
            &mut error,
            &fine_quant,
            CHANNELS,
        );

        let mut collapse_masks = [0u8; CHANNELS * NB_EBANDS];
        let mut seed = self.rng;
        quant_all_bands(BandQuantizer {
            enc: &mut enc,
            x: &mut x,
            collapse_masks: &mut collapse_masks,
            band_e: &band_e,
            pulses: &pulses,
            short_blocks: short_blocks != 0,
            spread: self.spread_decision,
            dual_stereo: dual,
            intensity: self.intensity,
            tf_res: &tf_res,
            total_bits: nb_compressed_bytes as i32 * (8 << BITRES) - anti_collapse_rsv,
            balance,
            lm: LM,
            coded_bands,
            seed: &mut seed,
            complexity: COMPLEXITY,
        });

        if anti_collapse_rsv > 0 {
            let on = self.consec_transient < 2;
            enc.encode_bits(on as u32, 1);
        }

        let bits_left = nb_compressed_bytes as i32 * 8 - enc.tell();
        quant_energy_finalise(
            &mut enc,
            &mut self.old_band_e,
            &mut error,
            &fine_quant,
            &fine_priority,
            bits_left,
            CHANNELS,
        );

        for i in 0..CHANNELS * NB_EBANDS {
            self.energy_error[i] = error[i].clamp(-0.5, 0.5);
        }
        if silence {
            for value in self.old_band_e.iter_mut() {
                *value = -28.0;
            }
        }

        self.prefilter_period = pitch_index;
        self.prefilter_gain = gain1;
        self.prefilter_tapset = prefilter_tapset;

        if !is_transient {
            self.old_log_e2.copy_from_slice(&self.old_log_e);
            self.old_log_e.copy_from_slice(&self.old_band_e);
        } else {
            for i in 0..CHANNELS * NB_EBANDS {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_band_e[i]);
            }
        }

        if is_transient || transient_got_disabled {
            self.consec_transient += 1;
        } else {
            self.consec_transient = 0;
        }
        self.rng = enc.rng();
        enc.done();
        if enc.error() {
            None
        } else {
            Some(nb_compressed_bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnv(p: &[u8]) -> u64 {
        let mut h = 14695981039346656037u64;
        for &b in p {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    fn generate(kind: u8, frame: usize, pcm: &mut [f32], s: &mut u32) {
        for i in 0..960 {
            for c in 0..2 {
                let pos = frame * 960 + i;
                let v = if kind == 3 {
                    0.0
                } else if kind == 1 {
                    let phase = (pos + c * 37) / 80;
                    let base = if phase & 1 == 1 { -0.35 } else { 0.35 };
                    *s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    base + ((*s >> 16) as i32 % 2001 - 1000) as f32 * 0.00005
                } else if kind == 2 {
                    let amp = if i < 240 { 0.9 } else { 0.02 };
                    *s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    (((*s >> 16) as i32 % 2001 - 1000) as f32 * 0.0005) * amp
                } else {
                    *s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((*s >> 16) as i32 % 2001 - 1000) as f32 * 0.0005
                };
                pcm[2 * i + c] = v;
            }
        }
    }

    const CASES: &[(&str, u8, u32, &[(usize, u64)])] = &[
        (
            "noise",
            0,
            0x12345678,
            &[
                (1049, 0x069ce3ceaba504b6),
                (770, 0x25a986a93a866729),
                (769, 0x5ee4c5c5027efb09),
                (772, 0xce06daed3262eed2),
                (769, 0x17944a2f5a658db9),
                (769, 0x88580bc8c0254aed),
                (769, 0x6b1e881ad9a450ab),
                (769, 0x1a4e39809f0745f4),
                (769, 0x18ab12ae26eeef2b),
                (770, 0x31c5211f2fe33ccd),
                (769, 0x90446bb56e0c5608),
                (770, 0x38e6fcc3169986b6),
            ],
        ),
        (
            "periodic",
            1,
            0x9abcdef0,
            &[
                (1073, 0xa8d92de862519d94),
                (803, 0x934709f117fa95bc),
                (798, 0x775f50a5c4adef7c),
                (796, 0xee568cfdd210ea1b),
                (798, 0xa02ccc95148cf275),
                (798, 0xb4e9ac8cf8b90610),
                (796, 0x2fd32d3b18bf309b),
                (795, 0x1829bf0d2df44370),
                (792, 0x5191a4b9d92771f0),
                (797, 0xbc413d7aa7dcef6d),
                (798, 0x36c0cef8eea551c2),
                (790, 0x2c11f158dd0f99b5),
            ],
        ),
        (
            "transient",
            2,
            0x0badc0de,
            &[
                (1275, 0x983d75ed320ff937),
                (1275, 0x82a142638ff804d3),
                (1275, 0xc0d2682a75fbc936),
                (1275, 0x288f1c6eeb9eb500),
                (1275, 0xcfc73526b384e77d),
                (1275, 0x51c3706911cb470a),
                (1275, 0x9f592da5e7973989),
                (1275, 0x82c36f939ff9abff),
                (1275, 0xd1538d2404d138a7),
                (1275, 0x303795a5cc395b90),
                (1275, 0x268e0e16cadef0a7),
                (1275, 0x181d1b396b0b2a90),
            ],
        ),
        (
            "silence",
            3,
            0xdeadbeef,
            &[
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
                (2, 0x0a99c807b6f645b0),
            ],
        ),
    ];

    #[test]
    #[ignore]
    fn write_conformance_stream() {
        use crate::ogg::PageWriter;
        let mut enc = Encoder::new();
        let mut pw = PageWriter::new(0x1234_5678);
        let mut out = Vec::new();
        out.extend_from_slice(&pw.head());
        out.extend_from_slice(&pw.tags());
        let mut pkt = vec![0u8; 1276];
        let mut pcm = vec![0f32; 1920];
        for frame in 0..250 {
            for i in 0..960 {
                let t = (frame * 960 + i) as f64 / 48000.0;
                let click = if (frame * 960 + i) % 48000 == 0 {
                    0.5
                } else {
                    0.0
                };
                pcm[2 * i] = (0.25 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as f32;
                pcm[2 * i + 1] =
                    (0.25 * (2.0 * std::f64::consts::PI * 660.0 * t).sin() + click) as f32;
            }
            let n = enc.encode(&pcm, &mut pkt).expect("encode failed");
            let granule = crate::ogg::PRESKIP + ((frame + 1) * 960) as u64;
            out.extend_from_slice(&pw.audio(granule, &pkt[..n]));
        }
        std::fs::write("target/conformance.opus", out).unwrap();
    }

    #[test]
    #[ignore]
    fn bench_encoder() {
        use std::time::Instant;
        const SECONDS: usize = 60;
        let frames = SECONDS * 50;
        let mut pcm = vec![0f32; 1920];
        let mut out = vec![0u8; 1276];
        let mut best = f64::INFINITY;
        for run in 0..5 {
            let mut enc = Encoder::new();
            let mut warm = 0x1234_5678u32 ^ run as u32;
            for _ in 0..50 {
                generate(0, 0, &mut pcm, &mut warm);
                enc.encode(&pcm, &mut out).expect("warmup encode failed");
            }
            let mut s = 0x9E37_79B9u32;
            let start = Instant::now();
            for frame in 0..frames {
                generate(0, frame, &mut pcm, &mut s);
                enc.encode(&pcm, &mut out).expect("encode failed");
            }
            let secs = start.elapsed().as_secs_f64();
            let ms_per_s = secs * 1000.0 / SECONDS as f64;
            println!(
                "run {run}: {ms_per_s:.3} ms per second of audio ({:.1}x realtime)",
                SECONDS as f64 / secs
            );
            best = best.min(ms_per_s);
        }
        println!(
            "best: {best:.3} ms per second of audio ({:.1}x realtime)",
            1000.0 / best
        );
    }

    #[test]
    fn frames_match_reference_encoder() {
        for &(tag, kind, seed, frames) in CASES {
            let mut enc = Encoder::new();
            let mut s = seed;
            let mut pcm = vec![0f32; 1920];
            let mut out = vec![0u8; 1276];
            for (i, &(want_len, want_hash)) in frames.iter().enumerate() {
                generate(kind, i, &mut pcm, &mut s);
                let n = enc.encode(&pcm, &mut out).expect("encode failed");
                assert_eq!(n, want_len + 1, "{tag} frame {i} length");
                let got = fnv(&out[1..n]);
                assert_eq!(got, want_hash, "{tag} frame {i} bytes");
            }
        }
    }
}
