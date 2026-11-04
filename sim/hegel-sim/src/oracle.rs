//! Invariants a simulated program states about itself.

/// `cond` must hold every time this runs; otherwise the case fails (with the
/// simulation's trace when one is running).
#[track_caller]
pub fn always(cond: bool, name: &'static str) {
    if !cond {
        panic!("hegel-sim: always({name}) failed");
    }
}

/// This point must never be reached.
#[track_caller]
pub fn unreachable(name: &'static str) -> ! {
    panic!("hegel-sim: unreachable({name}) was reached");
}

/// `cond` should hold at least once across the run; recorded for
/// `Sim::report`, never a failure.
pub fn sometimes(cond: bool, name: &'static str) {
    #[cfg(feature = "sim")]
    crate::rt::sometimes(name, cond);
    #[cfg(not(feature = "sim"))]
    let _ = (cond, name);
}
