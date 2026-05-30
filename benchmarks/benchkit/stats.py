"""Summary statistics over a list of samples.

Samples are floats (milliseconds for timings, bytes for memory). We report a
small fixed set: count, min, max, mean, stdev, and p50/p90/p99. Percentiles use
nearest-rank on the sorted samples, which is stable for the small N a benchmark
run produces and needs no interpolation assumptions.
"""

import math
import statistics


def percentile(sorted_samples, pct):
    """Nearest-rank percentile of an already-sorted, non-empty list."""
    if not sorted_samples:
        raise ValueError("percentile of empty sample set")
    if pct <= 0:
        return sorted_samples[0]
    if pct >= 100:
        return sorted_samples[-1]
    # Nearest-rank: rank = ceil(pct/100 * N), 1-indexed.
    rank = math.ceil((pct / 100.0) * len(sorted_samples))
    return sorted_samples[max(0, rank - 1)]


def summarize(samples):
    """Return a dict of summary stats, or None if there are no samples."""
    clean = [float(s) for s in samples if s is not None]
    if not clean:
        return None
    ordered = sorted(clean)
    return {
        "count": len(ordered),
        "min": ordered[0],
        "max": ordered[-1],
        "mean": statistics.fmean(ordered),
        "stdev": statistics.stdev(ordered) if len(ordered) > 1 else 0.0,
        "p50": percentile(ordered, 50),
        "p90": percentile(ordered, 90),
        "p99": percentile(ordered, 99),
    }
