// energy.rs — CELT band energies and coarse/fine energy quantisation.
//
// Port of the float build of `celt/bands.c` (compute_band_energies) and
// `celt/quant_bands.c`. The bitstream semantics are fixed; the float encoder
// path is what libopus uses unless built with --enable-fixed-point.

use crate::alloc::{EBANDS, MAX_FINE_BITS, NB_EBANDS};
#[cfg(test)]
use crate::range_coder::RangeDecoder;
use crate::range_coder::RangeEncoder;

pub(crate) const E_MEANS: [f32; 25] = [
    6.437500, 6.250000, 5.750000, 5.312500, 5.062500, 4.812500, 4.500000, 4.375000, 4.875000,
    4.687500, 4.562500, 4.437500, 4.875000, 4.625000, 4.312500, 4.500000, 4.375000, 4.625000,
    4.750000, 4.437500, 3.750000, 3.750000, 3.750000, 3.750000, 3.750000,
];

const PRED_COEF: [f32; 4] = [
    29440.0 / 32768.0,
    26112.0 / 32768.0,
    21248.0 / 32768.0,
    16384.0 / 32768.0,
];
const BETA_COEF: [f32; 4] = [
    30147.0 / 32768.0,
    22282.0 / 32768.0,
    12124.0 / 32768.0,
    6554.0 / 32768.0,
];
const BETA_INTRA: f32 = 4915.0 / 32768.0;
const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

const E_PROB_MODEL: [[[u8; 42]; 2]; 4] = [
    [
        [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79,
            92, 78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10,
            177, 11,
        ],
        [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70,
            96, 74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43,
            78, 50,
        ],
    ],
    [
        [
            83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74, 93, 74, 109, 40, 114, 36, 117,
            34, 117, 34, 143, 17, 145, 18, 146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177,
            9,
        ],
        [
            23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91, 73, 91, 78, 89, 86, 80, 92,
            66, 93, 64, 102, 59, 103, 60, 104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77,
            45,
        ],
    ],
    [
        [
            61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38, 112, 38, 124, 26, 132, 27,
            136, 19, 140, 20, 155, 14, 159, 16, 158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9,
            159, 10,
        ],
        [
            21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73, 87, 72, 92, 75, 98, 72, 105,
            58, 107, 54, 115, 52, 114, 55, 112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77,
            42,
        ],
    ],
    [
        [
            42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36, 119, 33, 127, 33, 134,
            34, 139, 21, 147, 23, 152, 20, 158, 25, 154, 26, 166, 21, 173, 16, 184, 13, 184, 10,
            150, 13, 139, 15,
        ],
        [
            22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72, 96, 67, 101, 73, 107, 72,
            113, 55, 118, 52, 125, 52, 118, 52, 117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97,
            33, 77, 40,
        ],
    ],
];

/// `celt_log2` in the float build: double `log`, multiplied and narrowed.
fn celt_log2(x: f32) -> f32 {
    (1.442695040888963387f64 * (x as f64).ln()) as f32
}

/// `celt_sqrt` in the float build: double `sqrt`, narrowed.
fn celt_sqrt(x: f32) -> f32 {
    (x as f64).sqrt() as f32
}

/// Band amplitude (square root of energy) for the stereo pair, from the
/// channel-major MDCT bins. The two channel sums run in one pass: each keeps
/// its own order (proofs/Energy.lean: `fused_sum`), so the values are identical
/// to two separate loops, but the dependency chains overlap and the index
/// arithmetic is shared.
pub fn compute_band_energies(x: &[f32], band_e: &mut [f32], lm: i32) {
    let n = 120usize << lm;
    for i in 0..NB_EBANDS {
        let start = (EBANDS[i] << lm) as usize;
        let stop = (EBANDS[i + 1] << lm) as usize;
        let mut sum_l = 1e-27f32;
        let mut sum_r = 1e-27f32;
        for (&l, &r) in x[start..stop].iter().zip(&x[n + start..n + stop]) {
            sum_l += l * l;
            sum_r += r * r;
        }
        band_e[i] = celt_sqrt(sum_l);
        band_e[NB_EBANDS + i] = celt_sqrt(sum_r);
    }
}

/// Convert band amplitudes to log energies, relative to the per-band mean.
pub fn amp2_log2(band_e: &[f32], band_log_e: &mut [f32], channels: usize) {
    for c in 0..channels {
        for i in 0..NB_EBANDS {
            band_log_e[i + c * NB_EBANDS] = celt_log2(band_e[i + c * NB_EBANDS]) - E_MEANS[i];
        }
    }
}

const LAPLACE_MINP: u32 = 1;
const LAPLACE_NMIN: u32 = 16;

fn laplace_freq1(fs0: u32, decay: i32) -> u32 {
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
    (ft * (16384 - decay) as u32) >> 15
}

fn laplace_encode(enc: &mut RangeEncoder, value: &mut i32, fs_in: u32, decay: i32) {
    let mut val = *value;
    let mut fs = fs_in;
    let mut fl = 0u32;
    if val != 0 {
        let s = if val < 0 { -1i32 } else { 0 };
        val = (val + s) ^ s;
        fl = fs;
        fs = laplace_freq1(fs, decay);
        let mut i = 1i32;
        while fs > 0 && i < val {
            fs *= 2;
            fl += fs + 2 * LAPLACE_MINP;
            fs = ((fs as i32) * decay >> 15) as u32;
            i += 1;
        }
        if fs == 0 {
            let mut ndi_max = ((32768 - fl + LAPLACE_MINP - 1) >> LAPLACE_LOG_MINP) as i32;
            ndi_max = (ndi_max - s) >> 1;
            let di = (val - i).min(ndi_max - 1);
            fl += ((2 * di + 1 + s) as u32) * LAPLACE_MINP;
            fs = LAPLACE_MINP.min(32768 - fl);
            *value = (i + di + s) ^ s;
        } else {
            fs += LAPLACE_MINP;
            fl += fs & (!s as u32);
        }
        debug_assert!(fl + fs <= 32768);
        debug_assert!(fs > 0);
    }
    enc.encode_bin(fl, fl + fs, 15);
}

const LAPLACE_LOG_MINP: u32 = 0;

#[cfg(test)]
fn laplace_decode(dec: &mut RangeDecoder, fs_in: u32, decay: i32) -> i32 {
    let mut val = 0i32;
    let mut fl = 0u32;
    let mut fs = fs_in;
    let fm = dec.decode_bin(15);
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = laplace_freq1(fs, decay) + LAPLACE_MINP;
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * LAPLACE_MINP) as i32 * decay) >> 15) as u32;
            fs += LAPLACE_MINP;
            val += 1;
        }
        if fs <= LAPLACE_MINP {
            let di = ((fm - fl) >> (LAPLACE_LOG_MINP + 1)) as i32;
            val += di;
            fl += 2 * di as u32 * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }
    debug_assert!(fl < 32768);
    debug_assert!(fs > 0);
    debug_assert!(fl <= fm);
    debug_assert!(fm < (fl + fs).min(32768));
    dec.update(fl, (fl + fs).min(32768), 32768);
    val
}

fn loss_distortion(e_bands: &[f32], old: &[f32], c: usize) -> f32 {
    let mut dist = 0.0f32;
    for ch in 0..c {
        for i in 0..NB_EBANDS {
            let d = e_bands[i + ch * NB_EBANDS] - old[i + ch * NB_EBANDS];
            dist += d * d;
        }
    }
    dist.min(200.0)
}

struct CoarseImpl<'a> {
    e_bands: &'a [f32],
    budget: i32,
    tell_in: i32,
    prob_model: &'a [u8; 42],
    c: usize,
    lm: i32,
    intra: bool,
    max_decay: f32,
}

fn quant_coarse_energy_impl(
    ec: &mut RangeEncoder,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    p: CoarseImpl<'_>,
) -> i32 {
    let CoarseImpl {
        e_bands,
        budget,
        tell_in,
        prob_model,
        c,
        lm,
        intra,
        max_decay,
    } = p;
    let mut prev = [0.0f32; 2];
    let (coef, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm as usize], BETA_COEF[lm as usize])
    };
    let mut badness = 0i32;

    if tell_in + 3 <= budget {
        ec.encode_bit_logp(intra, 3);
    }

    for i in 0..NB_EBANDS {
        for (ch, prev_ch) in prev.iter_mut().enumerate().take(c) {
            let idx = i + ch * NB_EBANDS;
            let x = e_bands[idx];
            let old_e = old_e_bands[idx].max(-9.0);
            let f = x - coef * old_e - *prev_ch;
            let mut qi = (0.5 + f).floor() as i32;
            let decay_bound = old_e_bands[idx].max(-28.0) - max_decay;
            if qi < 0 && x < decay_bound {
                qi += (decay_bound - x) as i32;
                if qi > 0 {
                    qi = 0;
                }
            }
            let qi0 = qi;
            let tell = ec.tell();
            let bits_left = budget - tell - 3 * c as i32 * (NB_EBANDS - i) as i32;
            if i != 0 && bits_left < 30 {
                if bits_left < 24 {
                    qi = qi.min(1);
                }
                if bits_left < 16 {
                    qi = qi.max(-1);
                }
            }

            if budget - tell >= 15 {
                let pi = 2 * i;
                laplace_encode(
                    ec,
                    &mut qi,
                    (prob_model[pi] as u32) << 7,
                    (prob_model[pi + 1] as i32) << 6,
                );
            } else if budget - tell >= 2 {
                qi = qi.clamp(-1, 1);
                let sym = (2 * qi) ^ (-((qi < 0) as i32));
                ec.encode_icdf(sym as usize, &SMALL_ENERGY_ICDF, 2);
            } else if budget - tell >= 1 {
                qi = qi.min(0);
                ec.encode_bit_logp(qi < 0, 1);
            } else {
                qi = -1;
            }
            error[idx] = f - qi as f32;
            badness += (qi0 - qi).abs();
            let tmp = coef * old_e + *prev_ch + qi as f32;
            old_e_bands[idx] = tmp;
            *prev_ch = *prev_ch + qi as f32 - beta * qi as f32;
        }
    }
    badness
}

/// Coarse energy coding with the reference two-pass intra/inter search.
/// Returns the chosen `intra` flag (also written to the stream).
pub(crate) struct CoarseRequest<'a> {
    pub e_bands: &'a [f32],
    pub budget: i32,
    pub c: usize,
    pub lm: i32,
    pub nb_available_bytes: i32,
}

pub fn quant_coarse_energy(
    ec: &mut RangeEncoder,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    delayed_intra: &mut f32,
    req: CoarseRequest<'_>,
) -> bool {
    let CoarseRequest {
        e_bands,
        budget,
        c,
        lm,
        nb_available_bytes,
    } = req;
    debug_assert!(c <= 2);
    // proofs/Energy.lean: every caller pins `force_intra = false`,
    // `two_pass = true` and `loss_rate = 0`.
    let mut two_pass = true;
    let mut intra = false;
    let new_distortion = loss_distortion(e_bands, old_e_bands, c);

    let tell = ec.tell();
    if tell + 3 > budget {
        two_pass = false;
    }

    let max_decay = 16.0f32.min(0.125 * nb_available_bytes as f32);

    let start_snap = ec.snapshot();
    let mut old_intra = [0.0f32; 2 * NB_EBANDS];
    old_intra[..c * NB_EBANDS].copy_from_slice(old_e_bands);
    let mut error_intra = [0.0f32; 2 * NB_EBANDS];
    let mut badness1 = 0;
    let mut intra_snap = None;
    let mut tell_intra = 0u32;

    if two_pass {
        badness1 = quant_coarse_energy_impl(
            ec,
            &mut old_intra,
            &mut error_intra,
            CoarseImpl {
                e_bands,
                budget,
                tell_in: tell,
                prob_model: &E_PROB_MODEL[lm as usize][1],
                c,
                lm,
                intra: true,
                max_decay,
            },
        );
        intra_snap = Some(ec.snapshot());
        tell_intra = ec.tell_frac();
    }

    ec.restore(&start_snap);
    let badness2 = quant_coarse_energy_impl(
        ec,
        old_e_bands,
        error,
        CoarseImpl {
            e_bands,
            budget,
            tell_in: tell,
            prob_model: &E_PROB_MODEL[lm as usize][0],
            c,
            lm,
            intra: false,
            max_decay,
        },
    );
    if two_pass
        && (badness1 < badness2
            || (badness1 == badness2 && ec.tell_frac() as i32 > tell_intra as i32))
    {
        ec.restore(intra_snap.as_ref().unwrap());
        old_e_bands.copy_from_slice(&old_intra[..c * NB_EBANDS]);
        error.copy_from_slice(&error_intra[..c * NB_EBANDS]);
        intra = true;
    }

    if intra {
        *delayed_intra = new_distortion;
    } else {
        let p = PRED_COEF[lm as usize];
        *delayed_intra = p * p * *delayed_intra + new_distortion;
    }
    intra
}

pub fn quant_fine_energy(
    ec: &mut RangeEncoder,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    fine_quant: &[i32],
    c: usize,
) {
    for i in 0..NB_EBANDS {
        let fq = fine_quant[i];
        if fq <= 0 {
            continue;
        }
        let frac = 1i32 << fq;
        for ch in 0..c {
            let idx = i + ch * NB_EBANDS;
            let mut q2 = ((error[idx] + 0.5) * frac as f32).floor() as i32;
            if q2 > frac - 1 {
                q2 = frac - 1;
            }
            if q2 < 0 {
                q2 = 0;
            }
            ec.encode_bits(q2 as u32, fq as u32);
            let offset = (q2 as f32 + 0.5) * ((1i32 << (14 - fq)) as f32) * (1.0 / 16384.0) - 0.5;
            old_e_bands[idx] += offset;
            error[idx] -= offset;
        }
    }
}

pub fn quant_energy_finalise(
    ec: &mut RangeEncoder,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    bits_left_in: i32,
    c: usize,
) {
    let mut bits_left = bits_left_in;
    for prio in 0..2 {
        let mut i = 0;
        while i < NB_EBANDS && bits_left >= c as i32 {
            if fine_quant[i] >= MAX_FINE_BITS || fine_priority[i] != prio {
                i += 1;
                continue;
            }
            for ch in 0..c {
                let idx = i + ch * NB_EBANDS;
                let q2 = if error[idx] < 0.0 { 0 } else { 1 };
                ec.encode_bits(q2 as u32, 1);
                let offset = (q2 as f32 - 0.5)
                    * ((1i32 << (14 - fine_quant[i] - 1)) as f32)
                    * (1.0 / 16384.0);
                old_e_bands[idx] += offset;
                error[idx] -= offset;
                bits_left -= 1;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
pub fn unquant_coarse_energy(
    dec: &mut RangeDecoder,
    old_e_bands: &mut [f32],
    intra: bool,
    c: usize,
    lm: i32,
) {
    let prob_model = &E_PROB_MODEL[lm as usize][intra as usize];
    let budget = dec.storage() as i32 * 8;
    let mut prev = [0.0f32; 2];
    let (coef, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm as usize], BETA_COEF[lm as usize])
    };

    for i in 0..NB_EBANDS {
        for (ch, prev_ch) in prev.iter_mut().enumerate().take(c) {
            let idx = i + ch * NB_EBANDS;
            let tell = dec.tell();
            let qi = if budget - tell >= 15 {
                let pi = 2 * i;
                laplace_decode(
                    dec,
                    (prob_model[pi] as u32) << 7,
                    (prob_model[pi + 1] as i32) << 6,
                )
            } else if budget - tell >= 2 {
                let s = dec.decode_icdf(&SMALL_ENERGY_ICDF, 2) as i32;
                (s >> 1) ^ -(s & 1)
            } else if budget - tell >= 1 {
                -(dec.decode_bit_logp(1) as i32)
            } else {
                -1
            };
            old_e_bands[idx] = old_e_bands[idx].max(-9.0);
            let tmp = coef * old_e_bands[idx] + *prev_ch + qi as f32;
            old_e_bands[idx] = tmp;
            *prev_ch = *prev_ch + qi as f32 - beta * qi as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_energy_and_log_match_the_formula() {
        let lm = 3;
        let n = 960;
        let mut x = vec![0.0f32; 2 * n];
        for i in 0..NB_EBANDS {
            let start = (EBANDS[i] << lm) as usize;
            let stop = (EBANDS[i + 1] << lm) as usize;
            for j in start..stop {
                x[j] = 0.5;
                x[n + j] = 0.25;
            }
        }
        let mut e = vec![0.0f32; 2 * NB_EBANDS];
        compute_band_energies(&x, &mut e, lm);
        for i in 0..NB_EBANDS {
            let width = ((EBANDS[i + 1] - EBANDS[i]) << lm) as f32;
            let want = (1e-27f32 + width * 0.25).sqrt();
            assert!((e[i] - want).abs() < 1e-6, "band {i}");
            let want2 = (1e-27f32 + width * 0.0625).sqrt();
            assert!((e[NB_EBANDS + i] - want2).abs() < 1e-6, "band {i} right");
        }
        let mut le = vec![0.0f32; 2 * NB_EBANDS];
        amp2_log2(&e, &mut le, 2);
        for i in 0..NB_EBANDS {
            assert!((le[i] - (celt_log2(e[i]) - E_MEANS[i])).abs() < 1e-6);
        }
    }

    #[test]
    fn laplace_round_trip_over_the_model_table() {
        let mut buf = vec![0u8; 16384];
        let mut expected = Vec::new();
        {
            let mut enc = RangeEncoder::new(&mut buf);
            for lm in 0..4 {
                for it in 0..2 {
                    let model = &E_PROB_MODEL[lm][it];
                    for pi in (0..42).step_by(2) {
                        let fs = (model[pi] as u32) << 7;
                        let decay = (model[pi + 1] as i32) << 6;
                        for v in -6..=6 {
                            let mut v = v;
                            laplace_encode(&mut enc, &mut v, fs, decay);
                            expected.push((fs, decay, v));
                        }
                    }
                }
            }
            enc.done();
            assert!(!enc.error());
        }
        let mut dec = RangeDecoder::new(&buf);
        for (fs, decay, want) in expected {
            assert_eq!(laplace_decode(&mut dec, fs, decay), want);
        }
    }

    fn fnv1a(data: &[u8]) -> u64 {
        let mut h: u64 = 14695981039346656037;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    const REF_OLD: [u32; 42] = [
        0xc07f0000, 0xc05dcd00, 0xc03dcd00, 0xc03e9a00, 0xc01e9a00, 0xc0209a00, 0xbffd3400,
        0xbfbace00, 0xbfc06800, 0xbf78d000, 0xbf806800, 0xbef1a000, 0x3d3fc000, 0x3b480000,
        0x3f08c800, 0x3f00c800, 0x3f846400, 0x3fc6ca00, 0x3fc2ca00, 0x40016500, 0x40009800,
        0xc03f0000, 0xc0409a00, 0xc01e9a00, 0xbff93400, 0xbffece00, 0xbfbace00, 0xbfc06800,
        0xbf78d000, 0xbef1a000, 0xbef80800, 0x3c7f0000, 0x3b480000, 0x3f08c800, 0x3f846400,
        0x3f82ca00, 0x3fc2ca00, 0x3fbeca00, 0x40016500, 0x40229800, 0x401fcb00, 0x4041cb00,
    ];

    const REF_ERR: [u32; 42] = [
        0xbc800000, 0xbc6b5200, 0x3bb8b800, 0xbc422900, 0x3c058500, 0x3c1a0000, 0xbaf28800,
        0xba1a3000, 0x3c57d800, 0x3afc2800, 0x3b500000, 0xbc045200, 0xbbdf4800, 0x3be3b000,
        0xbb8cf400, 0xbb480000, 0xbc6a5000, 0xbc55a200, 0xbc412800, 0x3c068600, 0xbc180000,
        0x3c6b8500, 0x3c1a0000, 0xbaf29000, 0xbc56a300, 0x3a2d8000, 0xbc2d7b00, 0x3b500400,
        0xbc045100, 0x3c435c00, 0xbbb65000, 0x3c6c8400, 0xbb480000, 0xbc6a5200, 0x3bbab800,
        0xbc412800, 0x3c068400, 0x3c1b0000, 0xbaea8000, 0xba0a4000, 0x3c58d800, 0x3b021000,
    ];

    /// Same inputs as `target/ref/check_energy.c`, which runs the reference
    /// float encoder. Compares the resulting bitstream and both energy states.
    #[test]
    fn matches_reference_energy_coder() {
        let c = 2;
        let lm = 3;
        let mut band_log_e = vec![0.0f32; c * NB_EBANDS];
        for i in 0..NB_EBANDS {
            band_log_e[i] = -4.0 + 0.3 * i as f32 + ((i * 7) % 5) as f32 * 0.11;
            band_log_e[NB_EBANDS + i] =
                -4.0 + 0.3 * i as f32 + 0.7 + ((i * 7 + 3) % 5) as f32 * 0.11;
        }
        let mut old = vec![-28.0f32; c * NB_EBANDS];
        let mut error = vec![0.0f32; c * NB_EBANDS];
        let mut fine_quant = [0i32; NB_EBANDS];
        let mut fine_priority = [0i32; NB_EBANDS];
        for i in 0..NB_EBANDS {
            fine_quant[i] = 4;
            fine_priority[i] = (i % 2) as i32;
        }
        let mut buf = vec![0u8; 800];
        let mut delayed = 1.0f32;
        let intra;
        {
            let mut enc = RangeEncoder::new(&mut buf);
            intra = quant_coarse_energy(
                &mut enc,
                &mut old,
                &mut error,
                &mut delayed,
                CoarseRequest {
                    e_bands: &band_log_e,
                    budget: 6400,
                    c,
                    lm,
                    nb_available_bytes: 800,
                },
            );
            quant_fine_energy(&mut enc, &mut old, &mut error, &fine_quant, c);
            quant_energy_finalise(
                &mut enc,
                &mut old,
                &mut error,
                &fine_quant,
                &fine_priority,
                100,
                c,
            );
            enc.done();
            assert!(!enc.error());
        }
        assert!(!intra);
        assert_eq!(delayed.to_bits(), 0x4348_4000);
        assert_eq!(fnv1a(&buf), 0x0cbd_f2e4_aa5c_1791);
        for i in 0..c * NB_EBANDS {
            assert_eq!(old[i].to_bits(), REF_OLD[i], "old band {i}");
        }
        for i in 0..c * NB_EBANDS {
            assert_eq!(error[i].to_bits(), REF_ERR[i], "error band {i}");
        }
    }

    #[test]
    fn coarse_and_fine_energy_round_trip() {
        let c = 2;
        let lm = 3;
        let mut band_log_e = vec![0.0f32; c * NB_EBANDS];
        for ch in 0..c {
            for i in 0..NB_EBANDS {
                band_log_e[i + ch * NB_EBANDS] =
                    -4.0 + 0.3 * i as f32 + 0.7 * ch as f32 + ((i * 7 + ch * 3) % 5) as f32 * 0.11;
            }
        }
        let mut enc_old = vec![-28.0f32; c * NB_EBANDS];
        let mut error = vec![0.0f32; c * NB_EBANDS];
        let mut fine_quant = [0i32; NB_EBANDS];
        let mut fine_priority = [0i32; NB_EBANDS];
        for i in 0..NB_EBANDS {
            fine_quant[i] = 4;
            fine_priority[i] = (i % 2) as i32;
        }
        let mut buf = vec![0u8; 800];
        let mut delayed = 1.0f32;
        let intra;
        {
            let mut enc = RangeEncoder::new(&mut buf);
            intra = quant_coarse_energy(
                &mut enc,
                &mut enc_old,
                &mut error,
                &mut delayed,
                CoarseRequest {
                    e_bands: &band_log_e,
                    budget: 6400,
                    c,
                    lm,
                    nb_available_bytes: 800,
                },
            );
            quant_fine_energy(&mut enc, &mut enc_old, &mut error, &fine_quant, c);
            quant_energy_finalise(
                &mut enc,
                &mut enc_old,
                &mut error,
                &fine_quant,
                &fine_priority,
                100,
                c,
            );
            enc.done();
            assert!(!enc.error());
        }

        let mut dec_old = vec![-28.0f32; c * NB_EBANDS];
        let mut dec = RangeDecoder::new(&buf);
        let budget = dec.storage() as i32 * 8;
        let dec_intra = if dec.tell() + 3 <= budget {
            dec.decode_bit_logp(3)
        } else {
            false
        };
        assert_eq!(dec_intra, intra);
        unquant_coarse_energy(&mut dec, &mut dec_old, dec_intra, c, lm);
        unquant_fine_energy(&mut dec, &mut dec_old, &fine_quant, c);
        unquant_energy_finalise(&mut dec, &mut dec_old, &fine_quant, &fine_priority, 100, c);
        for i in 0..c * NB_EBANDS {
            assert_eq!(dec_old[i].to_bits(), enc_old[i].to_bits(), "band {i}");
        }
    }
}

#[cfg(test)]
pub fn unquant_fine_energy(
    dec: &mut RangeDecoder,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    c: usize,
) {
    for i in 0..NB_EBANDS {
        if fine_quant[i] <= 0 {
            continue;
        }
        for ch in 0..c {
            let idx = i + ch * NB_EBANDS;
            let q2 = dec.decode_bits(fine_quant[i] as u32) as i32;
            let offset =
                (q2 as f32 + 0.5) * ((1i32 << (14 - fine_quant[i])) as f32) * (1.0 / 16384.0) - 0.5;
            old_e_bands[idx] += offset;
        }
    }
}

#[cfg(test)]
pub fn unquant_energy_finalise(
    dec: &mut RangeDecoder,
    old_e_bands: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    bits_left_in: i32,
    c: usize,
) {
    let mut bits_left = bits_left_in;
    for prio in 0..2 {
        let mut i = 0;
        while i < NB_EBANDS && bits_left >= c as i32 {
            if fine_quant[i] >= MAX_FINE_BITS || fine_priority[i] != prio {
                i += 1;
                continue;
            }
            for ch in 0..c {
                let idx = i + ch * NB_EBANDS;
                let q2 = dec.decode_bits(1) as i32;
                let offset = (q2 as f32 - 0.5)
                    * ((1i32 << (14 - fine_quant[i] - 1)) as f32)
                    * (1.0 / 16384.0);
                old_e_bands[idx] += offset;
                bits_left -= 1;
            }
            i += 1;
        }
    }
}
