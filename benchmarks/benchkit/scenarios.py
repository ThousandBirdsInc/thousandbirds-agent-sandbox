"""Provider-neutral benchmark scenarios.

Each scenario times one operation across a clean lifecycle and returns a result
dict. Scenarios own their lifecycle and always tear down what they create, even
on failure, so a crashed iteration never leaks a sandbox into the next one.

A scenario returns:
  {"unit": "ms"|"bytes", "samples": [...], "status": "ok"|"skipped"|"error",
   "reason": str|None}

``status`` is "skipped" when the provider raises Unsupported (capability gap,
reported as n/a) and "error" when an operation it claims to support fails.
"""

import time

from .errors import OperationFailed, Unsupported
from .measure import Timer


def _safe_destroy(provider, handle):
    if handle is None:
        return
    try:
        provider.destroy(handle)
    except Exception:
        pass


def _result(unit, samples=None, status="ok", reason=None):
    return {"unit": unit, "samples": samples or [], "status": status, "reason": reason}


# --- scenarios -------------------------------------------------------------


def cold_start(provider, spec, cfg, log):
    """Wall-clock from nothing to a sandbox that can run a command."""
    samples = []
    try:
        for i in range(cfg.warmup + cfg.repetitions):
            handle = None
            try:
                with Timer() as t:
                    handle = provider.launch(spec)
                if i >= cfg.warmup:
                    samples.append(t.ms)
                    log(f"  cold_start #{i - cfg.warmup + 1}: {t.ms:.0f} ms")
            finally:
                _safe_destroy(provider, handle)
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", samples, status="error", reason=str(exc))
    return _result("ms", samples)


def exec_roundtrip(provider, spec, cfg, log):
    """Time to run a trivial command in (or as) the sandbox.

    For persistent providers this is the exec attach overhead; for one-shot
    providers it legitimately includes a boot, which the report flags via the
    provider's notes.
    """
    handle = None
    try:
        handle = provider.launch(spec)
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", status="error", reason=f"launch: {exc}")
    samples = []
    try:
        for i in range(cfg.warmup + cfg.repetitions):
            with Timer() as t:
                res = provider.exec(handle, ["true"])
            if res.returncode != 0:
                return _result("ms", samples, status="error",
                               reason=f"exec returned {res.returncode}")
            if i >= cfg.warmup:
                samples.append(t.ms)
        log(f"  exec_roundtrip: {len(samples)} samples")
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", samples, status="error", reason=str(exc))
    finally:
        _safe_destroy(provider, handle)
    return _result("ms", samples)


def snapshot_create(provider, spec, cfg, log):
    """Time to capture a snapshot (memory + disk where supported)."""
    handle = None
    try:
        handle = provider.launch(spec)
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", status="error", reason=f"launch: {exc}")
    samples = []
    try:
        for i in range(cfg.warmup + cfg.repetitions):
            with Timer() as t:
                provider.snapshot(handle)
            if i >= cfg.warmup:
                samples.append(t.ms)
                log(f"  snapshot_create #{i - cfg.warmup + 1}: {t.ms:.0f} ms")
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", samples, status="error", reason=str(exc))
    finally:
        _safe_destroy(provider, handle)
    return _result("ms", samples)


def restore(provider, spec, cfg, log):
    """Time to bring up a running sandbox from a snapshot (resume)."""
    handle = None
    snap = None
    try:
        handle = provider.launch(spec)
        snap = provider.snapshot(handle)
    except Unsupported as exc:
        _safe_destroy(provider, handle)
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        _safe_destroy(provider, handle)
        return _result("ms", status="error", reason=f"setup: {exc}")
    # The source sandbox isn't needed once a snapshot exists.
    _safe_destroy(provider, handle)
    samples = []
    try:
        for i in range(cfg.warmup + cfg.repetitions):
            restored = None
            try:
                with Timer() as t:
                    restored = provider.restore(snap, spec)
                if i >= cfg.warmup:
                    samples.append(t.ms)
                    log(f"  restore #{i - cfg.warmup + 1}: {t.ms:.0f} ms")
            finally:
                _safe_destroy(provider, restored)
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", samples, status="error", reason=str(exc))
    return _result("ms", samples)


def idle_rss(provider, spec, cfg, log):
    """Host memory footprint (bytes) of one idle, booted sandbox.

    Reported in bytes; the report converts to MiB. Providers that cannot
    isolate per-sandbox host memory (shared backing VM) return None and the
    cell shows n/a.
    """
    samples = []
    try:
        for i in range(cfg.repetitions):
            handle = None
            try:
                handle = provider.launch(spec)
                time.sleep(cfg.settle_s)
                rss = provider.rss_bytes(handle)
                if rss is None:
                    return _result("bytes", status="skipped",
                                   reason="provider cannot isolate per-sandbox host memory")
                samples.append(float(rss))
                log(f"  idle_rss #{i+1}: {rss/1024/1024:.0f} MiB")
            finally:
                _safe_destroy(provider, handle)
    except Unsupported as exc:
        return _result("bytes", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("bytes", samples, status="error", reason=str(exc))
    return _result("bytes", samples)


def teardown(provider, spec, cfg, log):
    """Time to stop and delete a running sandbox."""
    samples = []
    try:
        for i in range(cfg.warmup + cfg.repetitions):
            handle = provider.launch(spec)
            try:
                with Timer() as t:
                    provider.destroy(handle)
                handle = None
                if i >= cfg.warmup:
                    samples.append(t.ms)
            finally:
                _safe_destroy(provider, handle)
    except Unsupported as exc:
        return _result("ms", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("ms", samples, status="error", reason=str(exc))
    return _result("ms", samples)


def density(provider, spec, cfg, log):
    """Aggregate host memory (bytes) for N concurrent idle sandboxes.

    Surfaces how footprint scales with packing. Like idle_rss it needs isolable
    host memory; providers without it are skipped.
    """
    handles = []
    try:
        for i in range(cfg.density_count):
            handles.append(provider.launch(spec))
            log(f"  density: launched {i+1}/{cfg.density_count}")
        time.sleep(cfg.settle_s)
        total = 0
        counted = 0
        for h in handles:
            rss = provider.rss_bytes(h)
            if rss is not None:
                total += rss
                counted += 1
        if counted == 0:
            return _result("bytes", status="skipped",
                           reason="provider cannot isolate per-sandbox host memory")
        log(f"  density: {counted} VMs, {total/1024/1024:.0f} MiB total")
        # Single aggregate sample; report annotates with the VM count.
        return {"unit": "bytes", "samples": [float(total)], "status": "ok",
                "reason": None, "vm_count": counted}
    except Unsupported as exc:
        return _result("bytes", status="skipped", reason=str(exc))
    except OperationFailed as exc:
        return _result("bytes", status="error", reason=str(exc))
    finally:
        for h in handles:
            _safe_destroy(provider, h)


# Registry: key -> (function, title, default-on). Density is opt-in (heavy).
SCENARIOS = {
    "cold_start": (cold_start, "Cold start → ready", True),
    "exec_roundtrip": (exec_roundtrip, "Exec round-trip (trivial cmd)", True),
    "snapshot_create": (snapshot_create, "Snapshot create", True),
    "restore": (restore, "Restore (resume from snapshot)", True),
    "idle_rss": (idle_rss, "Idle host memory footprint", True),
    "teardown": (teardown, "Teardown", True),
    "density": (density, "Density (N concurrent VMs, total RSS)", False),
}


def default_scenarios():
    return [k for k, (_, _, on) in SCENARIOS.items() if on]
