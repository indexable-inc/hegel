//! `std::sync::atomic`. Under the simulation every operation is a switch point
//! *before* it takes effect, which is what lets the scheduler interleave two
//! threads between any two atomics. The types are `#[repr(transparent)]` over
//! std's, so a pointer into shared memory can be cast to them.

#[cfg(not(feature = "sim"))]
pub use std::sync::atomic::*;

#[cfg(feature = "sim")]
pub use std::sync::atomic::{Ordering, compiler_fence};

#[cfg(feature = "sim")]
pub use imp::*;

#[cfg(feature = "sim")]
mod imp {
    use std::sync::atomic::{self as std_atomic, Ordering};

    use crate::rt;

    /// `std::sync::atomic::fence`, as a switch point.
    pub fn fence(order: Ordering) {
        rt::op("fence", 0);
        std_atomic::fence(order);
    }

    macro_rules! int_atomic {
        ($name:ident, $t:ty) => {
            #[doc = concat!("`std::sync::atomic::", stringify!($name), "`; every operation is a switch point under the simulation.")]
            #[repr(transparent)]
            #[derive(Default)]
            pub struct $name(std_atomic::$name);

            impl $name {
                /// See std.
                pub const fn new(v: $t) -> Self {
                    Self(std_atomic::$name::new(v))
                }
                fn addr(&self) -> usize {
                    self as *const Self as usize
                }
                /// See std.
                pub fn load(&self, o: Ordering) -> $t {
                    rt::op("atomic.load", self.addr());
                    let v = self.0.load(o);
                    rt::after_load(self.addr(), v as u64);
                    v
                }
                /// See std.
                pub fn store(&self, v: $t, o: Ordering) {
                    rt::op("atomic.store", self.addr());
                    self.0.store(v, o)
                }
                /// See std.
                pub fn swap(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.swap", self.addr());
                    self.0.swap(v, o)
                }
                /// See std.
                pub fn compare_exchange(&self, c: $t, n: $t, s: Ordering, f: Ordering) -> Result<$t, $t> {
                    rt::op("atomic.cas", self.addr());
                    self.0.compare_exchange(c, n, s, f)
                }
                /// See std. Never fails spuriously under the simulation.
                pub fn compare_exchange_weak(&self, c: $t, n: $t, s: Ordering, f: Ordering) -> Result<$t, $t> {
                    rt::op("atomic.cas", self.addr());
                    self.0.compare_exchange(c, n, s, f)
                }
                /// See std.
                pub fn fetch_add(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_add", self.addr());
                    self.0.fetch_add(v, o)
                }
                /// See std.
                pub fn fetch_sub(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_sub", self.addr());
                    self.0.fetch_sub(v, o)
                }
                /// See std.
                pub fn fetch_and(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_and", self.addr());
                    self.0.fetch_and(v, o)
                }
                /// See std.
                pub fn fetch_or(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_or", self.addr());
                    self.0.fetch_or(v, o)
                }
                /// See std.
                pub fn fetch_xor(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_xor", self.addr());
                    self.0.fetch_xor(v, o)
                }
                /// See std.
                pub fn fetch_max(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_max", self.addr());
                    self.0.fetch_max(v, o)
                }
                /// See std.
                pub fn fetch_min(&self, v: $t, o: Ordering) -> $t {
                    rt::op("atomic.fetch_min", self.addr());
                    self.0.fetch_min(v, o)
                }
                /// See std: a load then compare-exchange loop, each a switch point.
                pub fn fetch_update<F: FnMut($t) -> Option<$t>>(&self, s: Ordering, f: Ordering, mut g: F) -> Result<$t, $t> {
                    let mut prev = self.load(f);
                    while let Some(next) = g(prev) {
                        match self.compare_exchange_weak(prev, next, s, f) {
                            Ok(x) => return Ok(x),
                            Err(x) => prev = x,
                        }
                    }
                    Err(prev)
                }
                /// See std.
                pub fn get_mut(&mut self) -> &mut $t {
                    self.0.get_mut()
                }
                /// See std.
                pub fn into_inner(self) -> $t {
                    self.0.into_inner()
                }
                /// See std.
                pub const fn as_ptr(&self) -> *mut $t {
                    self.0.as_ptr()
                }
            }

            impl From<$t> for $name {
                fn from(v: $t) -> Self {
                    Self::new(v)
                }
            }

            impl std::fmt::Debug for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    std::fmt::Debug::fmt(&self.0, f)
                }
            }
        };
    }

    int_atomic!(AtomicU8, u8);
    int_atomic!(AtomicU16, u16);
    int_atomic!(AtomicU32, u32);
    int_atomic!(AtomicU64, u64);
    int_atomic!(AtomicUsize, usize);
    int_atomic!(AtomicI8, i8);
    int_atomic!(AtomicI16, i16);
    int_atomic!(AtomicI32, i32);
    int_atomic!(AtomicI64, i64);
    int_atomic!(AtomicIsize, isize);

    /// `std::sync::atomic::AtomicBool`; every operation is a switch point under the simulation.
    #[repr(transparent)]
    #[derive(Default)]
    pub struct AtomicBool(std_atomic::AtomicBool);

    impl AtomicBool {
        /// See std.
        pub const fn new(v: bool) -> Self {
            Self(std_atomic::AtomicBool::new(v))
        }
        fn addr(&self) -> usize {
            self as *const Self as usize
        }
        /// See std.
        pub fn load(&self, o: Ordering) -> bool {
            rt::op("atomic.load", self.addr());
            let v = self.0.load(o);
            rt::after_load(self.addr(), u64::from(v));
            v
        }
        /// See std.
        pub fn store(&self, v: bool, o: Ordering) {
            rt::op("atomic.store", self.addr());
            self.0.store(v, o)
        }
        /// See std.
        pub fn swap(&self, v: bool, o: Ordering) -> bool {
            rt::op("atomic.swap", self.addr());
            self.0.swap(v, o)
        }
        /// See std.
        pub fn compare_exchange(&self, c: bool, n: bool, s: Ordering, f: Ordering) -> Result<bool, bool> {
            rt::op("atomic.cas", self.addr());
            self.0.compare_exchange(c, n, s, f)
        }
        /// See std. Never fails spuriously under the simulation.
        pub fn compare_exchange_weak(&self, c: bool, n: bool, s: Ordering, f: Ordering) -> Result<bool, bool> {
            self.compare_exchange(c, n, s, f)
        }
        /// See std.
        pub fn fetch_and(&self, v: bool, o: Ordering) -> bool {
            rt::op("atomic.fetch_and", self.addr());
            self.0.fetch_and(v, o)
        }
        /// See std.
        pub fn fetch_or(&self, v: bool, o: Ordering) -> bool {
            rt::op("atomic.fetch_or", self.addr());
            self.0.fetch_or(v, o)
        }
        /// See std.
        pub fn fetch_xor(&self, v: bool, o: Ordering) -> bool {
            rt::op("atomic.fetch_xor", self.addr());
            self.0.fetch_xor(v, o)
        }
        /// See std.
        pub fn get_mut(&mut self) -> &mut bool {
            self.0.get_mut()
        }
        /// See std.
        pub fn into_inner(self) -> bool {
            self.0.into_inner()
        }
    }

    impl std::fmt::Debug for AtomicBool {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.0, f)
        }
    }

    /// `std::sync::atomic::AtomicPtr`; every operation is a switch point under the simulation.
    #[repr(transparent)]
    pub struct AtomicPtr<T>(std_atomic::AtomicPtr<T>);

    impl<T> Default for AtomicPtr<T> {
        fn default() -> Self {
            Self::new(std::ptr::null_mut())
        }
    }

    impl<T> AtomicPtr<T> {
        /// See std.
        pub const fn new(p: *mut T) -> Self {
            Self(std_atomic::AtomicPtr::new(p))
        }
        fn addr(&self) -> usize {
            self as *const Self as usize
        }
        /// See std.
        pub fn load(&self, o: Ordering) -> *mut T {
            rt::op("atomic.load", self.addr());
            let v = self.0.load(o);
            rt::after_load(self.addr(), v as usize as u64);
            v
        }
        /// See std.
        pub fn store(&self, v: *mut T, o: Ordering) {
            rt::op("atomic.store", self.addr());
            self.0.store(v, o)
        }
        /// See std.
        pub fn swap(&self, v: *mut T, o: Ordering) -> *mut T {
            rt::op("atomic.swap", self.addr());
            self.0.swap(v, o)
        }
        /// See std.
        pub fn compare_exchange(&self, c: *mut T, n: *mut T, s: Ordering, f: Ordering) -> Result<*mut T, *mut T> {
            rt::op("atomic.cas", self.addr());
            self.0.compare_exchange(c, n, s, f)
        }
        /// See std. Never fails spuriously under the simulation.
        pub fn compare_exchange_weak(&self, c: *mut T, n: *mut T, s: Ordering, f: Ordering) -> Result<*mut T, *mut T> {
            self.compare_exchange(c, n, s, f)
        }
        /// See std.
        pub fn get_mut(&mut self) -> &mut *mut T {
            self.0.get_mut()
        }
        /// See std.
        pub fn into_inner(self) -> *mut T {
            self.0.into_inner()
        }
    }

    impl<T> std::fmt::Debug for AtomicPtr<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            std::fmt::Debug::fmt(&self.0, f)
        }
    }
}
