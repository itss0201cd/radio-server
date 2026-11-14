/- Mixer accounting in src/station.rs.

   All counts are interleaved stereo samples. `buf` is the logical number of
   buffered samples; the implementation keeps already-committed samples at the
   physical front of the vector until the next bulk reclaim. State after
   `begin_track`:

     buf     samples buffered and not yet handed to the encoder
     cursor  write position: `[0, cursor)` is past, `[cursor, buf)` is the
             held tail still being mixed into
     hold    samples at the end of `buf` reserved for the next crossfade
     pushed  blocks (20 ms each) already sent to the encoder

   Proved:

   - `cursor <= buf` is preserved by every operation, so `buf.len() - cursor`
     in `add` and the drain in `commit` never underflow;
   - `commit` removes at most `cursor` and at most `buf`, and always removes
     whole blocks, so every emitted block is exactly `FRAME_FLOATS` samples;
   - `commit` leaves the timeline total `pushed * FRAME_FLOATS + buf`
     unchanged, while `add` extends it by exactly its non-overlapping part;
   - stereo alignment (`buf`, `cursor`, `hold` even) is preserved, which makes
     the `/ 2` in `end_secs` and `next_start_secs` exact frame counts;
   - `next_start_secs <= end_secs`;
   - the Opus page granule `PRESKIP + k * FRAME_SAMPLES` is strictly
     increasing, advances by 20 ms, and fits in `u64` for any stream that fits.

   The proofs assume `add` receives a whole number of frames, which the
   stream loop guarantees: it pushes `(l, r)` pairs, and `resample.rs` emits
   one pair per output frame. -/

namespace Station

/-- Samples per stereo block: 960 frames of 20 ms at 48 kHz. -/
def frameFloats : Nat := 1920

/-- Frames per Opus page. -/
def frameSamples : Nat := 960

/-- `PRESKIP` from src/ogg.rs. -/
def preskip : Nat := 120

/-- `BURST_PAGES = BURST_SECONDS * 1000 / 20`. -/
def burstPages : Nat := 10000 / 20

/-- `SAMPLE_RATE`. -/
def sampleRate : Nat := 48000

structure State where
  buf : Nat
  cursor : Nat
  hold : Nat
  pushed : Nat
  deriving Repr

/-- The write cursor never passes the end of the buffer. -/
def Safe (m : State) : Prop := m.cursor ≤ m.buf

/-- Every count is a whole number of stereo frames. -/
def Aligned (m : State) : Prop :=
  m.buf % 2 = 0 ∧ m.cursor % 2 = 0 ∧ m.hold % 2 = 0

/-- `begin_track`: rewind by the overlap and hold back this track's ring. -/
def begin (m : State) (ringFrames lenFrames : Nat) : State :=
  let h := min (min m.hold m.buf) (lenFrames * 2)
  { buf := m.buf, cursor := m.buf - h, hold := ringFrames * 2, pushed := m.pushed }

/-- `abort_track`: drop the held tail and everything buffered. -/
def abort (m : State) : State :=
  { buf := m.buf, cursor := m.buf, hold := 0, pushed := m.pushed }

/-- `add` `len` samples; the first `min (buf - cursor) len` mix into the tail. -/
def add (m : State) (len : Nat) : State :=
  let ov := min (m.buf - m.cursor) len
  { buf := m.buf + (len - ov), cursor := m.cursor + len,
    hold := m.hold, pushed := m.pushed }

/-- `commit`: emit whole blocks that lie before both the cursor and the hold. -/
def commit (m : State) : State × Nat :=
  let limit := min m.cursor (m.buf - m.hold)
  let n := limit / frameFloats
  ({ buf := m.buf - n * frameFloats, cursor := m.cursor - n * frameFloats,
     hold := m.hold, pushed := m.pushed + n }, n)

/-- Total samples ever placed on the timeline. -/
def endSamples (m : State) : Nat := m.pushed * frameFloats + m.buf

/-! ### Safety -/

theorem begin_safe (m : State) (ring len : Nat) (_h : Safe m) : Safe (begin m ring len) := by
  unfold Safe begin
  dsimp only
  exact Nat.sub_le _ _

theorem abort_safe (m : State) : Safe (abort m) := by
  unfold Safe abort
  exact Nat.le_refl _

theorem add_safe (m : State) (len : Nat) (h : Safe m) : Safe (add m len) := by
  unfold Safe at h ⊢
  unfold add
  dsimp only
  have h1 : min (m.buf - m.cursor) len ≤ m.buf - m.cursor := Nat.min_le_left _ _
  have h2 : min (m.buf - m.cursor) len ≤ len := Nat.min_le_right _ _
  omega

/-- `commit` only removes whole blocks that fit before the cursor and the hold. -/
theorem commit_bound (m : State) (_h : Safe m) :
    (commit m).2 * frameFloats ≤ m.cursor ∧
    (commit m).2 * frameFloats ≤ m.buf := by
  unfold commit
  dsimp only
  have hc : min m.cursor (m.buf - m.hold) ≤ m.cursor := Nat.min_le_left _ _
  have hb : min m.cursor (m.buf - m.hold) ≤ m.buf :=
    Nat.le_trans (Nat.min_le_right _ _) (Nat.sub_le _ _)
  have hd : (min m.cursor (m.buf - m.hold) / frameFloats) * frameFloats ≤
      min m.cursor (m.buf - m.hold) := Nat.div_mul_le_self _ _
  constructor <;> omega

theorem commit_safe (m : State) (h : Safe m) : Safe (commit m).1 := by
  unfold Safe at h ⊢
  unfold commit
  dsimp only
  exact Nat.sub_le_sub_right h _

/-! ### Timeline accounting -/

/-- `commit` moves samples from the buffer to the encoder without changing how
    much audio the timeline contains. -/
theorem commit_endSamples (m : State) (_h : Safe m) :
    endSamples (commit m).1 = endSamples m := by
  unfold endSamples commit
  dsimp only
  have hb : min m.cursor (m.buf - m.hold) ≤ m.buf :=
    Nat.le_trans (Nat.min_le_right _ _) (Nat.sub_le _ _)
  have hd : (min m.cursor (m.buf - m.hold) / frameFloats) * frameFloats ≤
      min m.cursor (m.buf - m.hold) := Nat.div_mul_le_self _ _
  simp only [frameFloats]
  omega

/-- `add` extends the timeline by everything except the part that mixed into
    the held tail. -/
theorem add_endSamples (m : State) (len : Nat) :
    endSamples (add m len) = endSamples m + (len - min (m.buf - m.cursor) len) := by
  unfold endSamples add
  dsimp only
  simp only [frameFloats]
  omega

/-- With the cursor at the end (no crossfade in progress) an add appends all of
    its samples. -/
theorem add_appends (m : State) (len : Nat) (hc : m.cursor = m.buf) :
    endSamples (add m len) = endSamples m + len := by
  rw [add_endSamples]
  simp [hc]

theorem add_timeline_mono (m : State) (len : Nat) :
    endSamples m ≤ endSamples (add m len) := by
  rw [add_endSamples]
  omega

/-- `end_secs` divides by two, so the sample total must be even. -/
theorem endSamples_even (m : State) (h : m.buf % 2 = 0) : endSamples m % 2 = 0 := by
  unfold endSamples frameFloats
  omega

/-- The next track starts no later than the current end. -/
theorem next_start_le_end (m : State) (_h : Safe m) :
    endSamples m / 2 - min m.hold m.buf / 2 ≤ endSamples m / 2 := by
  exact Nat.sub_le _ _

/-! ### Alignment -/

theorem min_even (a b : Nat) (ha : a % 2 = 0) (hb : b % 2 = 0) :
    min a b % 2 = 0 := by
  rcases Nat.le_total a b with h | h
  · rw [Nat.min_eq_left h]; exact ha
  · rw [Nat.min_eq_right h]; exact hb

theorem begin_aligned (m : State) (ring len : Nat) (h : Aligned m) :
    Aligned (begin m ring len) := by
  obtain ⟨hb, hc, hh⟩ := h
  unfold Aligned begin
  dsimp only
  have hmin : min (min m.hold m.buf) (len * 2) % 2 = 0 :=
    min_even _ _ (min_even _ _ hh hb) (by omega)
  have hle : min (min m.hold m.buf) (len * 2) ≤ m.buf :=
    Nat.le_trans (Nat.min_le_left _ _) (Nat.min_le_right _ _)
  exact ⟨hb, by omega, by omega⟩

theorem abort_aligned (m : State) (h : Aligned m) : Aligned (abort m) := by
  obtain ⟨hb, _, _⟩ := h
  unfold Aligned abort
  dsimp only
  exact ⟨hb, hb, by decide⟩

theorem add_aligned (m : State) (len : Nat) (h : Aligned m) (hs : Safe m)
    (hlen : len % 2 = 0) : Aligned (add m len) := by
  obtain ⟨hb, hc, hh⟩ := h
  unfold Aligned add
  dsimp only
  unfold Safe at hs
  have hgap : (m.buf - m.cursor) % 2 = 0 := by omega
  have hov : min (m.buf - m.cursor) len % 2 = 0 := min_even _ _ hgap hlen
  have hovle : min (m.buf - m.cursor) len ≤ len := Nat.min_le_right _ _
  exact ⟨by omega, by omega, hh⟩

theorem commit_aligned (m : State) (h : Aligned m) : Aligned (commit m).1 := by
  obtain ⟨hb, hc, hh⟩ := h
  unfold Aligned commit
  dsimp only
  simp only [frameFloats]
  exact ⟨by omega, by omega, hh⟩

/-! ### Framing -/

/-- Granule written on page `k` (one block per page). -/
def granule (k : Nat) : Nat := preskip + k * frameSamples

theorem granule_succ (k : Nat) : granule (k + 1) = granule k + frameSamples := by
  unfold granule frameSamples preskip
  omega

theorem granule_strict (k : Nat) : granule k < granule (k + 1) := by
  unfold granule frameSamples preskip
  omega

theorem granule_mono (a b : Nat) (h : a ≤ b) : granule a ≤ granule b := by
  unfold granule frameSamples preskip
  omega

/-- A `u64` granule needs no wrap check for any stream below 2^53 blocks
    (millions of years at 20 ms per block). -/
theorem granule_fits (k : Nat) (h : k < 9007199254740992) :
    granule k < 18446744073709551616 := by
  unfold granule frameSamples preskip
  omega

/-- 500 pages of 20 ms is exactly the 10 s priming window. -/
theorem burst_is_ten_seconds : burstPages * frameSamples = sampleRate * 10 := by decide

/-- One block is 960 stereo frames. -/
theorem block_is_frames : frameFloats = 2 * frameSamples := by decide

/-! ### Trim slicing

   `stream_flac` / `stream_wav` walk contiguous blocks, skip frames outside
   `[start, stop)`, and stop once `pos >= stop` (after the block containing
   `stop` has been processed). `pushes` is exactly the number of frames such a
   block contributes. -/

/-- Frames a block of `b` frames starting at `pos` contributes to `[start, stop)`:
    the count of `i < b` with `start ≤ pos + i < stop`. -/
def pushes (pos b start stop : Nat) : Nat :=
  min (pos + b) stop - max pos start

/-- Contiguous blocks partition the window: no frame is counted twice and none
    is lost between blocks. -/
theorem pushes_add (pos b1 b2 start stop : Nat) :
    pushes pos b1 start stop + pushes (pos + b1) b2 start stop =
      pushes pos (b1 + b2) start stop := by
  unfold pushes
  simp only [Nat.min_def, Nat.max_def]
  split <;> split <;> split <;> split <;> split <;> omega

/-- A block lying wholly inside the window contributes all of its frames. -/
theorem pushes_full (pos b start stop : Nat) (h1 : start ≤ pos) (h2 : pos + b ≤ stop) :
    pushes pos b start stop = b := by
  unfold pushes
  rw [Nat.min_eq_left h2, Nat.max_eq_left h1]
  omega

/-- A block never contributes more than its own frames. -/
theorem pushes_le_block (pos b start stop : Nat) : pushes pos b start stop ≤ b := by
  unfold pushes
  have h1 : min (pos + b) stop ≤ pos + b := Nat.min_le_left _ _
  have h2 : pos ≤ max pos start := Nat.le_max_left _ _
  omega

/-- A whole stream of `total` frames selects exactly `stop - start` frames. -/
theorem pushes_total (total start stop : Nat) (_h1 : start ≤ stop) (h2 : stop ≤ total) :
    pushes 0 total start stop = stop - start := by
  unfold pushes
  simp only [Nat.zero_add]
  rw [Nat.min_eq_right h2, Nat.max_eq_right (Nat.zero_le start)]

end Station
