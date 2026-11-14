// mdct.rs — CELT's low-overlap MDCT, bit-exact to `celt/mdct.c`.
//
// The input for a block is the previous `OVERLAP` samples followed by the
// block's own `N2` samples. The window is non-zero only in the two overlap
// regions, which is what the fold below implements. Twiddles, FFT tables and
// scales are the static 48 kHz mode's constants, and the FFT is a faithful
// `kiss_fft` port, so the output matches the reference to the last bit.
// `backward` is test-only and exists to verify TDAC.

use crate::celt_tables::MDCT_TWIDDLES;
use crate::fft::{Complex, Fft};
use crate::window::{OVERLAP, window};

pub struct Mdct {
    ffts: [Fft; 4],
    f: [f32; 960],
    f2: [Complex; 480],
}

impl Default for Mdct {
    fn default() -> Self {
        Self::new()
    }
}

impl Mdct {
    pub fn new() -> Self {
        Self {
            ffts: [Fft::new(0), Fft::new(1), Fft::new(2), Fft::new(3)],
            f: [0.0; 960],
            f2: [Complex::ZERO; 480],
        }
    }

    fn params(&self, shift: u32) -> (usize, usize, usize, usize) {
        let n = 1920 >> shift;
        let n2 = n >> 1;
        let n4 = n >> 2;
        let off = (0..shift).map(|s| (1920 >> s) >> 1).sum();
        (n, n2, n4, off)
    }

    /// `input` is `[previous OVERLAP][block N2]`; `out` receives `N2` bins with
    /// the given stride (the reference interleaves short blocks this way).
    pub fn forward(&mut self, input: &[f32], out: &mut [f32], shift: u32, stride: usize) {
        let (_n, n2, n4, off) = self.params(shift);
        let ov = OVERLAP;
        let half = ov >> 1;
        assert!(input.len() >= n2 + ov);
        assert!(out.len() >= stride * (n2 - 1) + 1);
        debug_assert!(n2 <= 960 && n4 <= 480);

        let trig = &MDCT_TWIDDLES[off..off + n2];
        let w = window();
        // `f[..n2]` is written exactly once by the three loops below
        // (proofs/Mdct.lean: `f_covers`), so it is never cleared.
        let f = &mut self.f[..n2];
        let first = (ov + 3) >> 2;

        let mut yp = 0;
        for i in 0..first {
            let wp1 = half + 2 * i;
            let wp2 = half - 1 - 2 * i;
            let xp1 = half + 2 * i;
            let xp2 = n2 - 1 + half - 2 * i;
            f[yp] = w[wp2] * input[xp1 + n2] + w[wp1] * input[xp2];
            f[yp + 1] = w[wp1] * input[xp1] - w[wp2] * input[xp2 - n2];
            yp += 2;
        }

        let mut xp1 = half + 2 * first;
        let mut xp2 = n2 - 1 + half - 2 * first;
        let mut wp1 = 0;
        let mut wp2 = ov - 1;
        let mut i = first;
        while i < n4 - first {
            f[yp] = input[xp2];
            f[yp + 1] = input[xp1];
            yp += 2;
            xp1 += 2;
            xp2 -= 2;
            i += 1;
        }
        while i < n4 {
            f[yp] = -w[wp1] * input[xp1 - n2] + w[wp2] * input[xp2];
            f[yp + 1] = w[wp2] * input[xp1] + w[wp1] * input[xp2 + n2];
            yp += 2;
            xp1 += 2;
            xp2 -= 2;
            wp1 += 2;
            wp2 -= 2;
            i += 1;
        }

        let fft = &self.ffts[shift as usize];
        let scale = fft.scale();
        // `f2[..n4]` is written once per slot by this loop
        // (proofs/Mdct.lean: `n4_bitrev_covers`), so it is never cleared.
        let f2 = &mut self.f2[..n4];
        for i in 0..n4 {
            let re = f[2 * i];
            let im = f[2 * i + 1];
            let t0 = trig[i];
            let t1 = trig[n4 + i];
            let yr = re * t0 - im * t1;
            let yi = im * t0 + re * t1;
            f2[fft.bitrev()[i] as usize] = Complex {
                re: scale * yr,
                im: scale * yi,
            };
        }

        fft.forward(f2);

        for i in 0..n4 {
            let re = f2[i].re;
            let im = f2[i].im;
            let t0 = trig[i];
            let t1 = trig[n4 + i];
            out[stride * 2 * i] = im * t1 - re * t0;
            out[stride * (n2 - 1 - 2 * i)] = re * t1 + im * t0;
        }
    }

    /// Test-only inverse. `out[..OVERLAP/2]` must hold the previous block's
    /// carried tail; `out[..N2]` is the reconstructed block and
    /// `out[N2..N2+OVERLAP/2]` is the tail for the next block.
    #[cfg(test)]
    pub fn backward(&self, input: &[f32], out: &mut [f32], shift: u32, stride: usize) {
        let (_n, n2, n4, off) = self.params(shift);
        let ov = OVERLAP;
        let half = ov >> 1;
        assert!(input.len() >= stride * (n2 - 1) + 1);
        assert!(out.len() >= n2 + half);

        let trig = &MDCT_TWIDDLES[off..off + n2];
        let w = window();
        let fft = &self.ffts[shift as usize];
        let bitrev = fft.bitrev();

        for i in 0..n4 {
            let xp1 = 2 * stride * i;
            let xp2 = stride * (n2 - 1 - 2 * i);
            let t0 = trig[i];
            let t1 = trig[n4 + i];
            let yr = input[xp2] * t0 + input[xp1] * t1;
            let yi = input[xp1] * t0 - input[xp2] * t1;
            let rev = bitrev[i] as usize;
            out[half + 2 * rev] = yi;
            out[half + 2 * rev + 1] = yr;
        }

        let mut buf: Vec<Complex> = (0..n4)
            .map(|i| Complex {
                re: out[half + 2 * i],
                im: out[half + 2 * i + 1],
            })
            .collect();
        fft.forward(&mut buf);
        for i in 0..n4 {
            out[half + 2 * i] = buf[i].re;
            out[half + 2 * i + 1] = buf[i].im;
        }

        let mut yp0 = half;
        let mut yp1 = half + n2 - 2;
        for i in 0..(n4 + 1) / 2 {
            let re = out[yp0 + 1];
            let im = out[yp0];
            let t0 = trig[i];
            let t1 = trig[n4 + i];
            let yr = re * t0 + im * t1;
            let yi = re * t1 - im * t0;

            let re2 = out[yp1 + 1];
            let im2 = out[yp1];
            out[yp0] = yr;
            out[yp1 + 1] = yi;

            let t0 = trig[n4 - i - 1];
            let t1 = trig[n2 - i - 1];
            let yr2 = re2 * t0 + im2 * t1;
            let yi2 = re2 * t1 - im2 * t0;
            out[yp1] = yr2;
            out[yp0 + 1] = yi2;

            yp0 += 2;
            yp1 -= 2;
        }

        let mut xp = ov - 1;
        let mut yp = 0;
        let mut wp1 = 0;
        let mut wp2 = ov - 1;
        for _ in 0..ov / 2 {
            let x1 = out[xp];
            let x2 = out[yp];
            out[yp] = w[wp2] * x2 - w[wp1] * x1;
            out[xp] = w[wp1] * x2 + w[wp2] * x1;
            yp += 1;
            xp -= 1;
            wp1 += 1;
            wp2 -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f64;
                (0.4 * (2.0 * std::f64::consts::PI * 0.013 * t).sin()
                    + 0.3 * (2.0 * std::f64::consts::PI * 0.037 * t + 0.7).sin()
                    + 0.2 * (2.0 * std::f64::consts::PI * 0.11 * t).cos()) as f32
            })
            .collect()
    }

    fn tdac(shift: u32) {
        let mut mdct = Mdct::new();
        let hop = (1920 >> shift) >> 1;
        let frames = 6;
        let len = OVERLAP + frames * hop + OVERLAP;
        let src = signal(len);
        let mut out = vec![0.0f32; len];
        let mut tail = vec![0.0f32; OVERLAP / 2];
        let mut bins = vec![0.0f32; hop];

        for t in 0..frames {
            let start = OVERLAP + t * hop;
            let mut input = vec![0.0f32; hop + OVERLAP];
            input.copy_from_slice(&src[start - OVERLAP..start + hop]);
            mdct.forward(&input, &mut bins, shift, 1);

            let mut syn = vec![0.0f32; hop + OVERLAP];
            syn[..OVERLAP / 2].copy_from_slice(&tail);
            mdct.backward(&bins, &mut syn, shift, 1);
            out[start..start + hop].copy_from_slice(&syn[..hop]);
            tail.copy_from_slice(&syn[hop..hop + OVERLAP / 2]);
        }

        let from = OVERLAP + hop;
        let to = OVERLAP + (frames - 1) * hop;
        let mut worst = 0.0f32;
        for i in from..to {
            worst = worst.max((out[i] - src[i - OVERLAP]).abs());
        }
        assert!(worst < 1e-4, "shift={shift} worst TDAC error {worst}");
    }

    #[test]
    fn tdac_long_block() {
        tdac(0);
    }

    #[test]
    fn tdac_medium_blocks() {
        tdac(1);
        tdac(2);
    }

    #[test]
    fn tdac_short_blocks() {
        tdac(3);
    }

    #[test]
    fn matches_reference_forward() {
        let mut mdct = Mdct::new();
        let mut input = vec![0.0f32; 1080];
        for (i, x) in input.iter_mut().enumerate() {
            let t = i as f64;
            *x = (0.4 * (2.0 * std::f64::consts::PI * 0.013 * t).sin()
                + 0.3 * (2.0 * std::f64::consts::PI * 0.037 * t + 0.7).sin()
                + 0.2 * (2.0 * std::f64::consts::PI * 0.11 * t).cos()) as f32;
        }
        let mut bins = vec![0.0f32; 960];
        mdct.forward(&input, &mut bins, 0, 1);
        let want: &[(usize, f32)] = &[
            (0, -0.001704435),
            (1, -0.001137111),
            (2, -0.002401369),
            (3, -0.000636460),
            (4, -0.003284748),
            (5, -0.000131213),
            (6, -0.004408461),
            (7, 0.000461048),
            (8, -0.005828993),
            (9, 0.001239142),
            (10, -0.007615836),
            (11, 0.002329489),
            (100, 0.000327334),
            (400, -0.000001153),
            (900, 0.000000034),
        ];
        for &(i, w) in want {
            assert!(
                (bins[i] - w).abs() < 1e-5,
                "bin {i}: got {} want {w}",
                bins[i]
            );
        }
    }

    #[test]
    fn matches_reference_backward() {
        let mut mdct = Mdct::new();
        let mut input = vec![0.0f32; 1080];
        for (i, x) in input.iter_mut().enumerate() {
            let t = i as f64;
            *x = (0.4 * (2.0 * std::f64::consts::PI * 0.013 * t).sin()
                + 0.3 * (2.0 * std::f64::consts::PI * 0.037 * t + 0.7).sin()
                + 0.2 * (2.0 * std::f64::consts::PI * 0.11 * t).cos()) as f32;
        }
        let mut bins = vec![0.0f32; 960];
        mdct.forward(&input, &mut bins, 0, 1);
        let mut syn = vec![0.0f32; 1080];
        mdct.backward(&bins, &mut syn, 0, 1);
        let head: &[(usize, f32)] = &[
            (0, -0.000001703),
            (1, -0.000094340),
            (2, -0.000325168),
            (3, -0.000535673),
            (4, -0.000636716),
            (5, -0.000916551),
            (6, -0.002064478),
            (7, -0.004781889),
            (8, -0.009164346),
            (9, -0.014273942),
            (10, -0.018296354),
            (11, -0.019362459),
        ];
        for &(i, w) in head {
            assert!(
                (syn[i] - w).abs() < 1e-5,
                "syn[{i}]: got {} want {w}",
                syn[i]
            );
        }
        let tail: &[(usize, f32)] = &[
            (960, -0.332165569),
            (961, -0.293672115),
            (962, -0.218672186),
            (963, -0.167308122),
            (964, -0.182467595),
            (965, -0.270002455),
        ];
        for &(i, w) in tail {
            assert!(
                (syn[i] - w).abs() < 1e-5,
                "syn[{i}]: got {} want {w}",
                syn[i]
            );
        }
    }

    #[test]
    #[ignore]
    fn bench_mdct() {
        use std::time::Instant;
        let mut mdct = Mdct::new();
        let input = vec![0.1f32; 1080];
        let mut bins = vec![0.0f32; 960];
        for _ in 0..10 {
            mdct.forward(&input, &mut bins, 0, 1);
        }
        let t0 = Instant::now();
        let frames = 1000;
        for _ in 0..frames {
            mdct.forward(&input, &mut bins, 0, 1);
        }
        let long = t0.elapsed().as_secs_f64() * 1000.0 / (frames as f64 * 0.02);

        let sinput = vec![0.1f32; 240];
        let mut sbins = vec![0.0f32; 960];
        for _ in 0..10 {
            for b in 0..8 {
                mdct.forward(&sinput, &mut sbins[b..], 3, 8);
            }
        }
        let t0 = Instant::now();
        for _ in 0..frames {
            for b in 0..8 {
                mdct.forward(&sinput, &mut sbins[b..], 3, 8);
            }
        }
        let short = t0.elapsed().as_secs_f64() * 1000.0 / (frames as f64 * 0.02);
        println!("mdct long: {long:.3} ms/s audio; 8x short: {short:.3} ms/s audio");
    }

    #[test]
    fn tone_lands_in_the_expected_bin() {
        let mut mdct = Mdct::new();
        let hop = 960;
        let mut input = vec![0.0f32; hop + OVERLAP];
        for (i, x) in input.iter_mut().enumerate() {
            *x = ((2.0 * std::f64::consts::PI * 1000.0 * i as f64 / 48_000.0).sin()) as f32;
        }
        let mut bins = vec![0.0f32; hop];
        mdct.forward(&input, &mut bins, 0, 1);
        let peak = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert!(
            (38..=42).contains(&peak),
            "1 kHz peaked at bin {peak}, expected near 40"
        );
    }
}
