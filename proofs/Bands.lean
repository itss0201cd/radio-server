module

import Mathlib.Tactic

/-!
# CELT band quantiser (`src/bands.rs`)

This file records the facts behind two changes in `quant_all_bands`:

* **Reused scratch.** `lowband_buf`, `x_save`, `y_save`, `x_save2`, `y_save2`
  and `norm_save2` are only ever read as the prefix `[..n]`, and that prefix is
  written before the read. Their zero-initialisation therefore cannot affect
  any result, so they are hoisted out of the band loop and not re-cleared
  per band (`init_irrelevant`). The same argument moves `quant_band`'s
  `owned` buffer and the two Hadamard transpose temps into buffers passed
  down from `quant_all_bands`; the Hadamard case additionally needs `ordery`
  to be a permutation (`ordery_perm`, `block_cover`).
* **Sizing.** Every band has `n = m * (eBands[i+1] - eBands[i])` with `m = 8`;
  the widest band is `100 - 78 = 22` units, so `n <= 176`. Splitting halves
  `n`, so `[f32; 176]` covers the whole recursion (`bandN_le`, `half_le_176`).

`norm_len = m * eBands[nbEBands - 1] = 8 * 78 = 624` and `n_total = m * 120 = 960`
pin the two buffers in `quant_all_bands`.
-/

namespace Bands

/-! ### Scratch write-before-read -/

section Scratch

variable {α : Type*}

/-- Writing `n` values over the first `n` entries and then reading the first
`n` entries returns exactly the written values, independent of what the buffer
held before. -/
theorem read_after_write (init vals : List α) (n : ℕ) (h : vals.length = n) :
    (vals ++ init.drop n).take n = vals := by
  rw [List.take_append_of_le_length (by omega), List.take_of_length_le (by omega)]

/-- The buffer's previous contents never reach a read of the written prefix. -/
theorem init_irrelevant (init init' vals : List α) (n : ℕ) (h : vals.length = n) :
    (vals ++ init.drop n).take n = (vals ++ init'.drop n).take n := by
  rw [read_after_write init vals n h, read_after_write init' vals n h]

end Scratch

/-- A noduplicate list of length `n`, bounded by `n`, is a permutation of
`[0, n)`: it contains every value below `n`. -/
theorem mem_of_nodup_length {l : List ℕ} {n d : ℕ}
    (hnd : l.Nodup) (hlen : l.length = n) (hb : l.all (fun x => decide (x < n)) = true)
    (hd : d < n) : d ∈ l := by
  have hb' : ∀ x ∈ l, x < n := by
    intro x hx
    have h := (List.all_eq_true.mp hb) x hx
    simpa using h
  have hcard : l.toFinset.card = n := by
    rw [List.toFinset_card_of_nodup hnd, hlen]
  have hsub : l.toFinset ⊆ Finset.range n := by
    intro x hx
    simp only [Finset.mem_range]
    exact hb' x (List.mem_toFinset.mp hx)
  have heq : l.toFinset = Finset.range n := by
    apply Finset.eq_of_subset_of_card_le hsub
    rw [hcard, Finset.card_range]
  rw [← List.mem_toFinset, heq, Finset.mem_range]
  exact hd

/-! ### Shared stereo angle

`quant_band_stereo` runs twice for each rate-distortion band. The second run
reaches `compute_theta` with `xb`/`yb` restored from `x_save`/`y_save`, and
`read_after_write` says that restore is exact, so both runs feed
`stereo_itheta` identical slices. A function of equal inputs is equal, so one
call can serve both (`shared_angle`); the implementation computes it once and
passes the value through `Theta`. -/

/-- Equal inputs give equal results, so a pure computed angle can be shared. -/
theorem shared_angle {α β : Type*} (f : α → β) {a b : α} (h : a = b) :
    f a = f b := by
  rw [h]

/-! ### Hadamard transpose

`deinterleave_hadamard`/`interleave_hadamard` write `tmp[0..n)` and then copy
that prefix out, so the temp needs no prior contents. The non-Hadamard branch
writes `i*n0 + j`; the Hadamard branch writes `ordery[i]*n0 + j`, which covers
the same range once `ordery = ORDERY_TABLE[stride-2..][..stride]` is a
permutation of `[0, stride)`. `stride = b0 << recombine` with `b0` a power of
two, so the reachable strides are 2, 4, 8 and 16. -/

/-- `ORDERY_TABLE` from `src/bands.rs`. -/
def orderyTable : List ℕ :=
  [1, 0, 3, 0, 2, 1, 7, 0, 4, 3, 6, 1, 5, 2, 15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2,
   10, 5]

/-- The slice `ordery_table + stride - 2` used by the transpose. -/
def orderySlice (stride : ℕ) : List ℕ := (orderyTable.drop (stride - 2)).take stride

/-- Every reachable stride gives a permutation of `[0, stride)`. -/
theorem ordery_perm (stride : ℕ)
    (hs : stride = 2 ∨ stride = 4 ∨ stride = 8 ∨ stride = 16) (d : ℕ) (hd : d < stride) :
    d ∈ orderySlice stride := by
  have h2 : (orderySlice 2).Nodup ∧ (orderySlice 2).length = 2 ∧
      (orderySlice 2).all (fun x => decide (x < 2)) = true := by decide
  have h4 : (orderySlice 4).Nodup ∧ (orderySlice 4).length = 4 ∧
      (orderySlice 4).all (fun x => decide (x < 4)) = true := by decide
  have h8 : (orderySlice 8).Nodup ∧ (orderySlice 8).length = 8 ∧
      (orderySlice 8).all (fun x => decide (x < 8)) = true := by decide
  have h16 : (orderySlice 16).Nodup ∧ (orderySlice 16).length = 16 ∧
      (orderySlice 16).all (fun x => decide (x < 16)) = true := by decide
  rcases hs with rfl | rfl | rfl | rfl
  · exact mem_of_nodup_length h2.1 h2.2.1 h2.2.2 hd
  · exact mem_of_nodup_length h4.1 h4.2.1 h4.2.2 hd
  · exact mem_of_nodup_length h8.1 h8.2.1 h8.2.2 hd
  · exact mem_of_nodup_length h16.1 h16.2.1 h16.2.2 hd

/-- If `perm` contains every value below `stride`, the pairs
`(perm[i], j)` cover every index below `stride * n0`, because `d` splits into
`d / n0` and `d % n0`. -/
theorem block_cover (perm : List ℕ) (stride n0 : ℕ) (hn0 : 0 < n0)
    (hperm : ∀ a, a < stride → a ∈ perm) (d : ℕ) (hd : d < stride * n0) :
    ∃ a, a < stride ∧ a ∈ perm ∧ d % n0 < n0 ∧ d = a * n0 + d % n0 := by
  refine ⟨d / n0, (Nat.div_lt_iff_lt_mul hn0).mpr hd, ?_, Nat.mod_lt d hn0, ?_⟩
  · exact hperm (d / n0) ((Nat.div_lt_iff_lt_mul hn0).mpr hd)
  · have h := Nat.div_add_mod d n0
    rw [Nat.mul_comm] at h
    exact h.symm

/-- Fusing two independent dot products into one pass leaves each sum's
order unchanged, so the float results are identical to two separate calls. -/
theorem fused_dot (a b c d : ℕ → ℝ) :
    ∀ (l : List ℕ) (p : ℝ × ℝ),
      l.foldl (fun p i => (p.1 + a i * b i, p.2 + c i * d i)) p
        = (l.foldl (fun s i => s + a i * b i) p.1,
           l.foldl (fun s i => s + c i * d i) p.2) := by
  intro l
  induction l with
  | nil => intro p; rfl
  | cons h t ih =>
      intro p
      simp only [List.foldl_cons]
      exact ih (p.1 + a h * b h, p.2 + c h * d h)

/-- The fused loop starting both accumulators at zero, as `inner_prod` does. -/
theorem fused_dot_zero (a b c d : ℕ → ℝ) (l : List ℕ) :
    l.foldl (fun p i => (p.1 + a i * b i, p.2 + c i * d i)) (0, 0)
      = (l.foldl (fun s i => s + a i * b i) 0,
         l.foldl (fun s i => s + c i * d i) 0) :=
  fused_dot a b c d l (0, 0)

/-! ### Band geometry -/

/-- `eBands` of the pinned 48 kHz mode. -/
def ebandList : List ℕ :=
  [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100]

def eband (i : ℕ) : ℕ := ebandList.getD i 0

/-- `M = 1 << LM` for `LM = 3`. -/
def m : ℕ := 8

def nbEbands : ℕ := 21

/-- Bins in band `i`. -/
def bandN (i : ℕ) : ℕ := m * (eband (i + 1) - eband i)

/-- Every band is non-empty. -/
theorem bandN_pos (i : ℕ) (hi : i < nbEbands) : 0 < bandN i := by
  have hi' : i < 21 := by simpa [nbEbands] using hi
  interval_cases i <;> decide

/-- The widest band is 22 units, so no band exceeds a 176-float scratch. -/
theorem bandN_le (i : ℕ) (hi : i < nbEbands) : bandN i ≤ 176 := by
  have hi' : i < 21 := by simpa [nbEbands] using hi
  interval_cases i <;> decide

/-- The split recursion halves `n`, so it stays inside the same scratch. -/
theorem half_le_176 (n : ℕ) (hn : n ≤ 176) : n / 2 ≤ 176 := by
  omega

/-- A band is a whole number of `M`-sized blocks, which is why the width
arithmetic never leaves a partial block. -/
theorem bandN_blocks (i : ℕ) : bandN i = m * (eband (i + 1) - eband i) := rfl

/-- `norm_len = m * eBands[nbEBands - 1]`. -/
theorem norm_len_eq : m * eband (nbEbands - 1) = 624 := by
  unfold m eband ebandList nbEbands
  decide

/-- `n_total = m * 120`. -/
theorem n_total_eq : m * 120 = 960 := by
  unfold m
  decide

/-- The `[f32; 176]` scratch is large enough for the widest band. -/
theorem scratch_covers (i : ℕ) (hi : i < nbEbands) : bandN i ≤ 176 := bandN_le i hi

end Bands
