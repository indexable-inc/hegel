---
status: plan
---

# Validating the kernel model against real kernels

Status: **plan**. The script is written and its self-test passes, but it has not yet been run against a kernel.
Design: `docs/dst.md`, "Phase 3".

`validate_kernel_model.py` records the block I/O of each small workload on a real Linux file system with
`dm-log-writes`. It replays that I/O into crash states, mounts each one and reads back what survived. It then
asks the model's membership checker whether `hegel-sim-core`'s kernel model can produce that state. A state the
model cannot produce is a model bug: real behaviour the model misses.

## What it does

For each file system, run concurrently, and each workload in turn:

1. Make two sparse backing files and loop devices (data and log). Put a `log-writes` target over them
   (`0 <sectors> log-writes <data> <log>`), run `mkfs`, add the mark `mkfs`, then mount.
2. Run the workload's ops in-process (`os.open`, `os.write`, `os.fsync`, `os.rename`, ...) and add the mark
   `op-<i>` after op `i`. Unmount, add the mark `unmounted`, and tear down the target and the loops.
3. Parse the log in Python. The format is in `drivers/md/dm-log-writes.c` and
   `Documentation/admin-guide/device-mapper/log-writes.rst`. Two cross-checks then run. First, replaying every
   entry must give exactly the bytes the data device holds. Second, if `replay_log` is set, Python's replay to
   the last `op-<i>` mark must match xfstests' `replay-log --end-mark` byte for byte.
4. Plan the crash states from the `mkfs` mark on:
   - every mark;
   - every flush or FUA entry;
   - CrashMonkey-style subsets of the writes in flight between two flush entries: all of them when the window
     has at most 8 writes, otherwise `max_states_per_window` subsets sampled with a seed made from `seed`,
     the file system and the workload.
5. For each state, copy the replayed base image, apply the subset, attach it to a loop device and mount it
   read-write, so journal recovery runs. Then dump every entry under the root except `lost+found`, unmount, and
   pipe the observed state to the checker.

## Prerequisites

A Linux VM where you are root, with:

- the `dm_log_writes` module (`modprobe dm_log_writes`), `dmsetup`, and `losetup` from util-linux;
- `mkfs.ext4`, `mkfs.xfs` and `mkfs.btrfs` for the file systems you list;
- xfstests' `src/log-writes/replay-log`, built (optional; set `replay_log` to `null` to skip that cross-check);
- Python 3.13 or newer (stdlib only);
- the checker: `cargo run -p hegel-sim-core --example crash_states` from this repository.

Check the parser, replayer and planner without root or Linux:

```sh
python3 sim/validation/validate_kernel_model.py --self-test
```

## Running

The script takes one JSON value, either as a file path argument or on stdin. Relative paths are resolved
against the config file's directory.

```json
{
  "workdir": "/var/tmp/hegel-validate",
  "workloads": ["workloads/rename_no_fsync.json", "workloads/rename_with_fsync.json",
                "workloads/append_torn.json", "workloads/create_no_dirsync.json",
                "workloads/overwrite_in_place.json"],
  "filesystems": [
    {"name": "ext4-ordered", "mkfs": ["mkfs.ext4", "-F", "-E", "nodiscard,lazy_itable_init=0,lazy_journal_init=0"], "mount_opts": "data=ordered"},
    {"name": "ext4-writeback", "mkfs": ["mkfs.ext4", "-F", "-E", "nodiscard,lazy_itable_init=0,lazy_journal_init=0"], "mount_opts": "data=writeback"},
    {"name": "xfs", "mkfs": ["mkfs.xfs", "-f", "-K"], "mount_opts": ""},
    {"name": "btrfs", "mkfs": ["mkfs.btrfs", "-f", "-K"], "mount_opts": ""}
  ],
  "device_mib": 256,
  "replay_log": "/opt/xfstests/src/log-writes/replay-log",
  "checker": ["cargo", "run", "-q", "-p", "hegel-sim-core", "--example", "crash_states", "--"],
  "max_states_per_window": 64,
  "seed": 1,
  "output": "/var/tmp/hegel-validate/report.json",
  "timeout_s": 300
}
```

```sh
sudo python3 sim/validation/validate_kernel_model.py config.json
```

Optional fields: `log_device_mib` (default `4 * device_mib`) and `checker_cwd` (default the repository root).
`lazy_itable_init=0` keeps ext4's background inode-table zeroing out of the workload's log. `nodiscard` (`-K`)
keeps mkfs from logging a whole-device discard.

The checker is run as `<checker...> <workload.json>`, with the observed state on stdin:

```json
{"workload": "rename_no_fsync", "fs": "ext4-ordered",
 "crash_point": {"kind": "subset", "label": "op-9", "entries": [412, 415]},
 "entries": {"/": {"type": "dir"}, "/d": {"type": "dir"},
             "/d/final": {"type": "file", "size": 8192, "content_b64": "..."}}}
```

It prints one JSON line, `{"allowed": bool, "reason": str, "rules": [str, ...]}`; `rules` is optional.

The checker is `sim/core/examples/crash_states.rs`. It replays the workload's ops in the model (with no syscall
faults on, these are deterministic), then asks `Kernel::could_crash_to` whether some power-loss crash after those
ops leaves exactly the observed tree. The crash's fates are which pending directory operations survive, and per
file the size and each page's version or tear. The query mirrors the crash's own resolution page by page, so it
is exact and needs no sampling. Each file system maps to a model profile and crash-shape classes: ext4 maps to
`LinuxExt4`, xfs and btrfs to `LinuxStrict`, every file system gets torn sectors, and ext4 writeback and xfs also
get zeros past EOF. `crash_states --self-test sim/validation/workloads [seeds]` checks the checker against the
model itself: every state the model's own crash draws must be allowed. That is 43200 states at the default 200
seeds.

`crash_point.entries` holds log entry indices: the mark or the flush entry itself, or the subset of the window
that was applied on top of everything up to the window's opening flush. `label` is the mark itself for a mark
state. For a flush or subset state it is the last mark logged before the state's end, so the crash happened
after at most that many ops.

## The report

`output` gets one JSON object:

- `ok`: true when every pair ran, every state mounted and the checker allowed every state.
- `results[]`, one per file system and workload:
  - `states` and `allowed`: how many crash states were checked and how many the model accepts.
  - `disallowed[]`: states the model cannot produce, each with its crash point and the checker's reason.
    Each one is a model bug.
  - `broken[]`: states that failed to mount or read back. These are file system findings, or a replay bug.
  - `rules`: how many allowed states each model rule was needed for. A rule with a nonzero count is backed by
    evidence.
  - `crosscheck`: the outcome of the two replay cross-checks.
  - `error` and `kept_dir`: a harness failure. The error names the step, the command, its exit code and the
    stderr tail. The scratch directory is kept for inspection.

Exit status: 0 when `ok`, 1 on any harness error, 2 when there are findings but no harness errors, and 64 for a
bad config.

## Known uncertainties (to settle on the first run)

- The log format was written from the kernel source as remembered, not checked against a live log. Entries
  start one device sector after the super block. Each entry is a one-sector header followed by `nr_sectors`
  device sectors of data (none for discards), and a mark's label sits in its header sector. The full-replay
  cross-check catches a mistake here.
- The full-replay cross-check assumes every write was flushed before teardown, because dm-log-writes logs a
  non-FUA write only when a later flush completes. Unmount flushes on ext4, xfs and btrfs.
- `replay-log` flags are built in `replay_log_argv` alone, so they are easy to fix.
- btrfs keeps a kernel registry of devices by fsid. If mounting replayed images trips over stale entries, run
  `btrfs device scan --forget` between states.
