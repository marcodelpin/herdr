#!/usr/bin/env bash
# herdr-pttw r4: control or mutant battery on a clean detached checkout of the branch head.
# Usage: run_battery_r4.sh <checkout> <outdir> control|<mutant>...
# Based on r3b-salvage/run_battery.sh. compile_errors counts rustc errors only
# ("error[E" or "could not compile"); nextest's closing "error: test run failed"
# line, which r3b's count picked up, is not a compile error.
set -u
copy=$1; out=$2; shift 2
here=$(cd "$(dirname "$0")" && pwd)
. /srv/scratch/pttw-r4/env.sh
filter='test(/shutdown_drain_tests|api::drain|accept_loop_tests|sigterm_answers|sigterm_during_startup|ipc::tests::run_with_bound/)'
results="$out/battery-results.txt"
cd "$copy" || exit 1
for what in "$@"; do
    git checkout -q -- . || exit 1
    if [ "$what" != control ] && ! python3 -I "$here/mutate_r4.py" "$copy" "$what" >>"$results"; then
        continue
    fi
    log="$out/mutant-$what.log"
    timeout 1500 cargo nextest run --locked --bin herdr --test server_headless \
        --no-fail-fast -E "$filter" >"$log" 2>&1
    rc=$?
    red=$(grep -E '^\s+(FAIL|TIMEOUT|SIGABRT|SIGSEGV)' "$log" | sed -E 's/.*:://; s/.*server_headless //' | sort -u | tr '\n' ' ')
    built=$(grep -cE '^error\[E|could not compile' "$log")
    echo "MUTANT $what rc=$rc compile_errors=$built $(grep -E '^\s+Summary' "$log" | tr -s ' ') RED: $red" >>"$results"
done
git checkout -q -- .
git status --short | sed 's/^/LEFTOVER /' >>"$results"
