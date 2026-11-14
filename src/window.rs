// window.rs — CELT's low-overlap window.
//
// The coefficients are the literal `window120` table from libopus
// `celt/static_modes_float.h`. The closed form is
// `sin(pi/2 * sin(.5*M_PI*(i + 0.5)/OVERLAP)^2)`, but the shipped literals
// differ from a recomputed table in the last few bits, and the MDCT must
// round identically to the reference.
//
// Power-complementary: w[i]^2 + w[OVERLAP-1-i]^2 = 1. The MDCT's perfect
// reconstruction depends on that property, so it is tested directly.

use crate::celt_tables::WINDOW120;

pub const OVERLAP: usize = 120;

pub fn window() -> &'static [f32; OVERLAP] {
    &WINDOW120
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_complementary() {
        let w = window();
        let mut worst = 0.0f32;
        for i in 0..OVERLAP {
            let sum = w[i] * w[i] + w[OVERLAP - 1 - i] * w[OVERLAP - 1 - i];
            worst = worst.max((sum - 1.0).abs());
        }
        assert!(worst < 1e-6, "worst power-complementarity error {worst}");
    }

    #[test]
    fn monotone_and_bounded() {
        let w = window();
        assert!(w[0] > 0.0 && w[0] < 0.01);
        assert!(w[OVERLAP - 1] > 0.999);
        for i in 1..OVERLAP {
            assert!(w[i] >= w[i - 1], "window not monotone at {i}");
        }
    }

    #[test]
    fn matches_the_closed_form() {
        let w = window();
        for i in 0..OVERLAP {
            let s = (std::f64::consts::FRAC_PI_2 * (i as f64 + 0.5) / OVERLAP as f64).sin();
            let want = (std::f64::consts::FRAC_PI_2 * s * s).sin();
            assert!((w[i] as f64 - want).abs() < 1e-7);
        }
    }
}
