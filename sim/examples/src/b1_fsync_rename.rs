//! Bug 1: compaction renames a manifest into place without fsyncing it (or the
//! segment it lists) first, then deletes its inputs. Power loss after the
//! directory fsync can leave `manifest.json` empty or torn, or listing a segment
//! whose bytes never reached media, with the inputs already gone.
//!
//! Fixed: fsync the segment and the temporary manifest, rename, fsync the
//! directory, and only then delete the inputs (and fsync their directory).

use std::io::Write;

use hegel::TestCase;
use hegel_sim::fs::{self, File};
use hegel_sim::{Config, Fault, Outcome};

use crate::Variant;

const INPUTS: [&str; 3] = ["/in/a", "/in/b", "/in/c"];

fn rows(i: usize) -> String {
    (0..40).map(|r| format!("row-{i}-{r:04}\n")).collect()
}

fn expected() -> String {
    (0..INPUTS.len()).map(rows).collect()
}

fn setup() -> i32 {
    fs::create_dir("/in").unwrap();
    fs::create_dir("/out").unwrap();
    fs::sync_dir("/").unwrap();
    for (i, p) in INPUTS.iter().enumerate() {
        let mut f = File::create(p).unwrap();
        f.write_all(rows(i).as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    fs::sync_dir("/in").unwrap();
    0
}

fn compact(v: Variant) -> i32 {
    let mut data = String::new();
    for p in INPUTS {
        data.push_str(&fs::read_to_string(p).unwrap());
    }
    let run = || -> std::io::Result<()> {
        let mut seg = File::create("/out/seg-1")?;
        seg.write_all(data.as_bytes())?;
        if v.fixed() {
            seg.sync_all()?;
        }
        let manifest = format!("{{\"segments\":[\"seg-1\"],\"bytes\":{}}}", data.len());
        let mut tmp = File::create("/out/manifest.json.tmp")?;
        tmp.write_all(manifest.as_bytes())?;
        if v.fixed() {
            tmp.sync_all()?;
        }
        drop(tmp);
        fs::rename("/out/manifest.json.tmp", "/out/manifest.json")?;
        fs::sync_dir("/out")?;
        for p in INPUTS {
            fs::remove_file(p)?;
        }
        fs::sync_dir("/in")?;
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// What recovery reads: the manifest's segments when it parses, else the inputs.
fn recover(sim: &Sim<'_>) -> Option<String> {
    let manifest = sim.read_file("/out/manifest.json").and_then(|m| String::from_utf8(m).ok());
    if let Some(m) = manifest
        && let Some(bytes) = m.strip_prefix("{\"segments\":[\"seg-1\"],\"bytes\":").and_then(|r| r.strip_suffix('}'))
        && let Ok(n) = bytes.parse::<usize>()
    {
        let seg = sim.read_file("/out/seg-1")?;
        return (seg.len() == n).then(|| String::from_utf8_lossy(&seg).into_owned());
    }
    let mut out = String::new();
    for p in INPUTS {
        out.push_str(&String::from_utf8(sim.read_file(p)?).ok()?);
    }
    Some(out)
}

use hegel_sim::Sim;

/// One case: set up durable inputs, compact under power loss, check that
/// recovery sees every row.
pub fn scenario(tc: &TestCase, v: Variant) {
    let cfg = Config::default().faults([Fault::PowerLoss, Fault::TornWrites, Fault::ZerosPastEof]).max_crashes(1);
    let mut sim = Sim::new(tc, cfg);
    sim.calm(|s| {
        s.spawn("setup", setup);
        s.run().expect_exited();
    });
    sim.spawn("compact", move || compact(v));
    let out = sim.run();
    let got = recover(&sim);
    assert!(
        got.as_deref() == Some(expected().as_str()),
        "rows lost after {out:?}: recovery read {} bytes, expected {}",
        got.map_or(0, |g| g.len()),
        expected().len()
    );
    let _ = matches!(out, Outcome::Crashed(_));
}
