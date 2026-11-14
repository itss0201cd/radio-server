module

import Mathlib.Tactic

/-!
# CELT MDCT scratch coverage (`src/mdct.rs`)

`Mdct::forward` fills two scratch buffers before use:

* `f[0..n2]`, assembled from the windowed input by three loops:
  - `for i in 0..first`: writes `f[2i]`, `f[2i+1]`;
  - `while i < n4 - first`: writes `f[2i]`, `f[2i+1]`;
  - `while i < n4`: writes `f[2i]`, `f[2i+1]`.
* `f2[0..n4]`, filled at `f2[bitrev[i]]`.

This file proves both buffers are written in full, so the implementation can
reuse them across calls without clearing them. The second needs the shipped
bit-reversal tables to be permutations of `[0, n4)`; that is proved below for
all four sizes.

Model: `OVERLAP = 120`, so `first = (120 + 3) / 4 = 30`. For `shift` in
`0..3`, `n2 = 1920 >> (shift + 1)` and `n4 = n2 / 2`.

Write positions of the three loops, as sets of `k` (each `k` writes the pair
`{2k, 2k+1}`):

* loop 1 visits `k in [0, first)`;
* loop 2 visits `k in [first, n4 - first)`;
* loop 3 starts at the index loop 2 stopped on, `max first (n4 - first)`, and
  visits `k in [max first (n4 - first), n4)`.

Loops 2 and 3 together cover `[first, n4)` (`loop23_set`), so all three cover
`[0, n4)`. Each `k` writes both `2k` and `2k+1`, and those pairs tile
`[0, 2*n4) = [0, n2)` (`pairs_range`).
-/

namespace Mdct

/-- CELT low-overlap length. -/
def overlap : ℕ := 120

/-- `first = (OVERLAP + 3) >> 2`. -/
def first : ℕ := (overlap + 3) / 4

theorem first_eq : first = 30 := by decide

/-- Samples per MDCT block for a given shift: `n2 = 1920 >> shift >> 1`. -/
def n2 (shift : ℕ) : ℕ := 960 / 2 ^ shift

/-- Complex bins per MDCT block: `n4 = n2 / 2`. -/
def n4 (shift : ℕ) : ℕ := n2 shift / 2

/-! ### Index sets -/

/-- The pair written for one loop index. -/
def pair (k : ℕ) : Finset ℕ := {2 * k, 2 * k + 1}

/-- Union of the pairs over a whole range of loop indices. -/
def pairsOver (s : Finset ℕ) : Finset ℕ := s.biUnion pair

/-- Pairs for `k < n` tile `[0, 2n)`. -/
theorem pairs_range (n : ℕ) : pairsOver (Finset.range n) = Finset.range (2 * n) := by
  ext j
  simp only [pairsOver, pair, Finset.mem_biUnion, Finset.mem_range, Finset.mem_insert,
    Finset.mem_singleton]
  constructor
  · rintro ⟨k, hk, h | h⟩ <;> omega
  · intro hj
    have hk : j / 2 < n :=
      (Nat.div_lt_iff_lt_mul (by decide : 0 < 2)).mpr (by omega)
    refine ⟨j / 2, hk, ?_⟩
    have hdm : 2 * (j / 2) + j % 2 = j := Nat.div_add_mod j 2
    rcases Nat.mod_two_eq_zero_or_one j with h2 | h2 <;> omega

/-- Loops 2 and 3 together visit exactly `[first, n4)`. Loop 2 stops after
`n4 - first` (or immediately when that is below `first`); loop 3 then runs to
`n4`. -/
theorem loop23_set (first n4 : ℕ) (h : first ≤ n4) :
    Finset.Ico first (n4 - first) ∪ Finset.Ico (max first (n4 - first)) n4
      = Finset.Ico first n4 := by
  by_cases hc : first ≤ n4 - first
  · rw [max_eq_right hc]
    ext j
    simp only [Finset.mem_union, Finset.mem_Ico]
    omega
  · rw [max_eq_left (by omega)]
    ext j
    simp only [Finset.mem_union, Finset.mem_Ico]
    omega

/-- The three loops write every `f` index in `[0, n2)` exactly by covering
`[0, n4)` with pairs. -/
theorem f_writes_cover (first n4 : ℕ) (h : first ≤ n4) :
    pairsOver (Finset.range first) ∪ pairsOver (Finset.Ico first n4)
      = Finset.range (2 * n4) := by
  have hunion : (Finset.range first ∪ Finset.Ico first n4).biUnion pair
      = (Finset.range first).biUnion pair ∪ (Finset.Ico first n4).biUnion pair := by
    ext j
    simp only [Finset.mem_biUnion, Finset.mem_union]
    constructor
    · rintro ⟨k, hk | hk, hj⟩
      · exact Or.inl ⟨k, hk, hj⟩
      · exact Or.inr ⟨k, hk, hj⟩
    · rintro (⟨k, hk, hj⟩ | ⟨k, hk, hj⟩)
      · exact ⟨k, Or.inl hk, hj⟩
      · exact ⟨k, Or.inr hk, hj⟩
  simp only [pairsOver]
  rw [← hunion]
  have hsplit : Finset.range first ∪ Finset.Ico first n4 = Finset.range n4 := by
    ext j
    simp only [Finset.mem_union, Finset.mem_range, Finset.mem_Ico]
    omega
  rw [hsplit]
  exact pairs_range n4

/-- For every supported shift the written set is all of `f[0..n2)`. -/
theorem f_covers (shift : ℕ) (hs : shift ≤ 3) :
    pairsOver (Finset.range first) ∪ pairsOver (Finset.Ico first (n4 shift))
      = Finset.range (n2 shift) := by
  have htn : first ≤ n4 shift := by
    interval_cases shift <;> decide
  have hn : n2 shift = 2 * n4 shift := by
    interval_cases shift <;> decide
  rw [hn]
  exact f_writes_cover first (n4 shift) htn

/-! ### Bit-reversal tables

`f2[bitrev[i]] = ...` fills `f2` for `i in 0..n4`. It covers every slot iff
`bitrev` is a permutation of `[0, n4)`. The four shipped tables below are
transcribed from `src/celt_tables.rs`; each is proved noduplicate, of length
`n4`, and bounded by `n4`, which makes it surjective. -/

/-- `BITREV480`. -/
def bitrev480 : List ℕ :=
  [0, 96, 192, 288, 384, 32, 128, 224, 320, 416, 64, 160, 256, 352, 448, 8,
   104, 200, 296, 392, 40, 136, 232, 328, 424, 72, 168, 264, 360, 456, 16, 112,
   208, 304, 400, 48, 144, 240, 336, 432, 80, 176, 272, 368, 464, 24, 120, 216,
   312, 408, 56, 152, 248, 344, 440, 88, 184, 280, 376, 472, 4, 100, 196, 292,
   388, 36, 132, 228, 324, 420, 68, 164, 260, 356, 452, 12, 108, 204, 300, 396,
   44, 140, 236, 332, 428, 76, 172, 268, 364, 460, 20, 116, 212, 308, 404, 52,
   148, 244, 340, 436, 84, 180, 276, 372, 468, 28, 124, 220, 316, 412, 60, 156,
   252, 348, 444, 92, 188, 284, 380, 476, 1, 97, 193, 289, 385, 33, 129, 225,
   321, 417, 65, 161, 257, 353, 449, 9, 105, 201, 297, 393, 41, 137, 233, 329,
   425, 73, 169, 265, 361, 457, 17, 113, 209, 305, 401, 49, 145, 241, 337, 433,
   81, 177, 273, 369, 465, 25, 121, 217, 313, 409, 57, 153, 249, 345, 441, 89,
   185, 281, 377, 473, 5, 101, 197, 293, 389, 37, 133, 229, 325, 421, 69, 165,
   261, 357, 453, 13, 109, 205, 301, 397, 45, 141, 237, 333, 429, 77, 173, 269,
   365, 461, 21, 117, 213, 309, 405, 53, 149, 245, 341, 437, 85, 181, 277, 373,
   469, 29, 125, 221, 317, 413, 61, 157, 253, 349, 445, 93, 189, 285, 381, 477,
   2, 98, 194, 290, 386, 34, 130, 226, 322, 418, 66, 162, 258, 354, 450, 10,
   106, 202, 298, 394, 42, 138, 234, 330, 426, 74, 170, 266, 362, 458, 18, 114,
   210, 306, 402, 50, 146, 242, 338, 434, 82, 178, 274, 370, 466, 26, 122, 218,
   314, 410, 58, 154, 250, 346, 442, 90, 186, 282, 378, 474, 6, 102, 198, 294,
   390, 38, 134, 230, 326, 422, 70, 166, 262, 358, 454, 14, 110, 206, 302, 398,
   46, 142, 238, 334, 430, 78, 174, 270, 366, 462, 22, 118, 214, 310, 406, 54,
   150, 246, 342, 438, 86, 182, 278, 374, 470, 30, 126, 222, 318, 414, 62, 158,
   254, 350, 446, 94, 190, 286, 382, 478, 3, 99, 195, 291, 387, 35, 131, 227,
   323, 419, 67, 163, 259, 355, 451, 11, 107, 203, 299, 395, 43, 139, 235, 331,
   427, 75, 171, 267, 363, 459, 19, 115, 211, 307, 403, 51, 147, 243, 339, 435,
   83, 179, 275, 371, 467, 27, 123, 219, 315, 411, 59, 155, 251, 347, 443, 91,
   187, 283, 379, 475, 7, 103, 199, 295, 391, 39, 135, 231, 327, 423, 71, 167,
   263, 359, 455, 15, 111, 207, 303, 399, 47, 143, 239, 335, 431, 79, 175, 271,
   367, 463, 23, 119, 215, 311, 407, 55, 151, 247, 343, 439, 87, 183, 279, 375,
   471, 31, 127, 223, 319, 415, 63, 159, 255, 351, 447, 95, 191, 287, 383, 479]

/-- `BITREV240`. -/
def bitrev240 : List ℕ :=
  [0, 48, 96, 144, 192, 16, 64, 112, 160, 208, 32, 80, 128, 176, 224, 4,
   52, 100, 148, 196, 20, 68, 116, 164, 212, 36, 84, 132, 180, 228, 8, 56,
   104, 152, 200, 24, 72, 120, 168, 216, 40, 88, 136, 184, 232, 12, 60, 108,
   156, 204, 28, 76, 124, 172, 220, 44, 92, 140, 188, 236, 1, 49, 97, 145,
   193, 17, 65, 113, 161, 209, 33, 81, 129, 177, 225, 5, 53, 101, 149, 197,
   21, 69, 117, 165, 213, 37, 85, 133, 181, 229, 9, 57, 105, 153, 201, 25,
   73, 121, 169, 217, 41, 89, 137, 185, 233, 13, 61, 109, 157, 205, 29, 77,
   125, 173, 221, 45, 93, 141, 189, 237, 2, 50, 98, 146, 194, 18, 66, 114,
   162, 210, 34, 82, 130, 178, 226, 6, 54, 102, 150, 198, 22, 70, 118, 166,
   214, 38, 86, 134, 182, 230, 10, 58, 106, 154, 202, 26, 74, 122, 170, 218,
   42, 90, 138, 186, 234, 14, 62, 110, 158, 206, 30, 78, 126, 174, 222, 46,
   94, 142, 190, 238, 3, 51, 99, 147, 195, 19, 67, 115, 163, 211, 35, 83,
   131, 179, 227, 7, 55, 103, 151, 199, 23, 71, 119, 167, 215, 39, 87, 135,
   183, 231, 11, 59, 107, 155, 203, 27, 75, 123, 171, 219, 43, 91, 139, 187,
   235, 15, 63, 111, 159, 207, 31, 79, 127, 175, 223, 47, 95, 143, 191, 239]

/-- `BITREV120`. -/
def bitrev120 : List ℕ :=
  [0, 24, 48, 72, 96, 8, 32, 56, 80, 104, 16, 40, 64, 88, 112, 4,
   28, 52, 76, 100, 12, 36, 60, 84, 108, 20, 44, 68, 92, 116, 1, 25,
   49, 73, 97, 9, 33, 57, 81, 105, 17, 41, 65, 89, 113, 5, 29, 53,
   77, 101, 13, 37, 61, 85, 109, 21, 45, 69, 93, 117, 2, 26, 50, 74,
   98, 10, 34, 58, 82, 106, 18, 42, 66, 90, 114, 6, 30, 54, 78, 102,
   14, 38, 62, 86, 110, 22, 46, 70, 94, 118, 3, 27, 51, 75, 99, 11,
   35, 59, 83, 107, 19, 43, 67, 91, 115, 7, 31, 55, 79, 103, 15, 39,
   63, 87, 111, 23, 47, 71, 95, 119]

/-- `BITREV60`. -/
def bitrev60 : List ℕ :=
  [0, 12, 24, 36, 48, 4, 16, 28, 40, 52, 8, 20, 32, 44, 56, 1,
   13, 25, 37, 49, 5, 17, 29, 41, 53, 9, 21, 33, 45, 57, 2, 14,
   26, 38, 50, 6, 18, 30, 42, 54, 10, 22, 34, 46, 58, 3, 15, 27,
   39, 51, 7, 19, 31, 43, 55, 11, 23, 35, 47, 59]

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

set_option maxRecDepth 20000 in
theorem bitrev480_spec : bitrev480.Nodup ∧ bitrev480.length = 480 ∧
    bitrev480.all (fun x => decide (x < 480)) = true := by decide

set_option maxRecDepth 20000 in
theorem bitrev240_spec : bitrev240.Nodup ∧ bitrev240.length = 240 ∧
    bitrev240.all (fun x => decide (x < 240)) = true := by decide

set_option maxRecDepth 20000 in
theorem bitrev120_spec : bitrev120.Nodup ∧ bitrev120.length = 120 ∧
    bitrev120.all (fun x => decide (x < 120)) = true := by decide

set_option maxRecDepth 20000 in
theorem bitrev60_spec : bitrev60.Nodup ∧ bitrev60.length = 60 ∧
    bitrev60.all (fun x => decide (x < 60)) = true := by decide

/-- Every destination slot is written by the `f2` fill loop for `BITREV480`. -/
theorem bitrev480_perm (d : ℕ) (hd : d < 480) : d ∈ bitrev480 :=
  mem_of_nodup_length bitrev480_spec.1 bitrev480_spec.2.1 bitrev480_spec.2.2 hd

theorem bitrev240_perm (d : ℕ) (hd : d < 240) : d ∈ bitrev240 :=
  mem_of_nodup_length bitrev240_spec.1 bitrev240_spec.2.1 bitrev240_spec.2.2 hd

theorem bitrev120_perm (d : ℕ) (hd : d < 120) : d ∈ bitrev120 :=
  mem_of_nodup_length bitrev120_spec.1 bitrev120_spec.2.1 bitrev120_spec.2.2 hd

theorem bitrev60_perm (d : ℕ) (hd : d < 60) : d ∈ bitrev60 :=
  mem_of_nodup_length bitrev60_spec.1 bitrev60_spec.2.1 bitrev60_spec.2.2 hd

/-- The table used for each shift is exactly `n4` long and covers `[0, n4)`. -/
theorem n4_bitrev_covers (shift : ℕ) (hs : shift ≤ 3) (d : ℕ) (hd : d < n4 shift) :
    (shift = 0 ∧ d ∈ bitrev480) ∨ (shift = 1 ∧ d ∈ bitrev240) ∨
      (shift = 2 ∧ d ∈ bitrev120) ∨ (shift = 3 ∧ d ∈ bitrev60) := by
  interval_cases shift
  · exact Or.inl ⟨rfl, bitrev480_perm d (by simpa [n4, n2] using hd)⟩
  · exact Or.inr (Or.inl ⟨rfl, bitrev240_perm d (by simpa [n4, n2] using hd)⟩)
  · exact Or.inr (Or.inr (Or.inl ⟨rfl, bitrev120_perm d (by simpa [n4, n2] using hd)⟩))
  · exact Or.inr (Or.inr (Or.inr ⟨rfl, bitrev60_perm d (by simpa [n4, n2] using hd)⟩))

end Mdct
