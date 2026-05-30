"""Adapter for OrbStack Linux machines (macOS).

OrbStack runs lightweight Linux machines inside a single shared host VM. Machine
create is fast and machines are persistent, so launch/exec/destroy map cleanly.
Two honest limitations are reported as n/a rather than faked:

  - No live snapshot/restore of a running machine's process+memory state.
  - All machines share one backing VM, so per-machine host RSS is not isolable;
    we report None for memory rather than a misleading shared figure.

CLI shape (orbctl/orb, OrbStack >= 1.x):
  orbctl create <distro> <name>
  orb -m <name> <cmd...>
  orbctl delete <name>
Override the distro with ORBSTACK_DISTRO (default: ubuntu).
"""

import os
import time

from ..errors import OperationFailed
from ..measure import have, run
from .base import ExecResult, Handle, Provider


class OrbStackProvider(Provider):
    name = "orbstack"
    kind = "vm"

    def __init__(self):
        self.distro = os.environ.get("ORBSTACK_DISTRO", "ubuntu")
        self._machines = []

    def _bin(self):
        return "orbctl" if have("orbctl") else ("orb" if have("orb") else None)

    def available(self):
        if not self._bin():
            return False
        # `orb status` / `orbctl list` confirms the engine is running.
        try:
            run([self._bin(), "list"], timeout=30, check=False)
            return True
        except Exception:
            return False

    def version(self):
        b = self._bin()
        if not b:
            return None
        try:
            return run([b, "version"], timeout=20, check=False).stdout.strip().splitlines()[0]
        except Exception:
            return None

    def notes(self):
        return ("Linux machine in OrbStack's shared VM; no live snapshot; "
                "per-machine host RSS not isolable (shared VM) → memory n/a")

    def _name(self, spec):
        return f"bench-{spec.label}-{int(time.monotonic()*1000)%100000}"

    def launch(self, spec):
        name = self._name(spec)
        cmd = [self._bin(), "create", self.distro, name]
        try:
            run(cmd, timeout=300, check=True)
        except Exception as exc:
            raise OperationFailed(f"orbstack create failed: {exc}", cmd=cmd,
                                  output=getattr(exc, "output", None)) from exc
        self._machines.append(name)
        return Handle(id=name)

    def exec(self, handle, argv):
        # `orb -m <name> -- <cmd>`: machines run commands without a separate boot.
        cmd = ["orb", "-m", handle.id] + list(argv)
        proc = run(cmd, timeout=120, check=False)
        return ExecResult(returncode=proc.returncode, stdout=proc.stdout)

    def destroy(self, handle):
        run([self._bin(), "delete", handle.id], timeout=60, check=False)
        if handle.id in self._machines:
            self._machines.remove(handle.id)

    def cleanup(self):
        for name in list(self._machines):
            run([self._bin(), "delete", name], timeout=60, check=False)
