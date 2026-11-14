// fft.rs — bit-exact port of libopus `kiss_fft` (float build), restricted to
// the four transform sizes the CELT MDCT uses.
//
// The MDCT pre-rotation stores its input in bit-reversed order, so `forward`
// operates in place without reordering, exactly like `opus_fft_impl`. Twiddles,
// bit-reversal tables and scales are the constants from the static 48 kHz mode,
// not recomputed, so every operation rounds identically to the reference.

use crate::celt_tables::{
    BITREV60, BITREV120, BITREV240, BITREV480, FACTORS60, FACTORS120, FACTORS240, FACTORS480,
    FFT_SCALES, FFT_TWIDDLES,
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };

    #[inline]
    fn add(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }

    #[inline]
    fn sub(self, other: Self) -> Self {
        Self {
            re: self.re - other.re,
            im: self.im - other.im,
        }
    }

    #[inline]
    fn mul(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
}

fn twiddle(index: usize) -> Complex {
    Complex {
        re: FFT_TWIDDLES[index][0],
        im: FFT_TWIDDLES[index][1],
    }
}

pub struct Fft {
    nfft: usize,
    scale: f32,
    shift: usize,
    factors: &'static [usize],
    bitrev: &'static [u16],
}

impl Fft {
    /// `shift` is the MDCT shift: 0 selects the 480-point base transform.
    pub fn new(shift: u32) -> Self {
        let (nfft, factors, bitrev, sh) = match shift {
            0 => (480, &FACTORS480[..], &BITREV480[..], 0),
            1 => (240, &FACTORS240[..], &BITREV240[..], 1),
            2 => (120, &FACTORS120[..], &BITREV120[..], 2),
            3 => (60, &FACTORS60[..], &BITREV60[..], 3),
            _ => panic!("unsupported MDCT shift {shift}"),
        };
        Self {
            nfft,
            scale: FFT_SCALES[shift as usize],
            shift: sh,
            factors,
            bitrev,
        }
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn bitrev(&self) -> &[u16] {
        self.bitrev
    }

    pub fn forward(&self, fout: &mut [Complex]) {
        debug_assert_eq!(fout.len(), self.nfft);
        let mut fstride = [0usize; 8];
        fstride[0] = 1;
        let mut l = 0usize;
        loop {
            let p = self.factors[2 * l];
            let m = self.factors[2 * l + 1];
            fstride[l + 1] = fstride[l] * p;
            l += 1;
            if m == 1 {
                break;
            }
        }
        let mut m = self.factors[2 * l - 1];
        for i in (0..l).rev() {
            let m2 = if i != 0 { self.factors[2 * i - 1] } else { 1 };
            match self.factors[2 * i] {
                2 => kf_bfly2(fout, m, fstride[i]),
                4 => kf_bfly4(fout, fstride[i] << self.shift, m, fstride[i], m2),
                3 => kf_bfly3(fout, fstride[i] << self.shift, m, fstride[i], m2),
                5 => kf_bfly5(fout, fstride[i] << self.shift, m, fstride[i], m2),
                p => panic!("unexpected FFT radix {p}"),
            }
            m = m2;
        }
    }
}

fn kf_bfly2(fout: &mut [Complex], m: usize, n: usize) {
    if m == 1 {
        let mut base = 0;
        for _ in 0..n {
            let t = fout[base + 1];
            fout[base + 1] = fout[base].sub(t);
            fout[base] = fout[base].add(t);
            base += 2;
        }
    } else {
        debug_assert_eq!(m, 4);
        let tw = 0.7071067812f32;
        let mut base = 0;
        for _ in 0..n {
            let f2 = base + 4;
            let t = fout[f2];
            fout[f2] = fout[base].sub(t);
            fout[base] = fout[base].add(t);

            let t1 = Complex {
                re: (fout[f2 + 1].re + fout[f2 + 1].im) * tw,
                im: (fout[f2 + 1].im - fout[f2 + 1].re) * tw,
            };
            fout[f2 + 1] = fout[base + 1].sub(t1);
            fout[base + 1] = fout[base + 1].add(t1);

            let t2 = Complex {
                re: fout[f2 + 2].im,
                im: -fout[f2 + 2].re,
            };
            fout[f2 + 2] = fout[base + 2].sub(t2);
            fout[base + 2] = fout[base + 2].add(t2);

            let t3 = Complex {
                re: (fout[f2 + 3].im - fout[f2 + 3].re) * tw,
                im: -((fout[f2 + 3].im + fout[f2 + 3].re) * tw),
            };
            fout[f2 + 3] = fout[base + 3].sub(t3);
            fout[base + 3] = fout[base + 3].add(t3);
            base += 8;
        }
    }
}

fn kf_bfly4(fout: &mut [Complex], fstride: usize, m: usize, n: usize, mm: usize) {
    if m == 1 {
        let mut base = 0;
        for _ in 0..n {
            let scratch0 = fout[base].sub(fout[base + 2]);
            fout[base] = fout[base].add(fout[base + 2]);
            let mut scratch1 = fout[base + 1].add(fout[base + 3]);
            fout[base + 2] = fout[base].sub(scratch1);
            fout[base] = fout[base].add(scratch1);
            scratch1 = fout[base + 1].sub(fout[base + 3]);
            fout[base + 1] = Complex {
                re: scratch0.re + scratch1.im,
                im: scratch0.im - scratch1.re,
            };
            fout[base + 3] = Complex {
                re: scratch0.re - scratch1.im,
                im: scratch0.im + scratch1.re,
            };
            base += 4;
        }
    } else {
        let m2 = 2 * m;
        let m3 = 3 * m;
        for i in 0..n {
            let mut base = i * mm;
            for j in 0..m {
                let scratch0 = fout[base + m].mul(twiddle(j * fstride));
                let scratch1 = fout[base + m2].mul(twiddle(2 * j * fstride));
                let scratch2 = fout[base + m3].mul(twiddle(3 * j * fstride));
                let scratch5 = fout[base].sub(scratch1);
                fout[base] = fout[base].add(scratch1);
                let scratch3 = scratch0.add(scratch2);
                let scratch4 = scratch0.sub(scratch2);
                fout[base + m2] = fout[base].sub(scratch3);
                fout[base] = fout[base].add(scratch3);
                fout[base + m] = Complex {
                    re: scratch5.re + scratch4.im,
                    im: scratch5.im - scratch4.re,
                };
                fout[base + m3] = Complex {
                    re: scratch5.re - scratch4.im,
                    im: scratch5.im + scratch4.re,
                };
                base += 1;
            }
        }
    }
}

fn kf_bfly3(fout: &mut [Complex], fstride: usize, m: usize, n: usize, mm: usize) {
    let m2 = 2 * m;
    let epi3 = twiddle(fstride * m);
    for i in 0..n {
        let mut base = i * mm;
        for j in 0..m {
            let scratch1 = fout[base + m].mul(twiddle(j * fstride));
            let scratch2 = fout[base + m2].mul(twiddle(2 * j * fstride));
            let scratch3 = scratch1.add(scratch2);
            let mut scratch0 = scratch1.sub(scratch2);
            let fm_r = fout[base].re - scratch3.re * 0.5;
            let fm_i = fout[base].im - scratch3.im * 0.5;
            scratch0.re *= epi3.im;
            scratch0.im *= epi3.im;
            fout[base] = fout[base].add(scratch3);
            fout[base + m2] = Complex {
                re: fm_r + scratch0.im,
                im: fm_i - scratch0.re,
            };
            fout[base + m] = Complex {
                re: fm_r - scratch0.im,
                im: fm_i + scratch0.re,
            };
            base += 1;
        }
    }
}

fn kf_bfly5(fout: &mut [Complex], fstride: usize, m: usize, n: usize, mm: usize) {
    let ya = twiddle(fstride * m);
    let yb = twiddle(fstride * 2 * m);
    for i in 0..n {
        let base = i * mm;
        for u in 0..m {
            let b0 = base + u;
            let b1 = b0 + m;
            let b2 = b0 + 2 * m;
            let b3 = b0 + 3 * m;
            let b4 = b0 + 4 * m;

            let scratch0 = fout[b0];
            let scratch1 = fout[b1].mul(twiddle(u * fstride));
            let scratch2 = fout[b2].mul(twiddle(2 * u * fstride));
            let scratch3 = fout[b3].mul(twiddle(3 * u * fstride));
            let scratch4 = fout[b4].mul(twiddle(4 * u * fstride));

            let scratch7 = scratch1.add(scratch4);
            let scratch10 = scratch1.sub(scratch4);
            let scratch8 = scratch2.add(scratch3);
            let scratch9 = scratch2.sub(scratch3);

            fout[b0].re = fout[b0].re + (scratch7.re + scratch8.re);
            fout[b0].im = fout[b0].im + (scratch7.im + scratch8.im);

            let scratch5 = Complex {
                re: scratch0.re + (scratch7.re * ya.re + scratch8.re * yb.re),
                im: scratch0.im + (scratch7.im * ya.re + scratch8.im * yb.re),
            };
            let scratch6 = Complex {
                re: scratch10.im * ya.im + scratch9.im * yb.im,
                im: -((scratch10.re * ya.im) + (scratch9.re * yb.im)),
            };
            fout[b1] = scratch5.sub(scratch6);
            fout[b4] = scratch5.add(scratch6);

            let scratch11 = Complex {
                re: scratch0.re + (scratch7.re * yb.re + scratch8.re * ya.re),
                im: scratch0.im + (scratch7.im * yb.re + scratch8.im * ya.re),
            };
            let scratch12 = Complex {
                re: scratch9.im * ya.im - scratch10.im * yb.im,
                im: scratch10.re * yb.im - scratch9.re * ya.im,
            };
            fout[b2] = scratch11.add(scratch12);
            fout[b3] = scratch11.sub(scratch12);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
        }
    }

    fn naive(x: &[Complex]) -> Vec<Complex> {
        let n = x.len();
        (0..n)
            .map(|k| {
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, v) in x.iter().enumerate() {
                    let phase = -2.0 * std::f64::consts::PI * (i * k % n) as f64 / n as f64;
                    let (s, c) = phase.sin_cos();
                    re += v.re as f64 * c - v.im as f64 * s;
                    im += v.re as f64 * s + v.im as f64 * c;
                }
                Complex {
                    re: re as f32,
                    im: im as f32,
                }
            })
            .collect()
    }

    #[test]
    fn matches_naive_dft() {
        for shift in 0..=3u32 {
            let fft = Fft::new(shift);
            let n = fft.bitrev().len();
            let mut rng = Rng(0x1234_5678_9ABC_DEF0 ^ n as u64);
            // `forward` expects bit-reversed input, so build natural input and
            // permute it the same way the MDCT does. The transform then yields
            // the natural-order DFT of the original vector.
            let natural: Vec<Complex> = (0..n)
                .map(|_| Complex {
                    re: rng.next() as f32,
                    im: rng.next() as f32,
                })
                .collect();
            let mut bitrev = vec![Complex::ZERO; n];
            for (i, &rev) in fft.bitrev().iter().enumerate() {
                bitrev[rev as usize] = natural[i];
            }
            // The pre-permutation means the transform recovers the DFT of the
            // original (natural-order) vector, not of the array it was handed.
            let want = naive(&natural);
            let mut got = bitrev.clone();
            fft.forward(&mut got);
            let mut worst = 0.0f32;
            for (a, b) in got.iter().zip(&want) {
                worst = worst.max((a.re - b.re).abs()).max((a.im - b.im).abs());
            }
            assert!(worst < 2e-4, "n={n} worst={worst}");
        }
    }

    #[test]
    fn impulse_maps_to_all_ones() {
        for shift in [0u32, 3] {
            let fft = Fft::new(shift);
            let n = fft.bitrev().len();
            let mut x = vec![Complex::ZERO; n];
            x[0] = Complex { re: 1.0, im: 0.0 };
            fft.forward(&mut x);
            for v in &x {
                assert!((v.re - 1.0).abs() < 1e-5 && v.im.abs() < 1e-5);
            }
        }
    }

    #[test]
    fn constant_maps_to_scaled_impulse() {
        for shift in [0u32, 3] {
            let fft = Fft::new(shift);
            let n = fft.bitrev().len();
            let mut x = vec![Complex { re: 1.0, im: 0.0 }; n];
            fft.forward(&mut x);
            assert!((x[0].re - n as f32).abs() < 1e-3);
            for v in &x[1..] {
                assert!(v.re.abs() < 1e-3 && v.im.abs() < 1e-3);
            }
        }
    }
}
