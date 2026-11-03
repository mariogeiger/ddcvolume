//! The time line and sample ring shared by the loopback pair: the visible
//! output device writes its final mix into the ring at its output sample time,
//! the hidden tap device reads it back at its input sample time and clears it.
//!
//! Both devices tick on one clock, so a sample time names the same ring slot
//! for both. The ring length is also the zero-time-stamp period, so the clock
//! is a pure function of the host time and the anchor taken when IO starts.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub const SAMPLE_RATE: f64 = 48_000.0;
pub const CHANNELS: usize = 2;
/// Frames in the ring and in one zero-time-stamp period (~341 ms).
pub const RING_FRAMES: usize = 16_384;

const RING_SAMPLES: usize = RING_FRAMES * CHANNELS;

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}

/// Host ticks per sample frame.
fn ticks_per_frame() -> f64 {
    static TICKS: OnceLock<f64> = OnceLock::new();
    *TICKS.get_or_init(|| {
        let mut base = [0u32; 2];
        unsafe { mach_timebase_info(&mut base) };
        let [numer, denom] = base;
        1e9 * denom as f64 / numer as f64 / SAMPLE_RATE
    })
}

static ANCHOR: AtomicU64 = AtomicU64::new(0);
static RUNNING: AtomicUsize = AtomicUsize::new(0);
/// f32 samples stored as bits, since writer and reader are distinct IO threads.
static RING: [AtomicU32; RING_SAMPLES] = [const { AtomicU32::new(0) }; RING_SAMPLES];

/// Count one more running IO client; the first one anchors the time line.
pub fn start() {
    ticks_per_frame();
    if RUNNING.fetch_add(1, Relaxed) == 0 {
        ANCHOR.store(unsafe { mach_absolute_time() }, Relaxed);
    }
}

pub fn stop() {
    RUNNING.fetch_sub(1, Relaxed);
}

/// The latest period boundary at or before now, as (sample time, host time).
pub fn zero_time_stamp() -> (f64, u64) {
    let anchor = ANCHOR.load(Relaxed);
    let period = ticks_per_frame() * RING_FRAMES as f64;
    let elapsed = unsafe { mach_absolute_time() }.saturating_sub(anchor);
    let periods = (elapsed as f64 / period).floor();
    (periods * RING_FRAMES as f64, anchor + (periods * period) as u64)
}

/// Ring slots of `samples` consecutive interleaved samples from `sample_time`.
fn slots(sample_time: f64, samples: usize) -> impl Iterator<Item = &'static AtomicU32> {
    let first = (sample_time as i64).rem_euclid(RING_FRAMES as i64) as usize * CHANNELS;
    (0..samples).map(move |i| &RING[(first + i) % RING_SAMPLES])
}

/// Store the device mix, or silence when muted.
pub fn write(sample_time: f64, mix: &[f32], muted: bool) {
    for (slot, &sample) in slots(sample_time, mix.len()).zip(mix) {
        slot.store(if muted { 0 } else { sample.to_bits() }, Relaxed);
    }
}

/// Take samples out of the ring, leaving silence so a stopped writer is heard
/// as silence rather than as the last period looping.
pub fn read(sample_time: f64, out: &mut [f32]) {
    for (slot, sample) in slots(sample_time, out.len()).zip(out) {
        *sample = f32::from_bits(slot.swap(0, Relaxed));
    }
}
