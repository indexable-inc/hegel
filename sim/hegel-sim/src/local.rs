//! `thread_local!`. Under the simulation each simulated thread gets its own
//! value (simulated threads share one OS thread, so std's would be shared). A
//! killed process's values are leaked, not dropped, as SIGKILL does.

#[cfg(not(feature = "sim"))]
pub use std::thread::LocalKey;
#[cfg(not(feature = "sim"))]
pub use std::thread_local;

#[cfg(feature = "sim")]
pub use imp::LocalKey;

/// `std::thread_local!`, keyed by simulated thread under the simulation.
#[cfg(feature = "sim")]
#[macro_export]
macro_rules! thread_local {
    () => {};
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = const $init:block; $($rest:tt)*) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init; $($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = const $init:block) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = $init:expr; $($rest:tt)*) => {
        $crate::thread_local!($(#[$attr])* $vis static $name: $t = $init);
        $crate::thread_local!($($rest)*);
    };
    ($(#[$attr:meta])* $vis:vis static $name:ident : $t:ty = $init:expr) => {
        $(#[$attr])* $vis static $name: $crate::local::LocalKey<$t> = {
            ::std::thread_local!(static INNER: $t = $init);
            fn __init() -> $t {
                $init
            }
            $crate::local::LocalKey::new(&INNER, __init)
        };
    };
}

#[cfg(feature = "sim")]
mod imp {
    use std::any::Any;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::thread::AccessError;

    use crate::rt;

    /// `std::thread::LocalKey`.
    pub struct LocalKey<T: 'static> {
        std: &'static std::thread::LocalKey<T>,
        init: fn() -> T,
    }

    impl<T: 'static> LocalKey<T> {
        #[doc(hidden)]
        pub const fn new(std: &'static std::thread::LocalKey<T>, init: fn() -> T) -> LocalKey<T> {
            LocalKey { std, init }
        }

        fn sim_value(&'static self) -> Rc<T> {
            let key = (rt::current_tid(), self as *const Self as usize);
            if let Some(v) = rt::with(|w| w.tls.get(&key).cloned()) {
                return v.downcast::<T>().unwrap_or_else(|_| unreachable!());
            }
            let v: Rc<T> = Rc::new((self.init)());
            let any: Rc<dyn Any> = v.clone();
            rt::with(|w| w.tls.insert(key, any));
            v
        }

        /// See std.
        pub fn with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
            if rt::in_sim() {
                let v = self.sim_value();
                f(&v)
            } else {
                self.std.with(f)
            }
        }

        /// See std.
        pub fn try_with<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> Result<R, AccessError> {
            if rt::in_sim() { Ok(self.with(f)) } else { self.std.try_with(f) }
        }
    }

    impl<T: 'static> LocalKey<Cell<T>> {
        /// See std.
        pub fn set(&'static self, v: T) {
            self.with(|c| c.set(v));
        }

        /// See std.
        pub fn get(&'static self) -> T
        where
            T: Copy,
        {
            self.with(Cell::get)
        }

        /// See std.
        pub fn take(&'static self) -> T
        where
            T: Default,
        {
            self.with(Cell::take)
        }

        /// See std.
        pub fn replace(&'static self, v: T) -> T {
            self.with(|c| c.replace(v))
        }
    }

    impl<T: 'static> LocalKey<RefCell<T>> {
        /// See std.
        pub fn with_borrow<F: FnOnce(&T) -> R, R>(&'static self, f: F) -> R {
            self.with(|c| f(&c.borrow()))
        }

        /// See std.
        pub fn with_borrow_mut<F: FnOnce(&mut T) -> R, R>(&'static self, f: F) -> R {
            self.with(|c| f(&mut c.borrow_mut()))
        }

        /// See std.
        pub fn set(&'static self, v: T) {
            self.with(|c| *c.borrow_mut() = v);
        }

        /// See std.
        pub fn take(&'static self) -> T
        where
            T: Default,
        {
            self.with(RefCell::take)
        }

        /// See std.
        pub fn replace(&'static self, v: T) -> T {
            self.with(|c| c.replace(v))
        }
    }
}
