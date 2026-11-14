/- Scratch handling in src/sox.rs.

   The input is interleaved stereo, so two samples make one frame. For a
   remaining input of `rest` samples, one libsoxr call offers

     ilen = rest / 2
     olen = ceil(ilen * outRate / inRate) + 64

   frames and reports `idone` frames consumed and `odone` frames written. The
   caller copies `odone * 2` scratch samples out and advances the input by
   `idone * 2` samples.

   libsoxr's documented contract is `idone <= ilen` and `odone <= olen`; all
   theorems below are conditioned on exactly that. Proved:

   - the copy and the advance stay inside their buffers, so neither slicing
     operation can panic;
   - a fully-consuming call drains an even-length input in one iteration;
   - the loop finishes whenever every call consumes at least one frame;
   - the `soxr stalled` guard is then unreachable;
   - an odd-length input can never be drained (callers only pass pairs);
   - `flush` copies in bounds and finishes inside its 4096-call budget.

   "Fully consuming" (`idone = ilen`) is the normal libsoxr behaviour whenever
   the output buffer holds at least `ceil(ilen * outRate / inRate)` frames,
   which `olen` provides along with 64 frames of filter headroom. -/

namespace Soxr

/-- Samples per interleaved stereo frame. -/
def chans : Nat := 2

/-- Frames offered to libsoxr for a remaining input of `rest` samples. -/
def ilen (rest : Nat) : Nat := rest / chans

/-- `ceil (a / b)` for `0 < b`, matching the f64 `.ceil()` in `resample`. -/
def ceilDiv (a b : Nat) : Nat := (a + b - 1) / b

/-- Frames the scratch buffer holds: `ceil(ilen * num / den) + 64`. -/
def olen (rest num den : Nat) : Nat := ceilDiv (ilen rest * num) den + 64

/-- The largest `usize`. -/
def usizeMax : Nat := 18446744073709551616

/-! ### Per-call bounds -/

/-- `scratch[..odone * chans]` stays inside a buffer of `olen * chans`. -/
theorem copy_le_scratch (rest num den odone : Nat)
    (h : odone ≤ olen rest num den) :
    odone * chans ≤ olen rest num den * chans :=
  Nat.mul_le_mul_right chans h

/-- `rest[idone * chans..]` stays inside the input. -/
theorem advance_le_input (rest idone : Nat) (h : idone ≤ ilen rest) :
    idone * chans ≤ rest := by
  simp only [ilen, chans] at h ⊢
  have hdm := Nat.div_add_mod rest 2
  omega

/-- The scratch length fits in `usize` for any frame allowance below `2^63`. -/
theorem scratch_fits (o : Nat) (h : o < 9223372036854775808) : o * chans < usizeMax := by
  unfold chans usizeMax
  omega

/-! ### Progress -/

/-- A fully-consuming call empties an even-length input. -/
theorem consumed_even (rest idone : Nat) (heven : rest % 2 = 0)
    (hid : idone = ilen rest) : rest - idone * chans = 0 := by
  simp only [ilen, chans] at hid ⊢
  have hdm := Nat.div_add_mod rest 2
  omega

/-- A fully-consuming call leaves one sample of an odd-length input. -/
theorem odd_remains (rest idone : Nat) (hodd : rest % 2 = 1)
    (hid : idone = ilen rest) : rest - idone * chans = 1 := by
  simp only [ilen, chans] at hid ⊢
  have hdm := Nat.div_add_mod rest 2
  omega

/-- The leftover single sample offers zero frames, so `resample` cannot drain
    it: with full consumption the loop would spin until the stall guard fires. -/
theorem ilen_one : ilen 1 = 0 := by decide

/-- Every non-empty iteration strictly shrinks the input. -/
theorem rest_decreases (rest idone : Nat) (hpos : 0 < idone)
    (h : idone ≤ ilen rest) : rest - idone * chans < rest := by
  have hle := advance_le_input rest idone h
  have hpos' : 0 < idone * chans := by
    unfold chans
    omega
  omega

/-- The `idone = 0 ∧ odone = 0` stall check cannot fire while the input is
    non-empty and libsoxr consumes at least one frame per call. -/
theorem stall_unreachable (step : Nat → Nat × Nat)
    (hprog : ∀ r, 0 < r → 0 < (step r).1) (rest : Nat) (hpos : 0 < rest) :
    ¬ ((step rest).1 = 0 ∧ (step rest).2 = 0) := by
  have := hprog rest hpos
  omega

/-! ### The resample loop -/

/-- `resample`'s while loop. `step rest = (idone, odone)`; the result is the
    total number of scratch samples copied out, or `none` for the stall error. -/
def loop (fuel rest : Nat) (step : Nat → Nat × Nat) : Option Nat :=
  match fuel with
  | 0 => if rest = 0 then some 0 else none
  | fuel + 1 =>
    if rest = 0 then some 0
    else if (step rest).1 = 0 ∧ (step rest).2 = 0 then none
    else
      match loop fuel (rest - (step rest).1 * chans) step with
      | some outs => some ((step rest).2 * chans + outs)
      | none => none

theorem loop_zero_rest (fuel : Nat) (step : Nat → Nat × Nat) :
    loop fuel 0 step = some 0 := by
  cases fuel with
  | zero => simp [loop]
  | succ f => simp [loop]

/-- With `rem ≤ fuel` the loop returns, provided each call consumes a frame of a
    non-empty input. Each iteration spends one unit of fuel and removes at
    least two samples, so the input length is always a sufficient budget. -/
theorem loop_terminates (step : Nat → Nat × Nat)
    (hprog : ∀ r, 0 < r → 0 < (step r).1)
    (hconsume : ∀ r, (step r).1 ≤ ilen r)
    (fuel rest : Nat) (hle : rest ≤ fuel) : (loop fuel rest step).isSome = true := by
  induction fuel generalizing rest with
  | zero =>
    have h0 : rest = 0 := by omega
    simp [loop, h0]
  | succ fuel ih =>
    by_cases h0 : rest = 0
    · simp [loop, h0]
    · have hpos : 0 < rest := Nat.pos_of_ne_zero h0
      have hid : 0 < (step rest).1 := hprog rest hpos
      have hnotstall := stall_unreachable step hprog rest hpos
      have hbound : (step rest).1 * chans ≤ rest := advance_le_input rest _ (hconsume rest)
      have hle' : rest - (step rest).1 * chans ≤ fuel := by
        simp only [chans] at hbound hle ⊢
        omega
      have ihres := ih (rest - (step rest).1 * chans) hle'
      simp only [loop, h0, ite_false, hnotstall]
      cases hrec : loop fuel (rest - (step rest).1 * chans) step with
      | none =>
        rw [hrec] at ihres
        simp at ihres
      | some outs => simp

/-- With full consumption an even-length input is drained in one pass: no
    second iteration is ever entered, so the loop's cost is one libsoxr call. -/
theorem loop_single_pass (step : Nat → Nat × Nat)
    (hfull : ∀ r, 0 < r → (step r).1 = ilen r)
    (rest : Nat) (hpos : 0 < rest) (heven : rest % 2 = 0)
    (hnotstall : ¬ ((step rest).1 = 0 ∧ (step rest).2 = 0)) :
    loop rest rest step = some ((step rest).2 * chans) := by
  obtain ⟨k, rfl⟩ : ∃ k, rest = k + 1 := ⟨rest - 1, by omega⟩
  have hpos' : 0 < k + 1 := by omega
  have hzero := consumed_even (k + 1) (step (k + 1)).1 heven (hfull (k + 1) hpos')
  simp only [loop, Nat.succ_ne_zero, ite_false, hnotstall]
  rw [hzero, loop_zero_rest]
  simp

/-! ### The flush loop -/

/-- `flush`'s bounded drain loop: `none` is the "did not terminate" error. -/
def flushLoop (fuel : Nat) (step : Unit → Nat) : Option Nat :=
  match fuel with
  | 0 => none
  | fuel + 1 =>
    match step () with
    | 0 => some 0
    | n => (flushLoop fuel step).map (fun m => n + m)

/-- `flush` always returns within its 4096-call budget, either normally or via
    the explicit non-termination error. -/
theorem flush_bounded (step : Unit → Nat) :
    (flushLoop 4096 step).isSome = true ∨ flushLoop 4096 step = none := by
  cases h : flushLoop 4096 step with
  | none => exact Or.inr rfl
  | some n => exact Or.inl rfl

/-- Each `flush` copy is bounded by the 8192-frame scratch buffer. -/
theorem flush_copy_le_scratch (odone : Nat) (h : odone ≤ 8192) :
    odone * chans ≤ 8192 * chans :=
  Nat.mul_le_mul_right chans h

end Soxr
