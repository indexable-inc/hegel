//! Bug 5: a daemon takes its startup lock (`flock`) and keeps it across a long
//! crash recovery. A second process (a CLI, a second daemon) that waits a
//! bounded time for the startup lock fails to start.
//!
//! Fixed: the daemon holds the startup lock only to register itself, and runs
//! recovery under its own recovery lock, which startup does not take.

use std::time::Duration;

use hegel::TestCase;
use hegel_sim::fs::{self, File, FileExtras, OpenOptions};
use hegel_sim::{Config, Fault, Sim, thread};

use crate::Variant;

fn open_lock(path: &str) -> File {
    OpenOptions::new().read(true).write(true).create(true).open(path).unwrap()
}

/// Take the startup lock, waiting at most 20ms (virtual).
fn startup_lock() -> Option<File> {
    let f = open_lock("/run/startup.lock");
    for _ in 0..20 {
        if f.try_lock_exclusive().unwrap() {
            return Some(f);
        }
        thread::sleep(Duration::from_millis(1));
    }
    None
}

fn recover(segments: u64) {
    for i in 0..segments {
        if let Ok(f) = File::open(format!("/data/seg-{i}")) {
            let _ = f.sync_all();
        }
    }
}

fn daemon(v: Variant, segments: u64) -> i32 {
    let Some(lock) = startup_lock() else { return 1 };
    if v.fixed() {
        drop(lock);
        let r = open_lock("/run/recovery.lock");
        r.lock_exclusive().unwrap();
        recover(segments);
        r.unlock().unwrap();
    } else {
        recover(segments);
        drop(lock);
    }
    0
}

fn cli() -> i32 {
    match startup_lock() {
        Some(_l) => 0,
        None => 1,
    }
}

/// One case: a daemon recovering a drawn number of segments, and a CLI
/// starting at a drawn moment; both must start.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::LateWakeups]).max_crashes(0);
    let mut sim = Sim::new(tc, cfg);
    let segments = sim.draw_int("segments", 1, 40);
    sim.calm(|s| {
        s.spawn("setup", move || {
            fs::create_dir("/run").unwrap();
            fs::create_dir("/data").unwrap();
            for i in 0..segments {
                File::create(format!("/data/seg-{i}")).unwrap();
            }
            0
        });
        s.run().expect_exited();
    });
    sim.spawn("daemon", move || daemon(v, segments));
    sim.spawn("cli", cli);
    match sim.run() {
        hegel_sim::Outcome::Exited(e) => {
            for x in &e {
                assert_eq!(x.code, 0, "{} failed to start (exit {})", x.name, x.code);
            }
        }
        o => panic!("unexpected {o:?}"),
    }
}
