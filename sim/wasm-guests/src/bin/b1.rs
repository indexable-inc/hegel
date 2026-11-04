//! Bug 1 as a wasm program: compaction that renames a manifest into place
//! without fsyncing it first (`buggy`) or with every fsync (`fixed`).
//! `b1 setup` writes durable inputs; `b1 compact <buggy|fixed>` compacts.

use std::fs::{self, File};
use std::io::Write;

const INPUTS: [&str; 3] = ["/in/a", "/in/b", "/in/c"];

fn rows(i: usize) -> String {
    (0..40).map(|r| format!("row-{i}-{r:04}\n")).collect()
}

fn sync_dir(p: &str) -> std::io::Result<()> {
    File::open(p)?.sync_all()
}

fn setup() {
    fs::create_dir("/in").unwrap();
    fs::create_dir("/out").unwrap();
    sync_dir("/").unwrap();
    for (i, p) in INPUTS.iter().enumerate() {
        let mut f = File::create(p).unwrap();
        f.write_all(rows(i).as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    sync_dir("/in").unwrap();
}

fn compact(fixed: bool) -> std::io::Result<()> {
    let mut data = String::new();
    for p in INPUTS {
        data.push_str(&fs::read_to_string(p)?);
    }
    let mut seg = File::create("/out/seg-1")?;
    seg.write_all(data.as_bytes())?;
    if fixed {
        seg.sync_all()?;
    }
    let manifest = format!("{{\"segments\":[\"seg-1\"],\"bytes\":{}}}", data.len());
    let mut tmp = File::create("/out/manifest.json.tmp")?;
    tmp.write_all(manifest.as_bytes())?;
    if fixed {
        tmp.sync_all()?;
    }
    drop(tmp);
    fs::rename("/out/manifest.json.tmp", "/out/manifest.json")?;
    sync_dir("/out")?;
    for p in INPUTS {
        fs::remove_file(p)?;
    }
    sync_dir("/in")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("setup") => setup(),
        Some("compact") => {
            if compact(args.get(2).map(String::as_str) == Some("fixed")).is_err() {
                std::process::exit(1);
            }
        }
        _ => std::process::exit(2),
    }
}
