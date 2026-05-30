"""Render a results document as a console table, JSON, and Markdown.

The comparison table puts scenarios in rows and providers in columns, with the
headline statistic per cell (p50 latency, or median MiB for memory scenarios).
Cells are "n/a" for unsupported capabilities and "err" for runtime failures, so
a capability gap reads differently from a broken measurement.
"""

import json

from .scenarios import SCENARIOS


def _cell(res):
    if res is None:
        return "-"
    status = res.get("status")
    if status == "skipped":
        return "n/a"
    if status == "error":
        return "err"
    st = res.get("stats")
    if not st:
        return "-"
    if res.get("unit") == "bytes":
        val = f"{st['p50']/1024/1024:.0f} MiB"
        if res.get("vm_count"):
            val += f"/{res['vm_count']}vm"
        return val
    return f"{st['p50']:.0f} ms"


def _provider_order(document):
    # Keep registry order but only include providers that ran.
    return list(document["providers"].keys())


def to_console(document):
    providers = _provider_order(document)
    lines = []
    host = document["host"]
    lines.append("Sandbox benchmark — provider comparison")
    lines.append(f"host: {host.get('model', host.get('platform'))} "
                 f"({host.get('cpu_count','?')} cpu, "
                 f"{_mib(host.get('mem_bytes'))} RAM)")
    if document.get("git_commit"):
        lines.append(f"commit: {document['git_commit']}")
    sp = document["spec"]
    lines.append(f"spec: {sp['cpus']} vCPU, {sp['memory_mib']} MiB, "
                 f"reps={document['config']['repetitions']} "
                 f"warmup={document['config']['warmup']}")
    lines.append("")

    # Column widths.
    label_w = max(len(SCENARIOS[k][1]) for k in document["scenarios"]) + 1
    col_w = max(12, max((len(p) for p in providers), default=8) + 1)
    header = "scenario".ljust(label_w) + "".join(p.ljust(col_w) for p in providers)
    lines.append(header)
    lines.append("-" * len(header))
    for key in document["scenarios"]:
        title = SCENARIOS[key][1]
        row = title.ljust(label_w)
        for p in providers:
            row += _cell(document["results"][key].get(p)).ljust(col_w)
        lines.append(row)
    lines.append("")
    lines.append("legend: p50 latency, or median host MiB for memory rows; "
                 "n/a = unsupported, err = failed, - = no data")
    lines.append("notes:")
    for p in providers:
        meta = document["providers"][p]
        note = meta.get("notes") or ""
        lines.append(f"  {p} ({meta.get('kind')}): {note}")
    return "\n".join(lines)


def to_markdown(document):
    providers = _provider_order(document)
    out = []
    out.append("# Sandbox benchmark — provider comparison\n")
    host = document["host"]
    out.append(f"- **Host:** {host.get('model', host.get('platform'))} — "
               f"{host.get('cpu_count','?')} CPU, {_mib(host.get('mem_bytes'))} RAM")
    out.append(f"- **Platform:** {host.get('platform')}")
    if document.get("git_commit"):
        out.append(f"- **Commit:** `{document['git_commit']}`")
    sp = document["spec"]
    cfg = document["config"]
    out.append(f"- **Spec:** {sp['cpus']} vCPU, {sp['memory_mib']} MiB "
               f"(max {sp['memory_max_mib']} MiB)")
    out.append(f"- **Runs:** {cfg['repetitions']} measured + {cfg['warmup']} warmup\n")

    out.append("## Results (headline p50)\n")
    out.append("| Scenario | " + " | ".join(providers) + " |")
    out.append("|" + "---|" * (len(providers) + 1))
    for key in document["scenarios"]:
        title = SCENARIOS[key][1]
        cells = [_cell(document["results"][key].get(p)) for p in providers]
        out.append(f"| {title} | " + " | ".join(cells) + " |")
    out.append("")
    out.append("_p50 latency; memory rows show median host RSS in MiB. "
               "`n/a` = capability not supported, `err` = measurement failed._\n")

    out.append("## Full statistics\n")
    for key in document["scenarios"]:
        out.append(f"### {SCENARIOS[key][1]}\n")
        out.append("| Provider | status | n | min | p50 | p90 | p99 | max | mean | stdev |")
        out.append("|---|---|---|---|---|---|---|---|---|---|")
        for p in providers:
            res = document["results"][key].get(p)
            out.append("| " + " | ".join(_stat_row(p, res)) + " |")
        out.append("")

    out.append("## Provider notes\n")
    for p in providers:
        meta = document["providers"][p]
        out.append(f"- **{p}** ({meta.get('kind')}, "
                   f"version `{meta.get('version') or 'unknown'}`): {meta.get('notes')}")
    return "\n".join(out)


def _stat_row(name, res):
    if res is None:
        return [name, "-", "", "", "", "", "", "", "", ""]
    status = res.get("status")
    st = res.get("stats")
    if status != "ok" or not st:
        reason = (res.get("reason") or status or "-").replace("|", "/")[:60]
        # name | status | n=0 | reason | (pad remaining columns)
        return [name, status or "-", "0", reason, "", "", "", "", "", ""]
    fmt = (lambda v: f"{v/1024/1024:.0f}MiB") if res.get("unit") == "bytes" else (lambda v: f"{v:.0f}")
    return [name, "ok", str(st["count"]),
            fmt(st["min"]), fmt(st["p50"]), fmt(st["p90"]), fmt(st["p99"]),
            fmt(st["max"]), fmt(st["mean"]), fmt(st["stdev"])]


def to_json(document):
    return json.dumps(document, indent=2, sort_keys=False)


def _mib(byte_str):
    try:
        gb = int(byte_str) / 1024 / 1024 / 1024
        return f"{gb:.0f} GiB"
    except (TypeError, ValueError):
        return "? RAM"
