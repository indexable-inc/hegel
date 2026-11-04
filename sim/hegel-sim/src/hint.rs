//! `std::hint`, with `spin_loop` as a yield under the simulation.

#[cfg(not(feature = "sim"))]
pub use std::hint::*;

#[cfg(feature = "sim")]
pub use std::hint::{black_box, unreachable_unchecked};

/// `std::hint::spin_loop`; under the simulation the caller yields (PCT demotes
/// it until another thread takes a step), so a spin loop cannot livelock.
#[cfg(feature = "sim")]
pub fn spin_loop() {
    if crate::rt::in_sim() {
        crate::rt::request(crate::rt::Req::Yield);
    }
    std::hint::spin_loop();
}
