"""benchkit: a provider-neutral sandbox benchmarking harness.

The harness defines a small set of provider-neutral *operations* (launch to
ready, exec a command, snapshot, restore, sample host memory, destroy) and a
set of *scenarios* that time those operations. Each sandbox provider implements
the operations through its own CLI/API behind a common adapter; the runner
executes scenarios across all available providers and emits comparable stats.

Nothing here imports a third-party package: the harness is stdlib-only so it
runs on any machine that has the providers installed.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
