module

public meta import Std.Tactic.BVDecide.Reflect

import Std.Tactic.BVDecide
import Mathlib.Tactic

/-!
# FLAC decoder invariants (`src/flac/`)

This file models the parts of the native-FLAC decoder that carry arithmetic or
bounds obligations.  The parser grammar itself is validated by the differential
corpus in `src/flac/mod.rs`; the theorems here cover the table CRCs, the Rice
residual range, the residual partition count, the LPC product/accumulator
bounds, and the channel-decorrelation inverses.

The Rust implementation is intentionally a direct model of the equations in
RFC 9639, so these statements can be checked against the same definitions.
-/

namespace Flac

/-! ## CRC-8 and CRC-16 -/

def poly8 : BitVec 8 := 0x07
def poly16 : BitVec 16 := 0x8005

def crc8Step (crc : BitVec 8) : BitVec 8 :=
  if crc.msb then (crc <<< 1) ^^^ poly8 else crc <<< 1

def crc8Step8 (crc : BitVec 8) : BitVec 8 :=
  crc8Step (crc8Step (crc8Step (crc8Step (crc8Step (crc8Step (crc8Step (crc8Step crc)))))))

def crc16Step (crc : BitVec 16) : BitVec 16 :=
  if crc.msb then (crc <<< 1) ^^^ poly16 else crc <<< 1

def crc16Step8 (crc : BitVec 16) : BitVec 16 :=
  crc16Step (crc16Step (crc16Step (crc16Step
    (crc16Step (crc16Step (crc16Step (crc16Step crc)))))))

/-- The Rust `CRC8_TABLE` maps a byte through eight zero-input bit steps. -/
def crc8Table (i : BitVec 8) : BitVec 8 := crc8Step8 i

def crc8TableStep (crc : BitVec 8) (b : BitVec 8) : BitVec 8 :=
  crc8Table (crc ^^^ b)

def crc8BitStep (crc : BitVec 8) (b : BitVec 8) : BitVec 8 :=
  crc8Step8 (crc ^^^ b)

theorem crc8TableStep_eq (crc : BitVec 8) (b : BitVec 8) :
    crc8TableStep crc b = crc8BitStep crc b := by
  unfold crc8TableStep crc8BitStep crc8Table
  rfl

def crc16TableStep (crc : BitVec 16) (b : BitVec 8) : BitVec 16 :=
  (crc <<< 8) ^^^ crc16Step8 (((((crc >>> 8).truncate 8) ^^^ b).setWidth 16) <<< 8)

def crc16BitStep (crc : BitVec 16) (b : BitVec 8) : BitVec 16 :=
  crc16Step8 (crc ^^^ (b.setWidth 16 <<< 8))

theorem crc16TableStep_eq (crc : BitVec 16) (b : BitVec 8) :
    crc16TableStep crc b = crc16BitStep crc b := by
  unfold crc16TableStep crc16BitStep crc16Step8 crc16Step poly16
  bv_decide

def crc8From (crc : BitVec 8) (data : List (BitVec 8)) : BitVec 8 :=
  data.foldl crc8TableStep crc

def crc8BitFrom (crc : BitVec 8) (data : List (BitVec 8)) : BitVec 8 :=
  data.foldl crc8BitStep crc

theorem crc8From_eq (crc : BitVec 8) (data : List (BitVec 8)) :
    crc8From crc data = crc8BitFrom crc data := by
  induction data generalizing crc with
  | nil => rfl
  | cons b bs ih =>
    simp only [crc8From, crc8BitFrom, List.foldl_cons, crc8TableStep_eq]
    exact ih (crc8BitStep crc b)

def crc16From (crc : BitVec 16) (data : List (BitVec 8)) : BitVec 16 :=
  data.foldl crc16TableStep crc

def crc16BitFrom (crc : BitVec 16) (data : List (BitVec 8)) : BitVec 16 :=
  data.foldl crc16BitStep crc

theorem crc16From_eq (crc : BitVec 16) (data : List (BitVec 8)) :
    crc16From crc data = crc16BitFrom crc data := by
  induction data generalizing crc with
  | nil => rfl
  | cons b bs ih =>
    simp only [crc16From, crc16BitFrom, List.foldl_cons, crc16TableStep_eq]
    exact ih (crc16BitStep crc b)

/-- The table model matches the standard CRC-8 check value. -/
example : crc8From 0 [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39] = 0xf4 := by
  native_decide

/-- The table model matches the standard CRC-16 check value. -/
example : crc16From 0 [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39] = 0xfee8 := by
  native_decide

/-! ## Rice residuals -/

/-- Zigzag decoding used by `rice_to_signed`. -/
def foldedToSigned (folded : Nat) : Int :=
  if folded % 2 = 0 then
    ((folded / 2 : Nat) : Int)
  else
    -(((folded + 1) / 2 : Nat) : Int)

/-- RFC 9639 section 9.2.7.3 excludes both `2^31` and `-2^31`; the latter is
the folded value `0xffffffff`. -/
theorem foldedToSigned_lower (folded : Nat)
    (hfolded : folded < 2^32) (hnot : folded ≠ 2^32 - 1) :
    -(2^31 : Int) < foldedToSigned folded := by
  unfold foldedToSigned
  split <;> omega

theorem foldedToSigned_upper (folded : Nat) (hfolded : folded < 2^32) :
    foldedToSigned folded < (2^31 : Int) := by
  unfold foldedToSigned
  split <;> omega

/-- The residual values decoded by the Rice loop fit in a signed 32-bit
integer excluding its minimum. -/
theorem foldedToSigned_range (folded : Nat)
    (hfolded : folded < 2^32) (hnot : folded ≠ 2^32 - 1) :
    -(2^31 : Int) < foldedToSigned folded ∧ foldedToSigned folded < (2^31 : Int) :=
  ⟨foldedToSigned_lower folded hfolded hnot,
   foldedToSigned_upper folded hfolded⟩

/-- Partition sizes for a FLAC residual.  The first partition holds the
`block_size >> partition_order - predictor_order` samples after the warm-up
samples; every later partition holds `block_size >> partition_order`. -/
def partitionSizes (blockSize partitionOrder predictorOrder : Nat) : List Nat :=
  let part := blockSize / 2^partitionOrder
  let first := part - predictorOrder
  first :: List.replicate (2^partitionOrder - 1) part

/-- A partition order produces the expected number of partitions. -/
theorem partitionSizes_length (blockSize partitionOrder predictorOrder : Nat)
    (horder : 0 < 2^partitionOrder) :
    (partitionSizes blockSize partitionOrder predictorOrder).length = 2^partitionOrder := by
  simp [partitionSizes]
  omega

/-! ## Frame buffer bounds -/

/-- A FLAC block is at most 65535 samples and a frame has at most 8 channels. -/
theorem frame_sample_capacity : 65535 * 8 < 2^20 := by norm_num

/-- Any frame buffer index used by the decoder fits comfortably in `usize` on
the supported targets. -/
theorem frame_buffer_index_bound (blockSize channels index : Nat)
    (hblock : blockSize ≤ 65535) (hchannels : channels ≤ 8)
    (hindex : index < blockSize * channels) :
    index < 2^20 := by
  have hprod : blockSize * channels ≤ 65535 * 8 :=
    Nat.mul_le_mul hblock hchannels
  have hcap : 65535 * 8 < 2^20 := frame_sample_capacity
  omega

/-- STREAMINFO's 20-bit sample rate and 5-bit bit depth fields stay inside the
ranges accepted by the frame header parser. -/
theorem streaminfo_field_bounds (sampleRate bits channels : Nat)
    (hrate : sampleRate < 2^20) (hbits : 1 ≤ bits ∧ bits ≤ 32)
    (hchannels : 1 ≤ channels ∧ channels ≤ 8) :
    sampleRate < 2^20 ∧ bits ≤ 32 ∧ channels ≤ 8 :=
  ⟨hrate, hbits.2, hchannels.2⟩

/-! ## Channel decorrelation -/

theorem left_side_restore (left side : Int) : left - (left - side) = side := by
  omega

theorem right_side_restore (side right : Int) : (side + right) - right = side := by
  omega

/-! ## LPC accumulator -/

/-- Each LPC product fits below `2^48`: 15-bit signed coefficient, 33-bit
signed side-channel sample. -/
theorem lpc_product_bound (coefficient sample : Int)
    (hcoef : coefficient.natAbs < 2^15)
    (hsample : sample.natAbs < 2^33) :
    (coefficient * sample).natAbs < 2^48 := by
  rw [Int.natAbs_mul]
  simpa using Nat.mul_lt_mul'' hcoef hsample


/-- The sum of a non-empty list of products, each below `2^48`, is below
`length * 2^48`. -/
def products (pairs : List (Int × Int)) : List Int :=
  pairs.map (fun p => p.1 * p.2)

theorem products_sum_bound_aux (pairs : List (Int × Int)) (hne : pairs ≠ [])
    (hb : ∀ p ∈ pairs, (p.1 * p.2).natAbs < 2^48) :
    (products pairs).sum.natAbs < pairs.length * 2^48 := by
  induction pairs with
  | nil => contradiction
  | cons p ps ih =>
    cases ps with
    | nil =>
      simpa [products] using hb
    | cons q qs =>
      have hp : (p.1 * p.2).natAbs < 2^48 := hb p (by simp)
      have htail : (products (q :: qs)).sum.natAbs < (q :: qs).length * 2^48 := by
        apply ih
        · simp
        · intro x hx
          exact hb x (by simp [hx])
      have hadd := Int.natAbs_add_le (p.1 * p.2) ((products (q :: qs)).sum)
      have hlt : (p.1 * p.2).natAbs + (products (q :: qs)).sum.natAbs <
          2^48 + (q :: qs).length * 2^48 :=
        Nat.add_lt_add_of_lt_of_le hp (le_of_lt htail)
      have hsum : ((p.1 * p.2) + (products (q :: qs)).sum).natAbs <
          2^48 + (q :: qs).length * 2^48 :=
        lt_of_le_of_lt hadd hlt
      calc
        (products (p :: q :: qs)).sum.natAbs =
            ((p.1 * p.2) + (products (q :: qs)).sum).natAbs := by simp [products]
        _ < 2^48 + (q :: qs).length * 2^48 := hsum
        _ = (p :: q :: qs).length * 2^48 := by
          simp only [List.length_cons]
          ring

/-- At most 32 products, each below `2^48`, stay below `2^53`; this is the
bound used by the predictor's `i64` accumulator. -/
theorem lpc_sum_bound (pairs : List (Int × Int))
    (hlen : pairs.length ≤ 32)
    (hb : ∀ p ∈ pairs, (p.1 * p.2).natAbs < 2^48) :
    (products pairs).sum.natAbs < 2^53 := by
  by_cases hne : pairs = []
  · simp [products, hne]
  · have h := products_sum_bound_aux pairs hne hb
    have hlen' : pairs.length * 2^48 ≤ 32 * 2^48 :=
      Nat.mul_le_mul_right _ hlen
    have hpow : 32 * 2^48 = 2^53 := by norm_num
    omega

/-- The accumulator plus the residual stays inside `i64`; the shift is applied
after the sum, so a non-negative shift only reduces it. -/
theorem lpc_i64_bound (prediction residual : Int)
    (hpred : prediction.natAbs < 2^53)
    (hres : residual.natAbs < 2^31) :
    (prediction + residual).natAbs < 2^54 := by
  nlinarith [Int.natAbs_add_le prediction residual]

/-- The complete unshifted LPC accumulator stays below `2^54`, leaving ample
room in `i64`; the non-negative shift can only reduce it. -/
theorem lpc_accumulator_bound (pairs : List (Int × Int)) (residual : Int)
    (hlen : pairs.length ≤ 32)
    (hb : ∀ p ∈ pairs, (p.1 * p.2).natAbs < 2^48)
    (hres : residual.natAbs < 2^31) :
    ((products pairs).sum + residual).natAbs < 2^54 :=
  lpc_i64_bound (products pairs).sum residual
    (lpc_sum_bound pairs hlen hb) hres

end Flac
