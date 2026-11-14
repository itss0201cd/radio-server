/-!
# Range-coder interval arithmetic (Opus / RFC 6716 §4.1)

This file proves the part of the range coder where a silent bug is invisible in
a smoke test: the interval update shared by the encoder and decoder.

The model follows `src/range_coder.rs`:

* `r = rng / ft`;
* for `fl = 0` the symbol owns `[lo, lo + rng - r*(ft-fh))`, i.e. it absorbs
  the division remainder;
* for `fl > 0` it owns `[lo + rng - r*(ft-fl), lo + rng - r*(ft-fh))`;
* the decoder's state `val` is the mirror of the encoded point,
  `val = lo + rng - 1 - x`, so a point in the sub-interval maps to
  `val ∈ [r*(ft-fh), r*(ft-fl))`;
* `decFs` recovers the cumulative frequency, and `updVal`/`updRng` re-establish
  `0 ≤ val < rng` for the next symbol.

Byte renormalisation, carry propagation and the raw-bit tail are not modelled
here; they are pinned by the byte-exact comparison with the reference C entropy
coder in the Rust tests.
-/

namespace RangeCoder

/-- Encoder interval lower bound, in the parent coordinate system. -/
def encLo (lo rng fl _fh ft : Nat) : Nat :=
  if fl = 0 then lo else lo + (rng - rng / ft * (ft - fl))

/-- Encoder interval width. -/
def encRng (rng fl fh ft : Nat) : Nat :=
  if fl = 0 then rng - rng / ft * (ft - fh)
  else rng / ft * (fh - fl)

/-- Decoder's cumulative-frequency output. -/
def decFs (rng ft val : Nat) : Nat :=
  ft - min (val / (rng / ft) + 1) ft

/-- Decoder value after `update`. -/
def updVal (rng ft val _fl fh : Nat) : Nat :=
  val - rng / ft * (ft - fh)

/-- Decoder range after `update`. -/
def updRng (rng fl fh ft : Nat) : Nat :=
  if fl = 0 then rng - rng / ft * (ft - fh)
  else rng / ft * (fh - fl)

/-- `r*(ft-fl) = r*(fh-fl) + r*(ft-fh)`. -/
theorem mul_split {r ft fl fh : Nat} (hfl : fl ≤ fh) (hfh : fh ≤ ft) :
    r * (ft - fl) = r * (fh - fl) + r * (ft - fh) := by
  rw [← Nat.mul_add]
  congr 1
  omega

/-! ### The encoder narrows the interval -/

theorem enc_within_zero (lo rng fh ft : Nat) :
    encLo lo rng 0 fh ft + encRng rng 0 fh ft ≤ lo + rng := by
  simp [encLo, encRng]

theorem enc_within_pos (lo rng fl fh ft : Nat)
    (hfl0 : 0 < fl) (hfl : fl < fh) (hfh : fh ≤ ft) :
    encLo lo rng fl fh ft + encRng rng fl fh ft ≤ lo + rng := by
  have hne : ¬ fl = 0 := by omega
  simp [encLo, encRng, hne]
  have hle : rng / ft * (ft - fl) ≤ rng :=
    Nat.le_trans (Nat.mul_le_mul_left _ (Nat.sub_le ft fl)) (Nat.div_mul_le_self rng ft)
  have hsplit := mul_split (Nat.le_of_lt hfl) hfh (r := rng / ft)
  omega

theorem encRng_pos_zero {rng fh ft : Nat}
    (hft : 0 < ft) (hfh0 : 0 < fh) (hfh : fh ≤ ft) (hrng : ft ≤ rng) :
    0 < encRng rng 0 fh ft := by
  simp [encRng]
  have hrpos : 0 < rng / ft := Nat.div_pos hrng hft
  have hle : rng / ft * (ft - fh) ≤ rng / ft * (ft - 1) :=
    Nat.mul_le_mul_left _ (by omega)
  have hsub : rng / ft * (ft - 1) = rng / ft * ft - rng / ft := by
    rw [Nat.mul_sub_left_distrib]
    simp
  have htop : rng / ft * ft ≤ rng := Nat.div_mul_le_self rng ft
  have hlt : rng / ft * ft - rng / ft < rng / ft * ft :=
    Nat.sub_lt (Nat.mul_pos hrpos hft) hrpos
  omega

theorem encRng_pos_pos {rng fl fh ft : Nat}
    (hft : 0 < ft) (hfl0 : 0 < fl) (hfl : fl < fh) (hrng : ft ≤ rng) :
    0 < encRng rng fl fh ft := by
  have hne : ¬ fl = 0 := by omega
  simp [encRng, hne]
  exact Nat.mul_pos (Nat.div_pos hrng hft) (by omega)

/-! ### The decoder selects the encoded symbol -/

theorem decFs_zero_lt {rng ft val fh : Nat}
    (hft : 0 < ft) (hfh0 : 0 < fh) (hfh : fh ≤ ft) (hrng : ft ≤ rng)
    (hlo : rng / ft * (ft - fh) ≤ val) :
    decFs rng ft val < fh := by
  simp [decFs]
  have hrpos : 0 < rng / ft := Nat.div_pos hrng hft
  have hdiv : rng / ft * (ft - fh) / (rng / ft) = ft - fh :=
    Nat.mul_div_right (ft - fh) hrpos
  have hs : ft - fh ≤ val / (rng / ft) := by
    calc ft - fh = rng / ft * (ft - fh) / (rng / ft) := hdiv.symm
      _ ≤ val / (rng / ft) := Nat.div_le_div_right hlo
  have hmin : ft - fh + 1 ≤ min (val / (rng / ft) + 1) ft :=
    Nat.le_min.mpr ⟨by omega, by omega⟩
  have hsub := Nat.sub_le_sub_left hmin ft
  omega

theorem decFs_pos {rng ft val fl fh : Nat}
    (hft : 0 < ft) (hfl0 : 0 < fl) (hfl : fl < fh) (hfh : fh ≤ ft)
    (hrng : ft ≤ rng)
    (hlo : rng / ft * (ft - fh) ≤ val)
    (hhi : val < rng / ft * (ft - fl)) :
    fl ≤ decFs rng ft val ∧ decFs rng ft val < fh := by
  constructor
  · simp [decFs]
    have hrpos : 0 < rng / ft := Nat.div_pos hrng hft
    have hs : val / (rng / ft) < ft - fl := by
      rw [Nat.div_lt_iff_lt_mul hrpos]
      calc val < rng / ft * (ft - fl) := hhi
        _ = (ft - fl) * (rng / ft) := Nat.mul_comm _ _
    have hs1 : val / (rng / ft) + 1 ≤ ft - fl := by omega
    have hmin : min (val / (rng / ft) + 1) ft ≤ ft - fl :=
      Nat.le_trans (Nat.min_le_left _ _) hs1
    have hsub := Nat.sub_le_sub_left hmin ft
    omega
  · exact decFs_zero_lt hft (by omega) hfh hrng hlo

/-! ### The decoder update restores the state invariant -/

theorem upd_zero {rng ft val fh : Nat}
    (hlo : rng / ft * (ft - fh) ≤ val) (hhi : val < rng) :
    updVal rng ft val 0 fh < updRng rng 0 fh ft := by
  simp [updVal, updRng]
  exact Nat.sub_lt_sub_right hlo hhi

theorem upd_pos {rng ft val fl fh : Nat}
    (hfl0 : 0 < fl) (hfl : fl < fh) (hfh : fh ≤ ft)
    (hlo : rng / ft * (ft - fh) ≤ val)
    (hhi : val < rng / ft * (ft - fl)) :
    updVal rng ft val fl fh < updRng rng fl fh ft := by
  have hne : ¬ fl = 0 := by omega
  simp [updVal, updRng, hne]
  have hsplit := mul_split (Nat.le_of_lt hfl) hfh (r := rng / ft)
  omega

/-! ### The mirror map

The decoder never sees `x`; it sees `val = lo + rng - 1 - x`. These lemmas
connect the encoder's sub-interval to the invariant the decoder theorems use.
-/

theorem mirror_zero {lo rng fh ft x : Nat}
    (hx0 : encLo lo rng 0 fh ft ≤ x)
    (hx1 : x < encLo lo rng 0 fh ft + encRng rng 0 fh ft) :
    rng / ft * (ft - fh) ≤ lo + rng - 1 - x ∧ lo + rng - 1 - x < rng := by
  simp [encLo, encRng] at hx0 hx1
  have hle : rng / ft * (ft - fh) ≤ rng :=
    Nat.le_trans (Nat.mul_le_mul_left _ (Nat.sub_le ft fh)) (Nat.div_mul_le_self rng ft)
  constructor <;> omega

theorem mirror_pos {lo rng fl fh ft x : Nat}
    (hfl0 : 0 < fl) (hfl : fl < fh) (hfh : fh ≤ ft)
    (hx0 : encLo lo rng fl fh ft ≤ x)
    (hx1 : x < encLo lo rng fl fh ft + encRng rng fl fh ft) :
    rng / ft * (ft - fh) ≤ lo + rng - 1 - x
      ∧ lo + rng - 1 - x < rng / ft * (ft - fl) := by
  have hne : ¬ fl = 0 := by omega
  simp [encLo, encRng, hne] at hx0 hx1
  have hsplit := mul_split (Nat.le_of_lt hfl) hfh (r := rng / ft)
  have hle : rng / ft * (ft - fl) ≤ rng :=
    Nat.le_trans (Nat.mul_le_mul_left _ (Nat.sub_le ft fl)) (Nat.div_mul_le_self rng ft)
  constructor <;> omega

/-! ### Round trip

Any point inside the encoder's sub-interval decodes as the symbol that owns it.
-/

theorem round_trip_zero {lo rng fh ft x : Nat}
    (hft : 0 < ft) (hfh0 : 0 < fh) (hfh : fh ≤ ft) (hrng : ft ≤ rng)
    (hx0 : encLo lo rng 0 fh ft ≤ x)
    (hx1 : x < encLo lo rng 0 fh ft + encRng rng 0 fh ft) :
    decFs rng ft (lo + rng - 1 - x) < fh := by
  obtain ⟨hlo, _⟩ := mirror_zero hx0 hx1
  exact decFs_zero_lt hft hfh0 hfh hrng hlo

theorem round_trip_pos {lo rng fl fh ft x : Nat}
    (hft : 0 < ft) (hfl0 : 0 < fl) (hfl : fl < fh) (hfh : fh ≤ ft)
    (hrng : ft ≤ rng)
    (hx0 : encLo lo rng fl fh ft ≤ x)
    (hx1 : x < encLo lo rng fl fh ft + encRng rng fl fh ft) :
    fl ≤ decFs rng ft (lo + rng - 1 - x)
      ∧ decFs rng ft (lo + rng - 1 - x) < fh := by
  obtain ⟨hlo, hhi⟩ := mirror_pos hfl0 hfl hfh hx0 hx1
  exact decFs_pos hft hfl0 hfl hfh hrng hlo hhi

end RangeCoder
