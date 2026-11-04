//! Every buggy variant is caught within the case budget; every fixed variant
//! passes it. Run with `--nocapture` to see how many cases each catch took.

use hegel::TestCase;
use hegel_sim_examples::harness::run;
use hegel_sim_examples::{Variant, b1_fsync_rename, b2_double_count, b3_partial_flush, b4_version_skew, b5_lock_recovery, b6_lost_update};

const BUDGET: u64 = 1000;
const SEEDS: [u64; 3] = [1, 2, 3];

fn check(name: &str, scenario: fn(&TestCase, Variant)) {
    let mut firsts = Vec::new();
    for seed in SEEDS {
        let buggy = run(BUDGET, seed, |tc| scenario(tc, Variant::Buggy));
        let first = buggy.first_failure.unwrap_or_else(|| panic!("{name}: the buggy variant passed {BUDGET} cases (seed {seed})"));
        firsts.push(first);
        let fixed = run(BUDGET, seed, |tc| scenario(tc, Variant::Fixed));
        assert!(
            fixed.first_failure.is_none(),
            "{name}: the fixed variant failed at case {:?} (seed {seed}): {}",
            fixed.first_failure,
            fixed.message.unwrap_or_default()
        );
    }
    eprintln!("{name}: buggy caught at case {firsts:?} (seeds {SEEDS:?}); fixed passed {BUDGET} cases per seed");
}

#[test]
fn b1_no_fsync_before_rename() {
    check("b1_fsync_rename", b1_fsync_rename::scenario);
}

#[test]
fn b2_crash_between_commits_double_counts() {
    check("b2_double_count", b2_double_count::scenario);
}

#[test]
fn b3_partial_flush_duplicates() {
    check("b3_partial_flush", b3_partial_flush::scenario);
}

#[test]
fn b4_version_skew_loses_keys() {
    check("b4_version_skew", b4_version_skew::scenario);
}

#[test]
fn b5_lock_held_across_recovery() {
    check("b5_lock_recovery", b5_lock_recovery::scenario);
}

#[test]
fn b6_lock_free_lost_update() {
    check("b6_lost_update", b6_lost_update::scenario);
}
