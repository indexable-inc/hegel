#!/usr/bin/env python3
# /// script
# requires-python = ">=3.13"
# dependencies = []
# ///
"""Check hegel-sim-core's kernel model against crash states recorded from real Linux file systems.

Input is one JSON value: a config file path as the only argument, or the JSON itself on stdin. The single
argument `--self-test` instead checks the log parser, the replayer and the crash-state planner on a synthetic
log, and parses the bundled workloads; it needs neither root nor Linux.

See sim/validation/README.md and docs/dst.md, "Phase 3".
"""

import asyncio
import base64
import hashlib
import json
import os
import random
import re
import shlex
import stat
import struct
import sys
import tempfile
import traceback
from collections import Counter
from collections.abc import Callable, Iterable, Sequence
from contextlib import AsyncExitStack
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent


class StepError(Exception):
    def __init__(self, step: str, argv: Sequence[str], returncode: int | None, detail: str) -> None:
        self.step = step
        self.argv = tuple(argv)
        self.returncode = returncode
        self.detail = detail
        status = "did not finish" if returncode is None else f"exited {returncode}"
        super().__init__(f"step {step!r}: `{shlex.join(self.argv)}` {status}; stderr tail:\n{detail}")


class InputError(Exception):
    """A config or workload that does not fit its schema; the message names the field."""


class LogFormatError(Exception):
    """A dm-log-writes log this parser does not understand; the message names the entry and byte offset."""


class CrossCheckError(Exception):
    """The Python replayer disagrees with the recorded device or with replay-log."""


def _tail(data: bytes, lines: int = 20) -> str:
    text = data.decode(errors="replace").rstrip()
    return "\n".join(text.splitlines()[-lines:]) or "(empty)"


async def run(
    step: str,
    argv: Sequence[str],
    timeout_s: float,
    *,
    stdin: bytes | None = None,
    cwd: Path | None = None,
) -> bytes:
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=cwd,
        )
    except OSError as e:
        raise StepError(step, argv, None, f"could not start: {e}") from e
    try:
        async with asyncio.timeout(timeout_s):
            out, err = await proc.communicate(stdin)
    except TimeoutError:
        raise StepError(step, argv, None, f"timed out after {timeout_s}s") from None
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()
    if proc.returncode != 0:
        raise StepError(step, argv, proc.returncode, _tail(err))
    return out


async def in_thread[T](step: str, timeout_s: float, fn: Callable[[], T]) -> T:
    try:
        async with asyncio.timeout(timeout_s):
            return await asyncio.to_thread(fn)
    except TimeoutError:
        raise StepError(step, ("<in-process>",), None, f"timed out after {timeout_s}s") from None


class _Fields:
    def __init__(self, value: object, where: str) -> None:
        if not isinstance(value, dict):
            raise InputError(f"{where}: expected an object, got {type(value).__name__}")
        raw = cast("dict[object, object]", value)
        self.where = where
        self.data: dict[str, object] = {}
        for k, v in raw.items():
            if not isinstance(k, str):
                raise InputError(f"{where}: key {k!r} is not a string")
            self.data[k] = v
        self.seen: set[str] = set()

    def _get(self, name: str) -> object:
        self.seen.add(name)
        if name not in self.data:
            raise InputError(f"{self.where}: missing field {name!r}")
        return self.data[name]

    def has(self, name: str) -> bool:
        return name in self.data

    def str_(self, name: str) -> str:
        v = self._get(name)
        if not isinstance(v, str):
            raise InputError(f"{self.where}.{name}: expected a string")
        return v

    def int_(self, name: str, lo: int = 0, hi: int | None = None) -> int:
        v = self._get(name)
        if not isinstance(v, int) or isinstance(v, bool):
            raise InputError(f"{self.where}.{name}: expected an integer")
        if v < lo or (hi is not None and v > hi):
            raise InputError(f"{self.where}.{name}: {v} is outside [{lo}, {hi if hi is not None else 'inf'}]")
        return v

    def str_list(self, name: str, nonempty: bool = False) -> tuple[str, ...]:
        v = self._get(name)
        if not isinstance(v, list):
            raise InputError(f"{self.where}.{name}: expected a list of strings")
        items = cast("list[object]", v)
        out: list[str] = []
        for i, item in enumerate(items):
            if not isinstance(item, str):
                raise InputError(f"{self.where}.{name}[{i}]: expected a string")
            out.append(item)
        if nonempty and not out:
            raise InputError(f"{self.where}.{name}: must not be empty")
        return tuple(out)

    def list_(self, name: str) -> list[object]:
        v = self._get(name)
        if not isinstance(v, list):
            raise InputError(f"{self.where}.{name}: expected a list")
        return cast("list[object]", v)

    def opt_str(self, name: str) -> str | None:
        if name not in self.data or self.data[name] is None:
            self.seen.add(name)
            return None
        return self.str_(name)

    def done(self) -> None:
        unknown = sorted(set(self.data) - self.seen)
        if unknown:
            raise InputError(f"{self.where}: unknown fields {unknown}")


@dataclass(frozen=True)
class FsConfig:
    name: str
    mkfs: tuple[str, ...]
    mount_opts: str


@dataclass(frozen=True)
class Config:
    workdir: Path
    workloads: tuple[Path, ...]
    filesystems: tuple[FsConfig, ...]
    device_mib: int
    log_device_mib: int
    replay_log: Path | None
    checker: tuple[str, ...]
    checker_cwd: Path
    max_states_per_window: int
    seed: int
    output: Path
    timeout_s: int


def parse_config(value: object, base: Path) -> Config:
    f = _Fields(value, "config")
    fss: list[FsConfig] = []
    names: set[str] = set()
    for i, raw in enumerate(f.list_("filesystems")):
        g = _Fields(raw, f"config.filesystems[{i}]")
        fs = FsConfig(name=g.str_("name"), mkfs=g.str_list("mkfs", nonempty=True), mount_opts=g.str_("mount_opts"))
        g.done()
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", fs.name) or fs.name in names:
            raise InputError(f"config.filesystems[{i}].name: {fs.name!r} must be unique and match [A-Za-z0-9_.-]+")
        names.add(fs.name)
        fss.append(fs)
    if not fss:
        raise InputError("config.filesystems: must not be empty")
    device_mib = f.int_("device_mib", lo=16)
    replay_log = f.opt_str("replay_log")
    checker_cwd = f.opt_str("checker_cwd")
    cfg = Config(
        workdir=base / f.str_("workdir"),
        workloads=tuple(base / p for p in f.str_list("workloads", nonempty=True)),
        filesystems=tuple(fss),
        device_mib=device_mib,
        log_device_mib=f.int_("log_device_mib", lo=16) if f.has("log_device_mib") else 4 * device_mib,
        replay_log=base / replay_log if replay_log is not None else None,
        checker=f.str_list("checker", nonempty=True),
        checker_cwd=base / checker_cwd if checker_cwd is not None else REPO_ROOT,
        max_states_per_window=f.int_("max_states_per_window", lo=1),
        seed=f.int_("seed", lo=-(2**63), hi=2**63 - 1),
        output=base / f.str_("output"),
        timeout_s=f.int_("timeout_s", lo=1),
    )
    f.done()
    return cfg


OPEN_FLAGS: dict[str, int] = {
    "create": os.O_CREAT,
    "trunc": os.O_TRUNC,
    "append": os.O_APPEND,
    "dsync": getattr(os, "O_DSYNC", 0),
    "sync": os.O_SYNC,
    "excl": os.O_EXCL,
    "read": 0,
    "write": 0,
}

OP_FIELDS: dict[str, frozenset[str]] = {
    "mkdir": frozenset({"path"}),
    "rmdir": frozenset({"path"}),
    "unlink": frozenset({"path"}),
    "fsync_dir": frozenset({"path"}),
    "open": frozenset({"fd", "path", "flags"}),
    "write": frozenset({"fd", "fill"}),
    "pwrite": frozenset({"fd", "offset", "fill"}),
    "fsync": frozenset({"fd"}),
    "fdatasync": frozenset({"fd"}),
    "ftruncate": frozenset({"fd", "len"}),
    "close": frozenset({"fd"}),
    "rename": frozenset({"from", "to"}),
}


@dataclass(frozen=True)
class Fill:
    byte: int
    len: int

    def data(self) -> bytes:
        return bytes([self.byte]) * self.len


@dataclass(frozen=True)
class Op:
    op: str
    path: str | None = None
    fd: str | None = None
    flags: tuple[str, ...] = ()
    fill: Fill | None = None
    offset: int | None = None
    len: int | None = None
    src: str | None = None
    dst: str | None = None


@dataclass(frozen=True)
class Workload:
    name: str
    ops: tuple[Op, ...]
    path: Path


def _check_path(p: str, where: str) -> str:
    if not p.startswith("/") or any(part in ("..", ".") for part in p.split("/")) or "\0" in p:
        raise InputError(f"{where}: path {p!r} must be absolute within the mount and have no '.' or '..'")
    return p


def parse_workload(value: object, path: Path) -> Workload:
    f = _Fields(value, f"workload {path}")
    name = f.str_("name")
    ops: list[Op] = []
    open_fds: set[str] = set()
    for i, raw in enumerate(f.list_("ops")):
        where = f"workload {name} ops[{i}]"
        g = _Fields(raw, where)
        kind = g.str_("op")
        if kind not in OP_FIELDS:
            raise InputError(f"{where}.op: unknown op {kind!r}; known: {sorted(OP_FIELDS)}")
        need = OP_FIELDS[kind]
        fill: Fill | None = None
        if "fill" in need:
            h = _Fields(g._get("fill"), f"{where}.fill")
            fill = Fill(byte=h.int_("byte", hi=255), len=h.int_("len", lo=1, hi=1 << 30))
            h.done()
        flags = g.str_list("flags") if "flags" in need else ()
        for fl in flags:
            if fl not in OPEN_FLAGS:
                raise InputError(f"{where}.flags: unknown flag {fl!r}; known: {sorted(OPEN_FLAGS)}")
        op = Op(
            op=kind,
            path=_check_path(g.str_("path"), f"{where}.path") if "path" in need else None,
            fd=g.str_("fd") if "fd" in need else None,
            flags=flags,
            fill=fill,
            offset=g.int_("offset") if "offset" in need else None,
            len=g.int_("len") if "len" in need else None,
            src=_check_path(g.str_("from"), f"{where}.from") if "from" in need else None,
            dst=_check_path(g.str_("to"), f"{where}.to") if "to" in need else None,
        )
        g.done()
        if op.fd is not None:
            if kind == "open":
                if op.fd in open_fds:
                    raise InputError(f"{where}: fd {op.fd!r} is already open")
                open_fds.add(op.fd)
            elif op.fd not in open_fds:
                raise InputError(f"{where}: fd {op.fd!r} is not open")
            elif kind == "close":
                open_fds.remove(op.fd)
        ops.append(op)
    f.done()
    if not ops:
        raise InputError(f"workload {name}: ops must not be empty")
    if open_fds:
        raise InputError(f"workload {name}: fds {sorted(open_fds)} are never closed (unmount needs them closed)")
    return Workload(name=name, ops=tuple(ops), path=path)


def load_workload(path: Path) -> Workload:
    try:
        value: object = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        raise InputError(f"workload {path}: {e}") from e
    return parse_workload(value, path)


# --- dm-log-writes log format -------------------------------------------------------------------------------
# drivers/md/dm-log-writes.c and Documentation/admin-guide/device-mapper/log-writes.rst. The super block
# (struct log_write_super) is at byte 0. Entries start one device sector later. Each entry is one device
# sector holding struct log_write_entry (a mark's label follows it in the same sector), then nr_sectors
# device sectors of data, absent for discards. `sector` and `nr_sectors` count device sectors of `sectorsize`
# bytes (the data device's logical block size), not 512-byte bio sectors.

LOG_FLUSH_FLAG = 1 << 0
LOG_FUA_FLAG = 1 << 1
LOG_DISCARD_FLAG = 1 << 2
LOG_MARK_FLAG = 1 << 3
LOG_METADATA_FLAG = 1 << 4
LOG_KNOWN_FLAGS = LOG_FLUSH_FLAG | LOG_FUA_FLAG | LOG_DISCARD_FLAG | LOG_MARK_FLAG | LOG_METADATA_FLAG
WRITE_LOG_MAGIC = 0x6A736677736872
WRITE_LOG_VERSION = 1
SUPER = struct.Struct("<QQQI")
ENTRY = struct.Struct("<QQQQ")


@dataclass(frozen=True)
class LogEntry:
    index: int
    sector: int
    nr_sectors: int
    flags: int
    data_offset: int
    mark: str | None

    @property
    def writes_data(self) -> bool:
        return self.mark is None and self.nr_sectors > 0

    @property
    def is_flush(self) -> bool:
        return bool(self.flags & LOG_FLUSH_FLAG)

    @property
    def is_fua(self) -> bool:
        return bool(self.flags & LOG_FUA_FLAG)


@dataclass(frozen=True)
class WriteLog:
    path: Path
    sectorsize: int
    entries: tuple[LogEntry, ...]

    def mark_index(self, label: str) -> int:
        for e in self.entries:
            if e.mark == label:
                return e.index
        raise LogFormatError(f"log {self.path}: no mark {label!r}")


def parse_log(path: Path) -> WriteLog:
    size = path.stat().st_size
    with path.open("rb") as f:
        head = f.read(SUPER.size)
        if len(head) < SUPER.size:
            raise LogFormatError(f"log {path}: shorter than the super block")
        magic, version, nr_entries, sectorsize = SUPER.unpack(head)
        if magic != WRITE_LOG_MAGIC or version != WRITE_LOG_VERSION:
            raise LogFormatError(f"log {path}: magic {magic:#x} version {version}, want {WRITE_LOG_MAGIC:#x} v1")
        if sectorsize not in (512, 1024, 2048, 4096):
            raise LogFormatError(f"log {path}: sectorsize {sectorsize} is not a block size")
        pos = sectorsize
        entries: list[LogEntry] = []
        for i in range(nr_entries):
            f.seek(pos)
            hdr = f.read(sectorsize)
            if len(hdr) < sectorsize:
                raise LogFormatError(f"log {path}: entry {i} of {nr_entries} at byte {pos} is past the end ({size})")
            sector, nr_sectors, flags, data_len = ENTRY.unpack_from(hdr)
            if flags & ~LOG_KNOWN_FLAGS:
                raise LogFormatError(f"log {path}: entry {i} at byte {pos} has unknown flags {flags:#x}")
            mark: str | None = None
            if flags & LOG_MARK_FLAG:
                if data_len > sectorsize - ENTRY.size or nr_sectors:
                    raise LogFormatError(f"log {path}: mark entry {i} at byte {pos} has data_len {data_len}")
                mark = hdr[ENTRY.size : ENTRY.size + data_len].decode(errors="replace")
            elif data_len:
                raise LogFormatError(
                    f"log {path}: entry {i} at byte {pos} carries {data_len} bytes of inline data, which this "
                    "parser does not support"
                )
            pos += sectorsize
            data_offset = pos
            if not flags & LOG_DISCARD_FLAG:
                pos += nr_sectors * sectorsize
            if pos > size:
                raise LogFormatError(f"log {path}: data of entry {i} runs past the end of the log ({size})")
            entries.append(LogEntry(i, sector, nr_sectors, flags, data_offset, mark))
    return WriteLog(path, sectorsize, tuple(entries))


CHUNK = 1 << 20


def create_sparse(path: Path, size: int) -> None:
    with path.open("wb") as f:
        f.truncate(size)


def sparse_copy(src: Path, dst: Path) -> None:
    size = src.stat().st_size
    with src.open("rb") as fi, dst.open("wb") as fo:
        fo.truncate(size)
        ifd, ofd = fi.fileno(), fo.fileno()
        pos = 0
        while pos < size:
            try:
                start = os.lseek(ifd, pos, os.SEEK_DATA)
            except OSError:
                break
            end = os.lseek(ifd, start, os.SEEK_HOLE)
            off = start
            while off < end:
                chunk = os.pread(ifd, min(CHUNK, end - off), off)
                if not chunk:
                    raise OSError(f"sparse_copy {src}: unexpected EOF at {off}")
                os.pwrite(ofd, chunk, off)
                off += len(chunk)
            pos = end


def apply_entries(image: Path, log: WriteLog, entries: Iterable[LogEntry]) -> None:
    ss = log.sectorsize
    size = image.stat().st_size
    zeros = bytes(CHUNK)
    with log.path.open("rb") as lf, image.open("r+b") as imf:
        lfd, ifd = lf.fileno(), imf.fileno()
        for e in entries:
            if not e.writes_data:
                continue
            off, length = e.sector * ss, e.nr_sectors * ss
            if off + length > size:
                raise LogFormatError(f"log entry {e.index}: sectors {e.sector}+{e.nr_sectors} exceed image {image}")
            done = 0
            while done < length:
                n = min(CHUNK, length - done)
                if e.flags & LOG_DISCARD_FLAG:
                    chunk = zeros[:n]
                else:
                    chunk = os.pread(lfd, n, e.data_offset + done)
                    if len(chunk) != n:
                        raise LogFormatError(f"log entry {e.index}: short read of its data at {e.data_offset + done}")
                os.pwrite(ifd, chunk, off + done)
                done += n


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(CHUNK):
            h.update(chunk)
    return h.hexdigest()


type CrashKind = Literal["mark", "flush", "subset"]


@dataclass(frozen=True)
class CrashPoint:
    kind: CrashKind
    label: str
    entries: tuple[int, ...]
    prefix_end: int
    """Every data entry with index below this is applied first."""
    extra: tuple[int, ...] = ()
    """Then these data entries, in log order."""

    def to_json(self) -> dict[str, object]:
        return {"kind": self.kind, "label": self.label, "entries": list(self.entries)}


def _window_subsets(n: int, cap: int, rng: random.Random) -> list[int]:
    full = (1 << n) - 1
    if n <= 8:
        return list(range(1, full))
    seen: set[int] = set()
    for _ in range(cap * 8):
        if len(seen) >= cap:
            break
        mask = rng.getrandbits(n)
        if mask not in (0, full):
            seen.add(mask)
    return sorted(seen)


def plan_crash_points(log: WriteLog, start_mark: str, cap: int, seed: str) -> list[CrashPoint]:
    """Crash states from `start_mark` on: every mark, every flush or FUA entry, and subsets of each window.

    dm-log-writes logs a non-FUA write only when a later flush completes, just before that flush entry, while
    marks and FUA writes are logged at once. So the writes between two flush entries are the ones in flight
    in that epoch: a crash may have persisted any subset of them (CrashMonkey, OSDI'18, section 4). A state's
    label is the last mark logged before the state's end, which bounds how many workload ops had run.
    """
    entries = log.entries
    start = log.mark_index(start_mark)

    def last_mark_before(i: int) -> str:
        for e in reversed(entries[:i]):
            if e.mark is not None:
                return e.mark
        return "(none)"

    points: list[CrashPoint] = []
    for e in entries[start:]:
        if e.mark is not None:
            points.append(CrashPoint("mark", e.mark, (e.index,), e.index))
        elif e.is_flush or e.is_fua:
            points.append(CrashPoint("flush", last_mark_before(e.index), (e.index,), e.index + 1))
    flushes = [e.index for e in entries if e.is_flush]
    bounds = [-1, *flushes, len(entries)]
    for k in range(len(bounds) - 1):
        lo, hi = bounds[k], bounds[k + 1]
        if hi <= start:
            continue
        window = [e.index for e in entries[lo + 1 : hi] if e.writes_data]
        rng = random.Random(f"{seed}:window-{k}")
        label = last_mark_before(hi)
        for mask in _window_subsets(len(window), cap, rng):
            chosen = tuple(idx for bit, idx in enumerate(window) if mask >> bit & 1)
            points.append(CrashPoint("subset", label, chosen, lo + 1, chosen))
    points.sort(key=lambda p: (p.prefix_end, ("mark", "flush", "subset").index(p.kind), p.extra))
    return points


@dataclass
class Mount:
    mnt: Path
    timeout_s: int
    mounted: bool = False

    async def mount(self, dev: str, opts: str, step: str) -> None:
        argv = ["mount", *(["-o", opts] if opts else []), dev, str(self.mnt)]
        await run(step, argv, self.timeout_s)
        self.mounted = True

    async def umount(self) -> None:
        if self.mounted:
            await run("umount", ["umount", str(self.mnt)], self.timeout_s)
            self.mounted = False


async def loop_attach(image: Path, timeout_s: int) -> str:
    out = await run("losetup attach", ["losetup", "--find", "--show", str(image)], timeout_s)
    dev = out.decode().strip()
    if not dev.startswith("/dev/"):
        raise StepError("losetup attach", ["losetup", "--find", "--show", str(image)], 0, f"odd output {dev!r}")
    return dev


async def loop_detach(dev: str, timeout_s: int) -> None:
    await run("losetup detach", ["losetup", "-d", dev], timeout_s)


async def dm_mark(name: str, label: str, timeout_s: int) -> None:
    await run(f"mark {label}", ["dmsetup", "message", name, "0", "mark", label], timeout_s)


def _host_path(root: Path, p: str) -> Path:
    return root / p.lstrip("/") if p != "/" else root


def _write_all(fd: int, data: bytes, offset: int | None) -> None:
    done = 0
    while done < len(data):
        if offset is None:
            n = os.write(fd, data[done:])
        else:
            n = os.pwrite(fd, data[done:], offset + done)
        if n <= 0:
            raise OSError(f"write returned {n} after {done} of {len(data)} bytes")
        done += n


def do_op(op: Op, root: Path, fds: dict[str, int]) -> None:
    match op.op:
        case "mkdir":
            os.mkdir(_host_path(root, cast(str, op.path)), 0o755)
        case "rmdir":
            os.rmdir(_host_path(root, cast(str, op.path)))
        case "unlink":
            os.unlink(_host_path(root, cast(str, op.path)))
        case "rename":
            os.rename(_host_path(root, cast(str, op.src)), _host_path(root, cast(str, op.dst)))
        case "fsync_dir":
            dfd = os.open(_host_path(root, cast(str, op.path)), os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(dfd)
            finally:
                os.close(dfd)
        case "open":
            rd, wr = "read" in op.flags, "write" in op.flags
            flags = os.O_RDWR if rd and wr else os.O_WRONLY if wr else os.O_RDONLY
            for fl in op.flags:
                flags |= OPEN_FLAGS[fl]
            fds[cast(str, op.fd)] = os.open(_host_path(root, cast(str, op.path)), flags, 0o644)
        case "write":
            _write_all(fds[cast(str, op.fd)], cast(Fill, op.fill).data(), None)
        case "pwrite":
            _write_all(fds[cast(str, op.fd)], cast(Fill, op.fill).data(), op.offset)
        case "fsync":
            os.fsync(fds[cast(str, op.fd)])
        case "fdatasync":
            os.fdatasync(fds[cast(str, op.fd)])
        case "ftruncate":
            os.ftruncate(fds[cast(str, op.fd)], cast(int, op.len))
        case "close":
            os.close(fds.pop(cast(str, op.fd)))
        case other:
            raise InputError(f"unknown op {other!r}")


def dump_state(root: Path) -> dict[str, dict[str, object]]:
    out: dict[str, dict[str, object]] = {"/": {"type": "dir"}}

    def walk(host: Path, rel: str) -> None:
        for name in sorted(os.listdir(host)):
            if rel == "" and name == "lost+found":
                continue
            hp, rp = host / name, f"{rel}/{name}"
            st = os.lstat(hp)
            if stat.S_ISDIR(st.st_mode):
                out[rp] = {"type": "dir"}
                walk(hp, rp)
            elif stat.S_ISREG(st.st_mode):
                data = hp.read_bytes()
                out[rp] = {"type": "file", "size": st.st_size, "content_b64": base64.b64encode(data).decode()}
            elif stat.S_ISLNK(st.st_mode):
                out[rp] = {"type": "symlink", "target": os.readlink(hp)}
            else:
                out[rp] = {"type": "other", "mode": st.st_mode}

    walk(root, "")
    return dict(sorted(out.items()))


@dataclass
class PairResult:
    fs: str
    workload: str
    log_entries: int = 0
    states: int = 0
    allowed: int = 0
    disallowed: list[dict[str, object]] = field(default_factory=list)
    broken: list[dict[str, object]] = field(default_factory=list)
    rules: dict[str, int] = field(default_factory=dict)
    crosscheck: dict[str, object] = field(default_factory=dict)
    error: str | None = None
    kept_dir: str | None = None


def _dm_name(fs: str, wl: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]", "-", f"hegel-lw-{os.getpid()}-{fs}-{wl}")[:120]


async def record(cfg: Config, fs: FsConfig, wl: Workload, lane: Path) -> tuple[Path, Path]:
    t = cfg.timeout_s
    data_img, log_img, mnt = lane / "data.img", lane / "log.img", lane / "mnt"
    mnt.mkdir()
    create_sparse(data_img, cfg.device_mib << 20)
    create_sparse(log_img, cfg.log_device_mib << 20)
    async with AsyncExitStack() as stack:
        data_loop = await loop_attach(data_img, t)
        stack.push_async_callback(loop_detach, data_loop, t)
        log_loop = await loop_attach(log_img, t)
        stack.push_async_callback(loop_detach, log_loop, t)
        name = _dm_name(fs.name, wl.name)
        table = f"0 {cfg.device_mib * 2048} log-writes {data_loop} {log_loop}"
        await run("dmsetup create", ["dmsetup", "create", name, "--table", table], t)
        stack.push_async_callback(run, "dmsetup remove", ["dmsetup", "remove", "--retry", name], t)
        await run("dmsetup mknodes", ["dmsetup", "mknodes", name], t)
        dev = f"/dev/mapper/{name}"
        await run("mkfs", [*fs.mkfs, dev], t)
        await dm_mark(name, "mkfs", t)
        m = Mount(mnt, t)
        stack.push_async_callback(m.umount)
        await m.mount(dev, fs.mount_opts, "mount for workload")
        fds: dict[str, int] = {}
        try:
            for i, op in enumerate(wl.ops):
                await in_thread(f"op-{i} {op.op}", t, lambda op=op: do_op(op, mnt, fds))
                await dm_mark(name, f"op-{i}", t)
        finally:
            for fd in fds.values():
                os.close(fd)
        await m.umount()
        await dm_mark(name, "unmounted", t)
    return data_img, log_img


async def cross_check(cfg: Config, log: WriteLog, data_img: Path, lane: Path, final_mark: str) -> dict[str, object]:
    t = cfg.timeout_s
    size = data_img.stat().st_size
    full = lane / "full.img"

    def full_replay() -> tuple[str, str]:
        create_sparse(full, size)
        apply_entries(full, log, log.entries)
        try:
            return sha256_file(full), sha256_file(data_img)
        finally:
            full.unlink()

    ours, recorded = await in_thread("cross-check full replay", t, full_replay)
    if ours != recorded:
        raise CrossCheckError(f"replaying all {len(log.entries)} entries gives {ours}, the device holds {recorded}")
    result: dict[str, object] = {"full_replay_matches_device": True}
    if cfg.replay_log is None:
        return result

    mark_idx = log.mark_index(final_mark)
    mine, theirs = lane / "mark-py.img", lane / "mark-replay-log.img"

    def py_mark() -> str:
        create_sparse(mine, size)
        apply_entries(mine, log, log.entries[:mark_idx])
        try:
            return sha256_file(mine)
        finally:
            mine.unlink()

    ours = await in_thread(f"cross-check python replay to {final_mark}", t, py_mark)
    create_sparse(theirs, size)
    try:
        loop = await loop_attach(theirs, t)
        try:
            await run("replay-log to mark", replay_log_argv(cfg.replay_log, log.path, loop, final_mark), t)
        finally:
            await loop_detach(loop, t)
        ref = await in_thread("hash replay-log image", t, lambda: sha256_file(theirs))
    finally:
        theirs.unlink()
    if ours != ref:
        raise CrossCheckError(f"to mark {final_mark!r}: python replay gives {ours}, replay-log gives {ref}")
    result["replay_log_mark"] = final_mark
    result["replay_log_matches"] = True
    return result


def replay_log_argv(binary: Path, log: Path, replay_dev: str, end_mark: str) -> list[str]:
    """xfstests src/log-writes/replay-log: replay every entry before `end_mark` onto `replay_dev`."""
    return [str(binary), "--log", str(log), "--replay", replay_dev, "--end-mark", end_mark]


@dataclass(frozen=True)
class Verdict:
    allowed: bool
    reason: str
    rules: tuple[str, ...]


def parse_verdict(out: bytes, argv: Sequence[str]) -> Verdict:
    lines = [ln for ln in out.decode(errors="replace").splitlines() if ln.strip()]
    if not lines:
        raise StepError("checker", argv, 0, "printed nothing on stdout")
    try:
        f = _Fields(json.loads(lines[-1]), "checker verdict")
        allowed = f._get("allowed")
        if not isinstance(allowed, bool):
            raise InputError("checker verdict.allowed: expected a bool")
        reason = f.str_("reason")
        rules = f.str_list("rules") if f.has("rules") else ()
        f.done()
    except (json.JSONDecodeError, InputError) as e:
        raise StepError("checker", argv, 0, f"bad verdict line {lines[-1]!r}: {e}") from e
    return Verdict(allowed, reason, rules)


async def check_states(cfg: Config, fs: FsConfig, wl: Workload, log: WriteLog, lane: Path, res: PairResult) -> None:
    t = cfg.timeout_s
    seed = f"{cfg.seed}:{fs.name}:{wl.name}"
    points = plan_crash_points(log, "mkfs", cfg.max_states_per_window, seed)
    base, state, mnt = lane / "base.img", lane / "state.img", lane / "mnt"
    size = cfg.device_mib << 20
    create_sparse(base, size)
    base_end = 0
    rules: Counter[str] = Counter()
    argv = [*cfg.checker, str(wl.path)]
    for cp in points:
        if cp.prefix_end > base_end:
            todo = log.entries[base_end : cp.prefix_end]
            await in_thread(
                f"advance base to entry {cp.prefix_end}", t, lambda todo=todo: apply_entries(base, log, todo)
            )
            base_end = cp.prefix_end

        def build(cp: CrashPoint = cp) -> None:
            sparse_copy(base, state)
            apply_entries(state, log, (log.entries[i] for i in cp.extra))

        await in_thread(f"build state {cp.kind} {cp.label} {list(cp.entries)}", t, build)
        res.states += 1
        try:
            loop = await loop_attach(state, t)
            m = Mount(mnt, t)
            try:
                try:
                    await m.mount(loop, fs.mount_opts, "mount crash state")
                except StepError as e:
                    res.broken.append({"crash_point": cp.to_json(), "stage": "mount", "error": str(e)})
                    continue
                try:
                    entries = await in_thread("read crash state", t, lambda: dump_state(mnt))
                except OSError as e:
                    res.broken.append({"crash_point": cp.to_json(), "stage": "read", "error": repr(e)})
                    continue
            finally:
                await m.umount()
                await loop_detach(loop, t)
        finally:
            state.unlink()
        observed = {"workload": wl.name, "fs": fs.name, "crash_point": cp.to_json(), "entries": entries}
        out = await run("checker", argv, t, stdin=json.dumps(observed).encode(), cwd=cfg.checker_cwd)
        v = parse_verdict(out, argv)
        if v.allowed:
            res.allowed += 1
            rules.update(v.rules)
        else:
            res.disallowed.append({"crash_point": cp.to_json(), "reason": v.reason, "rules": list(v.rules)})
    base.unlink()
    res.rules = dict(sorted(rules.items()))


async def run_pair(cfg: Config, fs: FsConfig, wl: Workload) -> PairResult:
    res = PairResult(fs=fs.name, workload=wl.name)
    lane = Path(tempfile.mkdtemp(prefix=f"{fs.name}--{wl.name}--", dir=cfg.workdir))
    try:
        data_img, log_img = await record(cfg, fs, wl, lane)
        log = await in_thread("parse log", cfg.timeout_s, lambda: parse_log(log_img))
        res.log_entries = len(log.entries)
        res.crosscheck = await cross_check(cfg, log, data_img, lane, f"op-{len(wl.ops) - 1}")
        await check_states(cfg, fs, wl, log, lane, res)
    except Exception as e:  # noqa: BLE001  recorded in the report and the exit code, never dropped
        res.error = "".join(traceback.format_exception(e))
        res.kept_dir = str(lane)
        print(f"[{fs.name} x {wl.name}] FAILED, kept {lane}:\n{res.error}", file=sys.stderr)
        return res
    for name in ("data.img", "log.img"):
        (lane / name).unlink()
    (lane / "mnt").rmdir()
    lane.rmdir()
    print(
        f"[{fs.name} x {wl.name}] {res.states} states, {res.allowed} allowed, "
        f"{len(res.disallowed)} disallowed, {len(res.broken)} broken",
        file=sys.stderr,
    )
    return res


async def run_lane(cfg: Config, fs: FsConfig, workloads: Sequence[Workload]) -> list[PairResult]:
    return [await run_pair(cfg, fs, wl) for wl in workloads]


async def run_all(cfg: Config) -> int:
    if os.geteuid() != 0:
        raise SystemExit("validate_kernel_model: needs root (losetup, dmsetup, mount)")
    workloads = [load_workload(p) for p in cfg.workloads]
    cfg.workdir.mkdir(parents=True, exist_ok=True)
    async with asyncio.TaskGroup() as tg:
        lanes = [tg.create_task(run_lane(cfg, fs, workloads)) for fs in cfg.filesystems]
    results = [r for lane in lanes for r in lane.result()]
    errors = sum(r.error is not None for r in results)
    findings = sum(len(r.disallowed) + len(r.broken) for r in results)
    report: dict[str, object] = {
        "ok": errors == 0 and findings == 0,
        "harness_errors": errors,
        "findings": findings,
        "config": json.loads(json.dumps(asdict(cfg), default=str)),
        "results": [asdict(r) for r in results],
    }
    cfg.output.parent.mkdir(parents=True, exist_ok=True)
    cfg.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(f"report: {cfg.output} ({errors} harness errors, {findings} findings)", file=sys.stderr)
    return 1 if errors else 2 if findings else 0


def _synthetic_log(path: Path, ss: int, items: Sequence[tuple[int, int, int, bytes | str]]) -> None:
    body = bytearray()
    for sector, nr, flags, payload in items:
        hdr = bytearray(ss)
        if isinstance(payload, str):
            label = payload.encode()
            ENTRY.pack_into(hdr, 0, 0, 0, flags | LOG_MARK_FLAG, len(label))
            hdr[ENTRY.size : ENTRY.size + len(label)] = label
            body += hdr
            continue
        ENTRY.pack_into(hdr, 0, sector, nr, flags, 0)
        body += hdr
        if not flags & LOG_DISCARD_FLAG:
            assert len(payload) == nr * ss
            body += payload
    sup = bytearray(ss)
    SUPER.pack_into(sup, 0, WRITE_LOG_MAGIC, WRITE_LOG_VERSION, len(items), ss)
    path.write_bytes(bytes(sup) + bytes(body))


def self_test() -> None:
    ss = 512

    def blk(b: int, n: int = 1) -> bytes:
        return bytes([b]) * (ss * n)

    items: list[tuple[int, int, int, bytes | str]] = [
        (0, 2, 0, blk(1, 2)),
        (0, 0, LOG_FLUSH_FLAG, b""),
        (0, 0, 0, "mkfs"),
        (0, 0, 0, "op-0"),
        (4, 1, LOG_FUA_FLAG, blk(2)),
        (8, 0, 0, b""),
        (5, 1, 0, blk(3)),
        (6, 1, 0, blk(4)),
        (0, 1, 0, blk(5)),
        (1, 1, LOG_DISCARD_FLAG, b""),
        (0, 0, LOG_FLUSH_FLAG, b""),
        (0, 0, 0, "op-1"),
        (0, 0, 0, "unmounted"),
    ]
    with tempfile.TemporaryDirectory() as d:
        dp = Path(d)
        _synthetic_log(dp / "log", ss, items)
        log = parse_log(dp / "log")
        assert len(log.entries) == len(items), log.entries
        assert [e.mark for e in log.entries if e.mark] == ["mkfs", "op-0", "op-1", "unmounted"]
        points = plan_crash_points(log, "mkfs", 16, "self-test")
        kinds = Counter(p.kind for p in points)
        window = [e.index for e in log.entries[2:10] if e.writes_data]
        assert window == [4, 6, 7, 8, 9], window
        assert kinds["subset"] == 2**5 - 2, kinds
        assert kinds["mark"] == 4 and kinds["flush"] == 2, kinds
        assert all(p.prefix_end == 2 for p in points if p.kind == "subset")
        assert [p.prefix_end for p in points] == sorted(p.prefix_end for p in points)
        img = dp / "img"
        create_sparse(img, 16 * ss)
        apply_entries(img, log, log.entries)
        got = img.read_bytes()
        want = blk(5) + bytes(ss) + bytes(2 * ss) + blk(2) + blk(3) + blk(4) + bytes(9 * ss)
        assert got == want, "full replay mismatch"
        copy = dp / "copy"
        sparse_copy(img, copy)
        assert copy.read_bytes() == got
        sub = next(p for p in points if p.kind == "subset" and p.extra == (6,))
        create_sparse(img, 16 * ss)
        apply_entries(img, log, log.entries[: sub.prefix_end])
        apply_entries(img, log, (log.entries[i] for i in sub.extra))
        assert img.read_bytes() == blk(1, 2) + bytes(3 * ss) + blk(3) + bytes(10 * ss)
        big = plan_crash_points(_fake_window_log(dp / "big", ss, 12), "mkfs", 10, "seed-a")
        again = plan_crash_points(parse_log(dp / "big"), "mkfs", 10, "seed-a")
        assert big == again and sum(p.kind == "subset" for p in big) == 10
    for p in sorted((HERE / "workloads").glob("*.json")):
        wl = load_workload(p)
        print(f"workload {wl.name}: {len(wl.ops)} ops")
    parse_config(
        {
            "workdir": "/tmp/w",
            "workloads": ["workloads/rename_no_fsync.json"],
            "filesystems": [{"name": "ext4-ordered", "mkfs": ["mkfs.ext4", "-F"], "mount_opts": "data=ordered"}],
            "device_mib": 256,
            "replay_log": None,
            "checker": ["cargo", "run", "-q", "-p", "hegel-sim-core", "--example", "crash_states", "--"],
            "max_states_per_window": 64,
            "seed": 1,
            "output": "report.json",
            "timeout_s": 120,
        },
        HERE,
    )
    print("self-test ok")


def _fake_window_log(path: Path, ss: int, n: int) -> WriteLog:
    items: list[tuple[int, int, int, bytes | str]] = [(0, 0, 0, "mkfs")]
    items += [(i, 1, 0, bytes([i + 1]) * ss) for i in range(n)]
    items.append((0, 0, LOG_FLUSH_FLAG, b""))
    _synthetic_log(path, ss, items)
    return parse_log(path)


def main(argv: Sequence[str]) -> int:
    if list(argv) == ["--self-test"]:
        self_test()
        return 0
    if len(argv) > 1:
        print("usage: validate_kernel_model.py [CONFIG.json | --self-test]  (or the config JSON on stdin)")
        return 64
    try:
        if argv:
            path = Path(argv[0])
            value: object = json.loads(path.read_text())
            base = path.resolve().parent
        else:
            value = json.loads(sys.stdin.read())
            base = Path.cwd()
        cfg = parse_config(value, base)
    except (OSError, json.JSONDecodeError, InputError) as e:
        print(f"validate_kernel_model: bad config: {e}", file=sys.stderr)
        return 64
    return asyncio.run(run_all(cfg))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
