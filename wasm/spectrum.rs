#![no_std]

use core::cell::UnsafeCell;

const MAX_COLS: usize = 8192;
const MAX_BINS: usize = 16384;
const MIN_DB: f32 = -90.0;
const POWER_TABLE_LEN: usize = 901;
const POWER_RATIO: f32 = 1.023_292_9; // 10^0.01

#[repr(C)]
struct State {
    fft: [f32; MAX_BINS],
    levels: [f32; MAX_COLS],
    bin_start: [f32; MAX_COLS],
    first: [u16; MAX_COLS],
    last: [u16; MAX_COLS],
    power: [f32; POWER_TABLE_LEN],
    bin_ratio: f32,
    cols: usize,
    bins: usize,
    min_db: f32,
    db_span: f32,
}

impl State {
    const fn new() -> Self {
        Self {
            fft: [0.0; MAX_BINS],
            levels: [0.0; MAX_COLS],
            bin_start: [0.0; MAX_COLS],
            first: [0; MAX_COLS],
            last: [0; MAX_COLS],
            power: [0.0; POWER_TABLE_LEN],
            bin_ratio: 1.0,
            cols: 0,
            bins: 0,
            min_db: MIN_DB,
            db_span: 80.0,
        }
    }
}

// The module is single-threaded and JS can only reach this state through the
// exported buffers, so mutable access cannot alias across calls.
struct SyncState(UnsafeCell<State>);
unsafe impl Sync for SyncState {}

static STATE: SyncState = SyncState(UnsafeCell::new(State::new()));

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

#[inline]
fn power_at(db: f32, table: &[f32; POWER_TABLE_LEN]) -> f32 {
    let scaled = if db.is_finite() {
        (db - MIN_DB) * 10.0
    } else {
        0.0
    };
    let scaled = if scaled < 0.0 {
        0.0
    } else if scaled > 900.0 {
        900.0
    } else {
        scaled
    };
    let index = scaled as usize;
    let fraction = scaled - index as f32;
    if index >= POWER_TABLE_LEN - 1 {
        table[POWER_TABLE_LEN - 1]
    } else {
        table[index] + (table[index + 1] - table[index]) * fraction
    }
}

#[inline]
fn log2(x: f32) -> f32 {
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    let mantissa = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    let s = (mantissa - 1.0) / (mantissa + 1.0);
    let s2 = s * s;
    let ln = 2.0 * s * (1.0 + s2 * (1.0 / 3.0 + s2 * (1.0 / 5.0 + s2 * (1.0 / 7.0))));
    exponent as f32 + ln * 1.442_695
}

#[unsafe(no_mangle)]
pub extern "C" fn fft_ptr() -> *mut f32 {
    unsafe { (*STATE.0.get()).fft.as_mut_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn levels_ptr() -> *mut f32 {
    unsafe { (*STATE.0.get()).levels.as_mut_ptr() }
}

#[unsafe(no_mangle)]
pub extern "C" fn init(
    cols: u32,
    bins: u32,
    start_bin: f32,
    ratio: f32,
    min_db: f32,
    db_span: f32,
) -> u32 {
    let state = unsafe { &mut *STATE.0.get() };
    let cols = (cols as usize).min(MAX_COLS);
    let bins = (bins as usize).min(MAX_BINS);
    state.cols = cols;
    state.bins = bins;
    state.bin_ratio = if ratio > 1.0 { ratio } else { 1.0 };
    state.min_db = min_db;
    state.db_span = if db_span > 0.0 { db_span } else { 1.0 };

    let mut power = 1e-9;
    for value in &mut state.power {
        *value = power;
        power *= POWER_RATIO;
    }

    let bins_f = bins as f32;
    let max_bin = bins.saturating_sub(1);
    let mut b0 = if start_bin > 0.0 { start_bin } else { 0.0 };
    for i in 0..cols {
        let b1 = b0 * state.bin_ratio;
        let end = if b1 < bins_f { b1 } else { bins_f };
        let first = if b0 < bins_f { b0 as usize } else { max_bin };
        let mut last = if end > 0.0 {
            (end as usize).min(max_bin)
        } else {
            0
        };
        if last as f32 >= end && last > 0 {
            last -= 1;
        }
        if last < first {
            last = first;
        }
        state.bin_start[i] = b0;
        state.first[i] = first as u16;
        state.last[i] = last as u16;
        b0 = b1;
    }
    state.levels[..cols].fill(0.0);
    cols as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn process(active: u32) {
    let state = unsafe { &mut *STATE.0.get() };
    let cols = state.cols;
    let bins = state.bins;
    if cols == 0 || bins == 0 {
        return;
    }

    let ratio = state.bin_ratio;
    let min_db = state.min_db;
    let span = state.db_span;
    let fft = &state.fft[..bins];
    let starts = &state.bin_start[..cols];
    let firsts = &state.first[..cols];
    let lasts = &state.last[..cols];
    let table = &state.power;
    let levels = &mut state.levels[..cols];
    let max_end = bins as f32;

    for (i, level) in levels.iter_mut().enumerate() {
        let target = if active == 0 {
            0.0
        } else {
            let b0 = if starts[i] < max_end {
                starts[i]
            } else {
                max_end
            };
            let mut b1 = b0 * ratio;
            if b1 > max_end {
                b1 = max_end;
            }
            let weight = b1 - b0;
            let first = firsts[i] as usize;
            let last = lasts[i] as usize;

            let power = if weight <= 0.0 {
                0.0
            } else if first == last {
                power_at(fft[first], table) * weight
            } else {
                let w_first = (first + 1) as f32 - b0;
                let w_last = b1 - last as f32;
                let middle = &fft[(first + 1)..last];
                let mut power = power_at(fft[first], table) * w_first;
                for &db in middle {
                    power += power_at(db, table);
                }
                power + power_at(fft[last], table) * w_last
            };

            if weight > 0.0 {
                let db = 3.010_3 * log2(power / weight);
                let level = (db - min_db) / span;
                if level < 0.0 {
                    0.0
                } else if level > 1.0 {
                    1.0
                } else {
                    level
                }
            } else {
                0.0
            }
        };

        let k = if target > *level { 1.0 } else { 0.12 };
        *level += (target - *level) * k;
    }
}
