"""Adapter for krunvm — the upstream libkrun microVM CLI.

krunvm is the closest peer to tb-sandbox: both run a libkrun microVM as a host
process, so their cold-boot and host-RSS numbers are directly comparable. It is
installed by `brew install krunvm` and backed by podman/buildah image storage.

krunvm has no daemon and no way to attach to an already-running microVM, so
each `krunvm start` is a fresh boot. We model launch as a *persistent* sleeper
VM (boot to a READY marker, then idle), which makes cold-boot and idle-RSS
directly comparable to tb-sandbox. exec/snapshot/restore are unsupported by the
tool and reported as n/a — that gap is itself a meaningful comparison result.
"""

import atexit
import os
import time

from ..errors import OperationFailed
from ..measure import have, kill_tree, popen, rss_bytes_tree, run
from .base import Handle, Provider

READY = "KRUNVM_BENCH_READY"


class KrunvmProvider(Provider):
    name = "krunvm"
    kind = "microvm"

    def __init__(self, *, image=None, workdir="results/krunvm"):
        # OCI image krunvm builds the microVM root from. Override with KRUNVM_IMAGE.
        self.image = image or os.environ.get("KRUNVM_IMAGE", "docker.io/library/debian:bookworm")
        self.workdir = workdir
        self._configs = []  # extra per-launch configs (deleted each cleanup)
        self._base = None   # reusable base config so launch times pure boot
        self._busy = set()  # base/unique names currently running
        self._atexit = False
        os.makedirs(workdir, exist_ok=True)

    def available(self):
        # krunvm needs podman/buildah present to manage image storage.
        return have("krunvm") and (have("podman") or have("buildah"))

    def version(self):
        if not have("krunvm"):
            return None
        try:
            return run(["krunvm", "--version"], timeout=30, check=False).stdout.strip()
        except Exception:
            return None

    def notes(self):
        return (
            "libkrun microVM, host process (RSS comparable to tb-sandbox); "
            "no exec-into-running-VM, no snapshot/restore — each start is a fresh boot"
        )

    def _name(self, spec):
        # krunvm names must be filesystem-safe and unique.
        return f"bench-{spec.label}-{int(time.monotonic()*1000)%100000}"

    def _create_config(self, name, spec):
        # `krunvm create` extracts/builds the microVM rootfs from the OCI image.
        # This is image preparation, not boot, so it is done once in prepare()
        # (and on-demand for extra concurrent VMs) — never inside the timed boot.
        create = [
            "krunvm", "create", self.image,
            "--name", name,
            "--cpus", str(spec.cpus),
            "--mem", str(spec.memory_mib),
        ]
        try:
            run(create, timeout=600, check=True)
        except Exception as exc:
            raise OperationFailed(f"krunvm create failed: {exc}", cmd=create,
                                  output=getattr(exc, "output", None)) from exc
        self._configs.append(name)
        return name

    def prepare(self, spec):
        # Pre-build a reusable base config so cold_start measures only boot,
        # comparably to tb-sandbox launching from a prebuilt image. The base is
        # tracked separately from per-launch configs so per-scenario cleanup
        # never deletes it; it is removed at process exit instead.
        self._base_name = f"bench-base-{spec.label}"
        if self._base is None:
            self._create_config(self._base_name, spec)
            self._configs.remove(self._base_name)  # base is not a disposable extra
            self._base = self._base_name
        if not self._atexit:
            atexit.register(self._final_cleanup)
            self._atexit = True

    def launch(self, spec):
        # Self-heal: if the base config went away, rebuild it (still untimed by
        # the boot scenarios only on the first launch after loss).
        if self._base is None or not self._config_exists(self._base):
            self.prepare(spec)
        # Reuse the prebuilt base config when it is free (the sequential
        # scenarios always destroy before the next launch); otherwise build an
        # extra config so concurrent launches (density) each get their own VM.
        if self._base and self._base not in self._busy:
            name = self._base
        else:
            name = self._create_config(self._name(spec), spec)
        self._busy.add(name)
        # Boot to a ready marker and idle, so the VM stays sample-able. krunvm
        # runs the entrypoint we pass; we print READY then block.
        log = os.path.join(self.workdir, f"{name}.log")
        cmd = ["krunvm", "start", name, "--", "/bin/sh", "-c",
               f"echo {READY}; exec sleep 100000"]
        proc = popen(cmd, logfile=log)
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise OperationFailed("krunvm start exited before ready",
                                      output=_tail(log))
            try:
                with open(log) as fh:
                    if READY in fh.read():
                        break
            except OSError:
                pass
            time.sleep(0.05)
        else:
            kill_tree(proc)
            self._busy.discard(name)
            raise OperationFailed("krunvm boot timed out", output=_tail(log))
        return Handle(id=name, extra={"proc": proc, "log": log, "base": name == self._base})

    def rss_bytes(self, handle):
        proc = handle.extra.get("proc")
        if not proc or proc.poll() is not None:
            return None
        # The `krunvm start` shell spawns the krun VMM child; sample the subtree.
        return rss_bytes_tree(proc.pid)

    def destroy(self, handle):
        kill_tree(handle.extra.get("proc"))
        self._busy.discard(handle.id)
        # Keep the reusable base config for the next launch; only delete the
        # extra per-launch configs created for concurrency.
        if not handle.extra.get("base"):
            run(["krunvm", "delete", handle.id], timeout=60, check=False)
            if handle.id in self._configs:
                self._configs.remove(handle.id)

    def _config_exists(self, name):
        try:
            out = run(["krunvm", "list"], timeout=30, check=False).stdout
            return name in out
        except Exception:
            return True  # assume present; launch will surface a real failure

    def cleanup(self):
        # Drop only the disposable per-launch configs; keep the reusable base.
        for name in list(self._configs):
            run(["krunvm", "delete", name], timeout=60, check=False)
            self._configs.remove(name)

    def _final_cleanup(self):
        self.cleanup()
        if self._base:
            run(["krunvm", "delete", self._base], timeout=60, check=False)
            self._base = None


def _tail(path, n=20):
    try:
        with open(path) as fh:
            return "".join(fh.readlines()[-n:])
    except OSError:
        return ""
