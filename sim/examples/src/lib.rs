//! Six planted bugs, each a small storage program in a buggy and a fixed
//! variant, written against `hegel::sim`'s facade as production code would be.
//! `tests/planted.rs` checks that the simulation catches every buggy variant
//! and passes every fixed one, and prints how many cases each catch took.

pub mod harness;

pub mod b1_fsync_rename;
pub mod b2_double_count;
pub mod b3_partial_flush;
pub mod b4_version_skew;
pub mod b5_lock_recovery;
pub mod b6_lost_update;

/// Which variant of a planted program to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// The bug is present.
    Buggy,
    /// The bug is fixed.
    Fixed,
}

impl Variant {
    /// Whether this is the fixed variant.
    pub fn fixed(self) -> bool {
        self == Variant::Fixed
    }
}
