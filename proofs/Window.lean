module
import Mathlib.Analysis.SpecialFunctions.Trigonometric.Basic
import Mathlib.Tactic

/-!
# CELT low-overlap window

`src/window.rs` uses

  w(i) = sin (π/2 * sin² (π/2 * (i + 1/2) / overlap)),  i ∈ [0, overlap).

Perfect reconstruction of the MDCT depends on the power-complementary identity

  w(i)² + w(overlap - 1 - i)² = 1.

This file proves it for every real position `x`, then instantiates the exact
index complement used by the code. The Rust test additionally checks the
shipped `f32` table numerically.
-/

noncomputable section

namespace Window

/-- CELT's window as a function of the normalised position `x = (i + 1/2) / overlap`. -/
def w (x : ℝ) : ℝ :=
  Real.sin (Real.pi / 2 * (Real.sin (Real.pi / 2 * x)) ^ 2)

/-- `w x` and `w (1 - x)` are power-complementary. -/
theorem w_power_complementary (x : ℝ) : w x ^ 2 + w (1 - x) ^ 2 = 1 := by
  unfold w
  have hs : Real.sin (Real.pi / 2 * (1 - x)) = Real.cos (Real.pi / 2 * x) := by
    have h : Real.pi / 2 * (1 - x) = Real.pi / 2 - Real.pi / 2 * x := by ring
    rw [h, Real.sin_pi_div_two_sub]
  rw [hs]
  have hcs := Real.sin_sq_add_cos_sq (Real.pi / 2 * x)
  have harg : Real.pi / 2 * (Real.cos (Real.pi / 2 * x)) ^ 2
      = Real.pi / 2 - Real.pi / 2 * (Real.sin (Real.pi / 2 * x)) ^ 2 := by
    linear_combination (Real.pi / 2) * hcs
  rw [harg, Real.sin_pi_div_two_sub]
  exact Real.sin_sq_add_cos_sq _

/-- The exact index complement used by `src/window.rs`: `overlap - 1 - i` is
the mirror of `i` about the centre of the window. -/
theorem w_index (overlap i : ℕ) (h : i < overlap) :
    w (((i : ℝ) + 1 / 2) / overlap) ^ 2
      + w ((((overlap - 1 - i : ℕ) : ℝ) + 1 / 2) / overlap) ^ 2 = 1 := by
  have ho : (0 : ℝ) < overlap := by exact_mod_cast (by omega : 0 < overlap)
  have hx : (((overlap - 1 - i : ℕ) : ℝ) + 1 / 2) / overlap
      = 1 - (((i : ℝ) + 1 / 2) / overlap) := by
    have hsub : overlap - 1 - i = overlap - (i + 1) := by omega
    rw [hsub, Nat.cast_sub (by omega : i + 1 ≤ overlap)]
    push_cast
    field_simp
    ring
  rw [hx, w_power_complementary]

/-- The window used by the encoder: overlap 120, as in the pinned CELT mode. -/
theorem w_index_120 (i : ℕ) (h : i < 120) :
    w (((i : ℝ) + 1 / 2) / 120) ^ 2
      + w ((((119 - i : ℕ) : ℝ) + 1 / 2) / 120) ^ 2 = 1 := by
  simpa using w_index 120 i h

end Window
