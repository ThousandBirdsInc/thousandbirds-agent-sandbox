#!/bin/sh
set -eu

# profile-sandbox.sh boots an agent sandbox to a guest-side "ready" marker,
# measures startup time, and samples the VM process's CPU and memory use.
#
# It drives `tb-sandbox shell` with TB_SANDBOX_PROFILE=1 (which makes the
# wrapper emit `tb-sandbox-profile <phase>=<unix_millis>` markers) and an
# --initial-command that prints a unique ready marker once the guest login
# shell is reached. The VM is started with stdin from /dev/null so it never
# blocks on interactive input; this script terminates it once it has the data
# it needs.

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

IMAGE=${IMAGE:-${TB_SANDBOX_IMAGE:-}}
WORKSPACE=${WORKSPACE:-"$ROOT_DIR"}
ITERATIONS=${ITERATIONS:-1}
SAMPLE_INTERVAL=${SAMPLE_INTERVAL:-0.25}
READY_TIMEOUT=${READY_TIMEOUT:-120}
SETTLE_SECONDS=${SETTLE_SECONDS:-3}
PROFILE=${PROFILE:-release}
TB_SANDBOX_BIN=${TB_SANDBOX_BIN:-}
RESOURCE_CSV=${RESOURCE_CSV:-}
KEEP_LOGS=${KEEP_LOGS:-0}
SYSTEMD_ANALYZE=${SYSTEMD_ANALYZE:-0}

READY_MARKER="TB_SANDBOX_PROFILE_READY"

usage() {
  cat <<EOF
Usage: $0 [--image PATH] [-- EXTRA_TB_SANDBOX_ARGS...]

Profile sandbox startup time and VM resource utilization. Boots a sandbox,
waits for a guest-side ready marker, samples the VM process, then tears it down.

Options:
  --image PATH                 Agent image directory (default: \$IMAGE / \$TB_SANDBOX_IMAGE)
  -h, --help                   Show this help

Everything after -- is forwarded verbatim to \`tb-sandbox shell\`, e.g.:
  $0 --image target/agent-images/debian-bookworm -- --gvproxy --credentials claude

Environment:
  IMAGE=PATH                   Agent image directory (required unless --image given)
  WORKSPACE=PATH               Host workspace mounted at /workspace (default: repo root)
  ITERATIONS=N                 Number of boot/teardown cycles to run (default: 1)
  SAMPLE_INTERVAL=SECONDS      Resource sampling interval (default: 0.25)
  READY_TIMEOUT=SECONDS        Max wait for the ready marker per iteration (default: 120)
  SETTLE_SECONDS=SECONDS       Keep sampling after ready before teardown (default: 3)
  PROFILE=debug|release        Cargo profile to build/run (default: release)
  TB_SANDBOX_BIN=PATH          Use a prebuilt tb-sandbox binary instead of building
  RESOURCE_CSV=PATH            Append per-sample resource rows (iteration,ms,rss_kib,cpu_pct)
  KEEP_LOGS=1                  Keep per-iteration logs under a temp dir and print the path
  SYSTEMD_ANALYZE=1            Dump 'systemd-analyze blame' from the guest on iteration 1

  Any TB_SANDBOX_* the wrapper honors (e.g. TB_SANDBOX_RUNNER, TB_SANDBOX_GVPROXY)
  is passed through to the child process.
EOF
}

FORWARD_ARGS=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --image) IMAGE=$2; shift 2 ;;
    --image=*) IMAGE=${1#*=}; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift; break ;;
    *) printf 'profile-sandbox: unexpected argument %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done
# Remaining args (after --) are forwarded to tb-sandbox shell.
FORWARD_ARGS="$*"

if [ -z "$IMAGE" ]; then
  printf 'profile-sandbox: --image (or IMAGE/TB_SANDBOX_IMAGE) is required\n' >&2
  exit 2
fi

# now_ms prints the current wall-clock time in milliseconds. macOS `date` has no
# %N, so prefer python3/perl and fall back to second resolution.
if command -v python3 >/dev/null 2>&1; then
  now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
elif command -v perl >/dev/null 2>&1; then
  now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'; }
else
  now_ms() { printf '%s000\n' "$(date +%s)"; }
fi

# Resolve the binary. Build once up front so build time is never counted as
# startup time.
if [ -n "$TB_SANDBOX_BIN" ]; then
  BIN=$TB_SANDBOX_BIN
else
  printf 'profile-sandbox: building tb-sandbox (%s profile)...\n' "$PROFILE" >&2
  if [ "$PROFILE" = "release" ]; then
    ( cd "$ROOT_DIR" && cargo build --release --bin tb-sandbox >&2 )
    BIN="$ROOT_DIR/target/release/tb-sandbox"
  else
    ( cd "$ROOT_DIR" && cargo build --bin tb-sandbox >&2 )
    BIN="$ROOT_DIR/target/debug/tb-sandbox"
  fi
fi
if [ ! -x "$BIN" ]; then
  printf 'profile-sandbox: tb-sandbox binary not found at %s\n' "$BIN" >&2
  exit 1
fi

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/tb-sandbox-profile.XXXXXX")
cleanup_workdir() {
  if [ "$KEEP_LOGS" = "1" ]; then
    printf 'profile-sandbox: logs kept in %s\n' "$WORKDIR" >&2
  else
    rm -rf "$WORKDIR"
  fi
}
trap cleanup_workdir EXIT INT TERM

if [ -n "$RESOURCE_CSV" ] && [ ! -f "$RESOURCE_CSV" ]; then
  printf 'iteration,elapsed_ms,rss_kib,cpu_pct\n' > "$RESOURCE_CSV"
fi

# find_vm_pid <root_pid> prints the pid of the VM runner process spawned as a
# descendant of the wrapper, or nothing if it is not up yet.
find_vm_pid() {
  root=$1
  for child in $(pgrep -P "$root" 2>/dev/null || true); do
    cmd=$(ps -o command= -p "$child" 2>/dev/null || true)
    case "$cmd" in
      *tb-sandbox-runner*|*os_mode*) printf '%s\n' "$child"; return 0 ;;
    esac
    # Recurse one level: cargo/run wrappers can interpose a process.
    sub=$(find_vm_pid "$child" || true)
    if [ -n "$sub" ]; then
      printf '%s\n' "$sub"; return 0
    fi
  done
  return 0
}

# Aggregation accumulators (space-separated lists for awk at the end).
total_list=""
boot_list=""
host_list=""
rss_list=""
cpu_list=""
prekernel_list=""
guest_list=""

run_iteration() {
  iter=$1
  log="$WORKDIR/iter-$iter.log"
  state_dir="$WORKDIR/state-$iter"
  cache_dir="$WORKDIR/cache-$iter"
  : > "$log"

  # The initial command runs in the guest once the login session is reached. It
  # records the guest's uptime (seconds since kernel boot) so we can split host
  # pre-kernel time from in-guest boot time, optionally dumps systemd-analyze, then
  # prints the ready marker the host watches for.
  initial_command="printf 'tb-sandbox-profile guest_ready=%s\\n' \"\$(cut -d' ' -f1 /proc/uptime)\";"
  if [ "$SYSTEMD_ANALYZE" = "1" ] && [ "$iter" = "1" ]; then
    initial_command="$initial_command printf '===SYSTEMD-ANALYZE-BLAME===\\n'; systemd-analyze blame 2>/dev/null | head -20; printf '===END-BLAME===\\n';"
  fi
  initial_command="$initial_command printf '%s\\n' '$READY_MARKER'"

  # Launch the sandbox in the background with stdin detached. Output (stdout +
  # stderr, including profile markers) is captured to the log.
  # shellcheck disable=SC2086
  TB_SANDBOX_PROFILE=1 \
  TB_SANDBOX_STATE_DIR="$state_dir" \
  TB_SANDBOX_CACHE_DIR="$cache_dir" \
    "$BIN" shell \
      --image "$IMAGE" \
      --workspace "$WORKSPACE" \
      --ephemeral \
      --initial-command "$initial_command" \
      $FORWARD_ARGS \
      < /dev/null > "$log" 2>&1 &
  tb_pid=$!

  launch_ms=$(now_ms)
  ready_ms=""
  peak_rss=0
  peak_cpu=0
  vm_pid=""

  deadline=$(( $(now_ms) + READY_TIMEOUT * 1000 ))
  while :; do
    # Stop early if the wrapper died before signalling ready.
    if ! kill -0 "$tb_pid" 2>/dev/null; then
      if [ -z "$ready_ms" ]; then
        printf 'profile-sandbox: iteration %s: wrapper exited before ready marker\n' "$iter" >&2
        printf '----- last 20 log lines -----\n' >&2
        tail -n 20 "$log" >&2 || true
        return 1
      fi
      break
    fi

    if [ -z "$vm_pid" ]; then
      vm_pid=$(find_vm_pid "$tb_pid" || true)
    fi

    # Sample VM resource use.
    if [ -n "$vm_pid" ] && kill -0 "$vm_pid" 2>/dev/null; then
      sample=$(ps -o rss=,%cpu= -p "$vm_pid" 2>/dev/null || true)
      if [ -n "$sample" ]; then
        rss=$(printf '%s\n' "$sample" | awk '{print $1}')
        cpu=$(printf '%s\n' "$sample" | awk '{print $2}')
        if [ -n "$rss" ] && awk -v a="$rss" -v b="$peak_rss" 'BEGIN{exit !(a>b)}'; then peak_rss=$rss; fi
        if [ -n "$cpu" ] && awk -v a="$cpu" -v b="$peak_cpu" 'BEGIN{exit !(a>b)}'; then peak_cpu=$cpu; fi
        if [ -n "$RESOURCE_CSV" ]; then
          printf '%s,%s,%s,%s\n' "$iter" "$(( $(now_ms) - launch_ms ))" "$rss" "$cpu" >> "$RESOURCE_CSV"
        fi
      fi
    fi

    # Detect the guest ready marker.
    if [ -z "$ready_ms" ] && grep -q "$READY_MARKER" "$log" 2>/dev/null; then
      ready_ms=$(now_ms)
      printf 'profile-sandbox: iteration %s: guest ready (settling %ss)\n' "$iter" "$SETTLE_SECONDS" >&2
      settle_deadline=$(( $(now_ms) + SETTLE_SECONDS * 1000 ))
    fi

    # After ready + settle window, tear down.
    if [ -n "$ready_ms" ] && [ "$(now_ms)" -ge "$settle_deadline" ]; then
      break
    fi

    if [ "$(now_ms)" -ge "$deadline" ]; then
      printf 'profile-sandbox: iteration %s: timed out after %ss waiting for ready marker\n' "$iter" "$READY_TIMEOUT" >&2
      kill "$tb_pid" 2>/dev/null || true
      return 1
    fi

    sleep "$SAMPLE_INTERVAL"
  done

  # Tear down the wrapper and its VM/network children.
  kill "$tb_pid" 2>/dev/null || true
  [ -n "$vm_pid" ] && kill "$vm_pid" 2>/dev/null || true
  wait "$tb_pid" 2>/dev/null || true

  # Pull host-side phase markers emitted by the wrapper.
  start_phase=$(awk -F= '/^tb-sandbox-profile run_shell_start=/{print $2}' "$log" | tail -n 1)
  created_phase=$(awk -F= '/^tb-sandbox-profile created=/{print $2}' "$log" | tail -n 1)
  launch_phase=$(awk -F= '/^tb-sandbox-profile launch=/{print $2}' "$log" | tail -n 1)

  : "${start_phase:=$launch_ms}"
  : "${launch_phase:=$launch_ms}"

  total_ms=$(( ready_ms - start_phase ))
  boot_ms=$(( ready_ms - launch_phase ))
  host_ms=$(( launch_phase - start_phase ))
  create_ms="-"
  if [ -n "$created_phase" ]; then
    create_ms=$(( created_phase - start_phase ))
  fi

  # Guest-side markers carry uptime (seconds since kernel boot). guest_ready is
  # always injected via the initial command, so the pre-kernel/in-guest split
  # works on any image. The finer guest_* stages need an image built with the
  # instrumented agent-sandbox-shell.
  g_ready=$(awk -F= '/tb-sandbox-profile guest_ready=/{print $2}' "$log" | tail -n1 | tr -cd '0-9.')
  g_shell=$(awk -F= '/tb-sandbox-profile guest_shell_start=/{print $2}' "$log" | tail -n1 | tr -cd '0-9.')
  g_precred=$(awk -F= '/tb-sandbox-profile guest_pre_credentials=/{print $2}' "$log" | tail -n1 | tr -cd '0-9.')
  g_postcred=$(awk -F= '/tb-sandbox-profile guest_post_credentials=/{print $2}' "$log" | tail -n1 | tr -cd '0-9.')

  prekernel_ms="-"; guest_ms="-"
  if [ -n "$g_ready" ]; then
    guest_ms=$(awk -v s="$g_ready" 'BEGIN{printf "%d", s*1000}')
    prekernel_ms=$(( boot_ms - guest_ms ))
  fi

  printf '\n[iteration %s]\n' "$iter"
  printf '  host create:          %6s ms\n' "$create_ms"
  printf '  host prelaunch:       %6s ms  (config + backend/proxy setup)\n' "$host_ms"
  printf '  pre-kernel VMM init:  %6s ms  (runner + libkrun VM create, before kernel)\n' "$prekernel_ms"
  printf '  guest kernel->ready:  %6s ms  (kernel + systemd + shell service + login)\n' "$guest_ms"
  if [ -n "$g_shell" ]; then
    gs=$(awk -v s="$g_shell" 'BEGIN{printf "%d", s*1000}')
    printf '    boot->shell svc:    %6s ms  (kernel + systemd to shell service)\n' "$gs"
    printf '    shell-service tail: %6s ms  (mounts + creds + login handoff)\n' "$(( guest_ms - gs ))"
    if [ -n "$g_precred" ] && [ -n "$g_postcred" ]; then
      printf '      credential import:%6s ms\n' "$(awk -v a="$g_precred" -v b="$g_postcred" 'BEGIN{printf "%d",(b-a)*1000}')"
    fi
  fi
  printf '  total startup:        %6s ms\n' "$total_ms"
  printf '  VM peak RSS:          %6s MiB\n' "$(awk -v k="$peak_rss" 'BEGIN{printf "%.0f", k/1024}')"
  printf '  VM peak CPU:          %6s %%\n' "$peak_cpu"

  total_list="$total_list $total_ms"
  boot_list="$boot_list $boot_ms"
  host_list="$host_list $host_ms"
  rss_list="$rss_list $peak_rss"
  cpu_list="$cpu_list $peak_cpu"
  if [ "$prekernel_ms" != "-" ]; then prekernel_list="$prekernel_list $prekernel_ms"; fi
  if [ "$guest_ms" != "-" ]; then guest_list="$guest_list $guest_ms"; fi
}

printf 'profile-sandbox: image=%s workspace=%s iterations=%s\n' "$IMAGE" "$WORKSPACE" "$ITERATIONS" >&2

failures=0
i=1
while [ "$i" -le "$ITERATIONS" ]; do
  if ! run_iteration "$i"; then
    failures=$((failures + 1))
  fi
  i=$((i + 1))
done

summarize() {
  label=$1
  unit=$2
  shift 2
  printf '%s' "$*" | tr ' ' '\n' | grep -v '^$' | sort -n | awk -v label="$label" -v unit="$unit" '
    { vals[n++]=$1; sum+=$1 }
    END {
      if (n==0) { printf "  %-18s (no data)\n", label; exit }
      med = (n%2) ? vals[int(n/2)] : (vals[n/2-1]+vals[n/2])/2
      printf "  %-18s min %.0f  median %.0f  avg %.0f  max %.0f %s\n", label, vals[0], med, sum/n, vals[n-1], unit
    }'
}

if [ "$ITERATIONS" -gt 1 ]; then
  printf '\n=== summary over %s iterations ===\n' "$ITERATIONS"
  summarize "total startup" "ms" "$total_list"
  summarize "pre-kernel VMM" "ms" "$prekernel_list"
  summarize "guest->ready" "ms" "$guest_list"
  summarize "host prelaunch" "ms" "$host_list"
  summarize "VM peak RSS" "KiB" "$rss_list"
  summarize "VM peak CPU" "%" "$cpu_list"
fi

if [ "$failures" -gt 0 ]; then
  printf '\nprofile-sandbox: %s of %s iteration(s) failed\n' "$failures" "$ITERATIONS" >&2
  exit 1
fi
