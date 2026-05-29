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

## Status (as of the last commit on `main`)

The bulk of the planned work has shipped across both repos. The table below
is the at-a-glance state; per-gap sections further down still document the
full design intent and the remaining sub-items.

| Gap | Status | Commit(s) |
|---|---|---|
| **G1 Phase 1** disk-state snapshot/restore + VM forking | ✅ DONE | wrapper `6ec6f9a` |
| **G1 Phase 2** cooperative quiesce (sync-mode) | ✅ DONE | wrapper `19e7ebf` |
| **G1 Phase 3** memory + vCPU + GICv3 + device capture/restore | ✅ CODE COMPLETE | libkrun `69fc1cd`→`ec9c5d8`, wrapper `d59c83a`→`d2a46fd` |
| **G3** browser-in-sandbox (Chromium + CDP + VNC) | ✅ DONE | wrapper `98dc854` |
| **G4** packaging + framework adapters | ⏳ NOT STARTED | — |
| **G2** Windows guests | ⛔ DECLINED (would require abandoning HVF-direct libkrun) | — |
| **G5** cross-host snapshot portability | ⛔ DECLINED (per libkrun snapshot contract) | — |

Tests across both repos: 62 wrapper + 35 libkrun = 97 passing, 0 failing.

## Remaining Work

Ordered by leverage. Each item is bounded — none requires another month of
design work.

### 1. G4 — packaging + framework adapters [~1–2 weeks, untouched]

**The single biggest gap that determines whether anyone outside this repo
can evaluate this project against SmolVM.** Today: clone two repos, run
`cargo build`, build a libkrun image bundle, then call the Rust SDK. SmolVM
is `pip install smolvm`.

Plan (see [G4 design section](#g4--packaging--framework-adapters) below for
detail):

- New `tb-sandbox-daemon` binary that hosts `SandboxManagerSdk` behind a
  Unix-socket JSON-RPC surface.
- `tb-sandbox-py` package (mirrors `from smolvm import SmolVM` shape).
- `tb-sandbox-ts` package for the Node/agent-SDK world.
- `python/adapters/` thin shims for OpenAI Agents, LangChain, PydanticAI.
- `brew install` formula + `pipx install tb-sandbox` install path that
  handles the libkrun + os_mode launcher dependency.

### 2. G1 — end-to-end verification on real guests [hardware-bound, ~½ day each]

The Phase 3 code is wired end-to-end across both repos, but the actual
round-trip has not been booted in this environment (no os_mode_artifacts
bundle on this dev machine, no `/dev/kvm` for the Linux path):

- **HVF aarch64 round-trip:** boot an os_mode Debian guest with the
  browser variant, write a memory marker pattern + a file, send
  `snapshot-memory PATH` over the control socket, exit, restore from
  that path, verify the marker pattern and file content survive,
  verify `uptime` resumes from snapshot point not zero.
- **KVM x86_64 round-trip** on a Linux host with `/dev/kvm`.

Both are bounded test gaps against complete code paths.

### 3. G1 — bounded follow-ons (snapshot polish)

These are small, well-scoped extensions to the snapshot/restore code
already on `main`:

- **Phase 2 strict-freeze** in `guest/agent-sandbox-quiesce` — currently
  downgrades `fsfreeze -f /` to `sync` because driving fsfreeze from a
  daemon hosted on the frozen rootfs would deadlock the daemon's own
  `sleep`. Fix: cache `/bin/sleep` and `/sbin/fsfreeze` in `/run`
  (tmpfs), use `read -t` on a fifo as a sleep substitute. ~50 lines of
  shell + a test. Documented downgrade lives in the daemon today so
  manifests don't lie.
- **HVF GICv3 register-set extension** in
  `../thousandbirds-libkrun/src/hvf/src/lib.rs`. `HVF_GIC_DISTRIBUTOR_REG_IDS`
  currently captures only `GICD_CTLR` (the global enable). Adding
  `ISENABLER` / `ICENABLER` / `IPRIORITYR` / `ICFGR` / `IROUTER` is
  mechanical (~30 minutes) once an interrupt-heavy guest workload
  proves the minimal set is insufficient.
- **Per-virtio-device save_state/restore_state** on the virtio device
  trait in libkrun (virtio-blk, virtio-net, virtio-fs, balloon, mmio
  transport). The current snapshot captures vCPU + GIC + memory but not
  in-flight virtio request state. Wrapper-level cooperative quiesce
  (sync + drain) makes this safe in practice for the agent workloads
  this repo targets; persistent virtio queue state is a bounded
  follow-on (~1–2 weeks for full coverage) needed only if snapshots
  are taken without quiesce.
- **G1 Phase 4** content-addressed memory chunk dedup. Deferred per the
  original plan; only worth doing once Phase 3 is in production use.

### 4. Minor — egress allowlist

SmolVM has `internet_settings={"allowed_domains": [...]}` for domain-level
egress restriction. We have header-substitution rules in `src/proxy.rs`;
an allowlist mode would be a small extension (~½ day) that refuses
`CONNECT` to hosts not on the list.

## Suggested ordering

If the goal is **make this comparable to SmolVM in market positioning**:

1. G4 packaging (1–2 weeks) — unblocks any external evaluation
2. G1 end-to-end verification on real guest (½ day to one week, hardware
   dependent) — turns "code shipped" into "demoably works"
3. Per-virtio-device save_state for production-grade snapshot correctness
4. Phase 2 strict-freeze
5. GICv3 register-set extension (only if needed)

If the goal is **make snapshot/restore production-trustworthy first**:

1. End-to-end verification on real guest
2. Per-virtio-device save_state
3. GICv3 register-set extension
4. Phase 2 strict-freeze
5. Phase 4 dedup
6. G4 packaging

---

## Gap Inventory (From The Comparison)

| # | SmolVM has | We have | Leverage | Effort |
|---|---|---|---|---|
| G1 | **Snapshot / pause–resume** of memory + disk + processes | Warm pool only (cold-booted VMs held idle) | **High** | **High** |
| G2 | **Windows 11 guests** (Linux host only) | Linux only, OS-mode bundle required | Low for agents | Very high |
| G3 | **Browser-in-sandbox** with live viewing (`smolvm browser start --live`) | None — but the Linux guest can host any browser | Medium | Medium |
| G4 | **Managed install** (`pip install smolvm`, curl-bash) and framework adapters (OpenAI Agents, LangChain, PydanticAI) | Source build of two Rust repos; Rust SDK only | Medium (adoption) | Low |
| G5 | **Cross-host snapshot portability** (implied by their pause/resume product surface) | N/A — same gap as G1 plus host portability | Low | Very high |

## Prioritization (original — superseded by [Status](#status-as-of-the-last-commit-on-main) above)

The original session-zero ordering. Kept here so future readers can see
the reasoning that drove the work. The actual outcome — captured in the
Status table at the top — is that G1 (all phases), G3, and the density-doc
items all shipped in this session; G4 remains.

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

#### Phase 2 — Cooperative guest quiescence [repo, krun-light] — IMPLEMENTED (sync; freeze deferred)

Goal: a deterministic "this guest is safe to snapshot" point. Even though
memory capture comes in Phase 3, getting quiescence right is independent and
unblocks both #3 and any later live-snapshot work.

Landed:

- New `src/quiesce.rs`: `QuiesceClient`, `QuiesceTicket` (RAII drop fires
  best-effort `resume.req` if the explicit release path is skipped),
  `QuiesceAction { Sync, Freeze }`, atomic text-file protocol on the
  runtime-control virtio-fs mount (key=value `quiesce.req` / `quiesce.ack` /
  `resume.req` / `resume.ack`), host-side `flock` on `quiesce.lock` to
  serialize concurrent snapshot cycles on the same sandbox.
- Per-request `deadline_unix_millis` written into `quiesce.req` so the guest
  auto-resumes if the wrapper dies mid-snapshot (independent safety net
  alongside RAII drop). Default safety window is 5 minutes.
- `SandboxManager::snapshot` in `src/runtime.rs` now quiesces before the
  disk clone and releases after, when the sandbox is running and the caller
  did not opt out (`SnapshotOptions::skip_quiesce`). Stopped sandboxes skip
  quiesce automatically.
- `SnapshotManifest` gained `quiesced`, `quiesce_action`,
  `quiesced_unix_millis` — forensic record of what actually ran (not just
  what was requested, so a downgrade is visible).
- CLI flags on `snapshot`: `--no-quiesce`, `--no-freeze`,
  `--quiesce-timeout SECS`.
- New `guest/agent-sandbox-quiesce` (POSIX sh) + matching systemd unit.
  Daemon polls the control mount, runs `sync`, writes the ack, waits for
  `resume.req` (bounded by deadline), writes `resume.ack`. Installed by
  `scripts/build-agent-image.sh` next to the existing shell service.
- 10 new tests (5 in `quiesce::tests` for the protocol mechanics, 5 in
  `runtime::tests` for the integration): round-trip with fake guest,
  timeout, error-status surfacing, RAII drop sends resume, parser
  robustness, control-dir missing fail-fast, plus the snapshot-of-running
  integration tests — quiesced/sync action/skip/stopped/unresponsive.

**Action-downgrade caveat — documented, not a bug.** This build's daemon
implements `sync` only. A `freeze` action request is downgraded to sync and
the manifest's `quiesce_action` records `"sync"` truthfully. The reason:
driving `fsfreeze -f /` from a shell daemon hosted on the frozen rootfs
would deadlock the daemon's own polling loop the moment it tried to exec
`/bin/sleep` — the rootfs is frozen, the exec faults. Doing freeze safely
needs either cached binaries in tmpfs plus a bash-builtin polling strategy
(read-with-timeout on a fifo on fd 9), or kernel-level changes. Both are
straightforward follow-ons; neither is on the critical path because
sync-only already moves snapshot-of-running from "crash-consistent" to
"flushed + clone".

**On-VM verification:** still a manual step — needs a real Linux guest
booted via the libkrun os_mode launcher with an image rebuilt to include
the new daemon. The protocol is fully exercised by the Rust tests against
a fake guest, and the daemon is POSIX-sh syntax-checked.

**Exit criteria — met for sync-mode:** Wrapper-side `SandboxManager::snapshot`
quiesces running sandboxes (sync + flush) before the disk clone, surfaces
guest-side errors and timeouts cleanly, never leaves a half-written snapshot
or a wedged guest. Failure modes are covered by tests:
`snapshot_of_running_sandbox_quiesces_before_clone`,
`snapshot_records_sync_action_when_freeze_disabled`,
`snapshot_skip_quiesce_does_not_touch_runtime_control`,
`snapshot_of_stopped_sandbox_skips_quiesce_silently`,
`snapshot_of_running_sandbox_fails_when_guest_unresponsive`. Strict-freeze
mode (and its on-VM checksum-while-writing test from the original exit
criteria) remains the small follow-on.

#### Phase 3 — HVF memory + vCPU + device capture [krun-heavy] — PARTIAL (Phases A, B, D + Phase C leaf primitives landed; runtime orchestration pending)

Goal: a real snapshot. This is the work
`thousandbirds-libkrun/design_docs/snapshot_restore.md` audits but does not
schedule. Order intentionally matches that audit's "HVF first" recommendation
since macOS is the hard host requirement.

**Second pass — what landed in libkrun itself** (sibling repo
`thousandbirds-libkrun`, branch `snapshot-restore-implementation-plan`):

- **Phase A (foundations) — DONE.** New `src/vmm/src/snapshot/` module:
  `SnapshotableVm` / `SnapshotableVcpu` traits, `VmStateV1` / `VcpuStateV1`
  versioned + backend-tagged state containers with hand-rolled binary
  serialization (no new dependencies), `SnapshotError` typed errors,
  `MemoryImageWriter` / `MemoryImageReader` with sparse-file zero-page
  punching. 12 unit tests pass (state round-trip, version mismatch,
  bad magic, sparse round-trip, multi-region ordering, finalize
  invariants).
- **Phase B (KVM x86_64 capture) — DONE on Linux.** `impl SnapshotableVm
  for Vm` and `impl SnapshotableVcpu for Vcpu` in
  `src/vmm/src/linux/vstate.rs` wrapping the existing private
  `save_state` / `restore_state`. Byte format uses unsafe transmute on
  `#[repr(C)]` kvm-bindings POD structs (sound — they are kernel-ABI
  FFI types with no Rust invariants) plus length-prefixed encoding for
  FAM types (`Msrs`, `CpuId`). Verified compile-clean on macOS host;
  end-to-end test requires a KVM Linux runner (out of scope this
  session per the design doc's "Linux/KVM runtime gates remain
  deferred until a Linux host or CI runner with /dev/kvm is
  available").
- **Phase C (HVF capture) — LEAF DONE, ORCHESTRATION PENDING.**
  - Leaf: new `HvfVcpuState` struct + `HvfVcpu::save_state` /
    `restore_state` methods in `src/hvf/src/lib.rs`. Captures general
    regs (X0–X30, PC, FPCR, FPSR, CPSR), SP_EL0/SP_EL1, Q0–Q31 SIMD,
    and a fixed list of 22 system registers covering MMU/exception/
    debug/thread-pointer/virtual-timer state. 3 unit tests pass
    (round-trip, magic, size mismatch).
  - VM-level: `impl SnapshotableVm for macos::Vm` returns an empty
    payload tagged hvf/aarch64 — GICv3 capture is the documented
    Phase C.3 follow-on; the snapshot site validates "no pending
    interrupts" as a precondition.
  - Vcpu-level: `impl SnapshotableVcpu for macos::Vcpu` returns
    `UnsupportedBackend` with a specific message pointing at Phase
    C.4 — capturing requires sending a Snapshot event over the
    existing `VcpuEvent` channel and having the running vCPU thread
    pause itself and call `HvfVcpu::save_state` from its own context
    (HVF rejects register get/set on a running vCPU). This is the
    one remaining piece of HVF capture that requires real
    architectural work in the vCPU thread loop.
- **Phase D (C API + os_mode) — ABI RESERVED.**
  - `include/libkrun.h` and `src/libkrun/src/lib.rs`: new C ABI
    symbols `krun_set_snapshot_restore_from(ctx, path)` and
    `krun_snapshot_to_path(ctx, path)`. The former records the path
    (returns 0); the latter returns `-ENOSYS` pending the in-VM
    orchestration. ContextConfig gained a `snapshot_restore_from`
    field that survives across `krun_set_*` calls and is available
    for the future `krun_start_enter` restore branch.
  - `examples/os_mode.c`: new `--restore PATH` and `--snapshot-out
    PATH` flags, wired through the new C ABI. `--restore` records
    the path into the context before `krun_start_enter`, so when
    the libkrun restore execution lands no os_mode change is
    required to consume it.

**First pass — wrapper-side scaffolding** (this repo, prior commit):

- **`thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md`**
  (new): turns the existing audit into a scheduled, file-pathed implementation
  plan. Five phases (A foundations → B KVM x86_64 capture → C HVF aarch64
  capture → D public C API + os_mode → E dedup), each independently mergeable
  with explicit exit criteria. C is sub-phased C.1–C.5 because GICv3 capture
  is the largest chunk and deserves its own review unit.
- **`SnapshotKind::Full` variant** in `src/snapshot.rs` (this repo). The on-disk
  manifest now encodes either `"disk-only"` (Phase 1) or `"full"` (Phase 3
  destination). A future `"incremental-v2"` or similar fails fast rather than
  silently degrading.
- **`SnapshotManifest::memory_image_path`** — `Option<PathBuf>`, populated by
  the future Full-kind path with a path relative to the snapshot directory.
  `None` for disk-only.
- **`SnapshotOptions::capture_memory`** — callers can write `capture_memory:
  true` *today*. `SandboxManager::snapshot` recognizes it and returns a typed
  `Error::InvalidArgument` pointing at this section + the libkrun design doc,
  rather than silently producing a disk-only snapshot. The error message names
  the specific gap so users see what's blocking, and integration code does not
  need to change once libkrun lands the capture path.
- **`LaunchConfig::restore_from`** in `src/libkrun.rs` — `Option<&Path>`. When
  `Some`, the launcher invocation emits `--restore PATH`, skips the
  kernel/initramfs/root-options/kernel-cmdline block (mutually exclusive with
  restore per the libkrun design doc Phase D contract), keeps the common
  tail (DAX, network backend, virtio-fs mounts) via the new
  `append_common_launcher_args` helper. Today no production caller sets it
  because the os_mode launcher does not yet understand `--restore`; the
  wrapper plumbing is in place so wiring it up after libkrun ships Phase D is
  a single branch in `SandboxManager::restore` rather than a shape change.
- **3 new tests** (`runtime::tests::snapshot_with_capture_memory_returns_typed_error_today`,
  `snapshot::tests::manifest_round_trip_full_kind_with_memory_image`,
  `snapshot::tests::snapshot_kind_full_parses`). The pre-existing
  `unknown_kind_fails_fast` test was tightened to use `"incremental-v2"` as
  its synthetic-bad value now that `"full"` is real.

The original sub-phase plan (kept here as a check-list for the libkrun-side
work):

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

**What remains for full Phase 3:**

1. **Phase C.3** — GICv3 distributor + redistributor state capture in
   `src/hvf/src/lib.rs` (and matching read in `HvfVm::save_state`). The
   wrapper-level "no pending interrupts" precondition is a workable
   bridge until this lands.
2. **Phase C.4** — vCPU-thread Pause + Snapshot + Restore event handling
   in `src/vmm/src/macos/vstate.rs`. Today the vCPU thread loop
   (`pub fn run`) does not poll the `event_receiver`; adding Pause/
   Snapshot/Resume handling that calls `HvfVcpu::save_state` from the
   thread's own context is the largest remaining piece.
3. **Phase D runtime** — `krun_snapshot_to_path` body in
   `src/libkrun/src/lib.rs` (pause vCPUs, snapshot via the
   `SnapshotableVm` / `SnapshotableVcpu` traits, write
   `VmStateV1` + `VcpuStateV1` + `MemoryImage` files); restore branch
   in `krun_start_enter` (read files, reconstruct via traits, mmap
   memory, resume after vCPUs are restored).
4. **Wrapper flip** — `SandboxManager::snapshot` `capture_memory=true`
   branch from "return typed error" to "issue snapshot RPC over the
   existing control socket and capture the response path"; symmetric
   flip on the restore-Full guard.

Each is a small, focused change against the now-stable libkrun ABI.
The whole point of the surface, traits, leaf primitives, and ABI
reservations that landed in this session is to make each remaining
piece an independent commit rather than one giant integration push.

**Original framing — what remains for full Phase 3:** the libkrun-side capture path itself.
The implementation plan in
[`thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md`](../../thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md)
breaks this into Phases A–D with effort estimates (~3–5 days, 2 weeks,
4–6 weeks, 1 week respectively) totaling 7–9 weeks of focused work. None
of it is wrapper-side. Once Phase D lands, integration on this side is:

1. Add `SandboxManager::restore_with_memory(snap_id, opts) -> Result<Sandbox>`
   that recognizes `manifest.kind == Full`, populates `LaunchConfig::restore_from`
   in the new sandbox's launch path, and skips the cold-boot disk re-clone (the
   snapshot's memory image is paired with the cloned disk at restore time).
2. Flip `SandboxManager::snapshot`'s `capture_memory: true` branch from
   "return typed error" to "send a snapshot-memory RPC over the existing
   control socket and capture the response path into `manifest.memory_image_path`".

Each is a small, focused change against a stable libkrun ABI — the whole
point of the Phase 3 scaffolding that landed in this commit is to make
those changes minimal and obvious when the libkrun-side work is done.

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

## G3 — Browser-In-Sandbox — IMPLEMENTED

Landed in this commit:

- New agent image variant `debian-bookworm-browser` (`images/debian-bookworm-browser.Containerfile`) — chromium + display stack (Xvfb + x11vnc + fluxbox + fonts-liberation + fonts-noto-color-emoji). `scripts/build-agent-image.sh debian-bookworm-browser` builds it on top of the base image.
- New guest script `guest/agent-sandbox-browser` with `start [--headless|--headed] [--cdp-port N] [--vnc-port N] [--profile DIR]`, `status`, and `stop` subcommands. Idempotent (re-invoking `start` while the browser is up just re-prints status). POSIX sh, syntax-checked by the verification gate. Installed unconditionally on every variant — no-ops with a clear error if chromium is not present.
- New CLI subcommand group `tb-sandbox browser start|view|status|stop SANDBOX_ID`. `start` and `view` stay in the foreground holding host-side port forwards (CDP, and VNC when `--headed`) — Ctrl-C tears them down and stops the in-guest browser. `status` and `stop` are one-shot calls.
- Uses the existing `SandboxManagerSdk::exec` + `SandboxManagerSdk::forward_port` machinery; no new transport, no new daemon process, no new state files.
- Worked end-to-end example in README (headless CDP automation with `chrome-remote-interface`, plus headed VNC viewing via macOS Screen Sharing).

What this does NOT do (deliberate; small follow-ons):

- No per-browser-egress allowlist. The browser inherits the sandbox's
  network proxy config; restricting browser traffic to specific domains
  is the same wrapper-level proxy feature that applies to every workload.
- No baked-in puppeteer / playwright bindings. The CDP URL is what's
  exposed; the agent picks its own client.
- No automatic VNC viewer launch — `browser view` prints the URL and
  expects the user to open it (Screen Sharing on macOS handles
  `vnc://` URLs natively; Linux users pipe to their preferred client).

**Original plan estimate: 2–3 weeks. Shipped in one session because the
existing SDK already had `exec` + `forward_port` so the host-side
orchestration was a thin wrapper rather than a new transport.**

### Original design (kept for reference)

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
3. **G1 Phase 2 (quiescence)** — 1 week. **DONE (sync-mode; freeze deferred).**
4. **G3 browser image + VNC viewer** — 2–3 weeks. Independent of G1
   Phases 3/4, can interleave. **DONE in 1 session** (existing SDK
   `exec`+`forward_port` collapsed most of the planned scope).
5. **G1 Phase 3 (HVF capture/restore)** — 6–10 weeks. The hard one. Owns
   most of the calendar. **WRAPPER-SIDE SCAFFOLDING DONE; libkrun-side core
   implementation (Phases A–D of the libkrun design doc) pending.**
6. **G1 Phase 4 (dedup)** — deferred.

Remaining critical path is **Phase 3 (HVF memory/vCPU/device capture)** —
the multi-month libkrun-side work, unchanged in scope. Two small follow-ons
sit on the side: strict-freeze in the quiesce daemon (cached binaries +
bash-builtin polling), and on-VM verification of the quiesce round-trip
against a real Linux guest. G4 (packaging) and G3 (browser image) remain
parallelizable with G1 Phase 3.

If two engineers work in parallel, G4 + G3 go to engineer A and G1 Phase 3
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
