module

import Mathlib.Tactic

/-!
# Pinned CELT encoder configuration

`src/encoder.rs` is a CELT-only port with the mode fixed at 48 kHz, stereo,
20 ms, 320 kbps, complexity 10. Many conditions in `celt_encoder.c` and
`bands.c` therefore have constant truth values. This file records the pinned
configuration and proves those facts, so the dead branches can be removed
without guesswork.

Everything here mirrors source expressions one-for-one. The definitions keep
the original (pre-simplification) guard expressions, so each removal can be
re-checked against the theorem; the Rust now uses the pinned form directly.

* `src/encoder.rs`: `CHANNELS = 2`, `LM = 3`, `M = 8`, `N = 960`,
  `BITRES = 3`, `COMPLEXITY = 10`, `end = NB_EBANDS = 21`, `start = 0`.
* `src/bands.rs::quant_all_bands`: `channels = 2`, `theta_rdo` and the
  `disable_inv` reset.
* `src/alloc.rs::compute_allocation`: the skip / intensity / dual-stereo
  reserves and the per-band `b_bits` clamp.

The `lfe` facts refer to the C branches that the port dropped: the port is
equivalent to the C encoder with `st->lfe == 0`.
-/

namespace Encoder

/-! ### Pinned configuration -/

/-- Stereo. C uses `C = Y_ != NULL ? 2 : 1`; the port always supplies `Y`. -/
def channels : Nat := 2

/-- Long-block log size for 20 ms at 48 kHz. -/
def lm : Nat := 3

/-- Spectral bins per band unit: `M = 1 << LM`. -/
def m : Nat := 1 <<< lm

/-- Samples per channel per frame. -/
def frameSize : Nat := 960

/-- Number of coded bands: `mode->effEBands = mode->nbEBands = 21`. -/
def nbEbands : Nat := 21

/-- `eBands` of the static 48 kHz mode. -/
def ebandList : List Nat :=
  [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100]

def eband (i : Nat) : Nat := ebandList.getD i 0

/-- `st->start`. -/
def start : Nat := 0

/-- `st->end = mode->effEBands`. -/
def stop : Nat := nbEbands

/-- C's `BITRES`. -/
def bitres : Nat := 3

/-- `COMPLEXITY`. -/
def complexity : Nat := 10

/-- The port is the C encoder with LFE disabled. -/
def lfe : Bool := false

/-- `quant_all_bands` is called with `disable_inv = false`. -/
def disableInv : Bool := false

/-- `BITRATE`. -/
def bitrate : Nat := 320_000

theorem channels_eq : channels = 2 := rfl

theorem lm_eq : lm = 3 := rfl

theorem m_eq : m = 8 := by unfold m lm; decide

theorem frame_eq : m * 120 = frameSize := by unfold m lm frameSize; decide

theorem start_eq : start = 0 := rfl

theorem stop_eq : stop = nbEbands := rfl

theorem eband_start : eband start = 0 := by unfold eband ebandList start; decide

theorem eband_zero : eband 0 = 0 := by unfold eband ebandList; decide

theorem eband_stop : eband stop = 100 := by
  unfold eband ebandList stop nbEbands; decide

theorem eband_last : eband (stop - 1) = 78 := by
  unfold eband ebandList stop nbEbands; decide

/-! ### Branch facts

Each definition below is the exact boolean expression from the source before
simplification. The theorem after it is its value under the pinned
configuration, which is what the Rust now computes.
-/

/-- `theta_rdo = channels == 2 && !dual_stereo_in && complexity >= 8`. -/
def thetaRdo (dualStereo : Bool) : Bool :=
  decide (channels = 2) && (!dualStereo) && decide (complexity ≥ 8)

/-- With 10 complexity, `theta_rdo` is exactly the negation of dual stereo. -/
theorem thetaRdo_pinned (d : Bool) : thetaRdo d = !d := by
  cases d <;> decide

/-- `resynth = theta_rdo`. -/
def resynth (dualStereo : Bool) : Bool := thetaRdo dualStereo

theorem resynth_pinned (d : Bool) : resynth d = !d := thetaRdo_pinned d

/-- `if LM != 0 then dual_stereo = stereo_analysis(&x)`. -/
def computesDualStereo : Bool := decide (lm ≠ 0)

theorem computesDualStereo_pinned : computesDualStereo = true := by decide

/-- C: `effectiveBytes > 50 && LM >= 1 && !lfe`. -/
def dynallocFollowers (effectiveBytes : Nat) : Bool :=
  decide (effectiveBytes > 50) && decide (lm ≥ 1) && !lfe

theorem dynallocFollowers_pinned (b : Nat) :
    dynallocFollowers b = decide (b > 50) := by
  simp [dynallocFollowers, lm, lfe]

/-- C: `complexity >= 1 && !lfe` gates the transient analysis. -/
def transientEnabled : Bool := decide (complexity ≥ 1) && !lfe

theorem transientEnabled_pinned : transientEnabled = true := by decide

/-- `enable_tf_analysis = effective_bytes >= 15 * CHANNELS && COMPLEXITY >= 2`. -/
def enableTf (effectiveBytes : Nat) : Bool :=
  decide (effectiveBytes ≥ 15 * channels) && decide (complexity ≥ 2)

theorem enableTf_pinned (b : Nat) : enableTf b = decide (b ≥ 30) := by
  simp [enableTf, channels, complexity]

/-- `anti_collapse_rsv = is_transient && LM >= 2 && bits >= (LM + 2) << BITRES`. -/
def antiCollapseReserve (transient : Bool) (bits : Nat) : Nat :=
  if transient && decide (lm ≥ 2) && decide (bits ≥ (lm + 2) <<< bitres) then
    1 <<< bitres
  else 0

theorem antiCollapseReserve_pinned (t : Bool) (bits : Nat) :
    antiCollapseReserve t bits = (if t && decide (bits ≥ 40) then 8 else 0) := by
  unfold antiCollapseReserve
  have h : (lm + 2) <<< bitres = 40 := by unfold lm bitres; decide
  have h2 : (1:Nat) <<< bitres = 8 := by unfold bitres; decide
  have h3 : decide (lm ≥ 2) = true := by unfold lm; decide
  simp only [h, h2, h3, Bool.and_true]

/-- `inv = itheta > 8192 && !disable_inv`. -/
def invDecision (itheta : Nat) (disabled : Bool) : Bool :=
  decide (itheta > 8192) && !disabled

theorem invDecision_pinned (i : Nat) :
    invDecision i disableInv = decide (i > 8192) := by
  simp [invDecision, disableInv]

/-- `if disable_inv then inv = false`. -/
def invReset (inv disabled : Bool) : Bool :=
  if disabled then false else inv

theorem invReset_pinned (b : Bool) : invReset b disableInv = b := by
  simp [invReset, disableInv]

/-- The mono branch: `channels == 1`. -/
def isMono : Bool := decide (channels = 1)

theorem isMono_pinned : isMono = false := by decide

theorem stereo_reachable : channels > 1 := by decide

/-- `spread` fallback: `if COMPLEXITY == 0 then SPREAD_NONE else SPREAD_NORMAL`.
    `SPREAD_NONE = 0`, `SPREAD_NORMAL = 1`. -/
def spreadFallback : Nat := if complexity = 0 then 0 else 1

theorem spreadFallback_pinned : spreadFallback = 1 := by decide

/-- `short_blocks != 0 || COMPLEXITY < 3 || nb_available_bytes < 10 * CHANNELS`. -/
def spreadIsAnalysed (shortBlocks : Bool) (nbAvailable : Nat) : Bool :=
  shortBlocks || decide (complexity < 3) || decide (nbAvailable < 10 * channels)

theorem spreadIsAnalysed_pinned (s : Bool) (b : Nat) :
    spreadIsAnalysed s b = (s || decide (b < 20)) := by
  unfold spreadIsAnalysed
  have h : decide (complexity < 3) = false := by unfold complexity; decide
  have h2 : 10 * channels = 20 := by unfold channels; decide
  simp only [h, h2, Bool.or_false]

/-- C's prefilter guard `(lfe && nbAvailableBytes > 3) || nbAvailableBytes > 12*C`.
    With `lfe = false` the LFE arm vanishes; `src/encoder.rs` additionally
    requires `!silence && COMPLEXITY >= 5`. -/
def prefilterLfeGuard (nbAvailable : Nat) : Bool :=
  (lfe && decide (nbAvailable > 3)) || decide (nbAvailable > 12 * channels)

theorem prefilterLfeGuard_pinned (b : Nat) :
    prefilterLfeGuard b = decide (b > 24) := by
  simp [prefilterLfeGuard, lfe, channels]

/-! ### VBR is always on

`den = 48000 >> BITRES`, `vbr_rate = (BITRATE * N + den/2) / den`. -/
def den : Nat := 48000 >>> bitres

def vbrRate : Nat := (bitrate * frameSize + den / 2) / den

theorem den_eq : den = 6000 := by unfold den bitres; decide

theorem vbrRate_eq : vbrRate = 51200 := by
  unfold vbrRate bitrate frameSize den bitres; decide

theorem vbrRate_pos : 0 < vbrRate := by
  unfold vbrRate bitrate frameSize den bitres; decide

/-! ### `start = 0` and `end = nbEBands` -/

/-- `norm_offset = m * eBands[start]`. -/
def normOffset (st : Nat) : Nat := m * eband st

theorem normOffset_pinned : normOffset start = 0 := by
  simp [normOffset, eband_start]

/-- `special_hybrid_folding` copies `M * eBands[start]` samples. -/
def hybridFoldSamples : Nat := m * eband start

theorem hybridFoldSamples_pinned : hybridFoldSamples = 0 := by
  simp [hybridFoldSamples, eband_start]

/-- `norm_len = m * eBands[nbEBands - 1] - norm_offset`. With `end = nbEBands`
and `start = 0`, this is the end of the last band whose output is written. -/
def normLen : Nat := m * eband (nbEbands - 1) - normOffset start

theorem normLen_pinned : normLen = m * eband (stop - 1) := by
  simp [normLen, normOffset, nbEbands, stop, start, eband_zero]

/-- No band index in `[start, end)` is beyond `effEBands = nbEBands = end`, so
the C aliasing branch `i >= effEBands` is unreachable. -/
theorem aliasing_unreachable (i effE : Nat) (hi : i < stop) (he : stop = effE) :
    ¬ (effE ≤ i) := by
  omega

/-- The `i == start + 1` test is the first non-zero band only because
`start = 0`. -/
theorem start_succ_pinned (i : Nat) : (i = start + 1 ↔ i = 1) := by
  simp [start]

/-! ### Allocation reserves

`compute_allocation` reserves `skip_rsv`, `intensity_rsv` and
`dual_stereo_rsv` from the quantiser budget before distributing pulses. The
first theorem says the reserves never exceed the available budget; the second
gives their exact values for the pinned mode. -/

/-- One reserve unit: `1 << BITRES`. -/
def reserveUnit : Nat := 1 <<< bitres

theorem reserveUnit_eq : reserveUnit = 8 := by unfold reserveUnit bitres; decide

/-- `LOG2_FRAC_TABLE` from `src/alloc.rs`. -/
def log2Frac : List Nat :=
  [0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37]

def log2FracAt (k : Nat) : Nat := log2Frac.getD k 0

/-- `LOG2_FRAC_TABLE[end - start]` for the pinned mode. -/
theorem log2Frac_pinned : log2FracAt (stop - start) = 36 := by
  unfold log2FracAt log2Frac stop start nbEbands; decide

/-- Reserves taken after the skip reserve has been removed. -/
structure Tail where
  intensity : Nat
  dual : Nat
  remaining : Nat

/-- The `c == 2` reserve logic, mirroring `compute_allocation`. -/
def tailReserve (total c st en : Nat) : Tail :=
  if c = 2 then
    let ir := log2FracAt (en - st)
    if ir > total then
      { intensity := 0, dual := 0, remaining := total }
    else
      let afterIntensity := total - ir
      if reserveUnit ≤ afterIntensity then
        { intensity := ir, dual := reserveUnit,
          remaining := afterIntensity - reserveUnit }
      else
        { intensity := ir, dual := 0, remaining := afterIntensity }
  else
    { intensity := 0, dual := 0, remaining := total }

/-- The reserves fit in the budget handed to them. -/
theorem tailReserve_fits (total c st en : Nat) :
    (tailReserve total c st en).intensity + (tailReserve total c st en).dual ≤ total := by
  unfold tailReserve
  dsimp only
  by_cases hc : c = 2
  · simp only [ite_eq_left hc]
    by_cases hi : log2FracAt (en - st) > total
    · simp only [ite_eq_left hi]
      omega
    · simp only [ite_eq_right hi]
      by_cases hd : reserveUnit ≤ total - log2FracAt (en - st)
      · simp only [ite_eq_left hd]
        omega
      · simp only [ite_eq_right hd]
        omega
  · simp only [ite_eq_right hc]
    omega

/-- Full reserve accounting, including the skip reserve. -/
structure Reserves where
  skip : Nat
  intensity : Nat
  dual : Nat
  remaining : Nat

def reserve (totalIn c st en : Nat) : Reserves :=
  let s := if reserveUnit ≤ totalIn then reserveUnit else 0
  let r := tailReserve (totalIn - s) c st en
  { skip := s, intensity := r.intensity, dual := r.dual, remaining := r.remaining }

/-- The total reserved bits never exceed the input budget, so the amount left
for pulse distribution cannot underflow. -/
theorem reserve_fits (totalIn c st en : Nat) :
    (reserve totalIn c st en).skip + (reserve totalIn c st en).intensity +
      (reserve totalIn c st en).dual ≤ totalIn := by
  unfold reserve
  dsimp only
  by_cases hs : reserveUnit ≤ totalIn
  · simp only [ite_eq_left hs]
    have h := tailReserve_fits (totalIn - reserveUnit) c st en
    omega
  · simp only [ite_eq_right hs, Nat.sub_zero]
    have h := tailReserve_fits totalIn c st en
    omega

/-- At the pinned mode and any budget of at least 52 quantiser bits, the
reserves are exactly 8 (skip) + 36 (intensity) + 8 (dual stereo), leaving
`totalIn - 52` for the pulse allocator. -/
theorem reserve_pinned (totalIn : Nat) (h : 52 ≤ totalIn) :
    let r := reserve totalIn channels start stop
    r.skip = 8 ∧ r.intensity = 36 ∧ r.dual = 8 ∧ r.remaining = totalIn - 52 := by
  have hs : (8:Nat) ≤ totalIn := by omega
  have hir : ¬ (36 > totalIn - 8) := by omega
  have hdu : (8:Nat) ≤ totalIn - 8 - 36 := by omega
  have hch : channels = 2 := rfl
  have hl : log2FracAt (stop - start) = 36 := log2Frac_pinned
  unfold reserve tailReserve
  dsimp only
  simp only [reserveUnit_eq, hl]
  simp only [ite_eq_left hs, ite_eq_left hch, ite_eq_right hir, ite_eq_left hdu]
  refine ⟨trivial, trivial, trivial, ?_⟩
  omega

/-! ### Quantiser budget

`total_bits` passed to `quant_all_bands` is `bytes * (8 << BITRES) - rsv`
(eighth-bits). The allocation budget subtracts the fractional bits already
spent plus one guard bit. -/

/-- Quantiser bits per byte with `BITRES = 3`: `8 << 3 = 64`. -/
def perByteBits : Nat := 8 <<< bitres

theorem perByteBits_eq : perByteBits = 64 := by unfold perByteBits bitres; decide

/-- `nb_compressed_bytes * (8 << BITRES) - anti_collapse_rsv`. -/
def quantBudget (bytes anti : Nat) : Nat := bytes * perByteBits - anti

/-- `((nb_compressed_bytes * 8) << BITRES) - tell_frac - 1 - anti_collapse_rsv`. -/
def allocBudget (bytes tellFrac anti : Nat) : Nat :=
  bytes * perByteBits - tellFrac - 1 - anti

/-- The allocation budget is exactly the quantiser budget minus the spent
fractional bits and the guard bit. -/
theorem allocBudget_eq (bytes tellFrac anti : Nat) :
    allocBudget bytes tellFrac anti = quantBudget bytes anti - tellFrac - 1 := by
  unfold allocBudget quantBudget
  rw [perByteBits_eq]
  omega

/-- Per-band pulse budget: zero once `i` reaches the coded-band limit, else
`max 0 (min 16383 (min (remaining + 1) (pulses + balance / min 3 (coded_bands - i))))`. -/
def bBits (remaining balance codedBands i pulses : Int) : Int :=
  if i ≤ codedBands - 1 then
    max 0
      (min 16383
        (min (remaining + 1) (pulses + balance / min 3 (codedBands - i))))
  else 0

/-- The per-band pulse budget is always in `[0, 16383]`, independently of the
balance and remaining-bit inputs. -/
theorem bBits_bounds (remaining balance codedBands i pulses : Int) :
    0 ≤ bBits remaining balance codedBands i pulses ∧
      bBits remaining balance codedBands i pulses ≤ 16383 := by
  unfold bBits
  split
  · constructor
    · exact le_max_left _ _
    · exact max_le (by decide) (min_le_left _ _)
  · constructor <;> decide

/-- `remaining_bits = total_bits - tell - 1` is non-negative whenever the
quantiser is within its budget. -/
theorem remainingBits_nonneg (total tell : Int) (h : tell + 1 ≤ total) :
    0 ≤ total - tell - 1 := by
  omega

end Encoder
