"""Low-level measurement helpers: timing, subprocess, and host RSS sampling.

These are shared by every provider adapter so the methodology is identical
across providers — the only thing that differs is which process tree gets
sampled, which each adapter resolves to a PID.
"""

import os
import re
import shutil
import signal
import subprocess
import time


def now_ms():
    """Monotonic clock in milliseconds (immune to wall-clock adjustments)."""
    return time.monotonic() * 1000.0


class Timer:
    """Context manager that records elapsed monotonic milliseconds.

    Usage:
        with Timer() as t:
            do_work()
        print(t.ms)
    """

    def __init__(self):
        self.ms = None
        self._start = None

    def __enter__(self):
        self._start = time.monotonic()
        return self

    def __exit__(self, *exc):
        self.ms = (time.monotonic() - self._start) * 1000.0
        return False


def have(binary):
    """True if ``binary`` resolves on PATH."""
    return shutil.which(binary) is not None


def run(cmd, *, timeout=None, env=None, check=True, input_text=None):
    """Run a command, capturing combined stdout/stderr as text.

    Returns a CompletedProcess. Raises subprocess.CalledProcessError on a
    non-zero exit when ``check`` is set, and subprocess.TimeoutExpired on
    timeout (the child is killed first).
    """
    full_env = None
    if env:
        full_env = dict(os.environ)
        full_env.update(env)
    proc = subprocess.run(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.PIPE if input_text is not None else subprocess.DEVNULL,
        input=input_text,
        text=True,
        env=full_env,
        timeout=timeout,
        check=check,
    )
    return proc


def popen(cmd, *, env=None, logfile=None):
    """Spawn a detached background process with stdin closed.

    Output is redirected to ``logfile`` (path) if given, else discarded. Used
    for providers whose "launch" is a long-lived foreground process (e.g.
    ``tb-sandbox shell``) that we sample and later kill.
    """
    full_env = None
    if env:
        full_env = dict(os.environ)
        full_env.update(env)
    out = open(logfile, "w") if logfile else subprocess.DEVNULL
    return subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=out,
        stderr=subprocess.STDOUT,
        env=full_env,
        # New process group so we can signal the whole tree on teardown.
        start_new_session=True,
    )


def kill_tree(proc, *, grace_s=3.0):
    """Terminate a Popen and its process group, escalating to SIGKILL."""
    if proc is None or proc.poll() is not None:
        return
    try:
        pgid = os.getpgid(proc.pid)
    except ProcessLookupError:
        return
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + grace_s
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return
        time.sleep(0.05)
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass


# --- host memory sampling --------------------------------------------------
#
# We sample resident set size (RSS) of a process subtree with `ps`, which is
# portable across macOS and Linux. RSS is a high-water-ish current resident
# figure; for VM providers it tracks host-backed guest pages plus VMM overhead.
# This mirrors scripts/profile-sandbox.sh so tb-sandbox numbers stay comparable.


def _ps_rss_kib(pid):
    """RSS (KiB) of a single pid via ps, or None if the pid is gone."""
    try:
        out = subprocess.run(
            ["ps", "-o", "rss=", "-p", str(pid)],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        ).stdout.strip()
    except Exception:
        return None
    if not out:
        return None
    try:
        return int(out.split()[0])
    except (ValueError, IndexError):
        return None


def _child_pids(pid):
    """Direct child pids of ``pid`` via ps (portable, no /proc dependency)."""
    try:
        out = subprocess.run(
            ["ps", "-o", "pid=", "-o", "ppid="],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        ).stdout
    except Exception:
        return []
    kids = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                cpid, ppid = int(parts[0]), int(parts[1])
            except ValueError:
                continue
            if ppid == pid:
                kids.append(cpid)
    return kids


def rss_bytes_pid(pid):
    """RSS (bytes) of a single pid, or None. Use when the pid *is* the VMM and
    summing a subtree would double-count shared mappings."""
    kib = _ps_rss_kib(pid)
    return kib * 1024 if kib else None


def rss_bytes_tree(pid, *, max_depth=4):
    """Sum RSS (bytes) of ``pid`` and its descendants up to ``max_depth``.

    Summing the subtree matters for wrappers that interpose a launcher process
    in front of the VMM (cargo/run shims, gvproxy, relays): the guest memory
    lives in a child, not the process we spawned.
    """
    seen = set()
    total_kib = 0
    frontier = [(pid, 0)]
    while frontier:
        cur, depth = frontier.pop()
        if cur in seen:
            continue
        seen.add(cur)
        kib = _ps_rss_kib(cur)
        if kib:
            total_kib += kib
        if depth < max_depth:
            for kid in _child_pids(cur):
                frontier.append((kid, depth + 1))
    return total_kib * 1024 if total_kib else None


_INT_RE = re.compile(r"-?\d+")


def first_int(text):
    """First integer found in ``text`` (handy for parsing CLI output)."""
    m = _INT_RE.search(text or "")
    return int(m.group()) if m else None
