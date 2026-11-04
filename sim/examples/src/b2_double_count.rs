//! Bug 2: an aggregator commits its new total (atomically: write, fsync,
//! rename, fsync the directory) and then, in a second commit, marks the input
//! rows consumed. A crash between the two commits makes the restarted
//! aggregator add the same rows again.
//!
//! Fixed: the consumed offset lives inside the committed state, so one rename
//! commits both.

use std::io::Write;

use hegel::TestCase;
use hegel_sim::fs::{self, File};
use hegel_sim::{Config, Fault, Sim};

use crate::Variant;

fn commit(path: &str, body: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, path)?;
    fs::sync_dir("/agg")
}

fn read_num(path: &str, field: usize) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.split_whitespace().nth(field).and_then(|n| n.parse().ok()))
        .unwrap_or(0)
}

fn setup(n: u64) -> i32 {
    fs::create_dir("/agg").unwrap();
    fs::sync_dir("/").unwrap();
    let mut f = File::create("/agg/rows").unwrap();
    for i in 0..n {
        f.write_all(format!("{}\n", i + 1).as_bytes()).unwrap();
    }
    f.sync_all().unwrap();
    fs::sync_dir("/agg").unwrap();
    0
}

/// Fold unconsumed rows into the total, in batches of `batch`.
fn aggregate(v: Variant, batch: usize) -> i32 {
    let rows: Vec<u64> = fs::read_to_string("/agg/rows").unwrap().lines().map(|l| l.parse().unwrap()).collect();
    loop {
        let (total, consumed) = if v.fixed() {
            (read_num("/agg/state", 0), read_num("/agg/state", 1) as usize)
        } else {
            (read_num("/agg/total", 0), read_num("/agg/consumed", 0) as usize)
        };
        if consumed >= rows.len() {
            return 0;
        }
        let end = (consumed + batch).min(rows.len());
        let total = total + rows[consumed..end].iter().sum::<u64>();
        let r = if v.fixed() {
            commit("/agg/state", &format!("{total} {end}"))
        } else {
            commit("/agg/total", &total.to_string()).and_then(|()| commit("/agg/consumed", &end.to_string()))
        };
        if r.is_err() {
            return 1;
        }
    }
}

/// One case: aggregate, crash anywhere, restart until done, check the total.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::PowerLoss, Fault::OsCrash, Fault::Kill9]).max_crashes(2);
    let mut sim = Sim::new(tc, cfg);
    let n = sim.draw_int("rows", 2, 12);
    let batch = sim.draw_int("batch", 1, 4) as usize;
    sim.calm(|s| {
        s.spawn("setup", move || setup(n));
        s.run().expect_exited();
    });
    for _ in 0..4 {
        sim.spawn("aggregate", move || aggregate(v, batch));
        if let hegel_sim::Outcome::Exited(e) = sim.run()
            && e.iter().all(|x| x.code == 0)
        {
            break;
        }
    }
    sim.calm(|s| {
        s.spawn("aggregate", move || aggregate(v, batch));
        s.run().expect_exited();
    });
    let total_path = if v.fixed() { "/agg/state" } else { "/agg/total" };
    let got: u64 = sim
        .read_file(total_path)
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_whitespace().next().and_then(|t| t.parse().ok()))
        .unwrap_or(0);
    assert_eq!(got, n * (n + 1) / 2, "the total counts some rows twice (or misses some)");
}
