"""Adapter for smolvm.

smolvm does not have a CLI this harness can safely assume, so it is wired up as
an env-configurable template provider (see template.py). Set SMOLVM_BIN plus the
SMOLVM_LAUNCH / SMOLVM_EXEC / SMOLVM_DESTROY templates (and optionally
SMOLVM_SNAPSHOT / SMOLVM_RESTORE / SMOLVM_RSS) to make it runnable. Until then it
reports unavailable and the run skips it.
"""

from .template import CommandTemplateProvider

_DEFAULTS = {
    "BIN": "smolvm",
    # No launch/exec/destroy defaults on purpose: smolvm's surface is unverified,
    # so requiring explicit templates avoids silently measuring the wrong thing.
}


def SmolVmProvider():
    return CommandTemplateProvider("smolvm", "SMOLVM", kind="microvm", defaults=_DEFAULTS)
