module

import Mathlib.Tactic

/-!
# FLAC bitstream grammar foundations

The RFC 9639 grammar is defined over a bit sequence.  This file starts the
formal parser model with the two primitives every field uses: `takeBits` and
`takeUnary`.  The theorems state the two facts the decoder's bounds rely on:
a successfully parsed field leaves exactly the unconsumed suffix, and an
`n`-bit field is below `2^n`.
-/

namespace FlacGrammar

abbrev Bits := List Bool

/-- Big-endian interpretation of a bit list. -/
def bitsToNat : Bits → Nat
  | [] => 0
  | bit :: rest =>
      (if bit then 1 else 0) * 2 ^ rest.length + bitsToNat rest

theorem bitsToNat_lt (bits : Bits) : bitsToNat bits < 2 ^ bits.length := by
  induction bits with
  | nil => simp [bitsToNat]
  | cons bit rest ih =>
    simp only [bitsToNat, List.length_cons, pow_succ]
    cases bit <;> simp <;> omega

/-- Read exactly `n` bits. -/
def takeBits (n : Nat) (bits : Bits) : Option (Nat × Bits) :=
  if _h : n ≤ bits.length then
    some (bitsToNat (bits.take n), bits.drop n)
  else
    none

theorem takeBits_sound {n : Nat} {bits : Bits} {value : Nat} {rest : Bits}
    (h : takeBits n bits = some (value, rest)) :
    value < 2 ^ n ∧ rest.length + n = bits.length := by
  unfold takeBits at h
  split at h
  case isTrue hle =>
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    rcases h with ⟨rfl, rfl⟩
    constructor
    · simpa [List.length_take, hle] using bitsToNat_lt (bits.take n)
    · simp [List.length_drop, hle]
  case isFalse => contradiction

/-- Read a unary code, returning the number of bits consumed including the
terminating one. -/
def takeUnaryFrom : Nat → Bits → Option (Nat × Bits)
  | _, [] => none
  | _consumed, true :: rest => some (_consumed + 1, rest)
  | consumed, false :: rest => takeUnaryFrom (consumed + 1) rest

def takeUnary (bits : Bits) : Option (Nat × Bits) := takeUnaryFrom 0 bits

theorem takeUnaryFrom_sound (consumed : Nat) (bits : Bits) {used : Nat} {rest : Bits}
    (h : takeUnaryFrom consumed bits = some (used, rest)) :
    rest.length + used = bits.length + consumed := by
  induction bits generalizing consumed with
  | nil => simp [takeUnaryFrom] at h
  | cons bit tail ih =>
    cases bit with
    | true =>
      simp only [takeUnaryFrom, Option.some.injEq, Prod.mk.injEq] at h
      rcases h with ⟨rfl, rfl⟩
      simp
      omega
    | false =>
      simp only [takeUnaryFrom] at h
      have ih' := ih (consumed + 1) h
      simp only [List.length_cons] at ih' ⊢
      omega

theorem takeUnary_sound {bits : Bits} {used : Nat} {rest : Bits}
    (h : takeUnary bits = some (used, rest)) :
    rest.length + used = bits.length := by
  simpa [takeUnary] using takeUnaryFrom_sound 0 bits h

end FlacGrammar
