"""Adapter for a plain container runtime (docker or podman) as a baseline.

Containers are not VMs, so this is a *floor* reference, not a peer: it shows how
much of the cost is the VM boundary itself. Daytona's default runner and
OrbStack both ultimately run containers, so the container baseline frames those
comparisons too.

Persistent model: `run -d ... sleep infinity`, then `exec` into the live
container. Live snapshot/restore of a running container is not a first-class
operation (commit loses process state), so those are reported n/a.

Memory is read from the runtime's own accounting (`stats`), which on macOS is
in-VM container memory, not host RSS — labeled accordingly in the report so it
is never confused with the microVM host-RSS figures.
"""

import os
import time

from ..errors import OperationFailed
from ..measure import first_int, have, run
from .base import ExecResult, Handle, Provider


class ContainerProvider(Provider):
    kind = "container"

    def __init__(self, binary, image=None):
        self._bin = binary
        self.name = binary  # "docker" or "podman"
        self.image = image or os.environ.get(
            f"{binary.upper()}_IMAGE", os.environ.get("BENCH_CONTAINER_IMAGE", "debian:bookworm")
        )
        self._containers = []

    def available(self):
        if not have(self._bin):
            return False
        # `info` confirms the daemon/VM is actually up, not just the CLI present.
        try:
            run([self._bin, "info"], timeout=30, check=True)
            return True
        except Exception:
            return False

    def version(self):
        try:
            return run([self._bin, "--version"], timeout=30, check=False).stdout.strip()
        except Exception:
            return None

    def notes(self):
        return ("container baseline (not a VM); memory is runtime-reported "
                "container memory, not host RSS — not comparable to microVM RSS")

    def _name(self, spec):
        return f"bench-{spec.label}-{int(time.monotonic()*1000)%100000}"

    def launch(self, spec):
        name = self._name(spec)
        cmd = [
            self._bin, "run", "-d", "--name", name,
            "--cpus", str(spec.cpus),
            "--memory", f"{spec.memory_mib}m",
        ]
        if spec.workspace:
            cmd += ["-v", f"{spec.workspace}:/workspace"]
        cmd += [self.image, "sleep", "infinity"]
        try:
            run(cmd, timeout=300, check=True)
        except Exception as exc:
            raise OperationFailed(f"{self._bin} run failed: {exc}", cmd=cmd,
                                  output=getattr(exc, "output", None)) from exc
        self._containers.append(name)
        return Handle(id=name)

    def exec(self, handle, argv):
        cmd = [self._bin, "exec", handle.id] + list(argv)
        proc = run(cmd, timeout=120, check=False)
        return ExecResult(returncode=proc.returncode, stdout=proc.stdout)

    def rss_bytes(self, handle):
        # `stats` MemUsage like "12.3MiB / 2GiB"; parse the first figure.
        try:
            out = run(
                [self._bin, "stats", "--no-stream", "--format", "{{.MemUsage}}", handle.id],
                timeout=30, check=False,
            ).stdout.strip()
        except Exception:
            return None
        return _parse_mem(out)

    def destroy(self, handle):
        run([self._bin, "rm", "-f", handle.id], timeout=60, check=False)
        if handle.id in self._containers:
            self._containers.remove(handle.id)

    def cleanup(self):
        for name in list(self._containers):
            run([self._bin, "rm", "-f", name], timeout=60, check=False)


def _parse_mem(text):
    """Parse 'MemUsage' like '12.34MiB / 2GiB' into bytes (first figure)."""
    if not text:
        return None
    first = text.split("/")[0].strip()
    units = {"B": 1, "KIB": 1024, "MIB": 1024**2, "GIB": 1024**3,
             "KB": 1000, "MB": 1000**2, "GB": 1000**3}
    num = ""
    unit = ""
    for ch in first:
        if ch.isdigit() or ch == ".":
            num += ch
        elif not ch.isspace():
            unit += ch
    try:
        value = float(num)
    except ValueError:
        return None
    return int(value * units.get(unit.upper(), 1))


def DockerProvider():
    return ContainerProvider("docker")


def PodmanProvider():
    return ContainerProvider("podman")
