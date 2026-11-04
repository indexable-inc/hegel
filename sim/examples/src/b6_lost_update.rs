//! Bug 6: a lock-free buffer. Writers publish a new state by copying the
//! current one (including its `flushed` counter), appending, and swapping an
//! `AtomicPtr`. The flusher writes `rows[flushed..]` to the log, fsyncs, and
//! stores the new `flushed` into the state it loaded. If a writer copied that
//! state before the store, the store lands on the old pointer and is lost: the
//! next flush writes the same rows again, and they survive a crash twice.
//!
//! Fixed: `flushed` lives beside the pointer (one counter for the buffer), not
//! inside the copied state. Finding the bug needs a switch point at every
//! atomic, which `hegel::sim::sync::atomic` provides.

use std::io::Write;
use std::sync::Arc;

use hegel::TestCase;
use hegel_sim::fs::{self, OpenOptions};
use hegel_sim::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use hegel_sim::{Config, CrashPoints, Fault, Sim, thread};

use crate::Variant;

struct State {
    rows: Vec<u64>,
    flushed: AtomicUsize,
}

struct Buffer {
    cur: AtomicPtr<State>,
    flushed: AtomicUsize,
}

fn leak(s: State) -> *mut State {
    Box::into_raw(Box::new(s))
}

impl Buffer {
    fn push(&self, row: u64) {
        loop {
            let old = self.cur.load(Ordering::Acquire);
            // SAFETY: states are never freed (leaked for the run).
            let o = unsafe { &*old };
            let mut rows = o.rows.clone();
            rows.push(row);
            let new = leak(State { rows, flushed: AtomicUsize::new(o.flushed.load(Ordering::Acquire)) });
            if self.cur.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return;
            }
        }
    }

    fn flush(&self, v: Variant) -> std::io::Result<()> {
        let cur = self.cur.load(Ordering::Acquire);
        // SAFETY: as in `push`.
        let s = unsafe { &*cur };
        let from = if v.fixed() { self.flushed.load(Ordering::Acquire) } else { s.flushed.load(Ordering::Acquire) };
        if from >= s.rows.len() {
            return Ok(());
        }
        let mut f = OpenOptions::new().append(true).create(true).open("/log")?;
        let text: String = s.rows[from..].iter().map(|r| format!("{r}\n")).collect();
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        if v.fixed() {
            self.flushed.store(s.rows.len(), Ordering::Release);
        } else {
            s.flushed.store(s.rows.len(), Ordering::Release);
        }
        Ok(())
    }
}

fn program(v: Variant, rows: u64) -> i32 {
    fs::sync_dir("/").unwrap();
    let buf = Arc::new(Buffer { cur: AtomicPtr::new(leak(State { rows: Vec::new(), flushed: AtomicUsize::new(0) })), flushed: AtomicUsize::new(0) });
    let b = Arc::clone(&buf);
    let writer = thread::spawn(move || {
        for r in 0..rows {
            b.push(r);
        }
    });
    let b = Arc::clone(&buf);
    let flusher = thread::spawn(move || {
        for _ in 0..3 {
            if b.flush(v).is_err() {
                return;
            }
        }
    });
    writer.join().unwrap();
    flusher.join().unwrap();
    let _ = buf.flush(v);
    let _ = fs::sync_dir("/");
    0
}

/// One case: a writer and a flusher race, with a crash possible at any atomic;
/// the durable log must never hold a row twice.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::PowerLoss]).max_crashes(1).crash_points(CrashPoints::EverySwitch);
    let mut sim = Sim::new(tc, cfg);
    let rows = sim.draw_int("rows", 1, 6);
    sim.spawn("buffer", move || program(v, rows));
    let _ = sim.run();
    let log = sim.read_file("/log").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    for line in log.lines() {
        if let Ok(r) = line.parse::<u64>() {
            assert!(seen.insert(r), "row {r} is in the durable log twice:\n{log}");
        }
    }
}
