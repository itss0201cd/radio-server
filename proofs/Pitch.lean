module

import Mathlib.Tactic

/-!
# Pitch search (`src/pitch.rs`)

`pitch_xcorr` computes

    out[i] = sum_j x[j] * y[i + j]

with the reduction written as the inner loop. Reversing the nesting — `j` outer,
`i` inner — keeps, for every `i`, exactly the same sequence of additions
(`j = 0, 1, ...`); only independent outputs are interleaved. Floating-point
results are therefore identical, while the inner loop becomes an AXPY that can
be vectorised.

`foldl_proj` is the general statement: folding a per-coordinate update over a
list of `j`s and then projecting coordinate `i` equals the scalar fold at `i`.
-/

namespace Pitch

/-- Folding a vector-valued `j`-update and projecting equals the scalar fold,
because each coordinate is updated independently and in the same `j` order. -/
theorem foldl_proj {n : ℕ} (x y : ℕ → ℝ) (s : Fin n → ℝ) (l : List ℕ) (i : Fin n) :
    (l.foldl (fun (o : Fin n → ℝ) j => fun k => o k + x j * y (k.1 + j)) s) i
      = l.foldl (fun a j => a + x j * y (i.1 + j)) (s i) := by
  induction l generalizing s with
  | nil => rfl
  | cons a l ih =>
      simp only [List.foldl_cons]
      exact ih (fun k => s k + x a * y (k.1 + a))

/-- `pitch_xcorr` after the loop swap computes the same value for every lag. -/
theorem xcorr_rowmajor {n L : ℕ} (x y : ℕ → ℝ) (i : Fin n) :
    (List.range L).foldl (fun (o : Fin n → ℝ) j => fun k => o k + x j * y (k.1 + j))
        (fun _ => 0) i
      = (List.range L).foldl (fun a j => a + x j * y (i.1 + j)) 0 :=
  foldl_proj x y (fun _ => 0) (List.range L) i

/-- The slide bound: the inner loop reads `y[j + i]` for `j < len`, `i < maxPitch`,
which fits exactly in `(len + maxPitch)` samples. -/
theorem y_index_bound (len maxPitch j i : ℕ) (hj : j < len) (hi : i < maxPitch) :
    j + i < len + maxPitch := by
  omega

/-! ### Pinned downsample frame

`pitch_downsample` is always called with `len = COMBFILTER_MAXPERIOD + N`
(`1024 + 960 = 1984`) and mixes both channels, so it takes no channel count.
The decimation loop indexes `x[2*i + 1]`. -/

def frameLen : ℕ := 1984

theorem half_eq : frameLen / 2 = 992 := by decide

/-- Every index the decimation loop reads stays inside the frame. -/
theorem downsample_index (i : ℕ) (hi : i < frameLen / 2) : 2 * i + 1 < frameLen := by
  have h := half_eq
  omega

end Pitch
