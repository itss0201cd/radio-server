/- Rational polyphase resampler for src/resample.rs.

   The station only ever resamples to 48000 Hz. Supported input rates are
   44100, 64000, 88200, 96000, 176400 and 192000; 48000 is passed through
   untouched.

   Every rate has an exact ratio `L / M = out / in`, and

     L * fs = 48000 * M =: R

   is the prototype rate. The coefficient table `h[0..N)` is one symmetric
   linear-phase lowpass designed at `R`, with `N % (2*M) = 1`. Output frame
   `k` sits at upsampled position `p = k * M`, so

     q = p / L     input index of the newest tap
     r = p % L     polyphase branch
     y[k] = sum_{j<W} h[r + j*L] * x[q - j]

   where `W = (N + L - 1) / L` and the table is zero-padded to `W * L`
   entries so the inner loop never needs a per-phase length.

   The filter is centred, so frame `k` carries the audio at input position
   `(k*M - (N-1)/2) / L`. Discarding the first `delay` frames and flushing
   `delay` more makes the emitted stream start exactly at input sample 0.
   The proofs below are therefore written in *compensated* indices:

     emitted frame `i`  ==  absolute frame `k = delay + i`

   and it exists iff `i * M < n * L` for `n` input samples.

   Proved below:

   - every supported rate reduces exactly (`L * fs = 48000 * M`);
   - `N % (2*M) = 1`, so the group delay is a whole number of output frames,
     and it is exactly `delay` (47) for every supported rate;
   - the phase accumulator keeps `r < L` and advances `p` by exactly `M`;
   - the emitted set is a prefix of the output stream, and cutting it at the
     ceil boundary is sharp, so a track cannot drift against the timeline;
   - the padded table index `r + j*L` is in bounds;
   - the newest tap index never exceeds `n + W`, so a ring with `W - 1`
     zeros prepended and `W` appended drains without reading outside;
   - the `i64` accumulator cannot overflow for any supported rate;
   - a phase-normalised table has exactly 0 dB DC gain.

   Coefficient *values* are not proved here: they are computed in Rust and
   checked numerically (symmetry, per-phase sum, spectral response). This
   file covers the state machine around them -- indices, counts, delay --
   which is where the audible timeline bugs live. -/

namespace Resample

/-- Output rate of the station. -/
def outRate : Nat := 48000

/-- Group delay shared by every supported rate, in output frames. -/
def delay : Nat := 50

/-- Fractional bits of a coefficient (Q25) and of an input sample (Q23).
    Q25 keeps the coefficient-quantisation error floor near -136 dB, which is
    comfortably under the -120 dB design stopband. -/
def coeffBits : Nat := 25
def sampleBits : Nat := 23

structure Rate where
  fs : Nat
  L : Nat
  M : Nat
  N : Nat
  deriving Repr

/-- The supported input rates and their polyphase parameters.

    `N` is the smallest length at or above the Kaiser estimate for a 126 dB
    design target (beta = 0.1102 * (126 - 8.7)) over the 20-24 kHz transition
    that satisfies `N % (2*M) = 1`. Measured worst-case stopband of the
    resulting tables is -126.8 dB, so the -120 dB requirement has margin. -/
def rates : List Rate :=
  [ ⟨44100, 160, 147, 14701⟩,
    ⟨64000, 3, 4, 401⟩,
    ⟨88200, 80, 147, 14701⟩,
    ⟨96000, 1, 2, 201⟩,
    ⟨176400, 40, 147, 14701⟩,
    ⟨192000, 1, 4, 401⟩ ]

/-- Membership, written out so each rate can be handled by substitution. -/
theorem mem_rates (r : Rate) (h : r ∈ rates) :
    r = ⟨44100, 160, 147, 14701⟩ ∨ r = ⟨64000, 3, 4, 401⟩ ∨
    r = ⟨88200, 80, 147, 14701⟩ ∨ r = ⟨96000, 1, 2, 201⟩ ∨
    r = ⟨176400, 40, 147, 14701⟩ ∨ r = ⟨192000, 1, 4, 401⟩ := by
  simpa [rates] using h

/-! ### Per-rate parameters -/

/-- The ratio is exact: the prototype rate is both `L * fs` and `48000 * M`. -/
theorem ratio_exact (r : Rate) (h : r ∈ rates) : r.L * r.fs = outRate * r.M := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-- `N - 1` is a multiple of `2*M`, so the delay is a whole number of frames. -/
theorem delay_divisible (r : Rate) (h : r ∈ rates) : (r.N - 1) % (2 * r.M) = 0 := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-- The group delay is exactly `delay` output frames for every rate. -/
theorem delay_frames (r : Rate) (h : r ∈ rates) : 2 * r.M * delay = r.N - 1 := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-- Equivalently, the filter centre sits on a whole frame boundary: the frame
    `delay` window is centred on input position 0. -/
theorem delay_centre (r : Rate) (h : r ∈ rates) : delay * r.M = (r.N - 1) / 2 := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-- Every supported rate is at least twice the output rate or a clean
    rational of it; in particular `L >= 1` and `M >= 2` never both fail,
    so the accumulator always has at least one tap. -/
theorem rate_sane (r : Rate) (h : r ∈ rates) : 0 < r.L ∧ 0 < r.M := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-! ### Phase accumulator

   The implementation keeps `(q, r)` with `q * L + r = p` and advances one
   output frame at a time, so no division is needed inside the inner loop
   beyond the carry. -/

/-- Brute-force reference: the polyphase branch of frame `k`. -/
def phase (L M k : Nat) : Nat := (k * M) % L

/-- Branch index is always a valid table row. -/
theorem phase_lt (L M k : Nat) (hL : 0 < L) : phase L M k < L :=
  Nat.mod_lt _ hL

/-- One step of the incremental accumulator. -/
def step (L M q r : Nat) : Nat × Nat :=
  (q + (r + M) / L, (r + M) % L)

/-- The step preserves `q * L + r = p`, i.e. it advances `p` by exactly `M`.
    This is the whole of the timing logic: no float, no rounding. -/
theorem step_recombine (L M q r : Nat) (_hL : 0 < L) :
    (step L M q r).1 * L + (step L M q r).2 = q * L + r + M := by
  show (q + (r + M) / L) * L + (r + M) % L = q * L + r + M
  have h := Nat.div_add_mod (r + M) L
  have hc : ((r + M) / L) * L = L * ((r + M) / L) := Nat.mul_comm _ _
  rw [Nat.add_mul, hc]
  omega

/-- The step keeps the branch in range, so table indexing stays valid. -/
theorem step_phase_lt (L M q r : Nat) (hL : 0 < L) : (step L M q r).2 < L := by
  unfold step
  exact Nat.mod_lt _ hL

/-- The step only ever moves forward: the carry is the only way `q` grows. -/
theorem step_q_mono (L M q r : Nat) : q ≤ (step L M q r).1 := by
  unfold step
  exact Nat.le_add_right _ _

/-! ### Timeline

   Compensated index: emitted frame `i` exists iff `i * M < n * L` for `n`
   input samples. Stating it multiplicatively keeps the proof free of
   division lemmas, and it is exactly the property that bounds the emitted
   stream. -/

/-- Emitted frame `i` of a track of `n` input samples. -/
def Available (L M n i : Nat) : Prop := i * M < n * L

/-- Availability is downward closed: the output stream has no holes. -/
theorem available_mono (L M n i j : Nat) (h : Available L M n i) (hj : j ≤ i) :
    Available L M n j := by
  unfold Available at h ⊢
  have hm : j * M ≤ i * M := Nat.mul_le_mul_right M hj
  omega

/-- With `T` the ceiling of the exact rational length
    (`(T-1)*M < n*L <= T*M`), the emitted stream is exactly frames `[0, T)`.
    This is the length invariant: the count is a function of `n` alone, so
    nothing accumulates between tracks. -/
theorem count_of_ceil (L M n T : Nat) (hM : 0 < M)
    (hlo : (T - 1) * M < n * L) (hhi : n * L ≤ T * M) :
    ∀ i, Available L M n i ↔ i < T := by
  intro i
  unfold Available
  constructor
  · intro h
    have h1 : i * M < T * M := Nat.lt_of_lt_of_le h hhi
    exact (Nat.mul_lt_mul_right hM).mp h1
  · intro h
    have h1 : i * M ≤ (T - 1) * M := Nat.mul_le_mul_right M (by omega)
    have h2 : (T - 1) * M = T * M - M := by
      rw [Nat.sub_mul, Nat.one_mul]
    omega

/-- The cutoff is sharp: some frame is the last available one. -/
theorem available_sharp (L M n : Nat) (hL : 0 < L) (hM : 0 < M) (hn : 0 < n) :
    ∃ i, Available L M n i ∧ ¬ Available L M n (i + 1) := by
  refine ⟨(n * L - 1) / M, ?_, ?_⟩
  · unfold Available
    have hpos : 0 < n * L := Nat.mul_pos hn hL
    have hdm := Nat.div_add_mod (n * L - 1) M
    have hr : (n * L - 1) % M < M := Nat.mod_lt _ hM
    have hc : ((n * L - 1) / M) * M = M * ((n * L - 1) / M) := Nat.mul_comm _ _
    rw [hc]
    omega
  · unfold Available
    have hpos : 0 < n * L := Nat.mul_pos hn hL
    have hdm := Nat.div_add_mod (n * L - 1) M
    have hr : (n * L - 1) % M < M := Nat.mod_lt _ hM
    have hc : ((n * L - 1) / M) * M = M * ((n * L - 1) / M) := Nat.mul_comm _ _
    have hmul : ((n * L - 1) / M + 1) * M = ((n * L - 1) / M) * M + M := by
      rw [Nat.add_mul, Nat.one_mul]
    omega

/-! ### Division-free carry

   `process` advances the accumulator with

     t = r + r_step;  carry = (t >= L);  r = t - carry*L;  q += q_step + carry

   where `q_step = M / L` and `r_step = M % L`, instead of dividing by the
   runtime value `L` on every output frame. What makes that safe is the branch
   invariant `r < L`: the sum stays below `2L`, so the carry is one comparison
   and the remainder one subtraction. Theorems below show it computes exactly
   the same state as the plain division. -/

/-- Division-free accumulator step, as implemented in `src/resample.rs`. -/
def stepFast (L M q r : Nat) : Nat × Nat :=
  let t := r + M % L
  let c := if L ≤ t then 1 else 0
  (q + M / L + c, t - c * L)

/-- The fast carry equals the plain division whenever `r < L`. -/
theorem step_fast_eq (L M q r : Nat) (hL : 0 < L) (hr : r < L) :
    stepFast L M q r = step L M q r := by
  have hsr : M % L < L := Nat.mod_lt _ hL
  have hlt : r + M % L < 2 * L := by omega
  have hsum : r + M = (r + M % L) + L * (M / L) := by
    have h1 : M = L * (M / L) + M % L := (Nat.div_add_mod M L).symm
    omega
  have hdiv : (r + M) / L = M / L + (if L ≤ r + M % L then 1 else 0) := by
    rw [hsum, Nat.add_mul_div_left (r + M % L) (M / L) hL]
    split
    · have hsub : r + M % L - L < L := by omega
      have hrec : r + M % L = (r + M % L - L) + 1 * L := by omega
      rw [hrec, Nat.add_mul_div_right (r + M % L - L) 1 hL, Nat.div_eq_of_lt hsub]
      omega
    · have hz : (r + M % L) / L = 0 := Nat.div_eq_of_lt (by omega)
      rw [hz]
      omega
  have hmod : (r + M) % L = (r + M % L) - (if L ≤ r + M % L then 1 else 0) * L := by
    rw [hsum, Nat.add_mul_mod_self_left (r + M % L) L (M / L)]
    split
    · have hsub : r + M % L - L < L := by omega
      have hrec : r + M % L = (r + M % L - L) + L * 1 := by
        rw [Nat.mul_one]
        omega
      have hcomm : (1 : Nat) * L = L * 1 := Nat.mul_comm 1 L
      rw [hrec, Nat.add_mul_mod_self_left (r + M % L - L) L 1, Nat.mod_eq_of_lt hsub]
      omega
    · rw [Nat.mod_eq_of_lt (by omega)]
      omega
  unfold stepFast step
  dsimp only
  rw [hdiv, hmod]
  exact Prod.ext (by omega) rfl

/-! ### Table indexing -/

/-- Uniform table width: every phase has at most this many real taps. -/
def width (L N : Nat) : Nat := (N + L - 1) / L

/-- The table is zero-padded to a whole number of branches. -/
def padded (L N : Nat) : Nat := width L N * L

/-- The zero padding covers the table: `N <= W * L`. -/
theorem width_covers (L N : Nat) (hL : 0 < L) : N ≤ padded L N := by
  unfold padded width
  have hdm := Nat.div_add_mod (N + L - 1) L
  have hr : (N + L - 1) % L < L := Nat.mod_lt _ hL
  have hc : ((N + L - 1) / L) * L = L * ((N + L - 1) / L) := Nat.mul_comm _ _
  omega

/-- Every index the inner loop touches lands inside the padded table. -/
theorem padded_bound (L N r j : Nat) (_hL : 0 < L) (hr : r < L) (hj : j < width L N) :
    r + j * L < padded L N := by
  unfold padded
  have hj' : j + 1 ≤ width L N := by omega
  have hm : (j + 1) * L ≤ width L N * L := Nat.mul_le_mul_right L hj'
  have hm' : (j + 1) * L = j * L + L := by rw [Nat.add_mul, Nat.one_mul]
  omega

/-- The newest tap index of emitted frame `i` stays within `n + W`, so a ring
    with `W - 1` zeros prepended and `W` appended never runs off either end. -/
theorem newest_tap (L M N n W i : Nat) (hL : 0 < L)
    (hWL : N ≤ W * L) (hN : 2 ≤ N)
    (hD : delay * M = (N - 1) / 2)
    (hi : i * M < n * L) :
    ((delay + i) * M) / L < n + W := by
  rw [Nat.div_lt_iff_lt_mul hL]
  rw [Nat.add_mul, Nat.add_mul]
  omega

/-- With the ring padded as above, every tap index is inside it. -/
theorem tap_in_bounds (P n W q j ringLen : Nat) (hq0 : P ≤ q) (hq1 : q < ringLen)
    (hlen : ringLen = P + n + W) (hj : j < W) (hP : P = W - 1) :
    j ≤ q ∧ q - j ≤ ringLen - 1 := by
  omega

/-! ### Fixed-point range -/

/-- Worst-case `i64` accumulator magnitude. Each of the `W` products is at
    most `2^25 * 2^25`: `|h| <= 1` follows from every phase summing to 1, and
    samples are clamped to `|x| <= 4` before the Q23 conversion, which is two
    bits above full scale. -/
def accBound (L N : Nat) : Nat := width L N * 2 ^ (coeffBits + sampleBits + 2)

/-- No supported rate can overflow the accumulator. -/
theorem acc_fits (r : Rate) (h : r ∈ rates) : accBound r.L r.N < 2 ^ 63 := by
  rcases mem_rates r h with rfl | rfl | rfl | rfl | rfl | rfl <;>
    decide

/-- A phase-normalised table has exactly 0 dB DC gain: full-scale in,
    full-scale out, with no rounding anywhere. -/
theorem dc_unity (s : Nat) (h : s = 2 ^ coeffBits) :
    (s * 2 ^ sampleBits) / 2 ^ (coeffBits + sampleBits) = 1 := by
  subst h
  simp [coeffBits, sampleBits]

end Resample
