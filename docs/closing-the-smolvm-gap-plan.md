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

| Gap | Status | Commit(s) |
|---|---|---|
| **G1 Phase 1** disk-state snapshot/restore + VM forking | ✅ DONE | wrapper `6ec6f9a` |
| **G1 Phase 2** cooperative quiesce (sync + strict freeze) | ✅ DONE | wrapper `19e7ebf`, strict-freeze `1f579d5` |
| **G1 Phase 3** memory + vCPU + GICv3 + device capture/restore | ✅ DONE — **on-VM verified** (HVF aarch64) | libkrun `69fc1cd`→`ec9c5d8`, GICv3 ext `11cd744`, **on-VM fixes uncommitted** (see Next Steps); wrapper `d59c83a`→`d2a46fd` |
| **G3** browser-in-sandbox (Chromium + CDP + VNC) | ✅ DONE | wrapper `98dc854` |
| **Egress allowlist** in proxy (SmolVM `allowed_domains` equiv.) | ✅ DONE | wrapper `1f579d5` |
| **G4** packaging + framework adapters | ⛔ OUT OF SCOPE (Rust SDK is the canonical embedder surface) | — |
| **G2** Windows guests | ⛔ DECLINED (would require abandoning HVF-direct libkrun) | — |
| **G5** cross-host snapshot portability | ⛔ DECLINED (per libkrun snapshot contract) | — |

Current wrapper verification: **81 passing, 0 failing** locally; libkrun
**krun-hvf 3/3 + krun-vmm 32/32**.

### On-VM HVF Phase 3 round-trip — VERIFIED (2026-05-31)

The Phase 3 snapshot/restore path was driven against a **real booted aarch64
guest** for the first time (it had only ever been unit-tested against fakes).
This surfaced and fixed several real bugs, and the full round-trip now works
end-to-end on macOS/HVF. Verified, repeatable results (driver:
`/tmp/snap-roundtrip/roundtrip.py`; bundle:
`thousandbirds-libkrun/os_mode_artifacts/balloon-test/`):

- **Running guest resumes** — a pre-snapshot CPU spinner's counter continues
  across restore (e.g. 8M → 28M): memory + registers + execution + console
  output all preserved.
- **Idle guest wakes** — after restore the idle shell wakes on serial input;
  the RAM-only tmpfs marker survives (memory restored), `boot_id` is identical
  (same kernel instance, not a reboot), and `uptime` continues (e.g. 5s → 35s,
  not reset).
- No cold-boot, no kernel panic.

Bugs found + fixed (all in `thousandbirds-libkrun`, **currently uncommitted**):
1. snapshot-Pause **deadlock** on an idle (WFI-parked) guest — kick vCPUs out of
   `hv_vcpu_run`/WFE before collecting Paused; capture now 100% reliable;
2. restore **cold-boot race** — vCPUs were running the kernel entry before the
   snapshot replayed; added a `restore_pending` gate + a resume step;
3. missing **PAC keys** — added the 10 ARMv8.3 pointer-auth key sysregs (guest
   panicked with `FPAC` otherwise);
4. **serial PL011** device-state save/restore;
5. **complete GIC** capture via HVF's native `hv_gic_state` blob (distributor +
   redistributors + pending/active), replacing the per-register lists;
6. **per-vCPU GICv3 CPU interface** (`ICC_*`: group enables, priority mask, SRE)
   restored on each vCPU's own thread via `hv_gic_set_icc_reg` — **this is what
   makes an idle vCPU accept interrupts and wake after restore**; the
   main-thread `hv_gic_set_state` blob does not cover it;
7. vtimer mask-clear on restore.

See the `smolvm-phase3-onvm-verification` memory for the full diagnostic trail.

## Next Steps

The HVF aarch64 round-trip (formerly the headline verification gap) is **done
and verified** — see the section above. Remaining work, in priority order:

### 1. Commit the on-VM Phase 3 fixes to `thousandbirds-libkrun`

The 7 fixes above are validated but **uncommitted** (8 files, ~+446/-21:
`src/hvf/src/lib.rs`, `src/vmm/src/{lib,builder,resources}.rs`,
`src/vmm/src/macos/vstate.rs`, `src/devices/src/legacy/{vcpu,aarch64/serial}.rs`,
`src/libkrun/src/lib.rs`). They build clean and pass krun-hvf 3/3 + krun-vmm
32/32. **Action:** review and commit on the libkrun side; then re-point this
repo's libkrun pin and refresh the Status-table commit hashes. **Effort: <½ day.**

### 2. KVM x86_64 Phase 3 round-trip — *needs a Linux host with `/dev/kvm`*

The one item that **cannot** run on this macOS/arm64 machine. Same shape as the
HVF round-trip: boot an os_mode x86_64 guest, write a tmpfs marker, snapshot via
the control socket, exit, restore with `--restore PATH`, verify marker survives
+ `uptime` continues + idle shell wakes. The Linux KVM `save_state`/
`restore_state` path (`src/vmm/src/linux/vstate.rs`, promoted to the public
traits in `e129032`) needs the same end-to-end exercise the HVF path just got.

**Caveat from the HVF work:** the idle-wake fix (per-vCPU GIC CPU-interface
restore on the vCPU thread, fix #6) is HVF-specific. The KVM equivalent is the
in-kernel GIC/LAPIC state — confirm KVM's `KVM_GET/SET_*` GIC/irqchip state is
captured in the Linux path (it may already be, via the irqchip save/restore);
if an idle KVM guest fails to wake after restore, this is the analogous gap to
investigate. **Effort: ~½ day on a Linux box, +up to ~1 day if the idle-wake
analog needs the same treatment.** Skippable if KVM x86_64 is not a shipping
target — HVF exercises the same Phase 3 orchestration through a different
backend.

### 3. Per-device `save_state`/`restore_state` for virtio-blk / -net / -fs

Serial (PL011) and the **complete GIC** are now captured; the remaining devices
(virtio-blk, -net, -fs, and the mmio transport queue state) are **not**.
In-flight virtio request state is lost across a snapshot. This is safe today
because the wrapper quiesces (sync + drain, optional strict freeze) before
capture, so there are no in-flight requests for the agent workloads we target.
**Needed only for snapshots taken without quiesce** (e.g. a future live-snapshot
path). **Effort: ~1–2 weeks.** Ship-when-needed.

### 4. Productionize snapshot/restore in the wrapper

The wrapper already wires `SandboxManager::snapshot { capture_memory: true }` →
`snapshot-memory` over the control socket, and Full-kind `restore` → launcher
`--restore`. **Action:** run a real `tb-sandbox snapshot --capture-memory` /
`tb-sandbox restore` against a live sandbox (not just the raw os_mode driver),
add a `TB_SANDBOX_SNAPSHOT_E2E=1`-gated `cargo test` that does the round-trip on
a real guest (skips cleanly without HVF), and extend `scripts/profile-snapshot.sh`
to measure restore wall-clock vs working-set. Then evaluate a restore-backed
**warm-pool** policy (`src/sdk.rs` `WarmPool`) where it beats cold-booted pool
entries. **Effort: ~2–3 days.**

### 5. Time-rebase + identity hooks on restore

The captured `CNTV_CVAL` is relative to the old VM's counter, and wall clock
jumps forward by the snapshot→restore gap. The design's `guest/agent-sandbox-restore`
hook (entropy reseed, `systemd-timesyncd` one-shot, fresh MAC) should run early
on resume to rebase time and refresh identity. Verify MAC refresh (currently
N/A in the no-network test bundle) once a networked bundle is used. **Effort:
~1 day.**

### 6. Lower-priority / deferred

- **Extending `HVF_GIC_DISTRIBUTOR_REG_IDS`** for workloads with >1024 active
  SPIs (mechanical; superseded in practice by the native `hv_gic_state` blob,
  which captures the full distributor — revisit only if the blob proves
  insufficient). **Effort: minutes.**
- **G1 Phase 4** content-addressed memory-chunk dedup for restored siblings.
  Deferred per the original plan; worth building once Phase 3 is in production.
  **Effort: ~1–2 weeks.**
- **Strict-freeze round-trip** on a real guest (the protocol is unit-tested;
  verify `fsfreeze -f /` takes hold + unfreezes cleanly on a booted guest).
  **Effort: ~½ day** with the existing bundle.
- **Test-harness reliability:** the `roundtrip.py` driver intermittently misses
  a pre-snapshot value capture under console output pressure (`pre=None`); add
  settle/retry around the pre-state grabs before promoting it to a committed
  `cargo test`.

## Explicitly out of scope

- **G4 packaging + framework adapters** — `tb-sandbox-daemon` JSON-RPC
  + Python/TS shims + OpenAI Agents/LangChain/PydanticAI adapters +
  pipx/brew installers. Genuinely shippable code (~1–2 weeks per the
  original design section below), but the project is keeping its
  surface area focused on the Rust SDK; external-language adoption is
  not a goal right now. The Rust `SandboxManagerSdk` remains the
  canonical embedder surface.

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
items all shipped; G4 was reclassified out of scope (not on roadmap).

**Do (in order):**
1. **G1 — snapshot / fast restore.** This is the only gap that changes the
   *shape* of what the product can do; everything else is layering. It is also
   the one item the density doc originally deferred as the long-term
   cheap-scheduling lever. The warm pool was the bridge until G1 landed, not
   the destination.
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
  device-state + manifest + cloned root disk — alongside the existing
  `sandbox.json` state directories under `<state>/sandboxes/<id>/`.

### Design

A snapshot is a **directory artifact** under
`<state>/snapshots/<snapshot_id>/`, identified by its validated snapshot ID.
Caller-supplied names are immutable artifact IDs, not mutable tags. Full
snapshots use this layout:

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
  root.raw               # cloned root disk paired with the memory/device state
```

The wrapper-side surface is three operations:

- **`tb-sandbox snapshot <id> [--name NAME] [--capture-memory]`** — quiesce
  the guest, clone the root disk, and, when `--capture-memory` is set, pause
  vCPUs and capture device + memory state. The sandbox is resumed on success.
- **`tb-sandbox restore <snapshot_id> [--name NEW_ID] [--workspace .]`** —
  validate the manifest against host capability, create a fresh clone from
  `root.raw`, materialize a new sandbox state dir, and, for Full snapshots,
  hand the artifact to a new launcher invocation that loads memory and device
  state instead of cold-booting.
- **`tb-sandbox snapshots / snapshot-delete / snapshot-inspect / snapshots prune`** —
  manage the artifact directory the same way `tb-sandbox list / delete / inspect` manages
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

Snapshot restore creates metadata and a root disk but does not launch a VM.
Admission control applies on the later `start`/`shell` path using the restored
sandbox's recorded memory size, identical to a cold-created sandbox.

The warm pool (`src/sdk.rs` `WarmPool`) currently prewarms by cold-booting from
a template config. A future policy layer can choose Full-snapshot restore as
an alternate prewarm strategy once real-workload measurements justify it.

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

Goal: the snapshot artifact format, manifest, and root-disk clone/copy tooling exist and
round-trip *without* memory/vCPU state. A "restore" at this phase is a
fast-cloned cold boot from the snapshot's recorded base disk + workspace +
sandbox config — i.e. a structured way to template sandboxes from a known
ready state. Sets up the artifact directory, the CLI surface, and the
manifest schema without depending on any libkrun snapshot work. **VM forking
came along with this phase** as a natural composition (fork = snapshot+restore
in one step, no persisted intermediate).

Landed:

- `src/snapshot.rs` (new): `SnapshotId`, `SnapshotManifest`, `SnapshotKind`
  (`"disk-only"` plus `"full"` for memory-capturing snapshots),
  `SnapshotSummary`, `SnapshotOptions`, `RestoreOptions`, `ForkOptions`.
  Manifest reuses the project's existing string-based JSON helpers
  (`json_string`, `json_number`, etc.) for consistency with `sandbox.json`.
- `SandboxManager::snapshot`, `restore`, `fork`, `snapshots`,
  `inspect_snapshot`, `delete_snapshot`, `snapshot_dir` in
  `src/runtime.rs`. `snapshot` and `fork` reuse `clone_or_copy_root`
  (APFS `cp -c` on macOS, normal copy elsewhere) for the disk; a new
  `copy_dir_recursive` helper handles the credentials-staging copy with
  `cp -cR` on macOS for CoW-shared bundling.
- CLI plumbing in `src/cli.rs`: `tb-sandbox snapshot|snapshots|snapshot-inspect|snapshot-delete|restore|fork`, with `--name`, `--workspace`,
  `--no-credentials`, `--ephemeral`, and hardware override flags
  (`--cpus`, `--memory-mib`, `--memory-max-mib`, `--memory-min-mib`).
  Full snapshots are exposed as `tb-sandbox snapshot --capture-memory`.
- Re-exports in `src/lib.rs` so embedders using `SandboxManagerSdk` /
  `SandboxManager` can call the new methods directly.
- 16 new unit tests in `src/runtime.rs` + `src/snapshot.rs`: round-trip,
  manifest schema, credentials bundling, fork divergence after write
  (parent and child diverge on first write because the clone is CoW),
  override application, snapshot-list skip-malformed, collision/missing
  error paths.

Phase 3 is now code-complete. Admission accounting intentionally remains on
`start`/`shell`: `restore` creates metadata and a root disk but does not launch
a VM, matching `create`.

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

#### Phase 2 — Cooperative guest quiescence [repo, krun-light] — IMPLEMENTED (sync + strict freeze)

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

**Action-downgrade behavior — documented, not a bug.** The daemon attempts
strict freeze with a cached `fsfreeze` binary and a fifo-backed sleep strategy
so it does not need to exec from the frozen rootfs. If `fsfreeze` is unavailable
or fails, it downgrades to `sync`; the manifest's `quiesce_action` records what
actually ran.

**On-VM verification:** still a manual step — needs a real Linux guest
booted via the libkrun os_mode launcher with an image rebuilt to include
the new daemon. The protocol is fully exercised by the Rust tests against
a fake guest, and the daemon is POSIX-sh syntax-checked.

**Exit criteria — met:** Wrapper-side `SandboxManager::snapshot` quiesces
running sandboxes (freeze when available, sync fallback) before the disk clone,
surfaces guest-side errors and timeouts cleanly, never leaves a half-written
snapshot or a wedged guest. Failure modes are covered by tests:
`snapshot_of_running_sandbox_quiesces_before_clone`,
`snapshot_records_sync_action_when_freeze_disabled`,
`snapshot_skip_quiesce_does_not_touch_runtime_control`,
`snapshot_of_stopped_sandbox_skips_quiesce_silently`,
`snapshot_of_running_sandbox_fails_when_guest_unresponsive`.

#### Phase 3 — HVF memory + vCPU + device capture [krun-heavy] — CODE COMPLETE

Goal: a real snapshot. This section is now historical implementation context:
the current status is the table at the top of this document. The wrapper path
has been flipped to issue the runtime `snapshot-memory` command for
`capture_memory=true`, Full-kind restore records `restore_source`, and the
launcher path passes `--restore` to os_mode. Remaining work is verification or
libkrun polish, not wrapper design.

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
- **Phase C (HVF capture) — DONE; historical note from the mid-implementation checkpoint.**
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
- **Phase D (C API + os_mode) — DONE; historical note from the ABI-reservation checkpoint.**
  - `include/libkrun.h` and `src/libkrun/src/lib.rs`: new C ABI
    symbols `krun_set_snapshot_restore_from(ctx, path)` and
    `krun_snapshot_to_path(ctx, path)`. This paragraph records the
    historical ABI-reservation step; the current status table above is
    authoritative for code-complete snapshot/restore support.
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
  the Full-kind path with a path relative to the snapshot directory. `None`
  for disk-only.
- **`SnapshotOptions::capture_memory`** — callers set `capture_memory: true`
  to request a Full snapshot. `SandboxManager::snapshot` drives the
  `snapshot-memory` runtime-control command and records `memory.bin` in the
  manifest when libkrun accepts it.
- **`LaunchConfig::restore_from`** in `src/libkrun.rs` — `Option<&Path>`. When
  `Some`, the launcher invocation emits `--restore PATH`, skips the
  kernel/initramfs/root-options/kernel-cmdline block (mutually exclusive with
  restore per the libkrun design doc Phase D contract), keeps the common
  tail (DAX, network backend, virtio-fs mounts) via the new
  `append_common_launcher_args` helper. Full-kind restore now persists
  `restore_source` so the next launch path passes `--restore PATH`.
- **Tests** (`runtime::tests::snapshot_with_capture_memory_requires_running_sandbox`,
  `runtime::tests::snapshot_with_capture_memory_requires_resize_headroom`,
  `runtime::tests::restore_of_full_kind_snapshot_sets_restore_source_on_new_sandbox`,
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
- Future scheduling policy can add a restore-backed `WarmPool` path if
  measurements show it beats pre-booted pool entries for the target workload.
- Density admission remains on VM launch (`start`/`shell`) and uses the
  restored sandbox's recorded memory size, not the snapshot file size.

**Exit criteria:** an instrumented test boots a sandbox, writes a marker file
+ a known pattern to anonymous memory, calls `snapshot`, exits the original
VM, calls `restore`, and verifies (a) the marker file is present with correct
contents, (b) the memory pattern is intact, (c) `uptime` continues from the
snapshot point not from zero, (d) MAC address has changed, (e) wall-clock has
advanced (no time stall observable to a userspace clock). Wall-clock restore
time on the macOS dev machine is < 200 ms for a 2048 MiB VM with a 256 MiB
working set (sparse memory image, mmap restore).

**Historical checklist from before Phase 3 code completion.** This records the
old integration breakdown; the status table above is authoritative for the
current wrapper/libkrun state.

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
4. **Wrapper flip** — DONE. `SandboxManager::snapshot` `capture_memory=true`
   now issues the snapshot RPC over the existing control socket and records
   `memory.bin`; Full-kind restore persists `restore_source` for the launch
   path.

The point of the surface, traits, leaf primitives, and ABI reservations was to
make the follow-on integration land as independent commits rather than one
giant integration push.

**Original framing before Phase 3 completion:** the libkrun-side capture path itself.
The implementation plan in
[`thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md`](../../thousandbirds-libkrun/design_docs/snapshot_restore_implementation.md)
breaks this into Phases A–D with effort estimates (~3–5 days, 2 weeks,
4–6 weeks, 1 week respectively) totaling 7–9 weeks of focused work. None
of it is wrapper-side. The original wrapper integration steps were:

1. Add `SandboxManager::restore_with_memory(snap_id, opts) -> Result<Sandbox>`
   that recognizes `manifest.kind == Full`, populates `LaunchConfig::restore_from`
   in the new sandbox's launch path, and skips the cold-boot disk re-clone (the
   snapshot's memory image is paired with the cloned disk at restore time).
2. Flip `SandboxManager::snapshot`'s `capture_memory: true` branch from
   "return typed error" to "send a snapshot-memory RPC over the existing
   control socket and capture the response path into
   `manifest.memory_image_path`. **DONE in wrapper.**

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

## G4 — Packaging + Framework Adapters — OUT OF SCOPE

The original plan flagged G4 as the highest-leverage gap for external
adoption (`pip install smolvm`-equivalent). The project has decided not
to pursue it: the Rust `SandboxManagerSdk` is the canonical embedder
surface, and external-language adoption is not a current goal.

If the project's stance ever changes, the intended shape is:

- `tb-sandbox-daemon` binary hosting `SandboxManagerSdk` behind a
  Unix-socket JSON-RPC surface.
- `tb-sandbox-py` package shelling out to the daemon (mirrors
  `from smolvm import SmolVM` API shape).
- `tb-sandbox-ts` package for the Node/agent-SDK world.
- Thin adapters under `python/adapters/` for OpenAI Agents, LangChain,
  PydanticAI.
- Homebrew formula + `pipx install tb-sandbox` that handle the
  libkrun + os_mode launcher dependency.

No code from this section exists. Not on any roadmap.

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

Original session-zero plan, kept as a record of the journey. Current
status (per the [Status table](#status-as-of-the-last-commit-on-main) at
the top) is what's live; this list shows what we expected vs what
actually happened.

1. **G4 packaging** — 1–2 weeks. Now declared out of scope.
2. **G1 Phase 1 (artifact + cold-boot-replay restore + fork)** — 1–2 weeks. **DONE.**
3. **G1 Phase 2 (quiescence)** — 1 week. **DONE (sync + strict freeze).**
4. **G3 browser image + VNC viewer** — 2–3 weeks. **DONE in 1 session**
   (existing SDK `exec`+`forward_port` collapsed most of the planned scope).
5. **G1 Phase 3 (HVF capture/restore)** — 6–10 weeks. The hard one. Owns
   most of the calendar. **DONE end-to-end across both repos** (memory +
   vCPU + GICv3 + device capture/restore wired via control socket + os_mode
   `--restore` flag + wrapper flip). End-to-end verification on a real
   booted guest remains the only outstanding piece.
6. **G1 Phase 4 (dedup)** — deferred per original plan.

The original critical path was Phase 3; that's now code-complete. The
only items left on `main` are hardware-bound verification gaps and the
per-virtio-device save_state polish item, neither of which blocks the
project's stated goals.

## Snapshot Policy Notes

- **Snapshot identity model.** Snapshots are immutable artifacts addressed by
  their directory id. Caller-supplied names are stable ids, not mutable tags.
  Content-addressed tags can be added later if registry-style workflows need
  them.
- **Snapshot of a VM with active network connections.** Phase 3 generates a
  fresh MAC; in-flight TCP connections are expected to break. Agent workloads
  tolerate this, so no TCP migration contract is promised.
- **Memory image storage.** Full snapshots use sparse `memory.bin` files. Later
  phases may add a content-addressed chunk store under `<cache>/snapshot-chunks/`
  for dedup, but sparse files are the first release format.
- **Snapshot GC policy.** **Resolved in wrapper:** `tb-sandbox snapshots prune`
  reports unusable snapshot directories (missing/invalid manifest, missing
  `root.raw`, missing image, or missing Full-snapshot memory image), and
  `tb-sandbox snapshots prune --delete` removes them.
