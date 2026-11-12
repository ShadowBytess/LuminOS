#!/usr/bin/env bash
# Runs the real init as PID 1 of a private PID + mount namespace (no --dev), to
# exercise what only PID 1 does: mounting the base filesystems, receiving
# signals as PID 1, reaping orphans, surviving a broken configuration, and the
# shutdown sequence.
#
# Needs root, unshare and nsenter. The kernel handles reboot() inside a child
# PID namespace by simply ending that namespace's init, so this cannot reboot
# or power off the machine it runs on.
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INIT="${INIT:-$ROOT/target/release/init}"
CTL="${CTL:-$ROOT/target/release/luminosctl}"
[[ -x "$INIT" && -x "$CTL" ]] || { echo "run 'cargo build --release' first (or set INIT and CTL)" >&2; exit 2; }
if [[ $EUID -ne 0 ]] || ! command -v unshare >/dev/null || ! command -v nsenter >/dev/null; then
    echo "skipped: needs root, unshare and nsenter"; exit 0
fi

pass=0; fail=0
check() {
    local desc="$1"; shift
    if "$@"; then echo "ok    - $desc"; pass=$((pass + 1))
    else echo "FAIL  - $desc"; fail=$((fail + 1)); fi
}

UNSHARE_PID=""; INIT_HOST=""
cleanup() {
    [[ -n "$INIT_HOST" ]] && kill -9 "$INIT_HOST" 2>/dev/null
    [[ -n "$UNSHARE_PID" ]] && kill -9 "$UNSHARE_PID" 2>/dev/null
    pkill -9 -f 'sleep 777[78]' 2>/dev/null
}
trap cleanup EXIT

inns()  { nsenter -t "$INIT_HOST" -m -p -- "$@"; }
state() { inns "$CTL" status "$1" 2>/dev/null | sed -n "s/^OK $1: \([a-z]*\).*/\1/p"; }
is_state() { [[ "$(state "$1")" == "$2" ]]; }
wait_for() {  # wait_for command args...   (polls for up to 10s)
    for _ in $(seq 1 100); do "$@" && return 0; sleep 0.1; done; return 1
}
gone()    { ! kill -0 "$1" 2>/dev/null; }
zombies() {  # zombie children of init
    local n=0 p
    for p in /proc/[0-9]*; do
        read -r _ _ st ppid _ < "$p/stat" 2>/dev/null || continue
        [[ "$st" == Z && "$ppid" == "$INIT_HOST" ]] && n=$((n + 1))
    done
    echo "$n"
}

start_ns() {  # start_ns SERVICES_DIR
    # /etc is replaced by a tmpfs holding only the given service files; init is
    # exec'd so that it is the namespace's first process and therefore PID 1.
    # The extra word after init is what a kernel command line can pass to it.
    unshare --pid --fork --mount --mount-proc --kill-child bash -c '
        mount -t tmpfs tmpfs /etc && mkdir -p /etc/luminos/services &&
        { cp "$1"/*.toml /etc/luminos/services/ 2>/dev/null; true; } && exec "$2" unexpected-kernel-word
    ' _ "$1" "$INIT" >/dev/null 2>&1 &
    UNSHARE_PID=$!
    INIT_HOST=""
    for _ in $(seq 1 50); do
        INIT_HOST="$(pgrep -P "$UNSHARE_PID" | head -1)"
        [[ -n "$INIT_HOST" ]] && break
        sleep 0.1
    done
}

run_case() {  # run_case "name" trigger-function
    local name="$1" trigger="$2"
    echo "== $name =="
    start_ns "$ROOT/tests/services"
    check "init is PID 1 in its namespace"         bash -c "grep NSpid /proc/$INIT_HOST/status | awk '{print \$NF}' | grep -qx 1"
    check "survives unknown kernel args, serves control socket" wait_for is_state app running
    check "base is running"                         is_state base running
    check "oneshot prep finished"                   is_state prep done
    inns bash -c 'for i in 1 2 3; do (sleep 0.2 &); done' 2>/dev/null
    sleep 1
    check "orphans re-parented to init are reaped"  test "$(zombies)" -eq 0
    local pid="$INIT_HOST"
    $trigger
    check "init exits after shutdown"               wait_for gone "$pid"
    check "no service processes left"               bash -c "! pgrep -f 'sleep 777[78]' >/dev/null"
    UNSHARE_PID=""; INIT_HOST=""
    pkill -9 -f 'sleep 777[78]' 2>/dev/null
}

by_sigusr2() { kill -USR2 "$INIT_HOST"; }              # what busybox `poweroff` sends
by_ctl()     { inns "$CTL" poweroff >/dev/null; }      # the control socket
by_sigint()  { kill -INT "$INIT_HOST"; }               # ctrl-alt-del

run_case "poweroff by SIGUSR2" by_sigusr2
run_case "poweroff by luminosctl" by_ctl
run_case "reboot by SIGINT"    by_sigint

# A broken configuration must never take PID 1 down (that would be a kernel panic).
echo "== broken configuration =="
BAD="$(mktemp -d)"
echo 'this is = not [valid toml' > "$BAD/garbage.toml"
printf 'name = "a"\ncommand = "/bin/true"\ndepends_on = ["b"]\n' > "$BAD/a.toml"
printf 'name = "b"\ncommand = "/bin/true"\ndepends_on = ["a"]\n' > "$BAD/b.toml"
start_ns "$BAD"
sleep 1
check "init stays up with only broken services" kill -0 "$INIT_HOST"
check "control socket still works"              inns "$CTL" status
check "emergency shell was registered"          bash -c "nsenter -t $INIT_HOST -m -p -- '$CTL' status | grep -q 'emergency-shell:'"
inns "$CTL" poweroff >/dev/null 2>&1
check "init exits after shutdown"               wait_for gone "$INIT_HOST"
UNSHARE_PID=""; INIT_HOST=""
rm -rf "$BAD"

echo
echo "passed: $pass   failed: $fail"
[[ $fail -eq 0 ]]
