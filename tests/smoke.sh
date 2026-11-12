#!/usr/bin/env bash
# End-to-end test: runs the real init in --dev mode against tests/services and
# drives it with the real luminosctl. Needs: cargo build --release
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INIT="${INIT:-$ROOT/target/release/init}"
CTL="${CTL:-$ROOT/target/release/luminosctl}"
[[ -x "$INIT" && -x "$CTL" ]] || { echo "run 'cargo build --release' first (or set INIT and CTL)" >&2; exit 2; }

WORK="$(mktemp -d)"
SOCK="$WORK/init.sock"
OUT="$WORK/init.out"
INIT_PID=""
cleanup() {
    [[ -n "$INIT_PID" ]] && kill -9 "$INIT_PID" 2>/dev/null
    pkill -9 -f 'sleep 777[789]' 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

pass=0; fail=0
check() {  # check "description" command...
    local desc="$1"; shift
    if "$@"; then echo "ok    - $desc"; pass=$((pass + 1))
    else echo "FAIL  - $desc"; fail=$((fail + 1)); fi
}
ctl()   { "$CTL" --socket "$SOCK" "$@"; }
# First word of a service's state: running stopped waiting stopping done failed
state() { ctl status "$1" 2>/dev/null | sed -n "s/^OK $1: \([a-z]*\).*/\1/p"; }
pidof_svc() { ctl status "$1" 2>/dev/null | sed -n 's/.*(pid \([0-9]*\)).*/\1/p'; }
wait_state() {  # wait_state name state [tenths-of-a-second]
    for _ in $(seq 1 "${3:-50}"); do [[ "$(state "$1")" == "$2" ]] && return 0; sleep 0.1; done
    return 1
}
line_of() { grep -n "$1" "$OUT" | head -1 | cut -d: -f1; }
before()  { local a b; a="$(line_of "$1")"; b="$(line_of "$2")"; [[ -n "$a" && -n "$b" && "$a" -lt "$b" ]]; }

# Logs go to ./build/logs in dev mode; run from WORK so tests leave no trace.
cd "$WORK" || exit 2
"$INIT" --dev --config "$ROOT/tests/services" --socket "$SOCK" >"$OUT" 2>&1 &
INIT_PID=$!
for _ in $(seq 1 50); do [[ -S "$SOCK" ]] && break; sleep 0.1; done

echo "== boot =="
check "control socket answers"                 ctl status
check "app reaches running"                    wait_state app running
check "base is running"                        test "$(state base)" = running
check "oneshot prep finished (done)"           test "$(state prep)" = done
check "start order: base, prep, app"           bash -c "$(declare -f line_of before); OUT='$OUT'; before 'started base' 'started prep' && before 'started prep' 'started app'"
check "enabled=false service stays stopped"    test "$(state flaky)" = stopped
check "init stayed up despite bad configs"     kill -0 "$INIT_PID"
check "cycle-a rejected, not fatal"            grep -q 'cycle-a: in or behind a dependency cycle' "$OUT"
check "unknown dependency rejected"            grep -q "orphan: depends on unknown service 'ghost'" "$OUT"
check "unknown key rejected"                   grep -q 'broken.toml: unknown field' "$OUT"
check "path-traversal name rejected"           grep -q "invalid service name '../evil'" "$OUT"
check "rejected services are not listed"       bash -c "! '$CTL' --socket '$SOCK' status | grep -qE '^(cycle-a|orphan|broken|../evil):'"
check "failed spawn does not block later services" wait_state after-failure running
check "failed spawn is reported as failed"     test "$(state failing-start)" = failed

echo "== stop cascades to dependents, dependents first =="
check "stop base succeeds"                     ctl stop base
check "app stopped"                            wait_state app stopped
check "base stopped"                           wait_state base stopped
check "finished oneshot reset by stop"         test "$(state prep)" = stopped
check "app stopped before base"                before 'app stopped' 'base stopped'
check "SIGTERM was used (not SIGKILL)"         grep -q 'base stopped (signal 15)' "$OUT"

echo "== start pulls in dependencies =="
check "start app succeeds"                     ctl start app
check "app running again"                      wait_state app running
check "base pulled in"                         test "$(state base)" = running
check "prep re-ran"                            test "$(state prep)" = done

echo "== restart =="
old="$(pidof_svc base)"
check "restart base succeeds"                  ctl restart base
check "base running with a new pid"            bash -c "for _ in \$(seq 1 50); do n=\$('$CTL' --socket '$SOCK' status base | sed -n 's/.*(pid \([0-9]*\)).*/\1/p'); [[ -n \$n && \$n != '$old' ]] && exit 0; sleep 0.1; done; exit 1"
check "dependent app left running"             test "$(state app)" = running

echo "== stopping kills the whole process group =="
check "start forker (shell with a background child)" ctl start forker
check "forker running"                         wait_state forker running
check "its background child exists"            bash -c "pgrep -f 'sleep 777[9]' >/dev/null"
check "stop forker"                            ctl stop forker
check "forker stopped"                         wait_state forker stopped
check "background child was killed too"        bash -c "! pgrep -f 'sleep 777[9]' >/dev/null"

echo "== crash handling and backoff =="
check "start flaky"                            ctl start flaky
sleep 5
runs="$(ctl log flaky 100 | grep -c 'flaky starting')"
check "flaky was restarted ($runs runs in 5s)" test "$runs" -ge 2
check "backoff slows restarts (not 5+ runs/5s)" test "$runs" -le 4
check "log shows init markers"                 bash -c "'$CTL' --socket '$SOCK' log flaky | grep -q '\[init\] exited (exit 1)'"
check "status shows pending restart or running" bash -c "'$CTL' --socket '$SOCK' status flaky | grep -qE 'waiting to restart|running'"
check "stop flaky"                             ctl stop flaky
check "flaky stopped, not restarting"          wait_state flaky stopped

echo "== SIGKILL escalation =="
check "start stubborn"                         ctl start stubborn
check "stubborn running"                       wait_state stubborn running
check "stop stubborn"                          ctl stop stubborn
check "stubborn eventually stopped"            wait_state stubborn stopped 50
check "init killed it after its timeout"       grep -q 'stubborn did not stop in time' "$OUT"

echo "== logs and errors =="
check "log shows service output"               bash -c "'$CTL' --socket '$SOCK' log base | grep -q 'base up'"
check "log of unknown service is an error"     bash -c "! '$CTL' --socket '$SOCK' log nope >/dev/null 2>&1"
check "start of unknown service is an error"   bash -c "! '$CTL' --socket '$SOCK' start nope >/dev/null 2>&1"
check "garbage command is an error"            bash -c "! '$CTL' --socket '$SOCK' frobnicate >/dev/null 2>&1"

echo "== shutdown =="
check "poweroff accepted"                      ctl poweroff
check "init exits on its own"                  bash -c "for _ in \$(seq 1 60); do kill -0 $INIT_PID 2>/dev/null || exit 0; sleep 0.1; done; exit 1"
wait "$INIT_PID"; rc=$?
check "init exited cleanly (status 0)"         test "$rc" -eq 0
INIT_PID=""
check "no service processes left behind"       bash -c "! pgrep -f 'sleep 777[78]' >/dev/null"
check "shutdown stopped app before base"       bash -c "a=\$(grep -n 'app stopped' '$OUT' | tail -1 | cut -d: -f1); b=\$(grep -n 'base stopped' '$OUT' | tail -1 | cut -d: -f1); [[ -n \$a && -n \$b && \$a -lt \$b ]]"

echo
echo "passed: $pass   failed: $fail"
[[ $fail -eq 0 ]] || { echo "--- init output ---"; cat "$OUT"; exit 1; }
