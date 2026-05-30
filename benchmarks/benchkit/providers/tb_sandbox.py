"""Adapter for thousandbirds-agent-sandbox (this repository).

Drives the ``tb-sandbox`` CLI. The current build has no in-guest daemon yet, so
the lifecycle is modeled on what actually happens:

  - ``create`` only *provisions* a stopped sandbox (clone disk + register); the
    VM is not running, so it is not "ready to run a command".
  - A VM is live only while a ``shell``/``start`` process holds it open.
  - ``exec <id>`` on a sandbox **cold-boots a fresh VM** (~1.3 s), runs, and
    powers off — there is no attach-to-running-session yet (that is the planned
    guest daemon). So ``exec_roundtrip`` legitimately includes a boot.

To get a live, sample-able VM (for idle RSS and memory snapshots) we background
``tb-sandbox shell`` with an ``--initial-command`` that prints a ready marker
then sleeps — exactly the technique in ``scripts/profile-sandbox.sh``. Each
launch gets an isolated ``TB_SANDBOX_STATE_DIR`` so sandboxes never collide and
density works.

Configuration (env vars):
  TB_SANDBOX_BIN      path to the binary (default: target/release then
                      target/debug under the repo, else `cargo run --release`)
  TB_SANDBOX_IMAGE    agent image directory (REQUIRED; without it the provider
                      reports unavailable so the run still proceeds)
"""

import os
import shutil
import time
from pathlib import Path

from ..errors import OperationFailed, Unsupported
from ..measure import kill_tree, popen, rss_bytes_pid, run
from .base import ExecResult, Handle, Provider

READY = "TBSANDBOX_BENCH_READY"
# Initial command run inside the guest once login is reached: announce ready,
# then hold the session (and thus the VM) open so it can be sampled/snapshotted.
HOLD_CMD = f'printf "%s\\n" {READY}; exec sleep 1000000'


class TbSandboxProvider(Provider):
    name = "tb-sandbox"
    kind = "microvm"

    def __init__(self, *, state_root=None):
        self.image = os.environ.get("TB_SANDBOX_IMAGE")
        self.repo_root = Path(__file__).resolve().parents[3]
        self.bin = self._resolve_bin()
        self.state_root = Path(state_root or "results/tb-sandbox-state").resolve()
        self.cache_dir = self.state_root / "cache"
        self._counter = 0
        self._live = []           # handles to kill on cleanup
        self._snap_state = {}     # snapshot id -> state dir it lives in
        self._snapshots = []

    # --- discovery ---------------------------------------------------------

    def _resolve_bin(self):
        explicit = os.environ.get("TB_SANDBOX_BIN")
        if explicit:
            return [explicit]
        for rel in ("target/release/tb-sandbox", "target/debug/tb-sandbox"):
            cand = self.repo_root / rel
            if cand.exists():
                return [str(cand)]
        if shutil.which("cargo"):
            return ["cargo", "run", "--release", "--quiet", "--"]
        return None

    def available(self):
        return bool(self.bin) and bool(self.image) and Path(self.image).exists()

    def version(self):
        return f"binary={self.bin[0]}" if self.bin else None

    def notes(self):
        return (
            "full Linux microVM via libkrun; VM held live by a background shell. "
            "exec currently cold-boots a fresh VM per call (no guest daemon yet), "
            "so exec_roundtrip includes a ~1.3s boot; snapshot --capture-memory "
            "captures vCPU+memory for resume-style restore"
        )

    # --- helpers -----------------------------------------------------------

    def _env(self, state_dir):
        return {
            "TB_SANDBOX_STATE_DIR": str(state_dir),
            "TB_SANDBOX_CACHE_DIR": str(self.cache_dir),
            "TB_SANDBOX_IMAGE": self.image,
        }

    def _new_state(self):
        self._counter += 1
        d = self.state_root / f"sb-{self._counter}-{int(time.monotonic()*1000)%100000}"
        (d / "cache").mkdir(parents=True, exist_ok=True)
        return d

    def _cli(self, args, state_dir, *, timeout):
        try:
            return run(self.bin + args, timeout=timeout, env=self._env(state_dir), check=True)
        except Exception as exc:
            out = getattr(exc, "output", None) or getattr(exc, "stdout", None)
            raise OperationFailed(f"tb-sandbox {args[0]} failed: {exc}",
                                  cmd=self.bin + args, output=out) from exc

    def _wait_ready(self, proc, log, state_dir, *, timeout=120):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise OperationFailed("tb-sandbox shell exited before ready",
                                      output=_tail(log))
            try:
                with open(log) as fh:
                    if READY in fh.read():
                        return self._find_sid(state_dir)
            except OSError:
                pass
            time.sleep(0.05)
        kill_tree(proc)
        raise OperationFailed("tb-sandbox boot timed out", output=_tail(log))

    @staticmethod
    def _find_sid(state_dir):
        boxes = Path(state_dir) / "sandboxes"
        ids = [p.name for p in boxes.iterdir()] if boxes.exists() else []
        if not ids:
            raise OperationFailed(f"no sandbox registered under {boxes}")
        return ids[0]  # isolated state dir => exactly one sandbox

    def _bg_shell(self, args, state_dir):
        log = str(state_dir / "shell.log")
        proc = popen(self.bin + args, env=self._env(state_dir), logfile=log)
        return proc, log

    # --- operations --------------------------------------------------------

    def launch(self, spec):
        state = self._new_state()
        args = [
            "shell", "--image", self.image,
            "--cpus", str(spec.cpus),
            "--memory-mib", str(spec.memory_mib),
            "--memory-max-mib", str(spec.memory_max_mib),
            "--ephemeral",
            "--initial-command", HOLD_CMD,
        ]
        if spec.workspace:
            args += ["--workspace", spec.workspace]
        args += spec.args_for(self.name)
        proc, log = self._bg_shell(args, state)
        sid = self._wait_ready(proc, log, state)
        handle = Handle(id=sid, extra={"proc": proc, "log": log, "state": state})
        self._live.append(handle)
        return handle

    def exec(self, handle, argv):
        # NB: cold-boots a transient VM (no daemon yet) — measured honestly.
        proc = self._cli(["exec", handle.id, "--"] + list(argv),
                         handle.extra["state"], timeout=120)
        return ExecResult(returncode=proc.returncode, stdout=proc.stdout)

    def snapshot(self, handle):
        # The VM is live (held by the background shell). --no-quiesce skips the
        # in-guest consistency handshake (the agent image here has no responding
        # quiesce daemon); --capture-memory pauses vCPUs and writes memory.bin.
        try:
            proc = self._cli(["snapshot", handle.id, "--capture-memory", "--no-quiesce"],
                             handle.extra["state"], timeout=300)
        except OperationFailed as exc:
            out = (exc.output or "") + str(exc)
            # Memory capture depends on libkrun's `snapshot-memory` runtime
            # command (Phase 3). Builds without it can't resume from memory — a
            # genuine capability gap in this build, not a measurement failure.
            if "snapshot-memory" in out or "quiesce" in out:
                raise Unsupported(
                    "memory snapshot unavailable in this build "
                    "(libkrun `snapshot-memory` command missing)") from exc
            raise
        snap = proc.stdout.strip().splitlines()[0].strip()
        self._snap_state[snap] = handle.extra["state"]
        self._snapshots.append(snap)
        return snap

    def restore(self, snapshot_id, spec):
        state = self._snap_state.get(snapshot_id)
        if state is None:
            raise OperationFailed(f"unknown snapshot {snapshot_id}")
        # Provision the restore-target sandbox from the snapshot...
        proc = self._cli(["restore", snapshot_id], state, timeout=120)
        sid = None
        for line in proc.stdout.splitlines():
            if line.startswith("restored "):
                sid = line.split()[1]
                break
        if not sid:
            raise OperationFailed("could not parse restored id", output=proc.stdout)
        # ...then boot it. `start` replays the snapshot's stored initial command,
        # which re-emits the ready marker, so this times resume-to-ready.
        bproc, log = self._bg_shell(["start", sid], state)
        try:
            self._wait_ready(bproc, log, state)
        except OperationFailed as exc:
            kill_tree(bproc)
            raise Unsupported(f"restore-to-ready not measurable via CLI: {exc}")
        handle = Handle(id=sid, extra={"proc": bproc, "log": log, "state": state,
                                       "restored": True})
        self._live.append(handle)
        return handle

    def rss_bytes(self, handle):
        marker = Path(handle.extra["state"]) / "sandboxes" / handle.id / "running.json"
        if not marker.exists():
            return None
        try:
            import json
            pid = int(json.loads(marker.read_text())["pid"])
        except (ValueError, KeyError, TypeError, OSError, json.JSONDecodeError):
            return None
        # running.json's pid is the VMM process itself; sample it directly
        # (summing the subtree would double-count shared guest-RAM mappings).
        return rss_bytes_pid(pid)

    def destroy(self, handle):
        # Kill the holding shell (tears down the VM); delete the provisioned
        # sandbox. Keep the state dir — a later restore may still need its
        # snapshots; cleanup() removes everything at the end.
        kill_tree(handle.extra.get("proc"))
        if handle in self._live:
            self._live.remove(handle)
        try:
            self._cli(["delete", handle.id], handle.extra["state"], timeout=60)
        except Exception:
            pass

    def cleanup(self):
        for handle in list(self._live):
            kill_tree(handle.extra.get("proc"))
        self._live.clear()
        time.sleep(0.2)
        shutil.rmtree(self.state_root, ignore_errors=True)
        self._snap_state.clear()
        self._snapshots.clear()


def _tail(path, n=25):
    try:
        with open(path) as fh:
            return "".join(fh.readlines()[-n:])
    except OSError:
        return ""
