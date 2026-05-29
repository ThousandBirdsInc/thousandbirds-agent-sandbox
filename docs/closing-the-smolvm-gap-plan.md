# Closing The SmolVM Gap — Plan

## Purpose

Companion to [`vm-density-and-resource-sharing-design.md`](vm-density-and-resource-sharing-design.md).
That doc closes the **density / efficiency** gap. This doc closes the **product
parity** gap against [SmolVM](https://github.com/CelestoAI/SmolVM) — the small
set of capabilities SmolVM ships today that `tb-sandbox` + `thousandbirds-libkrun`
do not.

Like the density doc, items tagged **[repo]** land here; items tagged **[krun]**
land in `../thousandbirds-libkrun` and are surfaced through launcher flags and
public APIs.

## Gap Inventory (From The Comparison)

| # | SmolVM has | We have | Leverage | Effort |
|---|---|---|---|---|
| G1 | **Snapshot / pause–resume** of memory + disk + processes | Warm pool only (cold-booted VMs held idle) | **High** | **High** |
| G2 | **Windows 11 guests** (Linux host only) | Linux only, OS-mode bundle required | Low for agents | Very high |
| G3 | **Browser-in-sandbox** with live viewing (`smolvm browser start --live`) | None — but the Linux guest can host any browser | Medium | Medium |
| G4 | **Managed install** (`pip install smolvm`, curl-bash) and framework adapters (OpenAI Agents, LangChain, PydanticAI) | Source build of two Rust repos; Rust SDK only | Medium (adoption) | Low |
| G5 | **Cross-host snapshot portability** (implied by their pause/resume product surface) | N/A — same gap as G1 plus host portability | Low | Very high |

## Prioritization

**Do (in order):**
1. **G1 — snapshot / fast restore.** This is the only gap that changes the
   *shape* of what the product can do; everything else is layering. It is also
   the one item the density doc explicitly defers ("snapshot/restore is the
   long-term cheap-scheduling lever; deferred in libkrun"). The warm pool is
   the bridge until G1 lands, not the destination.
2. **G4 — packaging + thin framework adapters.** Cheap, parallelizable with
   anyone, and the precondition for *anyone outside this repo* to compare us to
   SmolVM at all.
3. **G3 — browser-in-sandbox.** Almost entirely a guest-image and userspace
   layer; needs no VMM change. Worth doing once G1 makes session startup cheap
   enough that a browser-bearing image is not painful to boot.

**Decline:**
- **G2 — Windows guests.** Boot dependency on UEFI + emulated chipset that
  libkrun's OS-mode explicitly does not target (`libkrun/README.md`: "no
  firmware, GRUB, ISO installers, arbitrary cloud images"). Implementing this
  means abandoning libkrun-on-HVF for QEMU-on-HVF and rebuilding most of this
  repo. Cost is months of work; agent workloads almost universally run on
  Linux. Recommend: punt.
- **G5 — cross-host snapshot portability.** Even SmolVM's docs don't claim
  this convincingly. The libkrun snapshot design doc
  (`thousandbirds-libkrun/design_docs/snapshot_restore.md`) is explicit that
  the first production milestone is *same host/backend family only*. Treat as
  out of scope.

## Non-Goals

- Replacing libkrun with QEMU for any host. The HVF-direct bet is the whole
  reason we are competitive on density and boot time; we do not give it up to
  match a feature list.
- Cross-architecture (x86 guest on aarch64 host, or vice versa) snapshot
  restore.
- Live migration between hosts.
- A "browser as a service" with managed display/streaming infrastructure; we
  ship the in-guest browser and a host-side viewer, not a hosted product.

---

## G1 — Snapshot / Fast Restore

### Why this is the gap that matters

The density doc collapses cold-boot to ~1.3 s and lets a warm pool hide it for
templated sessions. It does not help when:

- The pooled template doesn't match the requested per-session config
  (different mounts, different credentials, different memory ceiling). The
  pool degrades to a cold boot.
- The agent wants to resume a *specific* prior session — a paused build, a
  debugging context, a long-running browser session. The warm pool has no
  notion of identity-preserving resume.

Snapshot/restore is what turns "we can launch sandboxes" into "we can
*hibernate and rehydrate* sandboxes". It is also the cheapest scheduling lever
that exists: a restored VM skips both pre-kernel VMM init (~700 ms) and guest
boot (~560 ms), collapsing total startup to roughly the time to map memory and
unpause vCPUs.

### Current state (audit)

What is already in place:

- **APFS copy-on-write root clones** (`src/runtime.rs:681`,
  `clone_or_copy_root`). These are the disk half of a restore on macOS.
- **libkrun KVM/x86_64 has partial save/restore helpers** for PIT, clock,
  PIC, IOAPIC, CPUID, MSRs, LAPIC, MP state, registers, XSAVE
  (`thousandbirds-libkrun/design_docs/snapshot_restore.md` "Linux/KVM
  Backend"). Not wired to a public API, no memory or device serialization, no
  HVF equivalent.
- **virtio-balloon control socket** and per-VM `control.sock`
  (`src/runtime.rs:553` `resize_memory`; `src/runtime.rs:587`
  `control_socket_path`). The serialization channel for "freeze me now" can
  reuse this socket and protocol.
- **A cooperative quiescence hook in the guest** —
  `guest/agent-sandbox-shell` already emits a `KRUN_OSMODE: ready` marker
  consumed by `scripts/profile-sandbox.sh`. The same hook can be extended into
  a "ready to snapshot" marker.

What is missing:

- Any HVF API for capturing GICv3, system registers, timer state, and full
  general register set as public methods on the libkrun HVF backend
  (`thousandbirds-libkrun/src/hvf/src/lib.rs`).
- Any guest-memory image format or dirty-tracking strategy in libkrun's VMM
  (`thousandbirds-libkrun/src/vmm/src/builder.rs`, `GuestMemoryMmap` regions).
- Any device-level `save_state()` / `restore_state()` on the virtio trait
  (`thousandbirds-libkrun/src/devices/src/virtio/device.rs`).
- A wrapper-side concept of "snapshot artifact" — a directory of memory +
  device-state + manifest + APFS clone source — alongside the existing
  `sandbox.json` state directories under `<state>/sandboxes/<id>/`.

### Design

A snapshot is a **directory artifact** under
`<state>/snapshots/<snapshot_id>/`, identified by a content-addressed ID, with
this layout:

```text
<state>/snapshots/<snapshot_id>/
  manifest.json          # libkrun version, format version, backend, arch,
                         # CPU profile, memory size, kernel/initramfs/disk
                         # base hashes, device list + per-device state versions,
                         # quiescence marker, host monotonic+wall clock at snap
  memory.bin             # guest physical memory, page-aligned, sparse where
                         # possible (host file holes; later: content-addressed
                         # chunks for dedup)
  devices/
    transport.json       # virtio-mmio: features, status, queue PFNs, ready
    vblk.json            # virtio-blk: features, queue state, backing-disk hash
    serial.json          # serial console: register state, drain marker
    vnet.json            # virtio-net (if enabled): features, queue state,
                         # backend socket policy — restore generates fresh MAC
    balloon.json         # balloon: target, reported pages
    vfs.json             # virtio-fs: per-mount tag policy, DAX window size
  vcpus/
    vcpu0.json           # general regs, system regs, timer state, exception
                         # state; one file per vCPU
  gic.json               # GICv3 distributor + redistributors + pending IRQs
  disk_base.txt          # absolute path to the immutable base disk used at
                         # snapshot time; restore validates checksum and clones
```

The wrapper-side surface is three operations:

- **`tb-sandbox snapshot <id> [--name NAME]`** — quiesce the guest, pause
  vCPUs, capture device + memory state, write the artifact. The sandbox is
  resumed (still running) on success unless `--stop` is passed.
- **`tb-sandbox restore <snapshot_id> [--name NEW_ID] [--workspace .]`** —
  validate the manifest against host capability, create a fresh APFS clone
  from `disk_base.txt`, materialize a new sandbox state dir, hand the
  artifact + clone to a new launcher invocation that loads memory and device
  state instead of booting a kernel.
- **`tb-sandbox snapshots list / delete / inspect`** — manage the artifact
  directory the same way `tb-sandbox list / delete / inspect` manages
  sandbox state.

The library/SDK surface adds three methods on `SandboxManager` mirroring the
existing pattern at `src/runtime.rs:553` (`resize_memory`):

```rust
impl SandboxManager {
    pub fn snapshot(&self, id: &SandboxId, opts: SnapshotOptions) -> Result<SnapshotId>;
    pub fn restore(&self, snapshot: &SnapshotId, opts: RestoreOptions) -> Result<Sandbox>;
    pub fn snapshots(&self) -> Result<Vec<SnapshotSummary>>;
}
```

Snapshot restore counts against admission control (`src/scheduler.rs`) using
the snapshot's recorded memory size as the reservation, identical to the cold
boot path.

The warm pool (`src/sdk.rs` `WarmPool`) gains an alternate constructor that
prewarms by **restoring from a templated snapshot** instead of cold-booting,
turning today's ~430 MiB / ~1.3 s pool entries into snapshot-restored entries
once G1 is functional.

### Cross-cutting requirements

Every restore must avoid cloning guest identity by accident
(`thousandbirds-libkrun/design_docs/snapshot_restore.md` "Entropy, Identity,
And Time"):

- **Fresh MAC** generated by default; opt-in flag to preserve.
- **Entropy reseed** via a `guest/agent-sandbox-restore` script that runs
  early on resume, reseeds `/dev/urandom` from a virtio-rng draw, and rotates
  any application secrets the agent profile registered as "regenerate on
  restore".
- **Time rebase**: the launcher records host monotonic + wall clock at
  snapshot and again at restore; the guest restore hook re-runs `systemd-timesyncd`
  one-shot before handing the shell back.
- **Workspace identity**: a restore opens with a *new* workspace mount by
  default. If the user passes `--workspace`, the wrapper either rebinds the
  same path (cheap, identical view) or refuses if the snapshot's recorded
  workspace path differs and `--allow-workspace-change` is not set.

### Phased plan

Phased so each phase ships independently. Phases 1 and 2 are useful before any
HVF code lands; phase 3 is the hard one.

#### Phase 1 — Snapshot artifact + restore as cold-boot replay [repo, krun-light] — IMPLEMENTED

Goal: the snapshot artifact format, manifest, and APFS clone tooling exist and
round-trip *without* memory/vCPU state. A "restore" at this phase is a
fast-cloned cold boot from the snapshot's recorded base disk + workspace +
sandbox config — i.e. a structured way to template sandboxes from a known
ready state. Sets up the artifact directory, the CLI surface, and the
manifest schema without depending on any libkrun snapshot work. **VM forking
came along with this phase** as a natural composition (fork = snapshot+restore
in one step, no persisted intermediate).

Landed:

- `src/snapshot.rs` (new): `SnapshotId`, `SnapshotManifest`, `SnapshotKind`
  (open enum so a future `"full"` kind that captures memory can land without
  breaking the format), `SnapshotSummary`, `SnapshotOptions`,
  `RestoreOptions`, `ForkOptions`. Manifest reuses the project's existing
  string-based JSON helpers (`json_string`, `json_number`, etc.) for
  consistency with `sandbox.json`.
- `SandboxManager::snapshot`, `restore`, `fork`, `snapshots`,
  `inspect_snapshot`, `delete_snapshot`, `snapshot_dir` in
  `src/runtime.rs`. `snapshot` and `fork` reuse `clone_or_copy_root`
  (APFS `cp -c`) for the disk; a new `copy_dir_recursive` helper handles
  the credentials-staging copy with `cp -cR` on macOS for CoW-shared
  bundling.
- CLI plumbing in `src/cli.rs`: `tb-sandbox snapshot|snapshots|snapshot-inspect|snapshot-delete|restore|fork`, with `--name`, `--workspace`,
  `--no-credentials`, `--ephemeral`, and hardware override flags
  (`--cpus`, `--memory-mib`, `--memory-max-mib`, `--memory-min-mib`).
- Re-exports in `src/lib.rs` so embedders using `SandboxManagerSdk` /
  `SandboxManager` can call the new methods directly.
- 16 new unit tests in `src/runtime.rs` + `src/snapshot.rs`: round-trip,
  manifest schema, credentials bundling, fork divergence after write
  (parent and child diverge on first write because the clone is CoW),
  override application, snapshot-list skip-malformed, collision/missing
  error paths.

Deferred to Phase 3 (still): guest-memory capture/restore, vCPU + GIC
state, snapshot of running processes, snapshot admission accounting for
restore.

Deferred to a smaller follow-on: admission control on `restore` (today
the new sandbox is only counted when `start`/`shell` runs against it,
since restore itself does not launch a VM). Matches how `create` already
works — the symmetry is intentional.

**Exit criteria — met:** `cargo run -- snapshot my-sandbox` writes a
manifest + APFS-cloned root.raw + (optionally) bundled credentials-staging
under `<state>/snapshots/<id>/`. `cargo run -- restore <snap_id>` writes a
new sandbox state directory whose root.raw is APFS-cloned from the
snapshot's root.raw, with overrideable config (name, workspace,
credentials, cpus, memory limits, ephemeral) and inherited fallback to the
manifest. `cargo run -- fork <sandbox_id>` does the same in one step,
inheriting the parent's hardware and credentials wholesale. `cargo run --
snapshots` lists them. `cargo run -- snapshot-delete` cleans up. All
verified by `cargo test` (`runtime::tests::snapshot_*`,
`runtime::tests::restore_*`, `runtime::tests::fork_*`,
`runtime::tests::snapshots_*`, `runtime::tests::delete_snapshot_*`,
`snapshot::tests::*`).

#### Phase 2 — Cooperative guest quiescence [repo, krun-light]

Goal: a deterministic "this guest is safe to snapshot" point. Even though
memory capture comes in Phase 3, getting quiescence right is independent and
unblocks both #3 and any later live-snapshot work.

Concrete changes:

- New `guest/agent-sandbox-quiesce` script + `agent-sandbox-quiesce.service`
  that, on signal over the control socket: refuses new shell sessions,
  flushes block devices (`sync`, `fsfreeze -f /` if supported), drains
  in-flight virtio requests, and emits a `KRUN_OSMODE: quiesced` marker.
- Extend the in-VM control listener (the existing balloon control path,
  per `src/runtime.rs:587`) with a `quiesce` / `resume` RPC.
- Wrapper-side `SandboxManager::snapshot` blocks on the `quiesced` marker
  (timeout configurable, default 10 s) before writing the artifact, then
  issues `resume` on success and on every error path.
- Manifest records the quiescence timestamp and the marker line for forensics.

**Exit criteria:** a long-running build in the guest can be quiesced and
resumed with no I/O loss, verified by checksumming a file the guest is
actively writing across the quiesce window. Failure of the guest to quiesce
within the timeout is a clean error, not a hang.

#### Phase 3 — HVF memory + vCPU + device capture [krun-heavy]

Goal: a real snapshot. This is the work
`thousandbirds-libkrun/design_docs/snapshot_restore.md` audits but does not
schedule. Order intentionally matches that audit's "HVF first" recommendation
since macOS is the hard host requirement.

Concrete libkrun changes (in dependency order):

1. **Public HVF state APIs** in `src/hvf/src/lib.rs`:
   - `Vcpu::save_state()` / `restore_state()` covering general registers,
     system registers (`SCTLR_EL1`, `TCR_EL1`, `TTBR0_EL1`, `TTBR1_EL1`,
     `MAIR_EL1`, `VBAR_EL1`, `SPSR_EL1`, `ELR_EL1`, `ESR_EL1`, `FAR_EL1`,
     `MDSCR_EL1`, FP/SIMD state), virtual timer state, and exception state.
   - A pause primitive that exits all vCPUs and proves quiescence
     (`hv_vcpus_exit` + per-vCPU acknowledgement) before any state read.
   - GICv3 distributor + redistributor capture/restore through the existing
     interrupt-controller code path.
2. **Guest memory image format + capture** in `src/vmm/src/builder.rs` and a
   new `src/vmm/src/snapshot.rs`:
   - Walk `GuestMemoryMmap` regions, write contents to `memory.bin`
     page-aligned with sparse holes for never-touched pages
     (the macOS path can use `lseek(SEEK_HOLE)` or per-page zero detection;
     the libkrun-side reclaim work for #A in the density doc already touches
     this region tracking).
   - Restore: `mmap` `memory.bin` into the new VM's guest memory before any
     vCPU runs. Use `MAP_PRIVATE` so post-restore writes are private (Phase 4
     can promote to shared+CoW for dedup).
3. **Per-device `save_state()` / `restore_state()`** on the virtio trait
   (`src/devices/src/virtio/device.rs`):
   - virtio-mmio transport: features, status, queue PFNs, ready bits,
     interrupt state.
   - virtio-blk: features, queue state, backing-file hash, in-flight request
     policy (Phase 3 requires zero in-flight requests; the quiesce hook from
     Phase 2 makes that real).
   - serial console: register state, drain marker; Phase 3 requires a
     quiescent console.
   - virtio-net: features, queue state, backend socket policy; restore
     always generates a fresh MAC.
   - virtio-fs: per-mount tag, DAX window size, policy that the host backing
     dirs are identical on restore (Phase 3 enforces this by manifest match).
   - balloon: target inflation, list of reported pages.
4. **A public `krun_snapshot(ctx, path)` / `krun_restore(ctx, path)` C API**
   exported in `include/libkrun.h` and wired through `examples/os_mode.c` as
   `--snapshot-out PATH` (during a paused run) and `--restore PATH` (in place
   of kernel/initramfs/disk on the launch command line).

Concrete wrapper changes:

- `src/libkrun.rs` `LaunchConfig` gains `restore_from: Option<&Path>`; when
  set, kernel/initramfs/disk-root are omitted from the launcher invocation
  and `--restore PATH` is passed instead.
- `SandboxManager::snapshot` upgrades from "manifest + clone only" (Phase 1)
  to "issue `snapshot` over the control socket, capture memory + device
  state to the artifact directory, then resume".
- `SandboxManager::restore` upgrades from "cold boot from manifest" to
  "invoke the launcher in restore mode with `restore_from`".
- `WarmPool` gains a `from_snapshot(...)` constructor; pool entries become
  restore-backed.
- Density admission accounts for restore memory using the manifest's
  recorded memory size, not the snapshot file size.

**Exit criteria:** an instrumented test boots a sandbox, writes a marker file
+ a known pattern to anonymous memory, calls `snapshot`, exits the original
VM, calls `restore`, and verifies (a) the marker file is present with correct
contents, (b) the memory pattern is intact, (c) `uptime` continues from the
snapshot point not from zero, (d) MAC address has changed, (e) wall-clock has
advanced (no time stall observable to a userspace clock). Wall-clock restore
time on the macOS dev machine is < 200 ms for a 2048 MiB VM with a 256 MiB
working set (sparse memory image, mmap restore).

#### Phase 4 — Memory dedup (optional follow-on) [krun]

Goal: density at restore time. Phase 3 gives every restored VM a private
memory image; this phase shares immutable pages across restored siblings.

The realistic macOS/HVF path is content-addressed chunking of `memory.bin`:
split into 2 MiB chunks, identify zero and duplicate chunks across snapshots,
back them with shared read-only mappings plus `MAP_PRIVATE` overlays. The
density doc explicitly notes (`vm-density-and-resource-sharing-design.md`
"Non-Goals") that KSM-style anonymous-memory dedup is not available on HVF;
the chunk-store approach is the workaround.

Defer Phase 4 until Phase 3 is in production. Mention only so the format in
Phase 3 (`memory.bin` page-aligned with sparse holes) does not preclude it.

### Verification

Same harness as the density doc — `scripts/profile-sandbox.sh` with
`RESOURCE_CSV=` and `ITERATIONS=` — extended with a new
`scripts/profile-snapshot.sh` that measures:

- **Snapshot wall-clock** vs guest memory footprint, in MiB and seconds.
- **Restore wall-clock** vs same, broken into manifest validation, APFS
  clone, mmap of memory image, device restore, GIC restore, vCPU resume.
- **Working-set delta** after restore — does the restored VM hold only the
  pages it actually touches post-resume, or does the mmap of `memory.bin`
  fault the whole thing in eagerly. On macOS, expect lazy faulting; verify
  with `vmmap` per the `macos-balloon-reclaim-hvf-pinning` notes.
- **Correctness sweep**: marker file, memory pattern, uptime, MAC, clock,
  workspace identity. Run as a `cargo test` against a real guest behind a
  `TB_SANDBOX_SNAPSHOT_E2E=1` env gate so CI without HVF skips cleanly.

---

## G4 — Packaging + Framework Adapters

### Why bother

Today, evaluating `tb-sandbox` requires cloning two repos and running
`cargo build` against a forked libkrun. SmolVM evaluates in one
`pip install`. The Rust SDK is real but invisible to the Python-shaped agent
ecosystem.

Goal is **not** to become a Python project. Goal is to ship a small Python
package and a small TypeScript package that each wrap the existing Rust CLI
+ SDK over a stable IPC surface.

### Design

- A `tb-sandbox-daemon` binary (new bin target in `Cargo.toml`) that hosts
  `SandboxManagerSdk` behind a Unix-socket JSON-RPC surface mirroring the
  existing Rust SDK methods (`create`, `shell`, `launch_piped`, `resize`,
  `snapshot`, `restore`, `list`). One process per user, started on demand.
- A `tb-sandbox-py` package (separate repo or `python/` subdir here) that
  shells out to the daemon socket. Public API mirrors SmolVM's surface so a
  reader can A/B compare:

  ```python
  from tb_sandbox import Sandbox

  vm = Sandbox(image="debian-bookworm")
  result = vm.run("echo hello")
  vm.stop()
  ```

- A `tb-sandbox-ts` package with the same surface for the Node/agent-SDK
  world.
- Adapters under `python/adapters/`: `openai_agents.py`, `langchain.py`,
  `pydantic_ai.py` — each one a 50-line tool wrapper around `Sandbox.run`,
  matching the integration surface SmolVM lists in their README.
- A `brew install` formula and a `pipx install tb-sandbox` path that handle
  the libkrun + os_mode launcher dependency for the user. The libkrun fork
  already ships `libkrun.pc` and a `.dylib`, so a Homebrew formula is
  feasible.

This is **purely additive** — no Rust code is moved, no existing CLI
changes. Phase ordering can run in parallel with G1.

**Exit criteria:** an outside user can `pipx install tb-sandbox`, run
`tb-sandbox doctor` to verify libkrun + os_mode are present (auto-install
through Homebrew if not), then do
`Sandbox(image="debian-bookworm").run("echo hi")` from Python. The
`openai_agents` adapter passes the trivial "agent runs `ls` in sandbox" test.

---

## G3 — Browser-In-Sandbox

### Why this is a guest-image problem, not a VMM problem

The browser is a userspace process. The hard part of "browser in sandbox" is
not the VMM — libkrun already runs the same Linux that hosts Chromium fine —
it is (a) shipping the browser in the image without bloating boot, (b)
exposing the display surface to the host, (c) feeding the agent a viewer.

### Design

- A new agent image variant `debian-bookworm-browser` under
  `scripts/build-agent-image.sh`, layered on the existing base. Adds
  Chromium, a small WM (Sway or Xvfb + Openbox depending on Wayland vs X),
  and `agent-sandbox-browser` — a launch script that starts the browser
  headless or headed.
- Display transport: **VNC over the existing host proxy port range**, not a
  new virtio-gpu integration. Rationale: virtio-gpu under HVF is its own
  rabbit hole (libkrun supports venus/native-context but the wrapper does
  not wire display), and a host-side `tb-sandbox browser view <id>` that
  shells out to a local VNC client over the proxy is shippable in days, not
  months.
- Host-side viewer: `tb-sandbox browser view <id>` opens the user's local
  VNC client against the sandbox's per-VM port, the same way `tb-sandbox
  shell` opens a serial PTY. No managed streaming infrastructure.
- Optional Phase 2: `--browser-allowlist DOMAINS` reuses the existing
  `src/proxy.rs` per-host policy to lock the browser's egress, matching
  SmolVM's `internet_settings={"allowed_domains": [...]}`.

**Exit criteria:** `tb-sandbox shell --image debian-bookworm-browser`
launches a sandbox with Chromium ready; `tb-sandbox browser view <id>` from
another terminal opens a working browser surface; the agent (over the
existing `GuestCommand` SDK) can drive Chromium via the standard CDP socket
exposed at `localhost:9222` inside the guest and forwarded out through the
existing port-forward path in `src/sdk.rs`.

---

## Why We Decline G2 (Windows) And G5 (Cross-Host Snapshots)

Recording the decision in this doc so future maintainers do not relitigate
it without new information.

**G2 — Windows guests** would require:

- A firmware/UEFI boot path. libkrun's OS-mode is explicitly direct-kernel
  only (`thousandbirds-libkrun/README.md` "OS mode … intentionally
  constrained to virtio-mmio devices and prepared images; it does not boot
  firmware, GRUB, ISO installers, or arbitrary cloud images").
- An emulated chipset (AHCI, PS/2, VGA, ACPI tables) that libkrun does not
  ship and is out of scope for the project's stated goals
  (`thousandbirds-libkrun/README.md` "Goals and non-goals" — "Become a
  generic VMM" is a **non-goal**).
- A second host hypervisor backend on macOS for guests our HVF setup cannot
  boot, since Windows-on-HVF historically requires QEMU + TCG or full
  Hypervisor.framework device emulation we have not built.

The cost is multiple engineer-months to get a degraded Windows boot that
SmolVM ships only on Linux hosts anyway. Agent workloads — `claude`,
`codex`, shell, build tools — are Linux-native. **Recommend: punt
indefinitely.**

**G5 — cross-host snapshot portability** is declined by the libkrun snapshot
design doc itself
(`thousandbirds-libkrun/design_docs/snapshot_restore.md` "Restore Contract":
"Do not promise cross-host or cross-backend restore until both KVM and HVF
have complete, tested state coverage."). G1 in this plan inherits that
contract. Same-host/backend restore covers every product use case we have
identified.

---

## Sequencing And Headcount

Original plan, calendar order if one engineer works end-to-end:

1. **G4 packaging** — 1–2 weeks. Unblocks all external evaluation.
2. **G1 Phase 1 (artifact + cold-boot-replay restore + fork)** — 1–2 weeks. **DONE.**
3. **G1 Phase 2 (quiescence)** — 1 week.
4. **G3 browser image + VNC viewer** — 2–3 weeks. Independent of G1
   Phases 3/4, can interleave.
5. **G1 Phase 3 (HVF capture/restore)** — 6–10 weeks. The hard one. Owns
   most of the calendar.
6. **G1 Phase 4 (dedup)** — deferred.

Remaining critical path is **Phase 2 (cooperative quiesce)** → **Phase 3
(HVF memory/vCPU/device capture)**. Phase 2 is a small in-VM control-listener
extension; Phase 3 is the multi-month libkrun-side work and is unchanged.
G4 (packaging) and G3 (browser image) remain parallelizable with G1.

If two engineers work in parallel, G4 + G3 go to engineer A and G1 Phase 2/3
to engineer B; merge after G1 Phase 3.

## Open Questions

- **Snapshot identity model.** Are snapshots immutable artifacts named by
  hash, mutable named tags, or both (a la container images: `latest`, `sha256:...`)?
  Start immutable + hash, add tags if the warm-pool-from-snapshot path
  benefits.
- **Snapshot of a VM with active network connections.** Phase 3 generates a
  fresh MAC; in-flight TCP connections will break. Document as expected;
  agent workloads tolerate this. Revisit only if a use case appears.
- **Memory image storage.** Sparse files on APFS are fine for the first
  release; later phases may want a content-addressed chunk store under
  `<cache>/snapshot-chunks/` for dedup. Format choice should not block
  Phase 3.
- **Snapshot GC policy.** A snapshot whose `disk_base.txt` no longer exists
  is unusable. `tb-sandbox snapshots prune` should detect and offer to
  delete those.
