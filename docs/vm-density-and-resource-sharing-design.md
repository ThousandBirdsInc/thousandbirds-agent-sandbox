# VM Density and Resource Sharing Design

## Purpose

Make `thousandbirds-agent-sandbox` run **as many concurrent agent VMs as a host can
safely hold**, with each VM consuming **only the resources it actually uses**, **cheap
and fast scheduling of new VMs**, and **resource sharing across VMs that preserves
isolation**.

This document is the design and implementation plan for the work identified in the
density/efficiency review. It is scoped to four goals:

1. **Per-VM efficiency** — a VM's host footprint tracks its live working set, not a
   high-water mark.
2. **Max density** — the host packs VMs up to a real budget and refuses/queues beyond it
   instead of swapping or OOMing.
3. **Low scheduling overhead** — starting a new VM is cheap (warm pool / snapshot) and
   boots faster (no initramfs).
4. **Cross-VM sharing** — read-only base assets and host page cache are shared across VMs
   while writes stay private per VM.

Changes span two repos. Items tagged **[repo]** land here; items tagged **[krun]** land
in `../thousandbirds-libkrun` (guest kernel config, `examples/os_mode` launcher, libkrun
device support) and are surfaced here through launcher flags.

## Non-Goals

- A general-purpose VM scheduler/orchestrator with multi-host placement. The scheduler
  here is single-host admission control, not a cluster manager.
- Strong host/guest isolation beyond libkrun's existing model (VMM and guest share a host
  security context). Sharing features below never weaken the existing isolation; they only
  share **read-only** data.
- Live migration between hosts.
- KSM-style cross-VM anonymous-memory dedup — not available on macOS/HVF, so it is out of
  scope; virtiofs DAX (below) is the supported path to shared read-only memory.

## Background: current behavior

What already serves these goals:

- **Disk sharing is free.** Each VM root is an APFS copy-on-write clone (`cp -c`) of the
  base image — `clone_or_copy_root` (`src/runtime.rs:681`). Blocks are shared with the
  base until written; create is ~5 ms.
- **Memory is demand-paged by HVF.** A 2048 MiB VM peaks at ~433 MiB RSS (README "Startup
  Performance Notes"): the host backs only touched pages plus a boot-time zeroing cost.
- **Workspaces are zero-copy virtiofs**, per-VM and isolated (`src/libkrun.rs:129`).
- **Manual memory resize exists** via virtio-balloon + a per-VM control socket
  (`SandboxConfig::resize_enabled` at `src/config.rs:229`; balloon args at
  `src/libkrun.rs:92-101`; `resize_memory` in `src/runtime.rs`).

The gaps this design closes:

- **Per-VM efficiency:** RSS is a high-water mark. The balloon is opt-in/off by default
  (ceiling defaults equal to initial — `src/config.rs:209-213`) and only moves on an
  explicit `resize`. Idle long-lived agents never return touched pages.
- **Max density:** `SandboxManager` (`src/runtime.rs`) has no host-level accounting; nothing
  prevents oversubscribing RAM into swap. `list()` enumerates state dirs with no notion of
  "running" or a budget.
- **Scheduling overhead:** every session is a ~1.3 s cold boot; no warm pool or snapshot.
- **Cross-VM sharing:** only the root disk shares pages, and only at clone time. Read-only
  toolchains/caches and the host page cache are not shared across running VMs.

---

## Design

### A. Automatic memory reclaim — free-page reporting

**Goal: per-VM efficiency, density. [krun] + [repo].**

Turn the balloon from a manual lever into continuous reclaim using virtio-balloon **free
page reporting** (`VIRTIO_BALLOON_F_REPORTING`): the guest kernel hands freed pages back
and the VMM `MADV_FREE`s them, so host RSS falls as the working set shrinks.

- **[krun]** Build the guest kernel with `CONFIG_PAGE_REPORTING=y` (it already has
  `CONFIG_VIRTIO_BALLOON`). In `examples/os_mode` + libkrun, negotiate
  `VIRTIO_BALLOON_F_REPORTING` and madvise-free reported page ranges. Add a launcher flag
  `--balloon-free-page-reporting` (default on for our images).
- **[repo]** Decouple "balloon present for reclaim" from "balloon headroom for growth".
  Today the balloon device is only attached when `memory_max_mib > memory_mib`
  (`src/libkrun.rs:92`). Always attach the balloon + control socket so reporting works even
  when ceiling == initial. Keep the inflate/deflate growth path gated on headroom as
  before.

Result: per-VM footprint tracks the live working set without operator action, raising
density at unchanged configured limits.

### B. Host memory admission control / scheduler

**Goal: max density. [repo].**

Add single-host accounting so the manager packs VMs safely and refuses/queues when full.

- A `ResourceBudget` (new, in `src/runtime.rs` or a new `src/scheduler.rs`): host memory
  budget from `TB_SANDBOX_MEM_BUDGET_MIB` (default: a fraction of physical RAM, queried via
  `sysctl hw.memsize` on macOS), minus a host reserve.
- Track **running** VMs and their reservations. Reserve on `memory_min_mib` / a working-set
  estimate (not the ceiling), because HVF is demand-paged and #A returns idle pages.
- Admission on `shell` / `launch_piped` (`src/runtime.rs:275`, `:352`): admit if
  `sum(reservations) + new VM <= budget`; otherwise return a typed `Error` (e.g.
  `ResourceExhausted`) or block on a queue, selectable via flag.
- Feedback signal: sample live VMM RSS (the technique in `scripts/profile-sandbox.sh`)
  periodically to correct static reservations.
- Surface state: extend `SandboxSummary` / `tb-sandbox list` (`src/runtime.rs:516`) and the
  SDK with running-state + live-RSS columns so headroom is visible.

### C. Shared read-only toolchain / cache mounts

**Goal: cross-VM sharing, density. [repo] (pairs with D).**

Stop baking heavy, identical assets into every image variant and every COW root. Mount a
**shared host directory read-only** into all VMs via virtiofs: language toolchains, package
caches (`~/.npm`, cargo, pip), model assets. Writes stay per-VM.

- Add a first-class "shared asset mount" concept alongside `WorkspaceMount`
  (`src/config.rs:14`). It reuses the existing `--virtiofs-ro` emission
  (`src/libkrun.rs:137`) and the guest mount loop in `guest/agent-sandbox-shell` (driven by
  `mounts.tsv`).
- Ship a default set + `--shared-mount TAG=HOST_PATH[:guest=/path]` CLI flag and a config
  default. Provide a writable per-VM overlay dir for caches that need writes (tmpfs or a
  per-VM dir), so the shared copy stays read-only.
- Trim `scripts/build-agent-image.sh` variants to stop duplicating these assets into each
  rootfs once they're shared mounts, shrinking image size and COW divergence.

### D. virtiofs DAX — shared host page cache

**Goal: cross-VM sharing, density. [krun] + [repo].**

DAX maps virtiofs file pages directly from the host page cache instead of copying into
guest RAM. *N* VMs reading the same read-only files then share the host pages **once**,
fully isolated. This is the realistic shared-read-only-memory path on macOS/HVF.

- **[krun]** Enable the virtiofs DAX window in libkrun/`os_mode` and guest
  (`CONFIG_FUSE_DAX`); expose a `--virtiofs-dax-size` knob.
- **[repo]** Opt read-only mounts into DAX once supported: the #C shared mounts,
  `agent-config`, and credentials (`src/libkrun.rs:137-149`). With DAX the marginal memory
  cost of a shared mount per extra VM approaches zero.

### E. Drop the initramfs

**Goal: scheduling, density. [krun] (image-build side [repo]).**

The README documents this as the single biggest startup lever. The guest kernel currently
loads `virtio_blk` from a 25 MB initramfs.

- **[krun]** Rebuild the guest kernel with `CONFIG_VIRTIO_BLK=y` + `CONFIG_EXT4_FS=y` so
  `os_mode` mounts `/dev/vda` directly, removing the initramfs load (pre-kernel) and the
  ~345 ms in-guest initramfs phase.
- **[repo]** Make the initramfs optional in the image manifest/build
  (`src/image.rs`, `src/libkrun.rs:87`, `scripts/build-agent-image.sh`) so images can ship
  without one when the kernel supports it. Faster boot = cheaper scheduling and less
  transient per-boot memory.

### F. Warm pool of pre-booted VMs

**Goal: low scheduling overhead. [repo] (+[krun] for the clean version).**

Keep a small pool of booted, balloon-deflated VMs; hand one out on `create`/`shell` and
attach the real workspace on assignment. Idle cost is ~430 MiB each today (less after #A),
so the pool is sized against the #B budget.

The hard part is late workspace attachment — virtiofs is set at launch (`src/libkrun.rs:129`).
Two options:

1. **Guest-side bind** [repo-only, near-term]: boot pooled VMs with a generic placeholder
   mount, then bind/remount the assigned workspace inside the guest via the control channel
   (`guest/agent-sandbox-shell` + the runtime-control mount).
2. **virtiofs hot-plug** [krun]: attach the workspace device after boot. Cleaner but needs
   launcher support.
3. **Snapshot/restore** [krun]: collapses most of boot. This is now
   code-complete; the remaining repo work is deciding when the scheduler should
   choose restore-backed startup over a pre-booted warm-pool VM.

Pool management lives in `SandboxManagerSdk` (`src/sdk.rs`), which already tracks per-VM
processes and lifecycle.

### G. Density-aware defaults

**Goal: efficiency, density. [repo].**

- Default `cpus` to 1 (`src/config.rs:209`): vCPUs are threads and oversubscribe fine; 1
  CPU only costs ~60 ms boot. Keep `--cpus` for parallel-build workloads.
- Keep `memory_mib` default at 2048 (documented startup sweet spot). Once #A lands, set a
  higher *ceiling* with a lower initial so VMs grow on demand while idle cost stays small.

### H. Idle auto-reclaim loop

**Goal: efficiency. [repo]. Interim to / complements #A.**

Host-side policy that periodically issues `resize` downward on VMs idle beyond a threshold
(no active session/process — the SDK already tracks processes per VM in `src/sdk.rs`).
Reclaims high-water-mark RSS from long-lived idle sandboxes until free-page reporting (#A)
makes it unnecessary. Implement as an opt-in background task in `SandboxManagerSdk` reusing
`resize_memory` (`src/runtime.rs:478`).

---

## Implementation plan

Phased so each phase is independently shippable and measurable. Repo-only phases land
first; libkrun-coupled phases follow.

### Phase 1 — Density safety + cheap sharing (repo only) — IMPLEMENTED

Highest value with no libkrun dependency. Landed in `src/scheduler.rs` (new),
with wiring in `src/runtime.rs`, `src/cli.rs`, `src/sdk.rs`, `src/config.rs`, and
`src/error.rs`; documented under "Run Many Sandboxes (Density)" in the README.
Items 1–4 below are done; the idle auto-reclaim (item 4) is a library opt-in
(`SandboxManagerSdk::spawn_idle_reclaim`).

1. **Scheduler / admission control (#B).**
   - New `src/scheduler.rs`: `ResourceBudget`, host-memory detection (`sysctl hw.memsize`),
     reservation map keyed by `SandboxId`.
   - Wire into `SandboxManager::shell` / `launch_piped` (`src/runtime.rs:275`, `:352`):
     reserve on admit, release on exit (alongside existing proxy/gvproxy cleanup).
   - New `Error::ResourceExhausted`; `--admission=reject|queue` flag and
     `TB_SANDBOX_MEM_BUDGET_MIB`.
   - Extend `SandboxSummary` + `list()` (`src/runtime.rs:516`) and CLI output with
     state + live RSS.
2. **Density-aware defaults (#G).** Change defaults in `src/config.rs:209-213` (cpus → 1);
   document ceiling/initial guidance.
3. **Shared read-only mounts (#C).** Add shared-asset mount type in `src/config.rs`,
   `--shared-mount` parsing in `src/cli.rs`, emission in `src/libkrun.rs`, default set;
   adjust `scripts/build-agent-image.sh` to stop duplicating shared assets.
4. **Idle auto-reclaim (#H).** Opt-in background task in `src/sdk.rs` reusing
   `resize_memory`.

**Exit criteria:** N concurrent VMs are admitted up to the budget and cleanly
refused/queued beyond it; `list` shows live RSS; a shared toolchain mount is read-only in
the guest and writes never cross VMs.

### Phase 2 — Automatic reclaim (#A) — IMPLEMENTED (guest kernel rebuild pending)

Discovery during implementation: libkrun's virtio-balloon device **already**
advertises `VIRTIO_BALLOON_F_REPORTING` (`src/devices/.../balloon/device.rs`
`AVAIL_FEATURES`), its free-page-reporting queue handler already madvise-frees
reported pages (`process_frq`), and the balloon device is attached to **every**
VM unconditionally (`src/vmm/src/builder.rs:977`). So no os_mode or wrapper code
change is needed for reporting — it is gated only by the *guest kernel*.

5. **Free-page reporting (#A).**
   - **[krun]** Added `CONFIG_VIRTIO_BALLOON=y` + `CONFIG_PAGE_REPORTING=y` to the
     canonical guest kernel config fragment in
     `design_docs/os_mode_guest_image.md`, with a note explaining the device-side
     is already wired. **Remaining external step:** rebuild the guest kernel with
     these options (cannot be done in this environment).
   - This is independent of the runtime *resize* path, which already works and is
     unchanged.

**Exit criteria:** an idle VM's RSS falls back toward its working set within seconds after
a memory-heavy task; measured via `profile-sandbox.sh` RSS sampling on a kernel built with
the two options above.

### Phase 3 — Faster boot (#E) — IMPLEMENTED (guest kernel rebuild pending)

6. **Drop initramfs (#E).**
   - **[repo]** `src/image.rs` already treats the initramfs as optional, and
     `src/libkrun.rs` only passes `--initramfs` when present. Added
     `NO_INITRAMFS=1` to `scripts/build-agent-image.sh` to emit a direct-boot
     image (no initramfs file, no `initramfs` key in `image.json`).
   - **[krun]** The canonical config fragment already lists `CONFIG_VIRTIO_BLK=y`
     + `CONFIG_EXT4_FS=y` (built-in root drivers). **Remaining external step:**
     build/ship a guest kernel with those built-in so `/dev/vda` mounts without
     an initramfs.

**Exit criteria:** cold-boot drops ~345 ms in-guest + the initramfs pre-kernel load in
`profile-sandbox.sh ITERATIONS=N`, using a `NO_INITRAMFS=1` image on a built-in-driver kernel.

### Phase 4 — Highest-ceiling sharing + scheduling — IMPLEMENTED (DAX kernel + on-VM verify pending)

7. **virtiofs DAX (#D).** Discovery: `krun_add_virtiofs3(... shm_size ...)` already
   exposes a DAX window; os_mode just hard-coded `shm_size=0`.
   - **[krun]** Added `--virtiofs-dax-size` to `examples/os_mode.c` and route both
     RO and RW mounts through `krun_add_virtiofs3` with the window size.
   - **[repo]** Added `SandboxConfig::virtiofs_dax_size`, the `--virtiofs-dax-size`
     CLI flag (accepts `1G` etc.), persisted in `sandbox.json`, emitted as
     `--virtiofs-dax-size` from `src/libkrun.rs`. Added `CONFIG_FUSE_DAX=y` (+ deps)
     to the kernel config fragment. **Remaining external step:** guest kernel with
     `CONFIG_FUSE_DAX` and an on-VM measurement of shared page cache.
8. **Warm pool (#F).** [repo] `WarmPool` in `src/sdk.rs`: pre-boots sandboxes from a
   template config and hands them out via `acquire()`, refilling in the background;
   admission control still applies per VM. Unit-tested with the fake-launcher harness.
   Per-session late workspace attach (guest-side bind / virtiofs hot-plug / snapshot)
   remains the follow-on so the pool can serve arbitrary per-session host directories;
   today it fits interchangeable sandboxes (shared or no workspace).

**Exit criteria:** shared-mount memory cost is ~flat across VM count (DAX); `create`→ready
served from the warm pool is dramatically faster than cold.

### On-VM verification results (real guest boot)

Verified by booting a real OS-mode Debian aarch64 guest on this macOS/HVF host with the
prebuilt libkrunfw 6.12.87 kernel — whose `.config` already has `CONFIG_PAGE_REPORTING`,
`CONFIG_VIRTIO_BALLOON`, built-in `CONFIG_VIRTIO_BLK`/`CONFIG_EXT4_FS`, and
`CONFIG_FUSE_DAX`/`CONFIG_VIRTIO_FS`/`CONFIG_DAX` all `=y`. The os_mode launcher
(with the `--virtiofs-dax-size` change) compiles, links, and runs against the
freshly built `libkrun.dylib`.

- **#E direct boot (no initramfs): PASS.** Booting with no `--initramfs` reached
  `KRUN_OSMODE: ready` in ~1.7 s with `EXT4-fs (vda)` mounted directly from `/dev/vda`.
- **#D virtio-fs DAX: PASS.** `--virtiofs-dax-size $((512*1024*1024))` produced a guest
  dmesg line `virtiofs virtio3: Cache len: 0x20000000` (exactly the 512 MiB window), and
  the read-only mount read back its shared file end-to-end.
- **#A free-page reporting: pipeline verified; macOS host reclaim needs an HVF unmap.**
  With libkrun DEBUG logging, the guest kernel reported freed pages and the host
  balloon free-page-reporting handler fired (`process_frq`, 719 × 4 MiB descriptors),
  i.e. `CONFIG_PAGE_REPORTING` works and the device handler runs. While verifying, found
  and fixed a real macOS bug: the handler used `madvise(MADV_DONTNEED)`, which is a no-op
  for anonymous memory on macOS; switched to `MADV_FREE_REUSABLE`
  (`src/devices/src/virtio/balloon/device.rs`, gated `#[cfg(target_os = "macos")]`), which
  a standalone test confirms reclaims anon memory (513 → 1 MiB). On **Linux/KVM** this
  path reclaims. On **macOS/HVF**, host RSS still does not drop because all guest RAM is
  mapped into the guest up front via `hv_vm_map` and stays resident; reclaiming it needs
  `hv_vm_unmap` of the reported regions **plus a lazy-remap data-abort handler** in the
  HVF vcpu loop (`src/vmm/src/macos/vstate.rs`), since the guest reuses reported pages
  later. That is a hypervisor-core change with real crash risk and is intentionally left
  as the one remaining macOS-host item rather than rushed.

### Remaining work

- **macOS physical reclaim for #A:** `hv_vm_unmap` reported ranges + on-fault remap in the
  HVF vcpu loop (above). Largest, riskiest item; deferred deliberately.
- **Warm pool late workspace attach (#F):** guest-side bind / virtio-fs hot-plug so a
  pooled VM can take an arbitrary per-session workspace.
- **Snapshot/restore-backed scheduling (#F):** libkrun snapshot/restore is now
  code-complete; the remaining scheduling work is product policy — when to
  prefer warm-pool reuse vs. restoring from a Full snapshot, and how to measure
  both paths under real workloads.

---

## Verification

Use the existing harness (`scripts/profile-sandbox.sh` with `RESOURCE_CSV=` and
`ITERATIONS=`) for every phase.

- **Per-VM efficiency (#A, #H):** boot, run a memory-heavy task, idle; sample VMM RSS over
  time. Expect RSS to fall back toward the working set (today it stays at the high-water
  mark).
- **Density (#B, #G):** launch N concurrent VMs until the budget is hit; confirm admission
  refuses/queues instead of swapping. Compare `sum(RSS)` and max-concurrent before/after.
- **Sharing (#C, #D):** mount a large shared RO toolchain into M VMs; compare host RSS /
  page-cache footprint for 1 vs M. With DAX the marginal cost per extra VM approaches zero.
- **Scheduling (#E, #F):** `profile-sandbox.sh ITERATIONS=N` for boot timing (#E); wall-clock
  `create`→ready from warm pool vs cold (#F).

**Isolation regression check (every phase):** shared mounts are read-only from the guest;
per-VM writes never leak across VMs; the COW root behavior in `clone_or_copy_root`
(`src/runtime.rs:681`) is unchanged.

## Open questions

- Reservation basis for the scheduler: static `memory_min_mib`, a learned working-set
  estimate, or live-RSS feedback with hysteresis? Start static, refine with feedback.
- Default host reserve fraction for `TB_SANDBOX_MEM_BUDGET_MIB` on typical dev machines.
- Warm-pool late-attach: ship the guest-side bind first, or wait for virtiofs hot-plug in
  libkrun?
- DAX window sizing vs. host memory pressure.
