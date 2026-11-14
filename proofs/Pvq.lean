module

import Mathlib.Tactic

/-!
# Pyramid vector quantiser (`src/pvq.rs`)

This file audits `op_pvq_search` and `exp_rotation` under the pinned encoder
configuration, looking for work that is provably unnecessary.

* **Unreachable branch.** `op_pvq_search` used to follow its floor pass with a
  shortcut for `pulses_left > n + 3`. The theorems below prove
  `pulses_left <= n - 1` for every non-negative input, so the shortcut could
  never run and has been deleted.
* **Redundant state.** During the greedy loop the float vector `y` satisfied
  `y[j] = 2 * iy[j]` exactly (both are small integers), and the accumulator
  `yy` was exactly `sum_j iy[j]^2`. The invariants are proved, so the separate
  `y` vector is gone: the implementation now derives `2 * iy[j]` directly and
  `yy_step` justifies the accumulator update. This measured -1.09% of retired
  instructions on the reference corpus (73,599,518 -> 72,797,068), byte-exact.
* **Index safety.** `exp_rotation`'s rotation factor is only defined for
  `spread in {1,2,3}`; the guard rejects `spread = 0` first. `icwrs` walks `j`
  down from `n - 1`, so it needs `n >= 2`, which is why `n == 1` is handled by
  `quant_band_n1` before `alg_quant` is called.

Arithmetic note: the floor-pass proof is over `ℚ` (exact arithmetic). The
implementation evaluates it in `f32`. Every term is at most `k + 0.8` with
`k <= 128`, so the f32 rounding error summed over at most 176 samples is below
`1e-2`, while the retired bound only needs the sum to stay above
`k + 4/5 - 4`. The margin is four whole pulses, so the branch is dead for the
implemented float path as well; `op_pvq_search`'s debug assertion pins this.
-/

namespace Pvq

/-! ### The floor pass

`op_pvq_search` initialises, when `2 * k > n`,

    rcp = (k + 4/5) / S,   iy_j = floor (rcp * x_j),   S = sum_j x_j,

with every `x_j >= 0`, and then sets `pulses_left = k - sum_j iy_j`.
The lemmas below bound `pulses_left` from above. -/

/-- Summing per-term floors loses less than one per term: `floor z > z - 1`,
and summing `n` of them leaves the total strictly above `sum z - n`. -/
theorem floor_sum_gt {n : ℕ} (hn : 0 < n) (x : Fin n → ℚ) (rcp : ℚ) (k : ℕ)
    (hsum : rcp * ∑ j, x j = (k : ℚ) + 4 / 5) :
    (∑ j, ((⌊rcp * x j⌋ : ℤ) : ℚ)) > (k : ℚ) + 4 / 5 - (n : ℚ) := by
  have hstep : ∀ j, (rcp * x j) - 1 < ((⌊rcp * x j⌋ : ℤ) : ℚ) := by
    intro j
    have h := Int.lt_floor_add_one (rcp * x j)
    linarith
  have hlt : (∑ j, (rcp * x j - 1)) < ∑ j, ((⌊rcp * x j⌋ : ℤ) : ℚ) := by
    apply Finset.sum_lt_sum
    · intro j _
      exact le_of_lt (hstep j)
    · exact ⟨⟨0, hn⟩, Finset.mem_univ _, hstep ⟨0, hn⟩⟩
  have h1 : (∑ j, (rcp * x j - 1)) = (∑ j, rcp * x j) - (n : ℚ) := by
    rw [Finset.sum_sub_distrib]
    simp [Finset.sum_const, Fintype.card_fin, nsmul_eq_mul]
  have h2 : (∑ j, rcp * x j) = rcp * ∑ j, x j := by
    rw [Finset.mul_sum]
  rw [h1, h2, hsum] at hlt
  exact hlt

/-- `pulses_left = k - sum_j floor(rcp * x_j)` is at most `n - 1`.

The floor sum is an integer strictly greater than `k + 4/5 - n`, hence at
least `k - n + 1`, because it is the next integer above a value with
fractional part `4/5`. -/
theorem pulses_left_le {n : ℕ} (hn : 0 < n) (x : Fin n → ℚ) (rcp : ℚ) (k : ℕ)
    (hsum : rcp * ∑ j, x j = (k : ℚ) + 4 / 5) :
    (k : ℤ) - (∑ j, ⌊rcp * x j⌋) ≤ (n : ℤ) - 1 := by
  let M : ℤ := ∑ j, ⌊rcp * x j⌋
  have hreal : (M : ℚ) > (k : ℚ) + 4 / 5 - (n : ℚ) := by
    have h := floor_sum_gt hn x rcp k hsum
    have hMeq : (M : ℚ) = ∑ j, ((⌊rcp * x j⌋ : ℤ) : ℚ) := by
      simp only [M]
      push_cast
      rfl
    rw [hMeq]
    exact h
  by_contra hcon
  have hle : M ≤ (k : ℤ) - (n : ℤ) := by omega
  have hleQ : (M : ℚ) ≤ (k : ℚ) - (n : ℚ) := by exact_mod_cast hle
  linarith

/-- The retired shortcut's condition `pulses_left > n + 3` is false. This is
the theorem that licenses deleting the branch. -/
theorem shortcut_dead {n : ℕ} (hn : 0 < n) (x : Fin n → ℚ) (rcp : ℚ) (k : ℕ)
    (hsum : rcp * ∑ j, x j = (k : ℚ) + 4 / 5) :
    ¬ ((k : ℤ) - (∑ j, ⌊rcp * x j⌋) > (n : ℤ) + 3) := by
  have h := pulses_left_le hn x rcp k hsum
  omega

/-- The `2 * k <= n` branch skips the floor pass, leaving `pulses_left = k`.
There `k <= n / 2`, so it stays below `n - 1` for the `n >= 2` the search is
called with. -/
theorem low_branch_left_le (n k : ℕ) (hn : 2 ≤ n) (hk : k ≤ n / 2) :
    k ≤ n - 1 := by
  omega

/-! ### Greedy-loop state

The loop carried `iy` (integer pulse counts) and `y`, whose entries were
`2 * iy`. Each iteration did `y[b] += 2; iy[b] += 1; yy += 1 + y[b]` with `b`
the argmax. `two_iy_step` shows `y` is redundant and `yy_step` shows the
accumulator stays equal to `sum_j iy[j]^2`, which is what the loop now
computes from `iy` alone. -/

/-- Point update of a finite vector. -/
def upd {n : ℕ} (f : Fin n → ℤ) (b : Fin n) (v : ℤ) : Fin n → ℤ :=
  fun j => if j = b then v else f j

theorem upd_same {n : ℕ} (f : Fin n → ℤ) (b : Fin n) (v : ℤ) :
    upd f b v b = v := by
  simp [upd]

theorem upd_noteq {n : ℕ} (f : Fin n → ℤ) (b j : Fin n) (v : ℤ) (h : j ≠ b) :
    upd f b v j = f j := by
  simp [upd, h]

/-- `y = 2 * iy` is preserved by one greedy update. -/
theorem two_iy_step {n : ℕ} (iy y : Fin n → ℤ) (b : Fin n)
    (h : ∀ j, y j = 2 * iy j) :
    ∀ j, upd y b (y b + 2) j = 2 * upd iy b (iy b + 1) j := by
  intro j
  by_cases hj : j = b
  · rw [hj]
    have hb := h b
    simp only [upd_same]
    omega
  · simp [upd, hj, h j]

/-- Incrementing one entry grows the sum of squares by `2 * iy[b] + 1`. -/
theorem sum_sq_increment {n : ℕ} (iy : Fin n → ℤ) (b : Fin n) :
    (∑ j, (upd iy b (iy b + 1) j) ^ 2)
      = (∑ j, (iy j) ^ 2) + 2 * iy b + 1 := by
  have hb : b ∈ (Finset.univ : Finset (Fin n)) := Finset.mem_univ b
  have hupd : ∀ j ∈ Finset.univ.erase b, upd iy b (iy b + 1) j = iy j := by
    intro j hj
    exact upd_noteq iy b j (iy b + 1) (Finset.mem_erase.mp hj).1
  calc
    (∑ j, (upd iy b (iy b + 1) j) ^ 2)
        = (upd iy b (iy b + 1) b) ^ 2
            + ∑ j ∈ Finset.univ.erase b, (upd iy b (iy b + 1) j) ^ 2 := by
          rw [← Finset.add_sum_erase _ (fun j => (upd iy b (iy b + 1) j) ^ 2) hb]
    _ = (iy b + 1) ^ 2 + ∑ j ∈ Finset.univ.erase b, (iy j) ^ 2 := by
          rw [upd_same]
          congr 1
          apply Finset.sum_congr rfl
          intro j hj
          rw [hupd j hj]
    _ = (iy b) ^ 2 + ∑ j ∈ Finset.univ.erase b, (iy j) ^ 2 + 2 * iy b + 1 := by
          ring
    _ = (∑ j, (iy j) ^ 2) + 2 * iy b + 1 := by
          rw [← Finset.add_sum_erase _ (fun j => (iy j) ^ 2) hb]

/-- `yy` tracks `sum_j iy[j]^2` through one greedy step, given the
`y = 2 * iy` invariant. This is exactly the accumulator update in
`op_pvq_search`. -/
theorem yy_step {n : ℕ} (iy y : Fin n → ℤ) (b : Fin n) (yy : ℤ)
    (hyy : yy = ∑ j, (iy j) ^ 2) (hb : y b = 2 * iy b) :
    yy + 1 + y b = ∑ j, (upd iy b (iy b + 1) j) ^ 2 := by
  rw [sum_sq_increment iy b, ← hyy, hb]
  ring

/-! ### Rotation

`exp_rotation` returns early when `spread = 0`, so the factor table is only
indexed for `1 <= spread <= 3`. -/

/-- The rotation strength used by `exp_rotation`, indexed by `SPREAD_* - 1`. -/
def rotFactor (spread : ℕ) : ℕ := [15, 10, 5].getD (spread - 1) 0

/-- For every live spread the factor is one of the three published values, so
the table lookup in `exp_rotation` cannot go out of bounds. -/
theorem rotFactor_mem (spread : ℕ) (h1 : 1 ≤ spread) (h3 : spread ≤ 3) :
    rotFactor spread = 15 ∨ rotFactor spread = 10 ∨ rotFactor spread = 5 := by
  interval_cases spread <;> decide

/-! ### Band widths

`alg_quant` is never called with `n = 1` (that case goes to
`quant_band_n1`), so `icwrs`'s `j = n - 1` start is always at least `1`.
The table below is the pinned 48 kHz mode's `eBands`; every width is
positive, so the derived `n = m * (eBands[i+1] - eBands[i])` is at least `m`. -/

/-- `eBands` of the pinned 48 kHz mode. -/
def ebandList : List Nat :=
  [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100]

/-- Every band of the pinned mode has positive width. -/
theorem band_widths_pos (i : ℕ) (hi : i < 20) :
    ebandList.getD i 0 < ebandList.getD (i + 1) 0 := by
  interval_cases i <;> decide

end Pvq
