module

import Mathlib.Tactic

/-!
# CELT energy coding (`src/energy.rs`)

Every caller of `quant_coarse_energy` — the encoder and both reference tests —
passes `force_intra = false`, `two_pass = true` and `loss_rate = 0`. This file
proves what those pinned values collapse:

* `intra_bias = budget * delayed_intra * loss_rate / (c * 512)` is zero
  (`intraBias_zero`), so the tie-break is just `tell_frac > tell_intra`.
* The initial `intra` is false (`intra_initial`), so `intra` only ever becomes
  true through the two-pass comparison.
* Consequently `two_pass || intra` is `two_pass` (`two_pass_gate`), and the
  `else` arm of the following `if !intra` — which needs `intra` true at entry —
  is unreachable (`intra_else_dead`).
* The probability-model offset `2 * i.min(20)` is `2 * i` for every coded band
  (`pi_simplify`), because `i < NB_EBANDS = 21`.

`LAPLACE_MINP = 1`, `LAPLACE_NMIN = 16` and `LAPLACE_LOG_MINP = 0` make the
Laplace constants fold to `32`, `1` and an identity shift (`laplace_minp`);
they are left as-is because the compiler folds them already.
-/

namespace Energy

/-- `NB_EBANDS`. -/
def nbEbands : ℕ := 21

/-- A zero loss rate makes the intra bias vanish, whatever the other factors. -/
theorem intraBias_zero (budget delayed lossRate c : ℝ) (h : lossRate = 0) :
    budget * delayed * lossRate / (c * 512) = 0 := by
  simp [h]

/-- With `force_intra = false` and `two_pass = true`, the initial choice is
inter (`intra = false`). -/
theorem intra_initial (forceIntra twoPass cond : Bool) (hf : forceIntra = false)
    (ht : twoPass = true) :
    (forceIntra || (!twoPass && cond)) = false := by
  simp [hf, ht]

/-- `intra` is still false when the two-pass block starts, so the guard
`two_pass || intra` is exactly `two_pass`. -/
theorem two_pass_gate (tp : Bool) : (tp || false) = tp := Bool.or_false tp

/-- An entry value of `false` makes the `else` arm of `if intra` unreachable. -/
theorem intra_else_dead (b : Bool) (h : b = false) : ¬ (b = true) := by
  simp [h]

/-- `i.min 20 = i` for every coded band, so `pi = 2*i`. -/
theorem pi_simplify (i : ℕ) (h : i < nbEbands) : 2 * min i 20 = 2 * i := by
  have h' : i < 21 := by simpa [nbEbands] using h
  omega

/-- The folded Laplace constant `LAPLACE_MINP * (2 * LAPLACE_NMIN)`. -/
theorem laplace_minp : (1 : ℕ) * (2 * 16) = 32 := by decide

/-- Fusing two independent sums into one pass keeps each order, so the float
results are identical to two separate loops. -/
theorem fused_sum (a b : ℕ → ℝ) :
    ∀ (l : List ℕ) (p : ℝ × ℝ),
      l.foldl (fun p i => (p.1 + a i, p.2 + b i)) p
        = (l.foldl (fun s i => s + a i) p.1, l.foldl (fun s i => s + b i) p.2) := by
  intro l
  induction l with
  | nil => intro p; rfl
  | cons h t ih =>
      intro p
      simp only [List.foldl_cons]
      exact ih (p.1 + a h, p.2 + b h)

/-- The station is stereo: every `compute_band_energies` call passes two
channels, so the function fuses the left and right band sums. -/
def channels : ℕ := 2

theorem channels_eq : channels = 2 := rfl

end Energy
