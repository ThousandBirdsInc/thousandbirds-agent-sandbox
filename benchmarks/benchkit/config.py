"""Specifications shared across providers.

A ``SandboxSpec`` is the provider-neutral description of the sandbox a scenario
wants. Each adapter maps these fields onto its own flags as best it can; fields
a provider cannot honor are ignored (and noted in the provider's metadata).
"""

from dataclasses import dataclass, field


@dataclass(frozen=True)
class SandboxSpec:
    """What a scenario asks a provider to launch.

    Kept deliberately small so every provider can map it. The intent is a
    minimal, comparable workload: a default Linux base, modest resources, no
    network, with a host workspace optionally mounted.
    """

    # A short label for the workload, used in handles/names.
    label: str = "bench"
    # vCPU count and memory ceiling. Providers clamp/ignore as needed.
    cpus: int = 1
    memory_mib: int = 2048
    # Resize headroom (only honored by providers that support ballooning,
    # e.g. tb-sandbox needs memory_max > memory for live resize / full snapshot).
    memory_max_mib: int = 4096
    # Optional host directory mounted into the guest workspace. None => skip.
    workspace: "str | None" = None
    # Extra provider-specific arguments, keyed by provider name. Lets a run
    # pass e.g. {"tb-sandbox": ["--gvproxy"]} without polluting the neutral spec.
    extra_args: dict = field(default_factory=dict)

    def args_for(self, provider_name):
        return list(self.extra_args.get(provider_name, []))


@dataclass(frozen=True)
class RunConfig:
    """How the runner should execute scenarios."""

    repetitions: int = 5
    warmup: int = 1
    # Per-operation timeout in seconds. A scenario that exceeds it is recorded
    # as a failure for that iteration and the lifecycle is torn down.
    timeout_s: float = 180.0
    # Seconds to let a freshly-booted guest settle before sampling idle RSS.
    settle_s: float = 2.0
    # Density scenario fan-out (concurrent sandboxes).
    density_count: int = 8
