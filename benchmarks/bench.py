#!/usr/bin/env python3
"""Entry point for the sandbox benchmark harness.

Run from the benchmarks/ directory:

    python bench.py list
    python bench.py run --providers tb-sandbox,krunvm

Stdlib only — no pip install needed. See README.md for setup and provider
configuration.
"""

import sys

from benchkit.cli import main

if __name__ == "__main__":
    sys.exit(main())
