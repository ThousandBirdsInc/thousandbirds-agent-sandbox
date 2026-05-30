"""Command-line interface for the benchmark harness."""

import argparse
import os
import sys
import time

from .config import RunConfig, SandboxSpec
from .providers import all_providers, available_providers
from .report import to_console, to_json, to_markdown
from .runner import run_suite
from .scenarios import SCENARIOS, default_scenarios


def _build_parser():
    p = argparse.ArgumentParser(
        prog="bench",
        description="Benchmark thousandbirds-agent-sandbox against alternative "
                    "sandbox providers (krunvm, smolvm, daytona, orbstack, containers).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "examples:\n"
            "  python bench.py list\n"
            "  python bench.py run --providers tb-sandbox,krunvm\n"
            "  python bench.py run --scenarios cold_start,idle_rss --reps 10\n"
            "  python bench.py run --all-scenarios --density 16\n"
        ),
    )
    sub = p.add_subparsers(dest="command", required=True)

    sub.add_parser("list", help="list providers and whether each is available")

    r = sub.add_parser("run", help="run the benchmark suite")
    r.add_argument("--providers", help="comma-separated provider names (default: all available)")
    r.add_argument("--scenarios", help="comma-separated scenario keys (default: the standard set)")
    r.add_argument("--all-scenarios", action="store_true", help="include opt-in scenarios (density)")
    r.add_argument("--reps", type=int, default=5, help="measured repetitions per scenario (default 5)")
    r.add_argument("--warmup", type=int, default=1, help="warmup repetitions, discarded (default 1)")
    r.add_argument("--settle", type=float, default=2.0, help="seconds to settle before RSS sampling")
    r.add_argument("--density", type=int, default=8, help="concurrent VMs for the density scenario")
    r.add_argument("--timeout", type=float, default=180.0, help="per-operation timeout seconds")
    r.add_argument("--cpus", type=int, default=1, help="vCPUs per sandbox")
    r.add_argument("--memory-mib", type=int, default=2048, help="memory MiB per sandbox")
    r.add_argument("--memory-max-mib", type=int, default=4096, help="resize ceiling MiB")
    r.add_argument("--workspace", help="host dir to mount as the guest workspace")
    r.add_argument("--out-dir", default="results", help="directory for JSON + Markdown output")
    r.add_argument("--tag", help="label appended to output filenames")
    r.add_argument("--quiet", action="store_true", help="suppress per-iteration progress")
    return p


def _cmd_list():
    print(f"{'provider':<14}{'kind':<11}{'available':<11}version")
    print("-" * 60)
    for prov in all_providers():
        try:
            avail = prov.available()
        except Exception:
            avail = False
        ver = ""
        if avail:
            try:
                ver = prov.version() or ""
            except Exception:
                ver = ""
        print(f"{prov.name:<14}{prov.kind:<11}{('yes' if avail else 'no'):<11}{ver}")
    print("\nUnavailable providers are skipped on `run`. See README.md for how to "
          "configure smolvm/daytona/orbstack.")


def _cmd_run(args):
    selected = set(args.providers.split(",")) if args.providers else None
    providers = available_providers(selected)
    if not providers:
        want = f" matching {sorted(selected)}" if selected else ""
        print(f"No available providers{want}. Run `python bench.py list` to see why.",
              file=sys.stderr)
        return 1

    if args.scenarios:
        keys = [k.strip() for k in args.scenarios.split(",")]
        unknown = [k for k in keys if k not in SCENARIOS]
        if unknown:
            print(f"unknown scenarios: {unknown}; valid: {list(SCENARIOS)}", file=sys.stderr)
            return 2
    else:
        keys = default_scenarios()
        if args.all_scenarios:
            keys = list(SCENARIOS.keys())

    spec = SandboxSpec(
        cpus=args.cpus,
        memory_mib=args.memory_mib,
        memory_max_mib=args.memory_max_mib,
        workspace=os.path.abspath(args.workspace) if args.workspace else None,
    )
    cfg = RunConfig(
        repetitions=args.reps,
        warmup=args.warmup,
        timeout_s=args.timeout,
        settle_s=args.settle,
        density_count=args.density,
    )

    print(f"Providers: {', '.join(p.name for p in providers)}")
    print(f"Scenarios: {', '.join(keys)}")
    document = run_suite(providers, keys, spec, cfg, verbose=not args.quiet)

    print("\n" + to_console(document))

    os.makedirs(args.out_dir, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S", time.localtime(document["started_unix"]))
    tag = f"-{args.tag}" if args.tag else ""
    base = os.path.join(args.out_dir, f"bench-{stamp}{tag}")
    with open(base + ".json", "w") as fh:
        fh.write(to_json(document))
    with open(base + ".md", "w") as fh:
        fh.write(to_markdown(document))
    print(f"\nWrote {base}.json and {base}.md")
    return 0


def main(argv=None):
    args = _build_parser().parse_args(argv)
    if args.command == "list":
        _cmd_list()
        return 0
    if args.command == "run":
        return _cmd_run(args)
    return 1


if __name__ == "__main__":
    sys.exit(main())
