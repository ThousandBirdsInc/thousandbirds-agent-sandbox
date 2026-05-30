"""Execute scenarios across providers and assemble a results document."""

import os
import platform
import subprocess
import time

from .scenarios import SCENARIOS
from .stats import summarize


def _sysctl(key):
    try:
        out = subprocess.run(["sysctl", "-n", key], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, text=True, check=False).stdout.strip()
        return out or None
    except Exception:
        return None


def _host_info():
    """Host CPU/memory/model, resolved on both macOS and Linux."""
    info = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }
    if platform.system() == "Darwin":
        info["cpu_count"] = _sysctl("hw.ncpu")
        info["mem_bytes"] = _sysctl("hw.memsize")
        info["model"] = _sysctl("hw.model")
    else:  # Linux and friends
        info["cpu_count"] = str(os.cpu_count()) if os.cpu_count() else None
        try:
            with open("/proc/meminfo") as fh:
                for line in fh:
                    if line.startswith("MemTotal:"):
                        info["mem_bytes"] = str(int(line.split()[1]) * 1024)
                        break
        except OSError:
            pass
        try:
            with open("/proc/cpuinfo") as fh:
                for line in fh:
                    if line.lower().startswith("model name"):
                        info["model"] = line.split(":", 1)[1].strip()
                        break
        except OSError:
            pass
    # Drop unresolved keys so the report renders cleanly.
    return {k: v for k, v in info.items() if v}


def _git_commit():
    try:
        return subprocess.run(["git", "rev-parse", "--short", "HEAD"],
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              text=True, check=False).stdout.strip() or None
    except Exception:
        return None


def run_suite(providers, scenario_keys, spec, cfg, *, verbose=True):
    """Run ``scenario_keys`` across ``providers``; return a results document."""
    def log(msg):
        if verbose:
            print(msg, flush=True)

    document = {
        "schema": "benchkit/v1",
        "started_unix": int(time.time()),
        "host": _host_info(),
        "git_commit": _git_commit(),
        "config": {
            "repetitions": cfg.repetitions,
            "warmup": cfg.warmup,
            "timeout_s": cfg.timeout_s,
            "settle_s": cfg.settle_s,
            "density_count": cfg.density_count,
        },
        "spec": {
            "label": spec.label,
            "cpus": spec.cpus,
            "memory_mib": spec.memory_mib,
            "memory_max_mib": spec.memory_max_mib,
            "workspace": spec.workspace,
        },
        "providers": {},
        "scenarios": list(scenario_keys),
        "results": {},  # scenario_key -> provider_name -> result
    }

    # Provider metadata up front so the report can annotate measurement caveats.
    for p in providers:
        document["providers"][p.name] = {
            "kind": p.kind,
            "version": _try(p.version),
            "notes": _try(p.notes) or "",
        }

    # One-time, untimed setup per provider (image build/pull, base config).
    # A provider whose prepare fails is kept but its scenarios will surface the
    # error per-cell; we don't drop it silently.
    prepared = []
    for p in providers:
        try:
            p.prepare(spec)
            prepared.append(p)
        except Exception as exc:
            log(f"warning: {p.name} prepare failed: {exc}")
            document["providers"][p.name]["prepare_error"] = str(exc)
            prepared.append(p)
    providers = prepared

    for key in scenario_keys:
        fn, title, _ = SCENARIOS[key]
        document["results"][key] = {}
        log(f"\n=== {title} [{key}] ===")
        for p in providers:
            log(f"- {p.name}")
            t0 = time.monotonic()
            try:
                res = fn(p, spec, cfg, log)
            except Exception as exc:  # last-resort guard: never sink the suite
                res = {"unit": "ms", "samples": [], "status": "error",
                       "reason": f"unhandled: {exc}"}
            res["stats"] = summarize(res.get("samples"))
            res["wall_s"] = round(time.monotonic() - t0, 2)
            document["results"][key][p.name] = res
            _log_cell(log, res)
        # Best-effort global cleanup after each scenario.
        for p in providers:
            _try(p.cleanup)

    document["finished_unix"] = int(time.time())
    return document


def _try(fn):
    try:
        return fn()
    except Exception:
        return None


def _log_cell(log, res):
    st = res["stats"]
    if res["status"] == "skipped":
        log(f"    n/a — {res['reason']}")
    elif res["status"] == "error":
        log(f"    ERROR — {res['reason']}")
    elif st:
        if res["unit"] == "bytes":
            log(f"    p50 {st['p50']/1024/1024:.0f} MiB  (n={st['count']})")
        else:
            log(f"    p50 {st['p50']:.0f} ms  mean {st['mean']:.0f} ms  (n={st['count']})")
    else:
        log("    no samples")
