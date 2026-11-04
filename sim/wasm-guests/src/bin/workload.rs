//! A blaze-like CPU workload for measuring the cost of instrumentation: hash
//! maps keyed by strings, sorting, string building and many small allocations,
//! with a file write at the end. Prints a checksum.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let mut map: HashMap<String, Vec<u32>> = HashMap::new();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let key = format!("crate-{}/mod-{}", x % 997, (x >> 20) % 31);
        map.entry(key).or_default().push(i as u32);
    }
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut tree: BTreeMap<u64, String> = BTreeMap::new();
    let mut sum: u64 = 0;
    for k in keys {
        let v = &map[k];
        let mut s = String::with_capacity(k.len() + 16);
        s.push_str(k);
        s.push(':');
        s.push_str(&v.len().to_string());
        sum = sum.wrapping_mul(31).wrapping_add(v.iter().map(|&i| i as u64).sum::<u64>());
        tree.insert(sum, s);
    }
    let out: String = tree.values().take(100).cloned().collect::<Vec<_>>().join("\n");
    let mut f = std::fs::File::create("/out.txt").unwrap();
    f.write_all(out.as_bytes()).unwrap();
    f.sync_all().unwrap();
    println!("{sum:x} {}", tree.len());
}
