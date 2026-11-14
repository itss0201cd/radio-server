/- Ogg lacing for one packet.

   The writer casts the segment count to u8. This models the lacing table as
   `255` repeated floor(n/255) times followed by n % 255, and proves:
   - the lacing values sum to the payload length;
   - every lacing value fits in a byte;
   - the segment count fits in a byte for any payload below 255 * 255.

   The station feeds at most one Opus packet from a 4000-byte buffer, so the
   count bound is far above what the caller can produce. -/

def lacing (n : Nat) : List Nat :=
  List.replicate (n / 255) 255 ++ [n % 255]

theorem lacing_sum (n : Nat) : (lacing n).sum = n := by
  have h := Nat.div_add_mod n 255
  simp [lacing]
  omega

theorem lacing_length (n : Nat) : (lacing n).length = n / 255 + 1 := by
  simp [lacing]

theorem lacing_entries_le (n : Nat) : ∀ x ∈ lacing n, x ≤ 255 := by
  intro x hx
  simp only [lacing, List.mem_append, List.mem_replicate, List.mem_singleton] at hx
  rcases hx with ⟨_, rfl⟩ | rfl
  · decide
  · have := Nat.mod_lt n (by decide : 0 < 255)
    omega

theorem lacing_count (n : Nat) (h : n < 255 * 255) :
    (lacing n).length ≤ 255 := by
  rw [lacing_length]
  have hdiv : n / 255 < 255 := by
    apply Nat.div_lt_of_lt_mul
    simpa [Nat.mul_comm] using h
  omega

example : (4000 : Nat) < 255 * 255 := by decide

example : (lacing 4000).length ≤ 255 :=
  lacing_count 4000 (by decide)

example : (lacing (255 * 255 - 1)).length = 255 := by
  rw [lacing_length]
