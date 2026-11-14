// alloc.rs — CELT bit allocation for the pinned 48 kHz / 20 ms / stereo mode.
//
// The decoder derives the allocation from the transmitted energies and the
// packet size, so this arithmetic is bitstream-normative: encoder and decoder
// must land on identical pulse counts. Direct port of `celt/rate.c` against the
// static `mode48000_960_120` tables.

#[cfg(test)]
use crate::range_coder::RangeDecoder;
use crate::range_coder::RangeEncoder;

pub const NB_EBANDS: usize = 21;

pub const EBANDS: [i32; 22] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

pub const LOG_N: [i32; 21] = [
    0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 16, 16, 16, 21, 21, 24, 29, 34, 36,
];

pub const BAND_ALLOCATION: [[u8; 21]; 11] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        90, 80, 75, 69, 63, 56, 49, 40, 34, 29, 20, 18, 10, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        110, 100, 90, 84, 78, 71, 65, 58, 51, 45, 39, 32, 26, 20, 12, 0, 0, 0, 0, 0, 0,
    ],
    [
        118, 110, 103, 93, 86, 80, 75, 70, 65, 59, 53, 47, 40, 31, 23, 15, 4, 0, 0, 0, 0,
    ],
    [
        126, 119, 112, 104, 95, 89, 83, 78, 72, 66, 60, 54, 47, 39, 32, 25, 17, 12, 1, 0, 0,
    ],
    [
        134, 127, 120, 114, 103, 97, 91, 85, 78, 72, 66, 60, 54, 47, 41, 35, 29, 23, 16, 10, 1,
    ],
    [
        144, 137, 130, 124, 113, 107, 101, 95, 88, 82, 76, 70, 64, 57, 51, 45, 39, 33, 26, 15, 1,
    ],
    [
        152, 145, 138, 132, 123, 117, 111, 105, 98, 92, 86, 80, 74, 67, 61, 55, 49, 43, 36, 20, 1,
    ],
    [
        162, 155, 148, 142, 133, 127, 121, 115, 108, 102, 96, 90, 84, 77, 71, 65, 59, 53, 46, 30, 1,
    ],
    [
        172, 165, 158, 152, 143, 137, 131, 125, 118, 112, 106, 100, 94, 87, 81, 75, 69, 63, 56, 45,
        20,
    ],
    [
        200, 200, 200, 200, 200, 200, 200, 200, 198, 193, 188, 183, 178, 173, 168, 163, 158, 153,
        148, 129, 104,
    ],
];

pub const CACHE_INDEX: [i16; 105] = [
    -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, 0, 0, 41, 41, 41, 82, 82, 123, 164, 200, 222, 0, 0, 0, 0,
    0, 0, 0, 0, 41, 41, 41, 41, 123, 123, 123, 164, 164, 240, 266, 283, 295, 41, 41, 41, 41, 41,
    41, 41, 41, 123, 123, 123, 123, 240, 240, 240, 266, 266, 305, 318, 328, 336, 123, 123, 123,
    123, 123, 123, 123, 123, 240, 240, 240, 240, 305, 305, 305, 318, 318, 343, 351, 358, 364, 240,
    240, 240, 240, 240, 240, 240, 240, 305, 305, 305, 305, 343, 343, 343, 351, 351, 370, 376, 382,
    387,
];

pub const CACHE_BITS: [u8; 392] = [
    40, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 40, 15, 23, 28, 31, 34, 36, 38, 39, 41, 42, 43, 44, 45, 46, 47,
    47, 49, 50, 51, 52, 53, 54, 55, 55, 57, 58, 59, 60, 61, 62, 63, 63, 65, 66, 67, 68, 69, 70, 71,
    71, 40, 20, 33, 41, 48, 53, 57, 61, 64, 66, 69, 71, 73, 75, 76, 78, 80, 82, 85, 87, 89, 91, 92,
    94, 96, 98, 101, 103, 105, 107, 108, 110, 112, 114, 117, 119, 121, 123, 124, 126, 128, 40, 23,
    39, 51, 60, 67, 73, 79, 83, 87, 91, 94, 97, 100, 102, 105, 107, 111, 115, 118, 121, 124, 126,
    129, 131, 135, 139, 142, 145, 148, 150, 153, 155, 159, 163, 166, 169, 172, 174, 177, 179, 35,
    28, 49, 65, 78, 89, 99, 107, 114, 120, 126, 132, 136, 141, 145, 149, 153, 159, 165, 171, 176,
    180, 185, 189, 192, 199, 205, 211, 216, 220, 225, 229, 232, 239, 245, 251, 21, 33, 58, 79, 97,
    112, 125, 137, 148, 157, 166, 174, 182, 189, 195, 201, 207, 217, 227, 235, 243, 251, 17, 35,
    63, 86, 106, 123, 139, 152, 165, 177, 187, 197, 206, 214, 222, 230, 237, 250, 25, 31, 55, 75,
    91, 105, 117, 128, 138, 146, 154, 161, 168, 174, 180, 185, 190, 200, 208, 215, 222, 229, 235,
    240, 245, 255, 16, 36, 65, 89, 110, 128, 144, 159, 173, 185, 196, 207, 217, 226, 234, 242, 250,
    11, 41, 74, 103, 128, 151, 172, 191, 209, 225, 241, 255, 9, 43, 79, 110, 138, 163, 186, 207,
    227, 246, 12, 39, 71, 99, 123, 144, 164, 182, 198, 214, 228, 241, 253, 9, 44, 81, 113, 142,
    168, 192, 214, 235, 255, 7, 49, 90, 127, 160, 191, 220, 247, 6, 51, 95, 134, 170, 203, 234, 7,
    47, 87, 123, 155, 184, 212, 237, 6, 52, 97, 137, 174, 208, 240, 5, 57, 106, 151, 192, 231, 5,
    59, 111, 158, 202, 243, 5, 55, 103, 147, 187, 224, 5, 60, 113, 161, 206, 248, 4, 65, 122, 175,
    224, 4, 67, 127, 182, 234,
];

pub const CACHE_CAPS: [u8; 168] = [
    224, 224, 224, 224, 224, 224, 224, 224, 160, 160, 160, 160, 185, 185, 185, 178, 178, 168, 134,
    61, 37, 224, 224, 224, 224, 224, 224, 224, 224, 240, 240, 240, 240, 207, 207, 207, 198, 198,
    183, 144, 66, 40, 160, 160, 160, 160, 160, 160, 160, 160, 185, 185, 185, 185, 193, 193, 193,
    183, 183, 172, 138, 64, 38, 240, 240, 240, 240, 240, 240, 240, 240, 207, 207, 207, 207, 204,
    204, 204, 193, 193, 180, 143, 66, 40, 185, 185, 185, 185, 185, 185, 185, 185, 193, 193, 193,
    193, 193, 193, 193, 183, 183, 172, 138, 65, 39, 207, 207, 207, 207, 207, 207, 207, 207, 204,
    204, 204, 204, 201, 201, 201, 188, 188, 176, 141, 66, 40, 193, 193, 193, 193, 193, 193, 193,
    193, 193, 193, 193, 193, 194, 194, 194, 184, 184, 173, 139, 65, 39, 204, 204, 204, 204, 204,
    204, 204, 204, 201, 201, 201, 201, 198, 198, 198, 187, 187, 175, 140, 66, 40,
];

const BITRES: i32 = 3;
const ALLOC_STEPS: i32 = 6;
pub(crate) const MAX_FINE_BITS: i32 = 8;
const FINE_OFFSET: i32 = 21;
const LOG_MAX_PSEUDO: i32 = 6;
const NB_ALLOC_VECTORS: i32 = 11;

const LOG2_FRAC_TABLE: [i32; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

pub fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

pub fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    let base = CACHE_INDEX[((lm + 1) as usize) * NB_EBANDS + band] as usize;
    let cache = &CACHE_BITS[base..];
    let mut lo = 0i32;
    let mut hi = cache[0] as i32;
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        if cache[mid as usize] as i32 >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let lo_cost = if lo == 0 {
        -1
    } else {
        cache[lo as usize] as i32
    };
    if bits - lo_cost <= cache[hi as usize] as i32 - bits {
        lo
    } else {
        hi
    }
}

pub fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    let base = CACHE_INDEX[((lm + 1) as usize) * NB_EBANDS + band] as usize;
    if pulses == 0 {
        0
    } else {
        CACHE_BITS[base + pulses as usize] as i32 + 1
    }
}

/// Per-band rate cap, `celt.c::init_caps`.
pub fn init_caps(cap: &mut [i32], lm: i32, c: i32) {
    for j in 0..NB_EBANDS {
        let n = (EBANDS[j + 1] - EBANDS[j]) << lm;
        let idx = NB_EBANDS * (2 * lm + c - 1) as usize + j;
        cap[j] = (CACHE_CAPS[idx] as i32 + 64) * c * n >> 2;
    }
}

/// The two things the allocator needs from the entropy coder. Only the skip,
/// intensity and dual-stereo decisions go through it; everything else is
/// derived identically on both sides.
pub(crate) trait AllocEc {
    fn code_skip<F: FnOnce() -> bool>(&mut self, decide: F) -> bool;
    fn code_intensity(&mut self, value: i32, coded_bands: i32) -> i32;
    fn code_dual_stereo(&mut self, value: bool) -> bool;
}

impl AllocEc for RangeEncoder<'_> {
    fn code_skip<F: FnOnce() -> bool>(&mut self, decide: F) -> bool {
        let d = decide();
        self.encode_bit_logp(d, 1);
        d
    }

    fn code_intensity(&mut self, value: i32, coded_bands: i32) -> i32 {
        let v = value.min(coded_bands);
        self.encode_uint(v as u32, (coded_bands + 1) as u32);
        v
    }

    fn code_dual_stereo(&mut self, value: bool) -> bool {
        self.encode_bit_logp(value, 1);
        value
    }
}

#[cfg(test)]
impl AllocEc for RangeDecoder<'_> {
    fn code_skip<F: FnOnce() -> bool>(&mut self, _decide: F) -> bool {
        self.decode_bit_logp(1)
    }

    fn code_intensity(&mut self, _value: i32, coded_bands: i32) -> i32 {
        self.decode_uint((coded_bands + 1) as u32) as i32
    }

    fn code_dual_stereo(&mut self, _value: bool) -> bool {
        self.decode_bit_logp(1)
    }
}

struct Interp<'a, EC> {
    ec: &'a mut EC,
    skip_start: usize,
    bits1: &'a [i32; NB_EBANDS],
    bits2: &'a [i32; NB_EBANDS],
    thresh: &'a [i32; NB_EBANDS],
    cap: &'a [i32],
    total_in: i32,
    balance_out: &'a mut i32,
    skip_rsv: i32,
    intensity: &'a mut i32,
    intensity_rsv: i32,
    dual_stereo: &'a mut bool,
    dual_stereo_rsv: i32,
    bits: &'a mut [i32],
    ebits: &'a mut [i32],
    fine_priority: &'a mut [i32],
    c: i32,
    lm: i32,
    prev: i32,
    signal_bandwidth: i32,
}

fn interp_bits2pulses<EC: AllocEc>(p: Interp<'_, EC>) -> i32 {
    let Interp {
        ec,
        skip_start,
        bits1,
        bits2,
        thresh,
        cap,
        total_in,
        balance_out,
        skip_rsv,
        intensity,
        mut intensity_rsv,
        dual_stereo,
        mut dual_stereo_rsv,
        bits,
        ebits,
        fine_priority,
        c,
        lm,
        prev,
        signal_bandwidth,
    } = p;
    let alloc_floor = c << BITRES;
    let stereo = (c > 1) as i32;
    let log_m = lm << BITRES;
    let mut total = total_in;

    let mut lo = 0i32;
    let mut hi = 1 << ALLOC_STEPS;
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        for j in (0..NB_EBANDS).rev() {
            let tmp = bits1[j] + (mid * bits2[j] >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    let mut psum = 0i32;
    let mut done = false;
    for j in (0..NB_EBANDS).rev() {
        let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    let mut coded_bands = NB_EBANDS as i32;
    loop {
        let j = (coded_bands - 1) as usize;
        if j <= skip_start {
            total += skip_rsv;
            break;
        }
        let mut left = total - psum;
        let percoeff = left / (EBANDS[coded_bands as usize]);
        left -= (EBANDS[coded_bands as usize]) * percoeff;
        let rem = (left - (EBANDS[j])).max(0);
        let band_width = EBANDS[coded_bands as usize] - EBANDS[j];
        let mut band_bits = bits[j] + percoeff * band_width + rem;
        if band_bits >= thresh[j].max(alloc_floor + (1 << BITRES)) {
            let skip = ec.code_skip(|| {
                let depth_threshold = if coded_bands > 17 {
                    if (j as i32) < prev { 7 } else { 9 }
                } else {
                    0
                };
                coded_bands <= 2
                    || (band_bits > (depth_threshold * band_width << lm << BITRES) >> 4
                        && (j as i32) <= signal_bandwidth)
            });
            if skip {
                break;
            }
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        psum -= bits[j] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = LOG2_FRAC_TABLE[j];
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            psum += alloc_floor;
            bits[j] = alloc_floor;
        } else {
            bits[j] = 0;
        }
        coded_bands -= 1;
    }

    debug_assert!(coded_bands > 0);

    if intensity_rsv > 0 {
        *intensity = ec.code_intensity(*intensity, coded_bands);
    } else {
        *intensity = 0;
    }
    if *intensity <= 0 {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    if dual_stereo_rsv > 0 {
        *dual_stereo = ec.code_dual_stereo(*dual_stereo);
    } else {
        *dual_stereo = false;
    }

    let mut left = total - psum;
    let percoeff = left / (EBANDS[coded_bands as usize]);
    left -= (EBANDS[coded_bands as usize]) * percoeff;
    for j in 0..coded_bands as usize {
        bits[j] += percoeff * (EBANDS[j + 1] - EBANDS[j]);
    }
    for j in 0..coded_bands as usize {
        let tmp = left.min(EBANDS[j + 1] - EBANDS[j]);
        bits[j] += tmp;
        left -= tmp;
    }

    let mut balance = 0i32;
    let mut j = 0;
    while j < coded_bands as usize {
        let n0 = EBANDS[j + 1] - EBANDS[j];
        let n = n0 << lm;
        let bit = bits[j] + balance;
        let mut excess;
        if n > 1 {
            excess = (bit - cap[j]).max(0);
            bits[j] = bit - excess;
            let den = c * n
                + if c == 2 && n > 2 && !*dual_stereo && (j as i32) < *intensity {
                    1
                } else {
                    0
                };
            let nclogn = den * (LOG_N[j] + log_m);
            let mut offset = (nclogn >> 1) - den * FINE_OFFSET;
            if n == 2 {
                offset += den << BITRES >> 2;
            }
            if bits[j] + offset < den * 2 << BITRES {
                offset += nclogn >> 2;
            } else if bits[j] + offset < den * 3 << BITRES {
                offset += nclogn >> 3;
            }
            let num = (bits[j] + offset + (den << (BITRES - 1))).max(0);
            ebits[j] = (num / den) >> BITRES;
            if c * ebits[j] > (bits[j] >> BITRES) {
                ebits[j] = bits[j] >> stereo >> BITRES;
            }
            ebits[j] = ebits[j].min(MAX_FINE_BITS);
            fine_priority[j] = if ebits[j] * (den << BITRES) >= bits[j] + offset {
                1
            } else {
                0
            };
            bits[j] -= c * ebits[j] << BITRES;
        } else {
            excess = (bit - (c << BITRES)).max(0);
            bits[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
        }
        if excess > 0 {
            let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = extra_fine * c << BITRES;
            fine_priority[j] = if extra_bits >= excess - balance { 1 } else { 0 };
            excess -= extra_bits;
        }
        balance = excess;
        j += 1;
    }
    *balance_out = balance;

    for j in j..NB_EBANDS {
        ebits[j] = bits[j] >> stereo >> BITRES;
        bits[j] = 0;
        fine_priority[j] = if ebits[j] < 1 { 1 } else { 0 };
    }

    coded_bands
}

pub(crate) struct Allocation<'a, EC> {
    pub ec: &'a mut EC,
    pub offsets: &'a [i32],
    pub cap: &'a [i32],
    pub alloc_trim: i32,
    pub intensity: &'a mut i32,
    pub dual_stereo: &'a mut bool,
    pub total_in: i32,
    pub balance: &'a mut i32,
    pub pulses: &'a mut [i32],
    pub ebits: &'a mut [i32],
    pub fine_priority: &'a mut [i32],
    pub c: i32,
    pub lm: i32,
    pub prev: i32,
    pub signal_bandwidth: i32,
}

pub(crate) fn compute_allocation<EC: AllocEc>(a: Allocation<'_, EC>) -> i32 {
    let Allocation {
        ec,
        offsets,
        cap,
        alloc_trim,
        intensity,
        dual_stereo,
        total_in,
        balance,
        pulses,
        ebits,
        fine_priority,
        c,
        lm,
        prev,
        signal_bandwidth,
    } = a;
    let mut total = total_in.max(0);
    let mut skip_start = 0;
    let skip_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
    total -= skip_rsv;
    let mut intensity_rsv = 0;
    let mut dual_stereo_rsv = 0;
    if c == 2 {
        intensity_rsv = LOG2_FRAC_TABLE[NB_EBANDS];
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= 1 << BITRES { 1 << BITRES } else { 0 };
            total -= dual_stereo_rsv;
        }
    }

    let mut bits1 = [0i32; NB_EBANDS];
    let mut bits2 = [0i32; NB_EBANDS];
    let mut thresh = [0i32; NB_EBANDS];
    let mut trim_offset = [0i32; NB_EBANDS];

    for j in 0..NB_EBANDS {
        thresh[j] = (c << BITRES).max((3 * (EBANDS[j + 1] - EBANDS[j]) << lm << BITRES) >> 4);
        trim_offset[j] = c
            * (EBANDS[j + 1] - EBANDS[j])
            * (alloc_trim - 5 - lm)
            * (NB_EBANDS - j - 1) as i32
            * (1 << (lm + BITRES))
            >> 6;
        if (EBANDS[j + 1] - EBANDS[j]) << lm == 1 {
            trim_offset[j] -= c << BITRES;
        }
    }

    let mut lo = 1;
    let mut hi = NB_ALLOC_VECTORS - 1;
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        let mut done = false;
        let mut psum = 0i32;
        for j in (0..NB_EBANDS).rev() {
            let n = EBANDS[j + 1] - EBANDS[j];
            let mut bitsj = c * n * (BAND_ALLOCATION[mid as usize][j] as i32) << lm >> 2;
            if bitsj > 0 {
                bitsj = (bitsj + trim_offset[j]).max(0);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= c << BITRES {
                psum += c << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
    }
    hi = lo;
    lo -= 1;

    for j in 0..NB_EBANDS {
        let n = EBANDS[j + 1] - EBANDS[j];
        let mut bits1j = c * n * (BAND_ALLOCATION[lo as usize][j] as i32) << lm >> 2;
        let mut bits2j = if hi >= NB_ALLOC_VECTORS {
            cap[j]
        } else {
            c * n * (BAND_ALLOCATION[hi as usize][j] as i32) << lm >> 2
        };
        if bits1j > 0 {
            bits1j = (bits1j + trim_offset[j]).max(0);
        }
        if bits2j > 0 {
            bits2j = (bits2j + trim_offset[j]).max(0);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j;
        }
        bits2j = (bits2j - bits1j).max(0);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }

    interp_bits2pulses(Interp {
        ec,
        skip_start,
        bits1: &bits1,
        bits2: &bits2,
        thresh: &thresh,
        cap,
        total_in: total,
        balance_out: balance,
        skip_rsv,
        intensity,
        intensity_rsv,
        dual_stereo,
        dual_stereo_rsv,
        bits: pulses,
        ebits,
        fine_priority,
        c,
        lm,
        prev,
        signal_bandwidth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fnv1a(data: &[u8]) -> u64 {
        let mut h: u64 = 14695981039346656037;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    struct Case {
        total: i32,
        trim: i32,
        prev: i32,
        off_mode: bool,
        inten: i32,
        dual: bool,
        coded: i32,
        balance: i32,
        intensity: i32,
        dual_out: bool,
        hash: u64,
        pulses: [i32; 21],
        ebits: [i32; 21],
        fine: [i32; 21],
    }

    const CASES: &[Case] = &[
        Case {
            total: 50000,
            trim: 5,
            prev: 21,
            off_mode: false,
            inten: 14,
            dual: false,
            coded: 21,
            balance: 0,
            intensity: 14,
            dual_out: false,
            hash: 0x3a08_4351_918f_e3f7,
            pulses: [
                597, 597, 597, 599, 597, 598, 599, 601, 1254, 1222, 1192, 1163, 2331, 2268, 2209,
                3241, 3168, 4127, 6019, 7941, 7652,
            ],
            ebits: [
                4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 5, 4, 4, 5, 4, 4,
            ],
            fine: [
                1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 1,
            ],
        },
        Case {
            total: 20000,
            trim: 5,
            prev: 21,
            off_mode: false,
            inten: 14,
            dual: false,
            coded: 21,
            balance: 0,
            intensity: 14,
            dual_out: false,
            hash: 0x3a08_4351_918f_e3f7,
            pulses: [
                496, 474, 452, 434, 404, 402, 368, 350, 705, 669, 633, 597, 1170, 1082, 1010, 1431,
                1323, 1635, 2203, 2425, 693,
            ],
            ebits: [
                3, 3, 3, 3, 3, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
            ],
            fine: [
                0, 0, 1, 1, 1, 0, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 1, 1, 0,
            ],
        },
        Case {
            total: 5000,
            trim: 5,
            prev: 21,
            off_mode: false,
            inten: 14,
            dual: false,
            coded: 16,
            balance: 0,
            intensity: 14,
            dual_out: false,
            hash: 0xe48b_a698_1705_e3cf,
            pulses: [
                329, 304, 282, 250, 226, 224, 209, 195, 360, 324, 286, 266, 445, 330, 230, 211, 0,
                0, 0, 0, 0,
            ],
            ebits: [
                2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 1, 2, 2, 2, 2, 1, 1, 0, 0, 0,
            ],
            fine: [
                0, 0, 1, 1, 1, 0, 0, 0, 1, 1, 1, 0, 0, 1, 1, 1, 0, 0, 1, 1, 1,
            ],
        },
        Case {
            total: 40000,
            trim: 5,
            prev: 21,
            off_mode: true,
            inten: 10,
            dual: true,
            coded: 21,
            balance: 0,
            intensity: 10,
            dual_out: true,
            hash: 0xf569_a7a6_f7b8_e39b,
            pulses: [
                551, 540, 529, 960, 525, 512, 503, 495, 994, 977, 1880, 911, 1805, 1729, 1663,
                2421, 2322, 2981, 4269, 7500, 4649,
            ],
            ebits: [
                4, 4, 4, 7, 3, 3, 3, 3, 4, 3, 7, 3, 3, 3, 3, 3, 3, 3, 3, 4, 4,
            ],
            fine: [
                1, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            ],
        },
    ];

    fn run_case(case: &Case) -> (i32, i32, i32, bool, [i32; 21], [i32; 21], [i32; 21], u64) {
        let mut cap = [0i32; NB_EBANDS];
        init_caps(&mut cap, 3, 2);
        let mut offsets = [0i32; NB_EBANDS];
        if case.off_mode {
            offsets[3] = 500;
            offsets[10] = 1000;
            offsets[19] = 2000;
        }
        let mut buf = vec![0u8; 4096];
        let mut pulses = [0i32; NB_EBANDS];
        let mut ebits = [0i32; NB_EBANDS];
        let mut fine = [0i32; NB_EBANDS];
        let mut balance = 0;
        let mut intensity = case.inten;
        let mut dual = case.dual;
        let coded;
        {
            let mut enc = RangeEncoder::new(&mut buf);
            coded = compute_allocation(Allocation {
                ec: &mut enc,
                offsets: &offsets,
                cap: &cap,
                alloc_trim: case.trim,
                intensity: &mut intensity,
                dual_stereo: &mut dual,
                total_in: case.total,
                balance: &mut balance,
                pulses: &mut pulses,
                ebits: &mut ebits,
                fine_priority: &mut fine,
                c: 2,
                lm: 3,
                prev: case.prev,
                signal_bandwidth: 20,
            });
            enc.done();
            assert!(!enc.error());
        }
        (
            coded,
            balance,
            intensity,
            dual,
            pulses,
            ebits,
            fine,
            fnv1a(&buf),
        )
    }

    #[test]
    fn matches_reference_allocation() {
        for case in CASES {
            let (coded, balance, intensity, dual, pulses, ebits, fine, hash) = run_case(case);
            assert_eq!(coded, case.coded);
            assert_eq!(balance, case.balance);
            assert_eq!(intensity, case.intensity);
            assert_eq!(dual, case.dual_out);
            assert_eq!(pulses, case.pulses, "pulses");
            assert_eq!(ebits, case.ebits, "ebits");
            assert_eq!(fine, case.fine, "fine_priority");
            assert_eq!(hash, case.hash, "range-coder stream");
        }
    }

    #[test]
    fn encoder_and_decoder_agree() {
        for case in CASES {
            let mut cap = [0i32; NB_EBANDS];
            init_caps(&mut cap, 3, 2);
            let mut offsets = [0i32; NB_EBANDS];
            if case.off_mode {
                offsets[3] = 500;
                offsets[10] = 1000;
                offsets[19] = 2000;
            }
            let mut buf = vec![0u8; 4096];
            let mut ep = [0i32; NB_EBANDS];
            let mut ee = [0i32; NB_EBANDS];
            let mut ef = [0i32; NB_EBANDS];
            let mut eb = 0;
            let mut ei = case.inten;
            let mut ed = case.dual;
            let ec;
            {
                let mut enc = RangeEncoder::new(&mut buf);
                ec = compute_allocation(Allocation {
                    ec: &mut enc,
                    offsets: &offsets,
                    cap: &cap,
                    alloc_trim: case.trim,
                    intensity: &mut ei,
                    dual_stereo: &mut ed,
                    total_in: case.total,
                    balance: &mut eb,
                    pulses: &mut ep,
                    ebits: &mut ee,
                    fine_priority: &mut ef,
                    c: 2,
                    lm: 3,
                    prev: case.prev,
                    signal_bandwidth: 20,
                });
                enc.done();
            }

            let mut dp = [0i32; NB_EBANDS];
            let mut de = [0i32; NB_EBANDS];
            let mut df = [0i32; NB_EBANDS];
            let mut db = 0;
            let mut di = 0;
            let mut dd = false;
            let mut dec = RangeDecoder::new(&buf);
            let dc = compute_allocation(Allocation {
                ec: &mut dec,
                offsets: &offsets,
                cap: &cap,
                alloc_trim: case.trim,
                intensity: &mut di,
                dual_stereo: &mut dd,
                total_in: case.total,
                balance: &mut db,
                pulses: &mut dp,
                ebits: &mut de,
                fine_priority: &mut df,
                c: 2,
                lm: 3,
                prev: case.prev,
                signal_bandwidth: 20,
            });
            assert_eq!(dc, ec);
            assert_eq!(db, eb);
            assert_eq!(di, ei);
            assert_eq!(dd, ed);
            assert_eq!(dp, ep);
            assert_eq!(de, ee);
            assert_eq!(df, ef);
            assert!(!dec.error());
        }
    }

    #[test]
    fn pulse_helpers_are_sane() {
        assert_eq!(get_pulses(0), 0);
        assert_eq!(get_pulses(7), 7);
        assert_eq!(get_pulses(8), 8);
        assert_eq!(get_pulses(9), 9);
        assert_eq!(get_pulses(16), 16);
        for band in 0..NB_EBANDS {
            let base = CACHE_INDEX[4 * NB_EBANDS + band] as usize;
            let max = CACHE_BITS[base] as i32;
            for pulses in 1..=max {
                assert!(pulses2bits(band, 3, pulses) > 0);
            }
        }
    }

    #[test]
    fn caps_are_in_range() {
        let mut cap = [0i32; NB_EBANDS];
        init_caps(&mut cap, 3, 2);
        for (j, &c) in cap.iter().enumerate() {
            assert!(c > 0, "cap[{j}] = {c}");
        }
    }

    #[test]
    fn pulse_cache_is_monotone() {
        for band in 0..NB_EBANDS {
            let mut prev = -1;
            for bits in 0..512 {
                let p = bits2pulses(band, 3, bits);
                assert!(p >= prev, "band {band} bits {bits}");
                prev = p;
            }
        }
    }
}
