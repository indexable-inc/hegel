//! Bug 3: a flush appends buffered rows to two table files and then commits a
//! manifest holding each table's committed length. When part of a flush fails
//! (ENOSPC on the second table, or the commit lock times out because a
//! compactor holds it across a slow fsync), the caller retries the whole flush.
//! The buggy flush appends at the end of the file and commits the file's size,
//! so the bytes the failed attempt left behind are committed too: duplicates.
//!
//! Fixed: write at the committed length (`write_at`), and commit committed +
//! written, so a failed attempt's bytes are overwritten or ignored.

use std::io::Write;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::time::Duration;

use hegel::TestCase;
use hegel_sim::fs::{self, File, OpenOptions};
use hegel_sim::sync::Mutex;
use hegel_sim::{Config, Fault, Sim, thread};

use crate::Variant;

const TABLES: [&str; 2] = ["/t/a", "/t/b"];

fn manifest() -> [u64; 2] {
    let s = fs::read_to_string("/t/manifest").unwrap_or_default();
    let mut it = s.split_whitespace().map(|n| n.parse::<u64>().unwrap_or(0));
    [it.next().unwrap_or(0), it.next().unwrap_or(0)]
}

fn flush(v: Variant, lock: &Mutex<()>, rows: &[String; 2]) -> std::io::Result<()> {
    let committed = manifest();
    let mut new = [0u64; 2];
    for (i, path) in TABLES.iter().enumerate() {
        let f = OpenOptions::new().read(true).write(true).create(true).open(path)?;
        if v.fixed() {
            let mut off = committed[i];
            let mut buf = rows[i].as_bytes();
            while !buf.is_empty() {
                let n = f.write_at(buf, off)?;
                off += n as u64;
                buf = &buf[n..];
            }
            new[i] = committed[i] + rows[i].len() as u64;
        } else {
            let mut f = OpenOptions::new().append(true).open(path)?;
            f.write_all(rows[i].as_bytes())?;
            new[i] = f.metadata()?.len();
        }
        f.sync_all()?;
    }
    let guard = lock
        .try_lock_for(Duration::from_millis(1))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "commit lock timed out"))?;
    let mut tmp = File::create("/t/manifest.tmp")?;
    tmp.write_all(format!("{} {}", new[0], new[1]).as_bytes())?;
    tmp.sync_all()?;
    drop(tmp);
    fs::rename("/t/manifest.tmp", "/t/manifest")?;
    fs::sync_dir("/t")?;
    drop(guard);
    Ok(())
}

fn writer(v: Variant, batches: u64) -> i32 {
    fs::create_dir("/t").unwrap();
    fs::sync_dir("/").unwrap();
    let lock = Arc::new(Mutex::new(()));
    let l2 = Arc::clone(&lock);
    let compactor = thread::spawn(move || {
        for _ in 0..3 {
            let g = l2.lock().unwrap();
            if let Ok(f) = File::open("/t/a") {
                let _ = f.sync_all();
                let _ = f.sync_all();
            }
            drop(g);
            thread::sleep(Duration::from_micros(500));
        }
    });
    for b in 0..batches {
        let rows = [format!("a{b}\n"), format!("b{b}\n")];
        let mut ok = false;
        for _attempt in 0..8 {
            if flush(v, &lock, &rows).is_ok() {
                ok = true;
                break;
            }
        }
        if !ok {
            break;
        }
    }
    compactor.join().unwrap();
    0
}

/// One case: flush batches under ENOSPC and lock contention; the committed
/// prefix of each table must hold every row at most once, in order.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::Enospc, Fault::LateWakeups]).max_crashes(0);
    let mut sim = Sim::new(tc, cfg);
    let batches = sim.draw_int("batches", 1, 4);
    sim.spawn("writer", move || writer(v, batches));
    sim.run().expect_exited();
    let m = sim.read_file("/t/manifest").map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let lens: Vec<usize> = m.split_whitespace().map(|n| n.parse().unwrap_or(0)).collect();
    for (i, path) in TABLES.iter().enumerate() {
        let data = sim.read_file(path).unwrap_or_default();
        let len = lens.get(i).copied().unwrap_or(0).min(data.len());
        let text = String::from_utf8_lossy(&data[..len]).into_owned();
        let prefix = if i == 0 { 'a' } else { 'b' };
        let want: String = (0..text.lines().count()).map(|b| format!("{prefix}{b}\n")).collect();
        assert_eq!(text, want, "table {path}: committed rows are duplicated or out of order");
    }
}
