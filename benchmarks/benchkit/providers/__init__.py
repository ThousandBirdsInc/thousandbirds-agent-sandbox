"""Provider registry and discovery.

``all_providers()`` instantiates every known adapter; ``available_providers()``
filters to the ones that can actually run on this host. Adding a provider means
writing an adapter module and appending its factory to ``_FACTORIES``.
"""

from .daytona import DaytonaProvider
from .docker import DockerProvider, PodmanProvider
from .krunvm import KrunvmProvider
from .orbstack import OrbStackProvider
from .smolvm import SmolVmProvider
from .tb_sandbox import TbSandboxProvider

# Order here is the default display order in reports: our system first, then the
# closest peers, then the container baselines.
_FACTORIES = [
    TbSandboxProvider,
    KrunvmProvider,
    SmolVmProvider,
    DaytonaProvider,
    OrbStackProvider,
    DockerProvider,
    PodmanProvider,
]


def all_providers():
    providers = []
    for factory in _FACTORIES:
        try:
            providers.append(factory())
        except Exception as exc:  # a broken adapter must not sink the whole run
            print(f"warning: provider {factory.__name__} failed to construct: {exc}")
    return providers


def available_providers(selected=None):
    """Available providers, optionally filtered to a set of names."""
    out = []
    for p in all_providers():
        if selected and p.name not in selected:
            continue
        try:
            ok = p.available()
        except Exception:
            ok = False
        if ok:
            out.append(p)
    return out


def provider_by_name(name):
    for p in all_providers():
        if p.name == name:
            return p
    return None
