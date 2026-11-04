//! `std::time`. Under the simulation `Instant` reads the virtual monotonic clock
//! and `SystemTime` the virtual realtime clock (which the `ClockJumps` fault
//! class moves backwards and forwards).

#[cfg(not(feature = "sim"))]
pub use std::time::*;

#[cfg(feature = "sim")]
pub use imp::*;
#[cfg(feature = "sim")]
pub use std::time::{Duration, TryFromFloatSecsError};

#[cfg(feature = "sim")]
pub(crate) fn mono_ns() -> u64 {
    crate::rt::with(|w| w.clock.mono_ns())
}

#[cfg(feature = "sim")]
mod imp {
    use std::ops::{Add, AddAssign, Sub, SubAssign};
    use std::sync::OnceLock;
    use std::time::Duration;

    use crate::rt;

    fn base() -> std::time::Instant {
        static BASE: OnceLock<std::time::Instant> = OnceLock::new();
        *BASE.get_or_init(std::time::Instant::now)
    }

    /// `std::time::Instant`: nanoseconds on the virtual monotonic clock under the
    /// simulation, on the host's monotonic clock otherwise.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Instant(u64);

    impl Instant {
        /// See std.
        pub fn now() -> Instant {
            if rt::in_sim() {
                Instant(super::mono_ns())
            } else {
                Instant(base().elapsed().as_nanos() as u64 + 1)
            }
        }

        /// See std.
        pub fn duration_since(&self, earlier: Instant) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(earlier.0))
        }

        /// See std.
        pub fn checked_duration_since(&self, earlier: Instant) -> Option<Duration> {
            self.0.checked_sub(earlier.0).map(Duration::from_nanos)
        }

        /// See std.
        pub fn saturating_duration_since(&self, earlier: Instant) -> Duration {
            self.duration_since(earlier)
        }

        /// See std.
        pub fn elapsed(&self) -> Duration {
            Instant::now().duration_since(*self)
        }

        /// See std.
        pub fn checked_add(&self, d: Duration) -> Option<Instant> {
            u64::try_from(d.as_nanos()).ok().and_then(|n| self.0.checked_add(n)).map(Instant)
        }

        /// See std.
        pub fn checked_sub(&self, d: Duration) -> Option<Instant> {
            u64::try_from(d.as_nanos()).ok().and_then(|n| self.0.checked_sub(n)).map(Instant)
        }
    }

    impl Add<Duration> for Instant {
        type Output = Instant;
        fn add(self, d: Duration) -> Instant {
            self.checked_add(d).unwrap_or_else(|| panic!("overflow when adding duration to instant"))
        }
    }

    impl AddAssign<Duration> for Instant {
        fn add_assign(&mut self, d: Duration) {
            *self = *self + d;
        }
    }

    impl Sub<Duration> for Instant {
        type Output = Instant;
        fn sub(self, d: Duration) -> Instant {
            self.checked_sub(d).unwrap_or_else(|| panic!("overflow when subtracting duration from instant"))
        }
    }

    impl SubAssign<Duration> for Instant {
        fn sub_assign(&mut self, d: Duration) {
            *self = *self - d;
        }
    }

    impl Sub<Instant> for Instant {
        type Output = Duration;
        fn sub(self, other: Instant) -> Duration {
            self.duration_since(other)
        }
    }

    /// `std::time::SystemTime`: nanoseconds since the Unix epoch.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct SystemTime(i128);

    /// See std.
    pub const UNIX_EPOCH: SystemTime = SystemTime(0);

    /// `std::time::SystemTimeError` (std's cannot be constructed outside std).
    #[derive(Clone, Debug)]
    pub struct SystemTimeError(Duration);

    impl SystemTimeError {
        /// See std.
        pub fn duration(&self) -> Duration {
            self.0
        }
    }

    impl std::fmt::Display for SystemTimeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("second time provided was later than self")
        }
    }

    impl std::error::Error for SystemTimeError {}

    fn dur(ns: i128) -> Duration {
        Duration::new((ns / 1_000_000_000) as u64, (ns % 1_000_000_000) as u32)
    }

    impl SystemTime {
        /// See std.
        pub const UNIX_EPOCH: SystemTime = UNIX_EPOCH;

        /// See std.
        pub fn now() -> SystemTime {
            if rt::in_sim() {
                SystemTime(rt::with(|w| w.clock.realtime_ns()))
            } else {
                match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    Ok(d) => SystemTime(d.as_nanos() as i128),
                    Err(e) => SystemTime(-(e.duration().as_nanos() as i128)),
                }
            }
        }

        /// See std.
        pub fn duration_since(&self, earlier: SystemTime) -> Result<Duration, SystemTimeError> {
            let d = self.0 - earlier.0;
            if d >= 0 { Ok(dur(d)) } else { Err(SystemTimeError(dur(-d))) }
        }

        /// See std.
        pub fn elapsed(&self) -> Result<Duration, SystemTimeError> {
            SystemTime::now().duration_since(*self)
        }

        /// See std.
        pub fn checked_add(&self, d: Duration) -> Option<SystemTime> {
            self.0.checked_add(d.as_nanos() as i128).map(SystemTime)
        }

        /// See std.
        pub fn checked_sub(&self, d: Duration) -> Option<SystemTime> {
            self.0.checked_sub(d.as_nanos() as i128).map(SystemTime)
        }
    }

    impl Add<Duration> for SystemTime {
        type Output = SystemTime;
        fn add(self, d: Duration) -> SystemTime {
            self.checked_add(d).unwrap_or_else(|| panic!("overflow when adding duration to system time"))
        }
    }

    impl Sub<Duration> for SystemTime {
        type Output = SystemTime;
        fn sub(self, d: Duration) -> SystemTime {
            self.checked_sub(d).unwrap_or_else(|| panic!("overflow when subtracting duration from system time"))
        }
    }
}
