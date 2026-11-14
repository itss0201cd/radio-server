import Lake
open Lake DSL

package radio_proofs

require mathlib from git
  "https://github.com/leanprover-community/mathlib4.git" @ "v4.35.0"

@[default_target]
lean_lib RadioProofs where
  srcDir := "proofs"
  globs := #[.one `Auth, .one `Bands, .one `Encoder, .one `Energy, .one `Flac, .one `FlacGrammar, .one `Mdct, .one `OggCrc, .one `OggLacing,
             .one `Pitch, .one `Pvq, .one `RangeCoder, .one `Resample, .one `Soxr, .one `Station,
             .one `Window]
