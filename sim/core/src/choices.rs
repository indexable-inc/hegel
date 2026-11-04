//! The port every simulated choice goes through.

use std::cell::Cell;

/// The source of every choice the simulation makes.
///
/// Draw 0 (the low end of a range, `false` for a coin) is always the calm
/// choice: no preemption, no fault, nothing lost. A tape of zeros is the
/// simplest run, so Hegel's shrinker moves a failing run toward calm.
pub trait Choices {
    /// An integer in `lo..=hi`.
    fn int(&self, label: &'static str, lo: u64, hi: u64) -> u64;
    /// A coin that comes up `true` with probability `p`; `false` is simplest.
    fn coin(&self, label: &'static str, p: f64) -> bool;
    /// A line of explanation, printed only with a failing case.
    fn note(&self, line: &str);
}

/// [`Choices`] from a seeded SplitMix64, for runs outside Hegel (no shrinking).
#[derive(Debug)]
pub struct SeededChoices {
    state: Cell<u64>,
    log: Cell<bool>,
}

impl SeededChoices {
    /// A source seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        SeededChoices {
            state: Cell::new(seed),
            log: Cell::new(false),
        }
    }

    /// Print notes to stderr (off by default).
    pub fn print_notes(self, on: bool) -> Self {
        self.log.set(on);
        self
    }

    fn next(&self) -> u64 {
        let s = self.state.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.state.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl Choices for SeededChoices {
    fn int(&self, _label: &'static str, lo: u64, hi: u64) -> u64 {
        if lo >= hi {
            return lo;
        }
        let span = hi - lo;
        if span == u64::MAX {
            return self.next();
        }
        lo + self.next() % (span + 1)
    }

    fn coin(&self, _label: &'static str, p: f64) -> bool {
        if p <= 0.0 {
            return false;
        }
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    fn note(&self, line: &str) {
        if self.log.get() {
            eprintln!("{line}");
        }
    }
}

impl<C: Choices + ?Sized> Choices for &C {
    fn int(&self, label: &'static str, lo: u64, hi: u64) -> u64 {
        (**self).int(label, lo, hi)
    }
    fn coin(&self, label: &'static str, p: f64) -> bool {
        (**self).coin(label, p)
    }
    fn note(&self, line: &str) {
        (**self).note(line)
    }
}
