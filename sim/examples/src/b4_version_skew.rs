//! Bug 4: version skew. v1 stores `key=value` lines and, on startup, treats the
//! first line it cannot parse as a torn tail and truncates there (the usual
//! repair). v2 writes `key:value` lines. When an old v1 binary restarts on a
//! file v2 wrote (a rollback, or two versions in one deploy), it "repairs" away
//! everything v2 added.
//!
//! Fixed: v2 starts the file with a format header that v1 checks; v1 refuses
//! to open a file with a newer header (it exits with an error and touches
//! nothing) instead of repairing it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use hegel::TestCase;
use hegel_sim::fs::{self, File, OpenOptions};
use hegel_sim::{Config, Fault, Sim};

use crate::Variant;

const DB: &str = "/db/kv";
const HEADER_V2: &str = "#format 2\n";

/// Keys whose append was acknowledged (fsync returned), across all processes.
type Acked = Arc<Mutex<Vec<String>>>;

fn append_durable(line: &str) -> std::io::Result<()> {
    let mut f = OpenOptions::new().append(true).create(true).open(DB)?;
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    fs::sync_dir("/db")
}

fn v1(v: Variant, key: String, acked: Acked) -> i32 {
    let text = fs::read_to_string(DB).unwrap_or_default();
    if v.fixed() && text.starts_with("#format ") && !text.starts_with("#format 1\n") {
        return 2;
    }
    let mut good = 0usize;
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') || !line.contains('=') {
            break;
        }
        good += line.len();
    }
    if good < text.len() {
        let f = OpenOptions::new().write(true).open(DB).unwrap();
        f.set_len(good as u64).unwrap();
        f.sync_all().unwrap();
    }
    if append_durable(&format!("{key}=1\n")).is_ok() {
        acked.lock().unwrap().push(key);
    }
    0
}

fn v2(v: Variant, key: String, acked: Acked) -> i32 {
    let mut text = String::from_utf8_lossy(&fs::read(DB).unwrap_or_default()).into_owned();
    if !text.is_empty() && !text.ends_with('\n') {
        let keep = text.rfind('\n').map_or(0, |i| i + 1);
        let f = OpenOptions::new().write(true).open(DB).unwrap();
        f.set_len(keep as u64).unwrap();
        f.sync_all().unwrap();
        text.truncate(keep);
    }
    if v.fixed() && !text.starts_with(HEADER_V2) {
        let mut body = String::from(HEADER_V2);
        body.push_str(&text);
        let mut f = File::create("/db/kv.tmp").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        fs::rename("/db/kv.tmp", DB).unwrap();
        fs::sync_dir("/db").unwrap();
    }
    if append_durable(&format!("{key}:1\n")).is_ok() {
        acked.lock().unwrap().push(key);
    }
    0
}

/// Keys a reader that understands both formats finds.
fn keys(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split(['=', ':']).next().filter(|_| l.contains(['=', ':'])))
        .map(str::to_owned)
        .collect()
}

/// One case: a drawn sequence of v1 and v2 restarts (with kill -9 and power
/// loss) on one disk; every acknowledged key must survive.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::Kill9, Fault::PowerLoss]).max_crashes(1);
    let mut sim = Sim::new(tc, cfg);
    sim.calm(|s| {
        s.spawn("setup", || {
            fs::create_dir("/db").unwrap();
            fs::sync_dir("/").unwrap();
            0
        });
        s.run().expect_exited();
    });
    let acked: Acked = Arc::new(Mutex::new(Vec::new()));
    let runs = sim.draw_int("runs", 1, 4);
    for i in 0..runs {
        let version = *sim.pick("version", &[1u8, 2]);
        let key = format!("k{i}");
        let a = Arc::clone(&acked);
        if version == 1 {
            sim.spawn("v1", move || v1(v, key, a));
        } else {
            sim.spawn("v2", move || v2(v, key, a));
        }
        let _ = sim.run();
    }
    let text = sim.read_file(DB).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let found = keys(&text);
    for k in acked.lock().unwrap().iter() {
        assert!(found.contains(k), "acknowledged key {k} is gone after a version change; file:\n{text}");
    }
}
