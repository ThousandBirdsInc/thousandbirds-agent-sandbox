"""The provider adapter contract.

A provider implements a handful of operations against its own CLI/API. The
runner only ever calls these methods, so adding a new provider means writing
one subclass — no changes to scenarios, stats, or reporting.

Operation semantics (every provider maps these onto its own mechanism):

  launch(spec)        -> Handle   boot a sandbox to the point a command can run
  exec(handle, argv)  -> ExecResult   run argv in the running sandbox
  snapshot(handle)    -> str      capture state, return a snapshot id
  restore(snap, spec) -> Handle   recreate a running sandbox from a snapshot
  rss_bytes(handle)   -> int|None current host memory footprint
  destroy(handle)     -> None     stop and delete the sandbox
  cleanup()           -> None     drop any global/leaked state for this provider

A provider raises ``Unsupported`` from any operation it cannot perform (e.g. a
container runtime with no live snapshot). The runner records those cells as
"n/a" instead of failures.
"""

from dataclasses import dataclass, field

from ..errors import Unsupported


@dataclass
class Handle:
    """An opaque, provider-owned reference to one running sandbox.

    ``id`` is the provider's own identifier (sandbox id, machine name,
    container id). ``extra`` carries provider-private bookkeeping (a Popen
    object, a state dir, a pid) the adapter needs across operations.
    """

    id: str
    extra: dict = field(default_factory=dict)


@dataclass
class ExecResult:
    returncode: int
    stdout: str = ""


class Provider:
    """Base adapter. Subclasses set ``name``/``kind`` and override operations."""

    # Short identifier used in flags, tables, and JSON keys.
    name = "base"
    # One of "microvm", "vm", "container" — informational, shown in reports.
    kind = "unknown"

    def available(self):
        """True if this provider can run on the current host (tool installed)."""
        return False

    def version(self):
        """Best-effort version string for the report metadata, or None."""
        return None

    def notes(self):
        """Human-readable caveats about how the neutral spec was mapped."""
        return ""

    # --- lifecycle (override in subclasses) --------------------------------

    def prepare(self, spec):
        """One-time, *untimed* setup (image build/pull). Default: nothing."""

    def launch(self, spec):
        raise Unsupported(f"{self.name}: launch not implemented")

    def exec(self, handle, argv):
        raise Unsupported(f"{self.name}: exec not implemented")

    def snapshot(self, handle):
        raise Unsupported(f"{self.name}: snapshots not supported")

    def restore(self, snapshot_id, spec):
        raise Unsupported(f"{self.name}: restore not supported")

    def rss_bytes(self, handle):
        return None

    def destroy(self, handle):
        raise Unsupported(f"{self.name}: destroy not implemented")

    def cleanup(self):
        """Drop global state (leaked sandboxes from crashed runs). Default: none."""
