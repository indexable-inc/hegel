//! `std::sync::mpsc`. A channel made inside a running simulation blocks
//! simulated threads; one made outside is std's. `sync_channel(0)` (a
//! rendezvous) is modelled as a one-slot buffer: a send can complete before the
//! receive that takes it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use std::sync::mpsc::{RecvError, RecvTimeoutError, SendError, TryRecvError, TrySendError};

use hegel_sim_core::WaitKey;

use crate::rt::{self, Req};

#[doc(hidden)]
pub struct Shared<T> {
    q: Mutex<VecDeque<T>>,
    senders: AtomicUsize,
    receiver: AtomicBool,
    cap: Option<usize>,
}

impl<T> Shared<T> {
    fn addr(&self) -> usize {
        self as *const Self as *const u8 as usize
    }
    fn key(&self) -> WaitKey {
        WaitKey::Chan(rt::with(|w| w.object(self.addr())))
    }
    fn q(&self) -> std::sync::MutexGuard<'_, VecDeque<T>> {
        self.q.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn new_shared<T>(cap: Option<usize>) -> Arc<Shared<T>> {
    Arc::new(Shared { q: Mutex::new(VecDeque::new()), senders: AtomicUsize::new(1), receiver: AtomicBool::new(true), cap })
}

/// `std::sync::mpsc::Sender`.
pub enum Sender<T> {
    #[doc(hidden)]
    Std(std::sync::mpsc::Sender<T>),
    #[doc(hidden)]
    Sim(Arc<Shared<T>>),
}

/// `std::sync::mpsc::SyncSender`.
pub enum SyncSender<T> {
    #[doc(hidden)]
    Std(std::sync::mpsc::SyncSender<T>),
    #[doc(hidden)]
    Sim(Arc<Shared<T>>),
}

/// `std::sync::mpsc::Receiver`.
pub enum Receiver<T> {
    #[doc(hidden)]
    Std(std::sync::mpsc::Receiver<T>),
    #[doc(hidden)]
    Sim(Arc<Shared<T>>),
}

/// See std.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    if rt::in_sim() {
        let s = new_shared(None);
        (Sender::Sim(Arc::clone(&s)), Receiver::Sim(s))
    } else {
        let (a, b) = std::sync::mpsc::channel();
        (Sender::Std(a), Receiver::Std(b))
    }
}

/// See std.
pub fn sync_channel<T>(bound: usize) -> (SyncSender<T>, Receiver<T>) {
    if rt::in_sim() {
        let s = new_shared(Some(bound.max(1)));
        (SyncSender::Sim(Arc::clone(&s)), Receiver::Sim(s))
    } else {
        let (a, b) = std::sync::mpsc::sync_channel(bound);
        (SyncSender::Std(a), Receiver::Std(b))
    }
}

fn sim_send<T>(s: &Shared<T>, t: T, block: bool) -> Result<(), TrySendError<T>> {
    loop {
        rt::op("chan.send", s.addr());
        if !s.receiver.load(Ordering::SeqCst) {
            return Err(TrySendError::Disconnected(t));
        }
        let full = s.cap.is_some_and(|c| s.q().len() >= c);
        if !full {
            s.q().push_back(t);
            rt::wake(s.key());
            return Ok(());
        }
        if !block {
            return Err(TrySendError::Full(t));
        }
        rt::request(Req::Block(s.key()));
    }
}

impl<T> Sender<T> {
    /// See std.
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        match self {
            Sender::Std(s) => s.send(t),
            Sender::Sim(s) => sim_send(s, t, true).map_err(|e| match e {
                TrySendError::Full(t) | TrySendError::Disconnected(t) => SendError(t),
            }),
        }
    }
}

impl<T> SyncSender<T> {
    /// See std.
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        match self {
            SyncSender::Std(s) => s.send(t),
            SyncSender::Sim(s) => sim_send(s, t, true).map_err(|e| match e {
                TrySendError::Full(t) | TrySendError::Disconnected(t) => SendError(t),
            }),
        }
    }

    /// See std.
    pub fn try_send(&self, t: T) -> Result<(), TrySendError<T>> {
        match self {
            SyncSender::Std(s) => s.try_send(t),
            SyncSender::Sim(s) => sim_send(s, t, false),
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        match self {
            Sender::Std(s) => Sender::Std(s.clone()),
            Sender::Sim(s) => {
                s.senders.fetch_add(1, Ordering::SeqCst);
                Sender::Sim(Arc::clone(s))
            }
        }
    }
}

impl<T> Clone for SyncSender<T> {
    fn clone(&self) -> Self {
        match self {
            SyncSender::Std(s) => SyncSender::Std(s.clone()),
            SyncSender::Sim(s) => {
                s.senders.fetch_add(1, Ordering::SeqCst);
                SyncSender::Sim(Arc::clone(s))
            }
        }
    }
}

fn drop_sender<T>(s: &Shared<T>) {
    if s.senders.fetch_sub(1, Ordering::SeqCst) == 1 && rt::in_sim() {
        rt::wake(s.key());
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Sender::Sim(s) = self {
            drop_sender(s);
        }
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        if let SyncSender::Sim(s) = self {
            drop_sender(s);
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if let Receiver::Sim(s) = self {
            s.receiver.store(false, Ordering::SeqCst);
            if rt::in_sim() {
                rt::wake(s.key());
            }
        }
    }
}

impl<T> Receiver<T> {
    /// See std.
    pub fn recv(&self) -> Result<T, RecvError> {
        match self {
            Receiver::Std(r) => r.recv(),
            Receiver::Sim(s) => loop {
                rt::op("chan.recv", s.addr());
                if let Some(v) = s.q().pop_front() {
                    rt::wake(s.key());
                    return Ok(v);
                }
                if s.senders.load(Ordering::SeqCst) == 0 {
                    return Err(RecvError);
                }
                rt::request(Req::Block(s.key()));
            },
        }
    }

    /// See std.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self {
            Receiver::Std(r) => r.try_recv(),
            Receiver::Sim(s) => {
                rt::op("chan.recv", s.addr());
                if let Some(v) = s.q().pop_front() {
                    rt::wake(s.key());
                    Ok(v)
                } else if s.senders.load(Ordering::SeqCst) == 0 {
                    Err(TryRecvError::Disconnected)
                } else {
                    Err(TryRecvError::Empty)
                }
            }
        }
    }

    /// See std; virtual time under the simulation.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        match self {
            Receiver::Std(r) => r.recv_timeout(timeout),
            Receiver::Sim(s) => {
                let deadline = crate::time::mono_ns().saturating_add(timeout.as_nanos() as u64);
                loop {
                    rt::op("chan.recv", s.addr());
                    if let Some(v) = s.q().pop_front() {
                        rt::wake(s.key());
                        return Ok(v);
                    }
                    if s.senders.load(Ordering::SeqCst) == 0 {
                        return Err(RecvTimeoutError::Disconnected);
                    }
                    if crate::time::mono_ns() >= deadline {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    rt::request(Req::TimedBlock(s.key(), deadline));
                }
            }
        }
    }

    /// See std.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        std::iter::from_fn(move || self.recv().ok())
    }
}
