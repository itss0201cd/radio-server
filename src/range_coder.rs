// range_coder.rs — RFC 6716 §4.1 range coder.
//
// Same state machine as the reference entropy coder: 32-bit range, byte
// renormalisation, carry propagation through `rem`/`ext`, and raw bits packed
// backwards from the end of the packet. The decoder is test-only: it exists to
// check the encoder's output, not to ship.

const EC_SYM_BITS: u32 = 8;
const EC_CODE_BITS: u32 = 32;
const EC_SYM_MAX: u32 = (1 << EC_SYM_BITS) - 1;
const EC_CODE_SHIFT: u32 = EC_CODE_BITS - EC_SYM_BITS - 1;
const EC_CODE_TOP: u32 = 1 << (EC_CODE_BITS - 1);
const EC_CODE_BOT: u32 = EC_CODE_TOP >> EC_SYM_BITS;
#[cfg(test)]
const EC_CODE_EXTRA: u32 = (EC_CODE_BITS - 2) % EC_SYM_BITS + 1;
const EC_UINT_BITS: u32 = 8;
const EC_WINDOW_SIZE: u32 = 32;
const BITRES: u32 = 3;

/// `EC_ILOG`: one plus the index of the highest set bit, and 0 for zero.
fn ilog(v: u32) -> u32 {
    if v == 0 { 0 } else { 32 - v.leading_zeros() }
}

pub struct RangeEncoder<'a> {
    buf: &'a mut [u8],
    storage: usize,
    offs: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
}

impl<'a> RangeEncoder<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            storage: buf.len(),
            buf,
            offs: 0,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: EC_CODE_BITS as i32 + 1,
            rng: EC_CODE_TOP,
            val: 0,
            ext: 0,
            rem: -1,
            error: false,
        }
    }

    fn write_byte(&mut self, value: u8) {
        if self.offs + self.end_offs >= self.storage {
            self.error = true;
            return;
        }
        self.buf[self.offs] = value;
        self.offs += 1;
    }

    fn write_byte_at_end(&mut self, value: u8) {
        if self.offs + self.end_offs >= self.storage {
            self.error = true;
            return;
        }
        self.end_offs += 1;
        self.buf[self.storage - self.end_offs] = value;
    }

    fn carry_out(&mut self, c: i32) {
        if c != EC_SYM_MAX as i32 {
            let carry = (c as u32) >> EC_SYM_BITS;
            if self.rem >= 0 {
                self.write_byte((self.rem as u32 + carry) as u8);
            }
            if self.ext > 0 {
                let sym = ((EC_SYM_MAX + carry) & EC_SYM_MAX) as u8;
                loop {
                    self.write_byte(sym);
                    self.ext -= 1;
                    if self.ext == 0 {
                        break;
                    }
                }
            }
            self.rem = c & EC_SYM_MAX as i32;
        } else {
            self.ext += 1;
        }
    }

    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            let c = (self.val >> EC_CODE_SHIFT) as i32;
            self.carry_out(c);
            self.val = (self.val << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            self.rng <<= EC_SYM_BITS;
            self.nbits_total += EC_SYM_BITS as i32;
        }
    }

    /// Encodes a symbol occupying `[fl, fh)` out of `ft`.
    pub fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
        debug_assert!(ft > 0 && fl < fh && fh <= ft);
        let r = self.rng / ft;
        if fl > 0 {
            self.val = self
                .val
                .wrapping_add(self.rng.wrapping_sub(r.wrapping_mul(ft - fl)));
            self.rng = r * (fh - fl);
        } else {
            self.rng = self.rng.wrapping_sub(r * (ft - fh));
        }
        self.normalize();
    }

    /// `encode` with `ft == 1 << bits`.
    pub fn encode_bin(&mut self, fl: u32, fh: u32, bits: u32) {
        debug_assert!((1..32).contains(&bits) && fl < fh && fh <= 1 << bits);
        let r = self.rng >> bits;
        if fl > 0 {
            let ft = 1u32 << bits;
            self.val = self
                .val
                .wrapping_add(self.rng.wrapping_sub(r.wrapping_mul(ft - fl)));
            self.rng = r * (fh - fl);
        } else {
            let ft = 1u32 << bits;
            self.rng = self.rng.wrapping_sub(r * (ft - fh));
        }
        self.normalize();
    }

    /// Encodes a one with probability `1 / (1 << logp)`.
    pub fn encode_bit_logp(&mut self, bit: bool, logp: u32) {
        debug_assert!(logp < 32);
        let r = self.rng;
        let l = self.val;
        let s = r >> logp;
        let r_hi = r - s;
        if bit {
            self.val = l.wrapping_add(r_hi);
            self.rng = s;
        } else {
            self.rng = r_hi;
        }
        self.normalize();
    }

    /// Encodes symbol `s` against an inverse CDF of precision `1 << ftb`.
    pub fn encode_icdf(&mut self, s: usize, icdf: &[u8], ftb: u32) {
        debug_assert!(s < icdf.len() && ftb < 32);
        let r = self.rng >> ftb;
        if s > 0 {
            let prev = icdf[s - 1] as u32;
            let cur = icdf[s] as u32;
            self.val = self
                .val
                .wrapping_add(self.rng.wrapping_sub(r.wrapping_mul(prev)));
            self.rng = r * (prev - cur);
        } else {
            self.rng = self.rng.wrapping_sub(r * icdf[0] as u32);
        }
        self.normalize();
    }

    /// Encodes a raw integer in `[0, ft)`.
    pub fn encode_uint(&mut self, fl: u32, ft: u32) {
        debug_assert!(ft > 1 && fl < ft);
        let last = ft - 1;
        let ftb = ilog(last);
        if ftb > EC_UINT_BITS {
            let ftb = ftb - EC_UINT_BITS;
            let f = (last >> ftb) + 1;
            let l = fl >> ftb;
            self.encode(l, l + 1, f);
            self.encode_bits(fl & ((1u32 << ftb) - 1), ftb);
        } else {
            self.encode(fl, fl + 1, last + 1);
        }
    }

    /// Encodes `bits` raw bits, least-significant first, from the end of the packet.
    pub fn encode_bits(&mut self, fl: u32, bits: u32) {
        debug_assert!((1..=25).contains(&bits));
        debug_assert!(fl < (1u32 << bits));
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        if used + bits as i32 > EC_WINDOW_SIZE as i32 {
            loop {
                self.write_byte_at_end((window & EC_SYM_MAX) as u8);
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
                if used < EC_SYM_BITS as i32 {
                    break;
                }
            }
        }
        window |= fl << used;
        used += bits as i32;
        self.end_window = window;
        self.nend_bits = used;
        self.nbits_total += bits as i32;
    }

    /// Compacts the raw bits at the end into a smaller packet.
    pub fn shrink(&mut self, size: usize) {
        debug_assert!(self.offs + self.end_offs <= size && size <= self.storage);
        let src = self.storage - self.end_offs;
        let dst = size - self.end_offs;
        self.buf.copy_within(src..src + self.end_offs, dst);
        self.storage = size;
    }

    /// Flushes the range coder; the packet is the whole (possibly shrunk) buffer.
    pub fn done(&mut self) {
        let mut l = EC_CODE_BITS as i32 - ilog(self.rng) as i32;
        let mut msk = (EC_CODE_TOP - 1) >> l as u32;
        let mut end = self.val.wrapping_add(msk) & !msk;
        if (end | msk) >= self.val.wrapping_add(self.rng) {
            l += 1;
            msk >>= 1;
            end = self.val.wrapping_add(msk) & !msk;
        }
        while l > 0 {
            let c = (end >> EC_CODE_SHIFT) as i32;
            self.carry_out(c);
            end = (end << EC_SYM_BITS) & (EC_CODE_TOP - 1);
            l -= EC_SYM_BITS as i32;
        }
        if self.rem >= 0 || self.ext > 0 {
            self.carry_out(0);
        }
        let mut window = self.end_window;
        let mut used = self.nend_bits;
        while used >= EC_SYM_BITS as i32 {
            self.write_byte_at_end((window & EC_SYM_MAX) as u8);
            window >>= EC_SYM_BITS;
            used -= EC_SYM_BITS as i32;
        }
        if !self.error {
            self.buf[self.offs..self.storage - self.end_offs].fill(0);
            if used > 0 {
                if self.end_offs >= self.storage {
                    self.error = true;
                } else {
                    let pad_bits = -l;
                    if self.offs + self.end_offs >= self.storage && pad_bits < used {
                        window &= (1u32 << pad_bits) - 1;
                        self.error = true;
                    }
                    self.buf[self.storage - self.end_offs - 1] |= window as u8;
                }
            }
        }
    }

    pub fn error(&self) -> bool {
        self.error
    }

    #[cfg(test)]
    /// Bytes written from the front; the total packet size is the buffer size.
    pub fn range_bytes(&self) -> usize {
        self.offs
    }

    /// The range coder's internal state, which seeds the next frame's folding PRNG.
    pub fn rng(&self) -> u32 {
        self.rng
    }

    pub fn storage(&self) -> usize {
        self.storage
    }

    /// Set the reported bit position, matching the reference silence shortcut.
    pub fn force_tell(&mut self, bits: i32) {
        self.nbits_total = bits + ilog(self.rng) as i32;
    }

    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng) as i32
    }

    /// Bit usage in units of `1 / (1 << BITRES)` bits.
    pub fn tell_frac(&self) -> u32 {
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = (self.nbits_total as u32) << BITRES;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12) - 8;
        b += (r > CORRECTION[b as usize]) as u32;
        nbits - ((l << 3) + b)
    }
}

/// Point-in-time copy of an encoder, including the bytes written so far. The
/// coarse-energy two-pass search runs intra and inter from the same state.
#[derive(Clone)]
pub struct RangeEncoderSnapshot {
    storage: usize,
    offs: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
    bytes: Vec<u8>,
}

impl<'a> RangeEncoder<'a> {
    pub fn snapshot(&self) -> RangeEncoderSnapshot {
        RangeEncoderSnapshot {
            storage: self.storage,
            offs: self.offs,
            end_offs: self.end_offs,
            end_window: self.end_window,
            nend_bits: self.nend_bits,
            nbits_total: self.nbits_total,
            rng: self.rng,
            val: self.val,
            ext: self.ext,
            rem: self.rem,
            error: self.error,
            bytes: self.buf[..self.storage].to_vec(),
        }
    }

    pub fn restore(&mut self, s: &RangeEncoderSnapshot) {
        assert!(s.storage <= self.buf.len());
        self.buf[..s.storage].copy_from_slice(&s.bytes);
        self.storage = s.storage;
        self.offs = s.offs;
        self.end_offs = s.end_offs;
        self.end_window = s.end_window;
        self.nend_bits = s.nend_bits;
        self.nbits_total = s.nbits_total;
        self.rng = s.rng;
        self.val = s.val;
        self.ext = s.ext;
        self.rem = s.rem;
        self.error = s.error;
    }
}

#[cfg(test)]
pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    storage: usize,
    offs: usize,
    end_offs: usize,
    end_window: u32,
    nend_bits: i32,
    nbits_total: i32,
    rng: u32,
    val: u32,
    ext: u32,
    rem: i32,
    error: bool,
}

#[cfg(test)]
impl<'a> RangeDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        let mut d = Self {
            buf,
            storage: buf.len(),
            offs: 0,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            nbits_total: EC_CODE_BITS as i32 + 1
                - (((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS) as i32,
            rng: 1 << EC_CODE_EXTRA,
            val: 0,
            ext: 0,
            rem: 0,
            error: false,
        };
        d.rem = d.read_byte();
        d.val = d.rng - 1 - ((d.rem as u32) >> (EC_SYM_BITS - EC_CODE_EXTRA));
        d.normalize();
        d
    }

    fn read_byte(&mut self) -> i32 {
        if self.offs < self.storage {
            let b = self.buf[self.offs];
            self.offs += 1;
            b as i32
        } else {
            0
        }
    }

    fn read_byte_from_end(&mut self) -> i32 {
        if self.end_offs < self.storage {
            self.end_offs += 1;
            self.buf[self.storage - self.end_offs] as i32
        } else {
            0
        }
    }

    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS as i32;
            self.rng <<= EC_SYM_BITS;
            let mut sym = self.rem;
            self.rem = self.read_byte();
            sym = ((sym << EC_SYM_BITS) | self.rem) >> (EC_SYM_BITS - EC_CODE_EXTRA);
            let inv = !(sym as u32) & EC_SYM_MAX;
            self.val = ((self.val << EC_SYM_BITS).wrapping_add(inv)) & (EC_CODE_TOP - 1);
        }
    }

    /// Returns the cumulative frequency of the next symbol; it lies in `[fl, fh)`.
    pub fn decode(&mut self, ft: u32) -> u32 {
        debug_assert!(ft > 0 && self.rng >= ft);
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        ft - (s + 1).min(ft)
    }

    /// `decode` with `ft == 1 << bits`.
    pub fn decode_bin(&mut self, bits: u32) -> u32 {
        debug_assert!((1..32).contains(&bits));
        self.ext = self.rng >> bits;
        let s = self.val / self.ext;
        (1u32 << bits) - (s + 1).min(1u32 << bits)
    }

    pub fn update(&mut self, fl: u32, fh: u32, ft: u32) {
        debug_assert!(fl < fh && fh <= ft);
        let s = self.ext * (ft - fh);
        self.val = self.val.wrapping_sub(s);
        self.rng = if fl > 0 {
            self.ext * (fh - fl)
        } else {
            self.rng.wrapping_sub(s)
        };
        self.normalize();
    }

    pub fn decode_bit_logp(&mut self, logp: u32) -> bool {
        debug_assert!(logp < 32);
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let bit = d < s;
        if !bit {
            self.val = d - s;
        }
        self.rng = if bit { s } else { r - s };
        self.normalize();
        bit
    }

    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> usize {
        debug_assert!(ftb < 32);
        let d = self.val;
        let r = self.rng >> ftb;
        let mut s = self.rng;
        let mut ret = -1i32;
        loop {
            let t = s;
            ret += 1;
            s = r * icdf[ret as usize] as u32;
            if d >= s {
                self.val = d - s;
                self.rng = t - s;
                break;
            }
        }
        self.normalize();
        ret as usize
    }

    pub fn decode_uint(&mut self, ft: u32) -> u32 {
        debug_assert!(ft > 1);
        let last = ft - 1;
        let ftb = ilog(last);
        if ftb > EC_UINT_BITS {
            let ftb = ftb - EC_UINT_BITS;
            let f = (last >> ftb) + 1;
            let s = self.decode(f);
            self.update(s, s + 1, f);
            let t = (s << ftb) | self.decode_bits(ftb);
            if t <= last {
                t
            } else {
                self.error = true;
                last
            }
        } else {
            let s = self.decode(ft);
            self.update(s, s + 1, ft);
            s
        }
    }

    pub fn decode_bits(&mut self, bits: u32) -> u32 {
        debug_assert!((1..=25).contains(&bits));
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if available < bits as i32 {
            loop {
                window |= (self.read_byte_from_end() as u32) << available;
                available += EC_SYM_BITS as i32;
                if available > EC_WINDOW_SIZE as i32 - EC_SYM_BITS as i32 {
                    break;
                }
            }
        }
        let ret = window & ((1u32 << bits) - 1);
        window >>= bits;
        available -= bits as i32;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits as i32;
        ret
    }

    pub fn error(&self) -> bool {
        self.error
    }

    pub fn storage(&self) -> usize {
        self.storage
    }

    pub fn tell(&self) -> i32 {
        self.nbits_total - ilog(self.rng) as i32
    }

    pub fn tell_frac(&self) -> u32 {
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = (self.nbits_total as u32) << BITRES;
        let l = ilog(self.rng);
        let r = self.rng >> (l - 16);
        let mut b = (r >> 12) - 8;
        b += (r > CORRECTION[b as usize]) as u32;
        nbits - ((l << 3) + b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u32) -> u32 {
            (self.next() % n as u64) as u32
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Freq { fl: u32, fh: u32, ft: u32 },
        Bin { fl: u32, fh: u32, bits: u32 },
        Logp { bit: bool, logp: u32 },
        Icdf { s: usize, icdf: Vec<u8>, ftb: u32 },
        Uint { v: u32, ft: u32 },
        Bits { v: u32, bits: u32 },
    }

    fn random_icdf(rng: &mut Rng, ftb: u32) -> Vec<u8> {
        let ft = 1u32 << ftb;
        let n = 1 + rng.below(6.min(ft)) as usize;
        let mut w = vec![1u32; n];
        let mut left = ft - n as u32;
        while left > 0 {
            let i = rng.below(n as u32) as usize;
            let add = 1 + rng.below(left);
            w[i] += add;
            left -= add;
        }
        let mut cum = 0u32;
        (0..n)
            .map(|s| {
                cum += w[s];
                (ft - cum) as u8
            })
            .collect()
    }

    fn gen_op(rng: &mut Rng) -> Op {
        match rng.below(6) {
            0 => {
                let ft = 2 + rng.below(4096);
                let fl = rng.below(ft);
                let fh = fl + 1 + rng.below(ft - fl);
                Op::Freq { fl, fh, ft }
            }
            1 => {
                let bits = 1 + rng.below(15);
                let ft = 1u32 << bits;
                let fl = rng.below(ft);
                let fh = fl + 1 + rng.below(ft - fl);
                Op::Bin { fl, fh, bits }
            }
            2 => Op::Logp {
                bit: rng.below(2) == 1,
                logp: 1 + rng.below(15),
            },
            3 => {
                let ftb = 1 + rng.below(8);
                let icdf = random_icdf(rng, ftb);
                let s = rng.below(icdf.len() as u32) as usize;
                Op::Icdf { s, icdf, ftb }
            }
            4 => {
                let ft = if rng.below(2) == 0 {
                    2 + rng.below(300)
                } else {
                    2 + rng.below(1_000_000_000)
                };
                Op::Uint {
                    v: rng.below(ft),
                    ft,
                }
            }
            _ => {
                let bits = 1 + rng.below(25);
                Op::Bits {
                    v: rng.below(1u32 << bits),
                    bits,
                }
            }
        }
    }

    fn encode_op(enc: &mut RangeEncoder, op: &Op) {
        match op {
            Op::Freq { fl, fh, ft } => enc.encode(*fl, *fh, *ft),
            Op::Bin { fl, fh, bits } => enc.encode_bin(*fl, *fh, *bits),
            Op::Logp { bit, logp } => enc.encode_bit_logp(*bit, *logp),
            Op::Icdf { s, icdf, ftb } => enc.encode_icdf(*s, icdf, *ftb),
            Op::Uint { v, ft } => enc.encode_uint(*v, *ft),
            Op::Bits { v, bits } => enc.encode_bits(*v, *bits),
        }
    }

    fn decode_op(dec: &mut RangeDecoder, op: &Op) {
        match op {
            Op::Freq { fl, fh, ft } => {
                let fs = dec.decode(*ft);
                assert!(
                    *fl <= fs && fs < *fh,
                    "freq {fs} outside [{fl},{fh}) of {ft}"
                );
                dec.update(*fl, *fh, *ft);
            }
            Op::Bin { fl, fh, bits } => {
                let fs = dec.decode_bin(*bits);
                assert!(
                    *fl <= fs && fs < *fh,
                    "bin {fs} outside [{fl},{fh}) of 2^{bits}"
                );
                dec.update(*fl, *fh, 1u32 << bits);
            }
            Op::Logp { bit, logp } => {
                assert_eq!(dec.decode_bit_logp(*logp), *bit);
            }
            Op::Icdf { s, icdf, ftb } => {
                assert_eq!(dec.decode_icdf(icdf, *ftb), *s);
            }
            Op::Uint { v, ft } => {
                assert_eq!(dec.decode_uint(*ft), *v);
            }
            Op::Bits { v, bits } => {
                assert_eq!(dec.decode_bits(*bits), *v);
            }
        }
    }

    #[test]
    fn mixed_round_trip_matches_tell_at_every_step() {
        for seed in 1..=64u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let ops: Vec<Op> = (0..128).map(|_| gen_op(&mut rng)).collect();

            let mut buf = vec![0u8; 4096];
            let mut tells = Vec::with_capacity(ops.len());
            {
                let mut enc = RangeEncoder::new(&mut buf);
                assert_eq!(enc.tell(), 1);
                for op in &ops {
                    encode_op(&mut enc, op);
                    tells.push((enc.tell(), enc.tell_frac()));
                }
                enc.done();
                assert!(!enc.error(), "seed {seed} overflowed the packet");
            }

            let mut dec = RangeDecoder::new(&buf);
            assert_eq!(dec.tell(), 1);
            for (op, &(tell, tell_frac)) in ops.iter().zip(&tells) {
                decode_op(&mut dec, op);
                assert_eq!(dec.tell(), tell, "seed {seed}");
                assert_eq!(dec.tell_frac(), tell_frac, "seed {seed}");
            }
            assert!(!dec.error());
        }
    }

    #[test]
    fn uint_round_trip_exhaustive_for_small_ranges() {
        let mut buf = vec![0u8; 8192];
        for ft in 2..=300u32 {
            let ops: Vec<Op> = (0..ft).map(|v| Op::Uint { v, ft }).collect();
            let mut enc = RangeEncoder::new(&mut buf);
            for op in &ops {
                encode_op(&mut enc, op);
            }
            enc.done();
            assert!(!enc.error());
            let mut dec = RangeDecoder::new(&buf);
            for op in &ops {
                decode_op(&mut dec, op);
            }
        }
    }

    #[test]
    fn bits_round_trip_exhaustive_for_small_widths() {
        let mut buf = vec![0u8; 16384];
        for bits in 1..=12u32 {
            let ops: Vec<Op> = (0..(1u32 << bits)).map(|v| Op::Bits { v, bits }).collect();
            let mut enc = RangeEncoder::new(&mut buf);
            for op in &ops {
                encode_op(&mut enc, op);
            }
            enc.done();
            assert!(!enc.error());
            let mut dec = RangeDecoder::new(&buf);
            for op in &ops {
                decode_op(&mut dec, op);
            }
        }
    }

    #[test]
    fn uint_round_trip_at_the_range_extremes() {
        let fts = [257u32, 258, 1000, 65535, 65_536, 1_000_000_000, u32::MAX];
        let mut buf = vec![0u8; 4096];
        for ft in fts {
            let vs = [0, 1, 2, ft / 3, ft / 2, ft - 2, ft - 1];
            let ops: Vec<Op> = vs.iter().map(|&v| Op::Uint { v, ft }).collect();
            let mut enc = RangeEncoder::new(&mut buf);
            for op in &ops {
                encode_op(&mut enc, op);
            }
            enc.done();
            assert!(!enc.error(), "ft {ft}");
            let mut dec = RangeDecoder::new(&buf);
            for op in &ops {
                decode_op(&mut dec, op);
            }
        }
    }

    #[test]
    fn icdf_round_trip_covers_every_symbol() {
        let mut rng = Rng(0xC0FF_EE12_3456_789A);
        let mut buf = vec![0u8; 4096];
        for ftb in 1..=8u32 {
            for _ in 0..32 {
                let icdf = random_icdf(&mut rng, ftb);
                let ops: Vec<Op> = (0..icdf.len())
                    .map(|s| Op::Icdf {
                        s,
                        icdf: icdf.clone(),
                        ftb,
                    })
                    .collect();
                let mut enc = RangeEncoder::new(&mut buf);
                for op in &ops {
                    encode_op(&mut enc, op);
                }
                enc.done();
                assert!(!enc.error());
                let mut dec = RangeDecoder::new(&buf);
                for op in &ops {
                    decode_op(&mut dec, op);
                }
            }
        }
    }

    #[test]
    fn shrink_moves_raw_bits_to_the_end_of_the_packet() {
        let mut buf = vec![0u8; 64];
        let mut enc = RangeEncoder::new(&mut buf);
        enc.encode_bits(0b1011, 4);
        enc.encode_uint(42, 1000);
        enc.shrink(16);
        enc.done();
        assert!(!enc.error());
        assert!(enc.range_bytes() <= 16);
        let mut dec = RangeDecoder::new(&buf[..16]);
        assert_eq!(dec.decode_bits(4), 0b1011);
        assert_eq!(dec.decode_uint(1000), 42);
    }

    fn fnv1a(data: &[u8]) -> u64 {
        let mut h: u64 = 14695981039346656037;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    /// Byte-for-byte match against the reference C entropy coder on a fixed
    /// symbol stream (`target/ref/check.c`). This is what catches a carry or
    /// renormalisation bug that a self-consistent decoder would mirror.
    #[test]
    fn matches_reference_stream() {
        let mut buf = vec![0u8; 8192];
        let mut enc = RangeEncoder::new(&mut buf);
        let mut x: u32 = 1;
        for i in 0..1024u32 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let r = x;
            match r % 4 {
                0 => {
                    let ft = 2 + (r >> 8) % 255;
                    let fl = (r >> 16) % ft;
                    let fh = fl + 1 + (r >> 24) % (ft - fl);
                    enc.encode(fl, fh, ft);
                }
                1 => {
                    let bits = 1 + (r >> 8) % 8;
                    let ft = 1u32 << bits;
                    let fl = (r >> 12) % ft;
                    let fh = fl + 1 + (r >> 24) % (ft - fl);
                    enc.encode_bin(fl, fh, bits);
                }
                2 => enc.encode_bit_logp(r & 1 == 1, 1 + (r >> 8) % 15),
                _ => {
                    let ft = 2 + (r >> 8) % 1_000_000;
                    enc.encode_uint(r % ft, ft);
                }
            }
            if i & 15 == 15 {
                enc.encode_bits(r & 0xFFFF, 16);
            }
        }
        enc.shrink(4096);
        enc.done();
        assert!(!enc.error());
        assert_eq!(enc.range_bytes(), 399);
        assert_eq!(fnv1a(&buf[..4096]), 0x54e9_ae33_bf47_ba35);
    }

    #[test]
    fn overflow_is_reported_not_panicked() {
        let mut buf = vec![0u8; 8];
        let mut enc = RangeEncoder::new(&mut buf);
        for _ in 0..1000 {
            enc.encode_bits(0x1F_FFFF, 25);
            if enc.error() {
                break;
            }
        }
        enc.done();
        assert!(enc.error());
    }
}
