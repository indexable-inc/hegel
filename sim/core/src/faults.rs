//! Fault classes, per-site rates, and swarm selection.

use crate::Choices;

/// A class of fault the simulation can inject. Each run enables a drawn subset
/// (swarm testing: Groce et al., ISSTA'12), so one class does not mask the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Fault {
    /// kill -9 of one process at a crash point.
    Kill9,
    /// Power loss: every process dies, unsynced pages and directory operations resolve.
    PowerLoss,
    /// Kernel crash: like power loss, but the device's volatile cache survives.
    OsCrash,
    /// A page written back at a crash can be torn at the atomic write unit.
    TornWrites,
    /// A size extension can survive a crash without its data (XFS, ext4 data=writeback).
    ZerosPastEof,
    /// `fsync` reports EIO once per description; dirty pages may be dropped.
    FsyncEio,
    /// A block-allocating `write` fails with ENOSPC or writes a short prefix.
    Enospc,
    /// Delayed allocation reports ENOSPC at `fsync`.
    EnospcAtFsync,
    /// Sleeps and timed waits return late (never early).
    LateWakeups,
    /// The realtime clock jumps.
    ClockJumps,
    /// Condition variable waits wake spuriously.
    SpuriousWakeups,
    /// An asynchronous SIGTERM arrives at a drawn point.
    AsyncSignals,
    /// `readdir` returns entries in a drawn order.
    ReaddirShuffle,
    /// Each switch point may also preempt to a drawn runnable thread.
    RandomPreemption,
}

impl Fault {
    /// Every class, in declaration order.
    pub const ALL: [Fault; 14] = [
        Fault::Kill9,
        Fault::PowerLoss,
        Fault::OsCrash,
        Fault::TornWrites,
        Fault::ZerosPastEof,
        Fault::FsyncEio,
        Fault::Enospc,
        Fault::EnospcAtFsync,
        Fault::LateWakeups,
        Fault::ClockJumps,
        Fault::SpuriousWakeups,
        Fault::AsyncSignals,
        Fault::ReaddirShuffle,
        Fault::RandomPreemption,
    ];

    /// The crash classes.
    pub const CRASHES: [Fault; 3] = [Fault::Kill9, Fault::PowerLoss, Fault::OsCrash];
}

/// A set of fault classes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FaultSet(u32);

impl FaultSet {
    /// No class.
    pub const fn empty() -> Self {
        FaultSet(0)
    }

    /// Every class.
    pub fn all() -> Self {
        Fault::ALL.iter().copied().collect()
    }

    /// Whether `f` is in the set.
    pub fn has(self, f: Fault) -> bool {
        self.0 & (1 << f as u8) != 0
    }

    /// Add `f`.
    pub fn insert(&mut self, f: Fault) {
        self.0 |= 1 << f as u8;
    }

    /// Remove `f`.
    pub fn remove(&mut self, f: Fault) {
        self.0 &= !(1 << f as u8);
    }

    /// The classes in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = Fault> {
        Fault::ALL.into_iter().filter(move |f| self.has(*f))
    }
}

impl FromIterator<Fault> for FaultSet {
    fn from_iter<I: IntoIterator<Item = Fault>>(iter: I) -> Self {
        let mut s = FaultSet::empty();
        for f in iter {
            s.insert(f);
        }
        s
    }
}

/// Per-site probabilities of each fault, used only when its class is enabled.
#[derive(Clone, Debug)]
pub struct Rates {
    /// Per `fsync` with dirty data.
    pub fsync_eio: f64,
    /// Per block-allocating `write`.
    pub enospc: f64,
    /// Per `fsync` that allocates.
    pub enospc_at_fsync: f64,
    /// Per dirty page at a crash: written back whole (otherwise old, or torn).
    pub page_written: f64,
    /// Per page written back at a crash: torn at the atomic write unit.
    pub torn: f64,
    /// Per sleep or timed wait.
    pub late_wakeup: f64,
    /// Per condition variable wait.
    pub spurious_wakeup: f64,
    /// Per syscall.
    pub async_signal: f64,
    /// Per syscall.
    pub clock_jump: f64,
    /// Per switch point, under `RandomPreemption`.
    pub random_preemption: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Rates {
            fsync_eio: 1.0 / 8.0,
            enospc: 1.0 / 16.0,
            enospc_at_fsync: 1.0 / 16.0,
            page_written: 0.5,
            torn: 0.25,
            late_wakeup: 0.25,
            spurious_wakeup: 0.125,
            async_signal: 1.0 / 128.0,
            clock_jump: 1.0 / 128.0,
            random_preemption: 1.0 / 64.0,
        }
    }
}

/// The fault classes this run enabled, and their rates.
#[derive(Clone, Debug)]
pub struct Swarm {
    /// Enabled classes.
    pub enabled: FaultSet,
    /// Rates for enabled classes.
    pub rates: Rates,
}

impl Swarm {
    /// Draw which of `allowed` this run enables. A `false` draw (the simplest)
    /// means enabled, so an all-zero tape runs with every allowed class on while
    /// each individual fault draw still defaults to no fault; shrinking turns off
    /// the classes a failure does not need.
    pub fn draw(c: &dyn Choices, allowed: FaultSet, rates: Rates) -> Swarm {
        let mut enabled = FaultSet::empty();
        for f in allowed.iter() {
            if !c.coin("swarm.off", 0.5) {
                enabled.insert(f);
            }
        }
        Swarm { enabled, rates }
    }

    /// Every allowed class on, no draw.
    pub fn fixed(enabled: FaultSet, rates: Rates) -> Swarm {
        Swarm { enabled, rates }
    }

    /// Whether `f` is enabled.
    pub fn on(&self, f: Fault) -> bool {
        self.enabled.has(f)
    }

    /// Draw a fault of class `f` at a site with probability `p`, if the class is on.
    pub fn fire(&self, c: &dyn Choices, f: Fault, p: f64) -> bool {
        self.on(f) && c.coin(fault_label(f), p)
    }
}

fn fault_label(f: Fault) -> &'static str {
    match f {
        Fault::Kill9 => "fault.kill9",
        Fault::PowerLoss => "fault.power_loss",
        Fault::OsCrash => "fault.os_crash",
        Fault::TornWrites => "fault.torn",
        Fault::ZerosPastEof => "fault.zeros_past_eof",
        Fault::FsyncEio => "fault.fsync_eio",
        Fault::Enospc => "fault.enospc",
        Fault::EnospcAtFsync => "fault.enospc_at_fsync",
        Fault::LateWakeups => "fault.late_wakeup",
        Fault::ClockJumps => "fault.clock_jump",
        Fault::SpuriousWakeups => "fault.spurious_wakeup",
        Fault::AsyncSignals => "fault.async_signal",
        Fault::ReaddirShuffle => "fault.readdir_shuffle",
        Fault::RandomPreemption => "fault.random_preemption",
    }
}
