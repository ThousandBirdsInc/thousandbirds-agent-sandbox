# Sandbox benchmarks

A provider-neutral harness for comparing `thousandbirds-agent-sandbox`
(`tb-sandbox`) against alternative sandbox/VM providers — **krunvm**, **smolvm**,
**Daytona**, **OrbStack**, and plain container runtimes (Docker/Podman) as a
baseline.

It measures the things that actually differentiate an agent-sandbox substrate:
how fast a sandbox is ready, how cheap it is to run a command in, whether it can
snapshot and resume, its idle host-memory footprint, and how that footprint
scales when you pack many on one host.

Everything here is **stdlib-only Python** — no `pip install`. It shells out to
each provider's own CLI behind a common adapter, so it runs anywhere the
providers are installed.

## Quick start

```sh
cd benchmarks

# What can run on this machine right now?
python3 bench.py list

# Run the standard suite across every available provider:
python3 bench.py run

# Focus on the microVM peers and bump repetitions:
python3 bench.py run --providers tb-sandbox,krunvm --reps 10

# Just cold start + memory, with the density scenario enabled:
python3 bench.py run --scenarios cold_start,idle_rss,density --all-scenarios --density 16
```

Each run prints a comparison table and writes `results/bench-<timestamp>.json`
(full sample data) and `results/bench-<timestamp>.md` (a shareable report).

## Latest results

Recorded 2026-05-29/30. p50 latency over 5 measured reps + 1 warmup; memory rows
are median host RSS (microVMs, single-VMM-process `ps` RSS) or runtime-reported
container memory (containers — a floor, **not** comparable to VM RSS). Spec:
1 vCPU / 2048 MiB. Base image `debian:bookworm` for krunvm/docker/podman;
tb-sandbox uses its `debian-bookworm` agent image. `n/a` = capability the
provider does not have in the tested build.

### Head-to-head comparison

All providers across both machines, key metrics (p50). See per-machine sections
and footnotes below for the caveats — especially the idle-memory note (⁵), which
is essential for reading the microVM memory numbers fairly.

| Provider (host) | Kind | Cold start | Exec | Idle mem | Snapshot/restore |
|---|---|---|---|---|---|
| **tb-sandbox** (Mac · HVF) | microVM | 1199 ms | 1323 ms¹ | 2597 MiB⁵ | n/a³ |
| **tb-sandbox** (cloud · KVM) | microVM | 4474 ms⁷ | —⁸ | 222 MiB⁵ | n/a³ |
| krunvm (Mac · HVF) | microVM | 323 ms | n/a² | 157 MiB⁵ | n/a² |
| krunvm (cloud · KVM) | microVM | 757 ms | n/a² | 85 MiB⁵ | n/a² |
| docker (Mac Desktop) | container | variable⁴ | 139 ms | ~1 MiB | n/a |
| docker (cloud) | container | 199 ms | 57 ms | ~0 MiB | n/a |
| podman (cloud) | container | 231 ms | 232 ms | ~0 MiB | n/a |

### Local — Apple Silicon Mac (libkrun on HVF)

**Host:** `Mac17,7`, Apple Silicon (arm64), 18 CPU, 128 GiB RAM, macOS 26.4.1.

| Scenario | tb-sandbox | krunvm | docker |
|---|---|---|---|
| Cold start → ready | 1199 ms | 323 ms | variable⁴ |
| Exec round-trip | 1323 ms¹ | n/a² | 139 ms |
| Snapshot create | n/a³ | n/a² | n/a |
| Restore (resume) | n/a³ | n/a² | n/a |
| Idle host memory | 2597 MiB⁵ | 157 MiB⁵ | ~1 MiB (container) |
| Teardown | 58 ms | 55 ms | variable⁴ |

### Cloud — GCP `n2-standard-4`, nested virtualization (libkrun on KVM)

**Host:** Ubuntu 22.04.5 LTS, x86_64, 4 vCPU, 15 GiB RAM, kernel 6.8.0-1058-gcp,
`/dev/kvm` **present** (created with `--enable-nested-virtualization`). Instance
`nested-kvm-bench`, zone `us-central1-a`. krunvm here is libkrunfw + libkrun +
krunvm built from source on the VM.

| Scenario | tb-sandbox | krunvm | docker | podman |
|---|---|---|---|---|
| Cold start → ready | 4474 ms⁷ | 757 ms | 199 ms | 231 ms |
| Exec round-trip | —⁸ | n/a² | 57 ms | 232 ms |
| Snapshot create | n/a³ | n/a² | n/a | n/a |
| Restore (resume) | n/a³ | n/a² | n/a | n/a |
| Idle host memory | 222 MiB⁵ | 85 MiB⁵ | ~0 MiB (container) | ~0 MiB (container) |
| Teardown | 456 ms⁹ | 50 ms | 133 ms | 10395 ms⁶ |

tb-sandbox here is the full stack — the `thousandbirds-libkrun` fork's libkrun + the
`os_mode` launcher + the `tb-sandbox` wrapper — **built from source on the VM**
and booting a from-scratch **x86_64** agent image. See "Cloud tb-sandbox port"
below for what that took.

**Notes**

1. **tb-sandbox `exec` cold-boots a fresh VM (~1.3 s) per call** — there is no
   in-guest daemon yet (that is the planned Phase 1 work), so `exec_roundtrip`
   measures a full boot, not attach overhead. `cold_start` (1199 ms) is measured
   by holding a VM open with a background `shell` to a ready marker.
2. **krunvm** has no daemon to attach to and no snapshot, so
   exec/snapshot/restore are genuine capability gaps (`n/a`). Each `krunvm start`
   is a fresh boot.
3. **tb-sandbox snapshot/restore are `n/a` in the tested build**: memory capture
   needs libkrun's `snapshot-memory` runtime command (the Phase 3 feature),
   which the locally-built launcher rejected as an unknown command. The harness
   issues `snapshot --capture-memory --no-quiesce`; wire up the Phase 3 libkrun
   build and these cells will populate.
4. **docker on macOS (Docker Desktop) container-start was pathologically
   variable** this session: `docker run` cold-start ranged from ~0.2 s (alpine,
   Docker idle) to 34–54 s (debian, after the microVM runs starved the LinuxKit
   VM), while `exec` (57 ms) and teardown (~140 ms) stayed fast — so it is the
   container *start* path that degraded, not the daemon. A single cold-start
   number would mislead, so it is left as "variable"; use the **cloud** container
   rows for a clean container baseline.
5. **Idle-memory caveat — read before comparing.** tb-sandbox's 2597 MiB is a
   *full systemd agent OS* that boots services and touches ~all of the 2048 MiB
   guest RAM; krunvm's 157 MiB (Mac) / 85 MiB (cloud) runs a *bare `sleep` as
   PID 1* in a minimal rootfs that touches almost nothing. The difference is the
   **guest workload/OS, not VMM efficiency**. Balloon-based reclaim (off by
   default) would shed unused pages, and on macOS `ps` RSS is a high-water figure
   that ignores `MADV_FREE_REUSABLE` — measure live working set with `vmmap`.
   Treat these as footprint *ceilings* for very different guests. The tb-sandbox
   Mac (2597 MiB) vs cloud (222 MiB) gap is **not** apples-to-apples either: the
   Mac figure is a high-water `ps` RSS of a fully-exercised guest (agent shell
   session live via the serial-pty), while the cloud figure is the demand-paged
   idle floor right after boot (serial-pty disabled on x86, so the guest sits idle
   at multi-user). The cloud 222 MiB is the cleaner "idle microVM" number and sits
   in the same range as krunvm's 85–157 MiB.
6. **podman** teardown reflects its default 10 s stop-grace before `SIGKILL` — a
   real runtime characteristic, not a harness stall.
7. **tb-sandbox cloud cold-start (4474 ms)** is measured with the sandbox's
   root disk on a **RAM-backed tmpfs**. This is deliberate: on the GCE nested-virt
   instance, the *guest's* virtio-blk reads against the host's virtual persistent
   disk were pathologically variable — identical boots reached "ready" at guest
   uptime ranging from **~0.9 s to ~116 s**, with the entire stall *before* the
   ext4 root mount (double-virtualized block I/O: guest → nested KVM → GCE virtual
   disk). With the root disk on tmpfs, boot-to-ready is a stable ~0.9 s of guest
   time, and the full wrapper cold-start settles at p50 4474 ms (one 10 s outlier
   in 5 reps from residual I/O variance). The number reflects libkrun/KVM boot +
   the wrapper's per-launch 4 GB disk **copy** (Linux has no APFS instant-clone),
   *not* the persistent-disk pathology — which is an environment artifact, not a
   tb-sandbox property. The Mac (1199 ms) is faster: APFS clone is instant and the
   cores are faster.
8. **Exec was not measured on the cloud** — it uses the same daemon-less
   cold-boot-per-call path as the Mac (note 1); the cold-start figure already
   captures that boot cost.
9. **Cloud teardown (456 ms)** includes deleting the 4 GB tmpfs root-disk copy
   (the harness frees each clone on teardown to fit tmpfs); the Mac figure does
   not, so the two teardown numbers are not directly comparable.

**Cross-machine note (krunvm, same tool both hosts):** cold boot 323 ms on the
Mac (arm64/HVF, 18 fast cores) vs 757 ms on the cloud VM (x86 KVM *nested* inside
GCE, 4 vCPU) — nested virt + fewer/slower vCPUs cost ~2.3×. Idle footprint was
lower on the cloud (85 vs 157 MiB).

### Cloud tb-sandbox port

tb-sandbox had only ever run on arm64/macOS (HVF). Bringing it up on the x86_64
KVM VM meant building the whole stack from source there and fixing a series of
Linux/x86-only issues. Recorded here because each is a real gap, not a benchmark
artifact:

- **Fork libkrun didn't compile on Linux.** The in-progress snapshot code in
  `src/vmm/src/linux/vstate.rs` (only compiled on the Linux target, never on the
  Mac) had two bugs: `Result<…, SnapshotError>` shadowed by the module's 1-arg
  `Result` alias, and `read_pod<T: Copy>` rejecting `kvm_xsave` (not `Copy` in
  current kvm-bindings). Patched (snapshot-path only; benchmark never exercises it).
- **`os_mode.c` PTY EFAULT.** `posix_openpt`/`grantpt`/`ptsname` need `_GNU_SOURCE`
  on glibc; without it they're implicitly declared as returning `int`, truncating
  the 64-bit pointer → `open()` EFAULT. macOS declares them by default, hiding it.
  The Linux CI compiles os_mode but defaults `RUN_KVM_SMOKE=0`, so it never booted.
  Fixed with `-D_GNU_SOURCE`.
- **x86 console is `hvc0`, not `ttyS0`.** libkrun on x86 exposes a virtio-console
  (`hvc0`); the image-build hardcodes `console=ttyS0` for `linux/amd64`, so the
  guest produced zero output. `--serial-pty` (an arm/Mac serial device) has no
  working x86 equivalent. Fixed by setting the image's console + kernel cmdline to
  `hvc0` and making `--serial-pty` skippable (`TB_SANDBOX_NO_SERIAL_PTY`); the
  guest's own `KRUN_OSMODE: ready` marker on hvc0 is used for readiness.
- **Py3.10 in the rootfs builder.** `os_mode_build_container_rootfs.py` imports
  `datetime.UTC` (3.11+); the VM ships Python 3.10. Shimmed.
- **No APFS / nested-virt disk I/O** — see notes 5 and 7: the wrapper copies the
  full root disk per launch (no reflink on ext4), and nested virtio-blk against the
  GCE virtual disk is wildly variable; the cloud number uses a tmpfs root disk to
  isolate the microVM cost.

Reproduce: build libkrunfw + the fork's libkrun + `os_mode` + `tb-sandbox` on the
VM, build the agent image with `KERNEL=<libkrunfw vmlinux> NO_INITRAMFS=1
PLATFORM=linux/amd64`, set the image console to `hvc0`, then run the harness with
`TB_SANDBOX_NO_SERIAL_PTY=1`, `TB_SANDBOX_READY_MARKER='KRUN_OSMODE: ready'`, and a
tmpfs `TB_SANDBOX_BENCH_STATE_ROOT`.

**First cloud attempt had no KVM.** The initial instance
(`instance-20260529-030451`, Debian 12 **arm64**, 2 vCPU) had **no `/dev/kvm`** —
no nested virtualization — so no VM provider could run there, only containers.
`nested-kvm-bench` was created with `--enable-nested-virtualization` to fix that.

## What it measures

| Scenario | Unit | What it captures |
|---|---|---|
| `cold_start` | ms | Wall-clock from nothing to a sandbox that can run a command (the headline boot number). |
| `exec_roundtrip` | ms | Time to run a trivial command in/against the sandbox. For persistent providers this is exec attach overhead; for one-shot providers it includes a boot (flagged in notes). |
| `snapshot_create` | ms | Time to capture a snapshot (memory + disk where supported). |
| `restore` | ms | Time to bring a running sandbox up from a snapshot — resume, not cold boot. |
| `idle_rss` | MiB | Host memory footprint of one idle, booted sandbox. |
| `teardown` | ms | Time to stop and delete a sandbox. |
| `density` *(opt-in)* | MiB | Aggregate host memory for N concurrent idle sandboxes. Enable with `--all-scenarios` or `--scenarios density`. |

Latency cells report **p50**; the JSON/Markdown carry min/p50/p90/p99/max/mean/stdev.
A cell reads `n/a` when the provider doesn't support the capability (e.g. a
container runtime has no live snapshot) and `err` when an operation it claims to
support failed — a capability gap is reported differently from a broken
measurement.

## Providers

| Provider | Kind | Status on a fresh checkout | Notes |
|---|---|---|---|
| `tb-sandbox` | microVM | needs a built binary + agent image | this repo; full lifecycle incl. memory snapshot/restore |
| `krunvm` | microVM | runs if `krunvm` + `podman` installed | closest peer — libkrun microVM as a host process; no exec-into-running-VM, no snapshot |
| `smolvm` | microVM | template — configure via env | CLI not assumed; you supply command templates |
| `daytona` | container | template — needs `daytona` + API key | command templates with Daytona-shaped defaults to verify |
| `orbstack` | VM | runs if OrbStack installed | shared backing VM ⇒ per-machine host RSS not isolable |
| `docker` / `podman` | container | runs if the daemon is up | **baseline floor**, not a peer — shows the cost of the VM boundary itself |

`list` shows which are live; unavailable providers are skipped on `run`.

### Enabling `tb-sandbox`

The adapter drives the `tb-sandbox` CLI and needs a built binary and an agent
image (see the repo root README for building both):

```sh
# from the repo root
cargo build --release
BASE_BUNDLE=... scripts/build-agent-image.sh debian-bookworm

# then, from benchmarks/
export TB_SANDBOX_IMAGE=../target/agent-images/debian-bookworm
python3 bench.py run --providers tb-sandbox
```

Optional env: `TB_SANDBOX_BIN` (defaults to `target/release/tb-sandbox`, falling
back to `cargo run --release`). The harness uses an isolated
`TB_SANDBOX_STATE_DIR`/`TB_SANDBOX_CACHE_DIR` under `results/` so a benchmark run
never touches your dev sandboxes. Pass provider-specific flags (e.g. `--gvproxy`)
through the spec's extra args by editing `benchkit/cli.py`, or run with the
relevant `TB_SANDBOX_*` env vars set.

### Enabling `krunvm`

```sh
brew install krunvm podman    # krunvm needs podman/buildah for image storage
export KRUNVM_IMAGE=docker.io/library/debian:bookworm   # default
python3 bench.py run --providers krunvm
```

The adapter pre-builds the microVM rootfs once in a setup step so `cold_start`
times **pure boot**, comparably to `tb-sandbox` launching from a prebuilt image.

### Enabling `smolvm` (and any other CLI)

`smolvm` is wired up as an env-configurable adapter because its CLI isn't
assumed. Provide command templates with `{name}`, `{image}`, `{cpus}`, `{mem}`,
`{workspace}`, `{cmd}`, `{snapshot}` placeholders:

```sh
export SMOLVM_BIN=smolvm
export SMOLVM_IMAGE=debian:bookworm
export SMOLVM_LAUNCH='smolvm run --name {name} --cpus {cpus} --mem {mem} {image}'
export SMOLVM_EXEC='smolvm exec {name} -- {cmd}'
export SMOLVM_DESTROY='smolvm rm {name}'
# optional:
export SMOLVM_SNAPSHOT='smolvm snapshot {name}'   # must print the snapshot id last
export SMOLVM_RESTORE='smolvm restore {snapshot} --name {name}'
export SMOLVM_RSS='...'                            # must print host bytes for {name}
python3 bench.py run --providers smolvm
```

`SMOLVM_LAUNCH` must block until the sandbox can run a command. Same mechanism
backs `daytona` (`DAYTONA_*`); see `benchkit/providers/daytona.py` for its
defaults, which you should confirm against your installed `daytona version`.

### Enabling `orbstack` / `daytona`

OrbStack: install OrbStack; the adapter uses `orbctl`/`orb`
(`ORBSTACK_DISTRO=ubuntu` by default). Daytona: install the `daytona` CLI, set
`DAYTONA_API_KEY`, and verify the `DAYTONA_*` templates against your CLI version.

## Methodology & honest caveats

Comparing a microVM, a shared-VM machine, and a container fairly means being
explicit about what each number is — the report annotates every provider:

- **Timing** uses a monotonic clock; the first `--warmup` repetitions are
  discarded so caches/JITs/image-extraction don't skew the measured runs.
- **Host memory (`idle_rss`)** is sampled with `ps -o rss` over the provider's
  process subtree (same method as `scripts/profile-sandbox.sh`), so `tb-sandbox`
  and `krunvm` — both libkrun host processes — are directly comparable. On macOS,
  `ps` RSS is a **high-water figure**; it does *not* reflect pages the VMM has
  returned via `MADV_FREE_REUSABLE`. For reclaim-accurate numbers use `vmmap`,
  per the project's macOS balloon-reclaim notes. Treat `idle_rss` as a footprint
  ceiling, not live working set.
- **Containers report runtime memory, not host RSS.** Docker/Podman memory comes
  from `stats` (in-VM container memory on macOS), so it is a floor reference and
  is *not* comparable to microVM host RSS — the report labels it as such.
- **Shared-VM providers** (OrbStack) can't isolate per-machine host memory; those
  cells are `n/a` rather than a misleading shared total.
- **One-shot providers** (krunvm) have no daemon to attach to, so `exec`,
  `snapshot`, and `restore` are `n/a`. That gap is itself a result.

Results are host-specific. The JSON embeds host model, CPU count, RAM, platform,
git commit, and the full spec/config so runs are reproducible and comparable.

## Architecture

```
benchmarks/
  bench.py                  entry point
  benchkit/
    cli.py                  argument parsing + orchestration
    config.py               SandboxSpec / RunConfig
    runner.py               runs scenarios × providers, assembles the document
    scenarios.py            the benchmark scenarios (what gets timed)
    stats.py                p50/p90/p99/mean/stdev
    measure.py              Timer, subprocess, host RSS sampling
    report.py               console table + JSON + Markdown
    providers/
      base.py               the adapter contract (Provider, Handle, ExecResult)
      tb_sandbox.py krunvm.py docker.py orbstack.py daytona.py smolvm.py
      template.py           env-configurable adapter (powers smolvm/daytona)
```

### Adding a provider

Subclass `benchkit.providers.base.Provider`, implement the operations you
support (raise `Unsupported` for the rest), and append your factory to
`_FACTORIES` in `benchkit/providers/__init__.py`. The runner, scenarios, stats,
and reporting need no changes. See `docker.py` for a minimal persistent provider
and `krunvm.py` for one with a one-time `prepare()` setup step.
