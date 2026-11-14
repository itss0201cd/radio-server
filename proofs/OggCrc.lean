import Std.Tactic.BVDecide

/- Ogg CRC-32: polynomial 0x04c11db7, MSB-first, init 0, no final xor.
   Models src/ogg.rs's table-driven CRC and proves it equals the bitwise update. -/

def poly : BitVec 32 := 0x04c11db7

def step (crc : BitVec 32) : BitVec 32 :=
  if crc.msb then (crc <<< 1) ^^^ poly else crc <<< 1

def step8 (crc : BitVec 32) : BitVec 32 :=
  step (step (step (step (step (step (step (step crc)))))))

def table (i : BitVec 8) : BitVec 32 :=
  step8 (i.setWidth 32 <<< 24)

def tableStep (crc : BitVec 32) (b : BitVec 8) : BitVec 32 :=
  (crc <<< 8) ^^^ table (((crc >>> 24).truncate 8) ^^^ b)

def bitStep (crc : BitVec 32) (b : BitVec 8) : BitVec 32 :=
  step8 (crc ^^^ (b.setWidth 32 <<< 24))

theorem tableStep_eq_bitStep (crc : BitVec 32) (b : BitVec 8) :
    tableStep crc b = bitStep crc b := by
  unfold tableStep bitStep table step8 step poly
  bv_decide

def crcTableFrom : BitVec 32 -> List (BitVec 8) -> BitVec 32
  | crc, [] => crc
  | crc, b :: bs => crcTableFrom (tableStep crc b) bs

def crcBitFrom : BitVec 32 -> List (BitVec 8) -> BitVec 32
  | crc, [] => crc
  | crc, b :: bs => crcBitFrom (bitStep crc b) bs

theorem crcTableFrom_eq_crcBitFrom (crc : BitVec 32) (data : List (BitVec 8)) :
    crcTableFrom crc data = crcBitFrom crc data := by
  induction data generalizing crc with
  | nil => rfl
  | cons b bs ih =>
    simp [crcTableFrom, crcBitFrom, tableStep_eq_bitStep, ih]

theorem crcTable_eq_crcBit (data : List (BitVec 8)) :
    crcTableFrom 0 data = crcBitFrom 0 data :=
  crcTableFrom_eq_crcBitFrom 0 data
