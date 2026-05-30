"""A fully env-configurable provider for tools the harness can't hardcode.

Some targets (notably smolvm) don't have a CLI this harness can assume. Rather
than guess and ship something that silently measures the wrong thing, those
providers are expressed as *command templates* the operator supplies and
verifies. Each template is a shell command string with ``{placeholder}`` fields:

  {name}       a unique per-sandbox name the harness generates
  {image}      the configured base image
  {cpus}       vCPU count from the spec
  {mem}        memory MiB from the spec
  {workspace}  host workspace path (empty when none)
  {cmd}        the command to exec (exec template only)
  {snapshot}   snapshot id (restore template only)

Templates are read from env vars prefixed with the provider's slug, e.g. for
slug ``SMOLVM``:

  SMOLVM_BIN          required: the binary, used only for availability detection
  SMOLVM_IMAGE        base image
  SMOLVM_LAUNCH       launch command; blocks until the sandbox can run a command
  SMOLVM_EXEC         exec command (must include {name} and {cmd})
  SMOLVM_DESTROY      teardown command
  SMOLVM_SNAPSHOT     (optional) prints a snapshot id on its last stdout line
  SMOLVM_RESTORE      (optional) restore from {snapshot}; prints nothing required
  SMOLVM_RSS          (optional) prints host bytes for {name}
  SMOLVM_LAUNCH_ID_RE (optional) regex w/ one group to extract the id from
                      launch stdout; default uses {name}

If the required templates are unset, the provider reports unavailable and the
run skips it cleanly.
"""

import os
import re
import shlex
import time

from ..errors import OperationFailed, Unsupported
from ..measure import first_int, have, run
from .base import ExecResult, Handle, Provider


class CommandTemplateProvider(Provider):
    def __init__(self, name, slug, *, kind="vm", defaults=None):
        self.name = name
        self.kind = kind
        self._slug = slug
        self._defaults = defaults or {}
        self._launched = []

    def _t(self, key):
        return os.environ.get(f"{self._slug}_{key}", self._defaults.get(key))

    @property
    def _binary(self):
        return self._t("BIN")

    def available(self):
        return bool(self._binary) and have(self._binary) \
            and bool(self._t("LAUNCH")) and bool(self._t("EXEC")) and bool(self._t("DESTROY"))

    def version(self):
        if not (self._binary and have(self._binary)):
            return None
        for flag in ("--version", "version", "-v"):
            try:
                out = run([self._binary, flag], timeout=20, check=False).stdout.strip()
                if out:
                    return out.splitlines()[0]
            except Exception:
                continue
        return None

    def notes(self):
        return f"env-configured via {self._slug}_* templates; verify before trusting numbers"

    def _fill(self, template, spec, **extra):
        fields = {
            "image": self._t("IMAGE") or "",
            "cpus": str(spec.cpus),
            "mem": str(spec.memory_mib),
            "workspace": spec.workspace or "",
        }
        fields.update(extra)
        return shlex.split(template.format(**fields))

    def _name(self, spec):
        return f"bench-{spec.label}-{int(time.monotonic()*1000)%100000}"

    def launch(self, spec):
        name = self._name(spec)
        cmd = self._fill(self._t("LAUNCH"), spec, name=name)
        try:
            proc = run(cmd, timeout=600, check=True)
        except Exception as exc:
            raise OperationFailed(f"{self.name} launch failed: {exc}", cmd=cmd,
                                  output=getattr(exc, "output", None)) from exc
        ident = name
        id_re = self._t("LAUNCH_ID_RE")
        if id_re:
            m = re.search(id_re, proc.stdout)
            if m:
                ident = m.group(1)
        self._launched.append(ident)
        return Handle(id=ident)

    def exec(self, handle, argv):
        joined = " ".join(shlex.quote(a) for a in argv)
        cmd = self._fill(self._t("EXEC"), _SpecLess(), name=handle.id, cmd=joined)
        proc = run(cmd, timeout=120, check=False)
        return ExecResult(returncode=proc.returncode, stdout=proc.stdout)

    def snapshot(self, handle):
        tmpl = self._t("SNAPSHOT")
        if not tmpl:
            raise Unsupported(f"{self.name}: no SNAPSHOT template configured")
        cmd = self._fill(tmpl, _SpecLess(), name=handle.id)
        proc = run(cmd, timeout=300, check=True)
        lines = [l for l in proc.stdout.splitlines() if l.strip()]
        if not lines:
            raise OperationFailed("snapshot produced no id", output=proc.stdout)
        return lines[-1].strip()

    def restore(self, snapshot_id, spec):
        tmpl = self._t("RESTORE")
        if not tmpl:
            raise Unsupported(f"{self.name}: no RESTORE template configured")
        name = self._name(spec)
        cmd = self._fill(tmpl, spec, name=name, snapshot=snapshot_id)
        run(cmd, timeout=300, check=True)
        self._launched.append(name)
        return Handle(id=name)

    def rss_bytes(self, handle):
        tmpl = self._t("RSS")
        if not tmpl:
            return None
        try:
            out = run(self._fill(tmpl, _SpecLess(), name=handle.id),
                      timeout=30, check=False).stdout
        except Exception:
            return None
        return first_int(out)

    def destroy(self, handle):
        cmd = self._fill(self._t("DESTROY"), _SpecLess(), name=handle.id)
        run(cmd, timeout=120, check=False)
        if handle.id in self._launched:
            self._launched.remove(handle.id)

    def cleanup(self):
        tmpl = self._t("DESTROY")
        if not tmpl:
            return
        for name in list(self._launched):
            try:
                run(self._fill(tmpl, _SpecLess(), name=name), timeout=60, check=False)
            except Exception:
                pass


class _SpecLess:
    """A stand-in spec for templates that only need name/cmd/snapshot."""
    cpus = 1
    memory_mib = 0
    workspace = None
