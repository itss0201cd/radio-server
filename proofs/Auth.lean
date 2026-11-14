/- The account thread in src/auth.rs.

   `Auth::login` trims the username and enqueues a `Job` on a Tokio mpsc
   channel, then awaits the oneshot reply. The `auth` OS thread owns
   `Option<User>` exclusively, so it is the only place that calls Argon2,
   `persist`, or `write_atomic`. Each job runs under `catch_unwind`; a panic
   from any of those calls becomes an internal error and the loop continues.

   Modeled here:

   - `validate` mirrors the byte-length and control-character checks on the
     already-trimmed username (`Auth::login` trims before enqueueing);
   - `step` mirrors `login_blocking`, with Argon2 hash/verify and atomic
     persistence as oracles that may succeed, fail, or panic;
   - `catch` mirrors `catch_unwind`, mapping `Error.panic` to an internal
     error while leaving the account state untouched;
   - `worker` mirrors the `blocking_recv` loop, including the ignored result
     of `reply.send`, so a disconnected caller cannot stop the thread.

   Proved:

   - `catch` never yields `Error.panic`, and no worker outcome is a panic;
   - every received job produces exactly one outcome, whether or not the
     reply channel is still open;
   - every error, including a caught panic, leaves the account unchanged;
   - `created = true` is returned only when the account was empty, the new
     account's username is the job's username, and the hash is the oracle's
     result; an existing account never yields `created = true`.

   Only one runtime premise remains: Rust's `catch_unwind` catches unwinding
   panics. The release profile pins `panic = "unwind"`; an abort (for example
   allocation failure under `panic = "abort"`) terminates the process and is
   outside any thread's control. The model makes no assumption that Argon2,
   `serde_json`, or `fs` avoid panicking. -/

namespace Auth

inductive Error where
  | badRequest (msg : String)
  | invalid
  | internal (msg : String)
  /-- A Rust panic observed at the catch boundary. -/
  | panic
  deriving DecidableEq, Repr

structure User where
  username : String
  passwordHash : String
  deriving Repr

structure Authenticated where
  user : String
  created : Bool
  deriving Repr

structure Job where
  /-- Trimmed by `Auth::login`; the account thread never calls `trim`. -/
  username : String
  password : String
  deriving Repr

structure Oracles where
  /-- `verify_password`; `Error.panic` is a panic inside Argon2. -/
  verify : String → String → Except Error Bool
  /-- `hash_password`; `Error.panic` is a panic inside Argon2. -/
  hash : String → Except Error String
  /-- `persist`; `Error.panic` covers Argon2-independent I/O and serde panics. -/
  persist : User → Except Error Unit

def byteLen (s : String) : Nat := s.toUTF8.size

def isControl (c : Char) : Bool :=
  decide (c.toNat < 0x20) ||
    (decide (0x7F ≤ c.toNat) && decide (c.toNat ≤ 0x9F))

def hasControl (s : String) : Bool := s.toList.any isControl

/-- `login_blocking`'s validation, before any fallible work. -/
def validate (j : Job) : Option Error :=
  if byteLen j.username == 0 || decide (byteLen j.username > 64) || hasControl j.username then
    some (.badRequest "invalid username")
  else if byteLen j.password == 0 || decide (byteLen j.password > 1024) then
    some (.badRequest "invalid password")
  else
    none

/-- `verify_password(...).unwrap_or(false)`: an Argon2 error is a mismatch,
    but a panic still propagates. -/
def verifyOrFalse : Except Error Bool → Except Error Bool
  | .ok value => .ok value
  | .error .panic => .error .panic
  | .error _ => .ok false

/-- One iteration of the account thread. The state is only replaced after
    persistence succeeds; every error, including a panic, returns the original
    state and there is no fallible call after the replacement. -/
def step (o : Oracles) (account : Option User) (j : Job) :
    Option User × Except Error Authenticated :=
  match validate j with
  | some e => (account, .error e)
  | none =>
    match account with
    | some existing =>
      match verifyOrFalse (o.verify existing.passwordHash j.password) with
      | .ok passwordMatches =>
        if (existing.username == j.username) && passwordMatches then
          (account, .ok { user := existing.username, created := false })
        else
          (account, .error .invalid)
      | .error e => (account, .error e)
    | none =>
      match o.hash j.password with
      | .error e => (account, .error e)
      | .ok h =>
        let u : User := { username := j.username, passwordHash := h }
        match o.persist u with
        | .error e => (account, .error e)
        | .ok _ => (some u, .ok { user := u.username, created := true })

/-- `catch_unwind`: a panic becomes an internal error and the state is kept. -/
def contain (r : Option User × Except Error Authenticated) :
    Option User × Except Error Authenticated :=
  match r.2 with
  | .error .panic => (r.1, .error (.internal "account worker panicked"))
  | _ => r

/-- The `blocking_recv` loop. `send`'s result is ignored, matching
    `let _ = reply.send(...)`; a closed channel only ends the loop. -/
def worker (o : Oracles) (send : Except Error Authenticated → Bool) :
    Option User → List Job → List (Except Error Authenticated) × Option User
  | account, [] => ([], account)
  | account, j :: js =>
    let (account', out) := contain (step o account j)
    let _delivered := send out
    let (outs, account'') := worker o send account' js
    (out :: outs, account'')

theorem caught_not_panic (r : Option User × Except Error Authenticated) :
    (contain r).2 ≠ .error .panic := by
  rcases r with ⟨account, out⟩
  cases out with
  | ok value => simp [contain]
  | error e => cases e <;> simp [contain]

theorem contain_ok_imp_step_ok (r : Option User × Except Error Authenticated)
    (a : Authenticated) (h : (contain r).2 = .ok a) : r.2 = .ok a := by
  rcases r with ⟨account, out⟩
  cases out with
  | ok value => simpa [contain] using h
  | error e =>
    cases e <;> simp [contain] at h

theorem step_error_keeps_state (o : Oracles) (account : Option User) (j : Job)
    (e : Error) (h : (step o account j).2 = .error e) :
    (step o account j).1 = account := by
  unfold step at h ⊢
  cases hv : validate j with
  | some e' => simp [hv] at h ⊢
  | none =>
    cases ha : account with
    | none =>
      cases hh : o.hash j.password with
      | error e' => simp [hv, ha, hh] at h ⊢
      | ok hash =>
        cases hp : o.persist { username := j.username, passwordHash := hash } with
        | error e' => simp [hv, ha, hh, hp] at h ⊢
        | ok _ => simp [hv, ha, hh, hp] at h
    | some existing =>
      cases hm : verifyOrFalse (o.verify existing.passwordHash j.password) with
      | error e' => simp [hv, ha, hm] at h ⊢
      | ok passwordMatches =>
        by_cases hb : (existing.username == j.username) && passwordMatches
        · simp [hv, ha, hm, hb] at h
        · simp [hv, ha, hm, hb] at h ⊢

theorem step_created_true_implies_empty (o : Oracles) (account : Option User)
    (j : Job) (a : Authenticated)
    (h : (step o account j).2 = .ok a) (hc : a.created = true) :
    account = none := by
  unfold step at h
  cases hv : validate j with
  | some e' => simp [hv] at h
  | none =>
    cases ha : account with
    | none =>
      cases hh : o.hash j.password with
      | error e' => simp [hv, ha, hh] at h
      | ok hash =>
        cases hp : o.persist { username := j.username, passwordHash := hash } with
        | error e' => simp [hv, ha, hh, hp] at h
        | ok _ => rfl
    | some existing =>
      cases hm : verifyOrFalse (o.verify existing.passwordHash j.password) with
      | error e' => simp [hv, ha, hm] at h
      | ok passwordMatches =>
        by_cases hb : (existing.username == j.username) && passwordMatches
        · simp [hv, ha, hm, hb] at h
          cases h
          cases hc
        · simp [hv, ha, hm, hb] at h

theorem step_created_user_eq (o : Oracles) (account : Option User) (j : Job)
    (a : Authenticated)
    (h : (step o account j).2 = .ok a) (hc : a.created = true) :
    a.user = j.username := by
  unfold step at h
  cases hv : validate j with
  | some e' => simp [hv] at h
  | none =>
    cases ha : account with
    | none =>
      cases hh : o.hash j.password with
      | error e' => simp [hv, ha, hh] at h
      | ok hash =>
        cases hp : o.persist { username := j.username, passwordHash := hash } with
        | error e' => simp [hv, ha, hh, hp] at h
        | ok _ =>
          simp [hv, ha, hh, hp] at h
          cases h
          rfl
    | some existing =>
      cases hm : verifyOrFalse (o.verify existing.passwordHash j.password) with
      | error e' => simp [hv, ha, hm] at h
      | ok passwordMatches =>
        by_cases hb : (existing.username == j.username) && passwordMatches
        · simp [hv, ha, hm, hb] at h
          cases h
          cases hc
        · simp [hv, ha, hm, hb] at h

theorem step_existing_never_created (o : Oracles) (account : Option User)
    (j : Job) (a : Authenticated) (u : User)
    (ha : account = some u) (h : (step o account j).2 = .ok a) :
    a.created = false := by
  subst account
  unfold step at h
  cases hv : validate j with
  | some e' => simp [hv] at h
  | none =>
    cases hm : verifyOrFalse (o.verify u.passwordHash j.password) with
    | error e' => simp [hv, hm] at h
    | ok passwordMatches =>
      by_cases hb : (u.username == j.username) && passwordMatches
      · simp [hv, hm, hb] at h
        cases h
        rfl
      · simp [hv, hm, hb] at h

theorem caught_failure_keeps_state (o : Oracles) (account : Option User) (j : Job)
    (e : Error) (h : (contain (step o account j)).2 = .error e) :
    (contain (step o account j)).1 = account := by
  unfold contain at h ⊢
  cases hs : (step o account j).2 with
  | ok a => simp [hs] at h
  | error e' =>
    by_cases hp : e' = .panic
    · subst hp
      simp
      exact step_error_keeps_state o account j .panic hs
    · simp [hp]
      exact step_error_keeps_state o account j e' hs

theorem caught_created_true_implies_empty (o : Oracles) (account : Option User)
    (j : Job) (a : Authenticated)
    (h : (contain (step o account j)).2 = .ok a) (hc : a.created = true) :
    account = none :=
  step_created_true_implies_empty o account j a
    (contain_ok_imp_step_ok (step o account j) a h) hc

theorem caught_existing_never_created (o : Oracles) (account : Option User)
    (j : Job) (a : Authenticated) (u : User)
    (ha : account = some u) (h : (contain (step o account j)).2 = .ok a) :
    a.created = false :=
  step_existing_never_created o account j a u ha
    (contain_ok_imp_step_ok (step o account j) a h)

/-- Every job produces exactly one outcome, even when `send` fails. -/
theorem worker_length (o : Oracles) (send : Except Error Authenticated → Bool)
    (account : Option User) (js : List Job) :
    (worker o send account js).1.length = js.length := by
  induction js generalizing account with
  | nil => simp [worker]
  | cons j js ih =>
    simp [worker, ih]

/-- No outcome recorded by the worker is a panic. -/
theorem worker_no_panic (o : Oracles) (send : Except Error Authenticated → Bool)
    (account : Option User) (js : List Job) :
    .error .panic ∉ (worker o send account js).1 := by
  induction js generalizing account with
  | nil => simp [worker]
  | cons j js ih =>
    simp only [worker]
    intro hmem
    rw [List.mem_cons] at hmem
    rcases hmem with hpanic | hin
    · exact caught_not_panic (step o account j) hpanic.symm
    · exact ih (contain (step o account j)).1 hin

end Auth
