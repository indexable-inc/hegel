//! The virtual clock: monotonic and realtime time, and timers.

use std::collections::BTreeMap;

use crate::sched::WaitKey;

/// Monotonic time never goes backward (clock_gettime(2), `CLOCK_MONOTONIC`: "a
/// nonsettable system-wide clock"); realtime is monotonic plus an offset that a
/// clock step can move either way (clock_settime(2), `CLOCK_REALTIME`). Time
/// moves only when the driver advances it: per step, per syscall, or by jumping
/// to the earliest timer when nothing is runnable.
#[derive(Clone, Debug)]
pub struct Clock {
    mono_ns: u64,
    realtime_offset_ns: i128,
    timers: BTreeMap<(u64, u64), WaitKey>,
    next_seq: u64,
}

/// A registered timer, for cancelling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimerId(u64, u64);

/// Realtime at monotonic zero: 2026-01-01T00:00:00Z, so the realtime clock reads
/// as a plausible date.
pub const REALTIME_EPOCH_NS: i128 = 1_767_225_600 * 1_000_000_000;

impl Default for Clock {
    fn default() -> Self {
        Clock {
            mono_ns: 0,
            realtime_offset_ns: REALTIME_EPOCH_NS,
            timers: BTreeMap::new(),
            next_seq: 0,
        }
    }
}

impl Clock {
    /// A clock at monotonic zero.
    pub fn new() -> Self {
        Clock::default()
    }

    /// Monotonic nanoseconds since the simulated machine booted.
    pub fn mono_ns(&self) -> u64 {
        self.mono_ns
    }

    /// Realtime nanoseconds since the Unix epoch (can be negative after a jump).
    pub fn realtime_ns(&self) -> i128 {
        self.mono_ns as i128 + self.realtime_offset_ns
    }

    /// Move monotonic time forward by `ns`.
    pub fn advance(&mut self, ns: u64) {
        self.mono_ns = self.mono_ns.saturating_add(ns);
    }

    /// Step the realtime clock by `delta_ns` (either sign); monotonic is untouched.
    pub fn jump_realtime(&mut self, delta_ns: i128) {
        self.realtime_offset_ns += delta_ns;
    }

    /// Wake `key` at monotonic `deadline_ns` (or at once if it has passed).
    pub fn add_timer(&mut self, deadline_ns: u64, key: WaitKey) -> TimerId {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.timers.insert((deadline_ns, seq), key);
        TimerId(deadline_ns, seq)
    }

    /// Remove a timer that has not fired.
    pub fn cancel(&mut self, id: TimerId) {
        self.timers.remove(&(id.0, id.1));
    }

    /// The earliest pending deadline.
    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.keys().next().map(|k| k.0)
    }

    /// Remove and return every timer due at or before now, in deadline order.
    pub fn fire_due(&mut self) -> Vec<WaitKey> {
        let mut out = Vec::new();
        while let Some((&(d, s), _)) = self.timers.iter().next() {
            if d > self.mono_ns {
                break;
            }
            if let Some(k) = self.timers.remove(&(d, s)) {
                out.push(k);
            }
        }
        out
    }

    /// Jump monotonic time to the earliest deadline, if there is one; returns
    /// whether time moved or a timer is due.
    pub fn jump_to_next(&mut self) -> bool {
        match self.next_deadline() {
            Some(d) => {
                self.mono_ns = self.mono_ns.max(d);
                true
            }
            None => false,
        }
    }
}
