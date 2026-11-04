//! `std::thread`. Under the simulation a spawned thread is a fiber the PCT
//! scheduler interleaves at every switch point, `sleep` advances virtual time,
//! and a panic in any simulated thread fails the run (the trace says which).

#[cfg(not(feature = "sim"))]
pub use std::thread::*;

#[cfg(feature = "sim")]
pub use imp::*;
#[cfg(feature = "sim")]
pub use std::thread::{Result, ThreadId, available_parallelism, panicking};

#[cfg(feature = "sim")]
mod imp {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hegel_sim_core::{Tid, WaitKey};

    use crate::rt::{self, Reply, Req};

    enum Inner<T> {
        Std(std::thread::JoinHandle<T>),
        Sim { tid: Tid, slot: Arc<Mutex<Option<T>>>, thread: Thread },
    }

    /// `std::thread::JoinHandle`.
    pub struct JoinHandle<T>(Inner<T>);

    impl<T> JoinHandle<T> {
        /// See std. Under the simulation, blocks the simulated thread until the
        /// target finishes.
        pub fn join(self) -> std::thread::Result<T> {
            match self.0 {
                Inner::Std(h) => h.join(),
                Inner::Sim { tid, slot, .. } => loop {
                    rt::op("thread.join", tid as usize);
                    if let Some(v) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        return Ok(v);
                    }
                    rt::request(Req::Block(WaitKey::Join(tid)));
                },
            }
        }

        /// See std.
        pub fn is_finished(&self) -> bool {
            match &self.0 {
                Inner::Std(h) => h.is_finished(),
                Inner::Sim { slot, .. } => slot.lock().unwrap_or_else(|p| p.into_inner()).is_some(),
            }
        }

        /// See std.
        pub fn thread(&self) -> Thread {
            match &self.0 {
                Inner::Std(h) => Thread(ThreadRepr::Std(h.thread().clone())),
                Inner::Sim { thread, .. } => thread.clone(),
            }
        }
    }

    #[derive(Clone, Debug)]
    enum ThreadRepr {
        Std(std::thread::Thread),
        Sim(Tid, Option<Arc<str>>),
    }

    /// `std::thread::Thread`.
    #[derive(Clone, Debug)]
    pub struct Thread(ThreadRepr);

    impl Thread {
        /// See std.
        pub fn unpark(&self) {
            match &self.0 {
                ThreadRepr::Std(t) => t.unpark(),
                ThreadRepr::Sim(tid, _) => {
                    let tid = *tid;
                    rt::op("thread.unpark", tid as usize);
                    rt::with(|w| {
                        w.threads[tid as usize].park_token = true;
                        w.sched.wake(WaitKey::Park(tid));
                    });
                }
            }
        }

        /// See std.
        pub fn name(&self) -> Option<&str> {
            match &self.0 {
                ThreadRepr::Std(t) => t.name(),
                ThreadRepr::Sim(_, n) => n.as_deref(),
            }
        }
    }

    /// See std.
    pub fn current() -> Thread {
        if !rt::in_sim() {
            return Thread(ThreadRepr::Std(std::thread::current()));
        }
        let (tid, name) = rt::with(|w| {
            let tid = w.current.unwrap_or_default();
            (tid, w.threads[tid as usize].name.clone())
        });
        Thread(ThreadRepr::Sim(tid, Some(name.into())))
    }

    /// `std::thread::Builder`.
    #[derive(Debug, Default)]
    pub struct Builder {
        name: Option<String>,
        stack_size: Option<usize>,
    }

    impl Builder {
        /// See std.
        pub fn new() -> Builder {
            Builder::default()
        }

        /// See std.
        pub fn name(mut self, name: String) -> Builder {
            self.name = Some(name);
            self
        }

        /// See std. Ignored under the simulation (fibers use the configured size).
        pub fn stack_size(mut self, size: usize) -> Builder {
            self.stack_size = Some(size);
            self
        }

        /// See std.
        pub fn spawn<F, T>(self, f: F) -> io::Result<JoinHandle<T>>
        where
            F: FnOnce() -> T + Send + 'static,
            T: Send + 'static,
        {
            if !rt::in_sim() {
                let mut b = std::thread::Builder::new();
                if let Some(n) = self.name {
                    b = b.name(n);
                }
                if let Some(s) = self.stack_size {
                    b = b.stack_size(s);
                }
                return b.spawn(f).map(|h| JoinHandle(Inner::Std(h)));
            }
            let slot = Arc::new(Mutex::new(None));
            let out = Arc::clone(&slot);
            let name = self.name.unwrap_or_else(|| "thread".into());
            let body: Box<dyn FnOnce()> = Box::new(move || {
                let v = f();
                *out.lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
            });
            match rt::request(Req::Spawn(name.clone(), body)) {
                Reply::Spawned(tid) => Ok(JoinHandle(Inner::Sim { tid, slot, thread: Thread(ThreadRepr::Sim(tid, Some(name.into()))) })),
                _ => panic!("hegel-sim: thread spawn was resumed without a thread id"),
            }
        }
    }

    /// See std.
    pub fn spawn<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        Builder::new().spawn(f).unwrap_or_else(|e| panic!("failed to spawn thread: {e}"))
    }

    /// See std; virtual time under the simulation.
    pub fn sleep(dur: Duration) {
        if !rt::in_sim() {
            return std::thread::sleep(dur);
        }
        let deadline = crate::time::mono_ns().saturating_add(dur.as_nanos() as u64);
        rt::request(Req::Sleep(deadline));
    }

    /// See std.
    pub fn yield_now() {
        if !rt::in_sim() {
            return std::thread::yield_now();
        }
        rt::request(Req::Yield);
    }

    fn take_token() -> bool {
        rt::with(|w| {
            let tid = w.current.unwrap_or_default();
            std::mem::take(&mut w.threads[tid as usize].park_token)
        })
    }

    /// See std.
    pub fn park() {
        if !rt::in_sim() {
            return std::thread::park();
        }
        rt::op("thread.park", 0);
        if !take_token() {
            let tid = rt::current_tid();
            rt::request(Req::Block(WaitKey::Park(tid)));
            take_token();
        }
    }

    /// See std; virtual time under the simulation.
    pub fn park_timeout(dur: Duration) {
        if !rt::in_sim() {
            return std::thread::park_timeout(dur);
        }
        rt::op("thread.park", 0);
        if !take_token() {
            let tid = rt::current_tid();
            let deadline = crate::time::mono_ns().saturating_add(dur.as_nanos() as u64);
            rt::request(Req::TimedBlock(WaitKey::Park(tid), deadline));
            take_token();
        }
    }
}
