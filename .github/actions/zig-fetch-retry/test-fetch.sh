#!/usr/bin/env bash
# Offline suite for fetch.sh (bd herdr-5bka): a scripted fake zig replays one canned
# output per attempt, and every case asserts on the step's exit code and on how many
# times the fake was invoked (the fake's own log), never on prose alone. No network.
#
# The canned diagnostics are the literal shapes zig 0.16.0 prints for a lazy package
# (measured 2026-10-22 against a toy build.zig.zon: unresolvable host, refused port,
# local HTTP server returning a wrong tarball and a 404).
#
# A second section checks the fork workflows: in every job that removes the Zig caches,
# that cleanup runs BEFORE the prefetch step, so the prefetched build is not thrown away.
#
# Usage: test-fetch.sh [fetch.sh subject] [workflows dir]
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
subject=${1:-$here/fetch.sh}
workflows=${2:-$here/../../workflows}
[ -f "$subject" ] || { echo "INFRA: subject not found: $subject"; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pass=0
fail=0

ok() { pass=$((pass + 1)); echo "PASS $1"; }
ko() { fail=$((fail + 1)); echo "FAIL $1: $2"; }

# The fake zig: attempt N prints script/N.out and exits with script/N.rc; it appends one
# line per invocation to calls.log. A missing N.rc means success.
cat > "$work/zig" <<'EOF'
#!/usr/bin/env bash
dir=${FAKE_ZIG_DIR:?}
echo call >> "$dir/calls.log"
n=$(wc -l < "$dir/calls.log" | tr -d ' ')
[ -f "$dir/script/$n.out" ] && cat "$dir/script/$n.out" >&2
if [ -f "$dir/script/$n.rc" ]; then exit "$(cat "$dir/script/$n.rc")"; fi
exit 0
EOF
chmod +x "$work/zig"

zon='/r/vendor/libghostty-vt/build.zig.zon'
dns="$zon:14:20: error: unable to connect to server: NameServerFailure"
refused="$zon:22:20: error: unable to connect to server: ConnectionRefused"
mismatch="$zon:9:21: error: hash mismatch: manifest declares N-V-__8AAAA but the fetched package has N-V-__8AAAB"
notfound="$zon:14:20: error: bad HTTP response code: '404 Not Found'"
unavailable="$zon:14:20: error: bad HTTP response code: '503 Service Unavailable'"
reqfail="$zon:14:20: error: HTTP request failed: WriteFailed"
readreset="$zon:14:20: error: HTTP response read failure: ConnectionResetByPeer"
timeout="$zon:14:20: error: unable to connect to server: Timeout"
# Permanent causes behind the same wrapper prefixes (zig 0.16.0 std.http.Client.RequestError
# and std.crypto.tls.Client.ReadError): a prefix alone never makes a cause transient.
nohost="$zon:14:20: error: unable to connect to server: UriMissingHost"
badscheme="$zon:14:20: error: unable to connect to server: UnsupportedUriScheme"
badmac="$zon:22:20: error: HTTP response read failure: TlsBadRecordMac"
fingerprint="$zon:1:2: error: invalid fingerprint: 0x0; if this is a new or forked package, use this value: 0x6705a76eea54dde9"
compile="/r/vendor/libghostty-vt/src/lib_vt.zig:10:5: error: use of undeclared identifier 'foo'"

# run_case NAME WANT_RC WANT_CALLS ATTEMPTS OUT1 [OUT2 ...]: OUTk is the text of attempt k
# ('-' = success); attempts past the list succeed.
run_case() {
  local name=$1 want_rc=$2 want_calls=$3 attempts=$4
  shift 4
  local d="$work/$name"
  mkdir -p "$d/script" "$d/root/vendor/libghostty-vt"
  : > "$d/root/vendor/libghostty-vt/build.zig.zon"
  echo 0.0.0 > "$d/root/vendor/libghostty-vt/VERSION"
  : > "$d/calls.log"
  local k=1 out
  for out in "$@"; do
    if [ "$out" != - ]; then
      printf '%s\n' "$out" > "$d/script/$k.out"
      echo 1 > "$d/script/$k.rc"
    fi
    k=$((k + 1))
  done
  FAKE_ZIG_DIR=$d ZIG="$work/zig" ZIG_FETCH_ROOT="$d/root" \
    ZIG_FETCH_RUST_TARGET=x86_64-unknown-linux-gnu ZIG_FETCH_ATTEMPTS=$attempts \
    ZIG_FETCH_BACKOFF_SECONDS=0 bash "$subject" > "$d/step.log" 2>&1
  local rc=$? calls
  calls=$(wc -l < "$d/calls.log" | tr -d ' ')
  if [ "$rc" -eq "$want_rc" ] && [ "$calls" -eq "$want_calls" ]; then
    ok "$name"
  else
    ko "$name" "rc=$rc calls=$calls, want rc=$want_rc calls=$want_calls; step log: $(tr '\n' '|' < "$d/step.log" | cut -c1-400)"
  fi
}

# Retried: only recognised transient fetch errors.
run_case dns-then-success 0 2 4 "$dns"
run_case refused-then-success 0 2 4 "$refused"
run_case two-deps-dns-then-success 0 2 4 "$(printf '%s\n%s' "$dns" "$refused")"
run_case http-503-then-success 0 2 4 "$unavailable"
run_case http-request-failed-then-success 0 2 4 "$reqfail"
run_case read-reset-then-success 0 2 4 "$readreset"
run_case dns-every-attempt 1 4 4 "$dns" "$dns" "$dns" "$dns"
run_case timeout-every-attempt 1 4 4 "$timeout" "$timeout" "$timeout" "$timeout"
run_case success-first-attempt 0 1 4 -

# Not retried: a fatal diagnostic anywhere in the attempt's log fails the step at once.
run_case zon-syntax-error 1 1 4 "$fingerprint"
run_case http-404 1 1 4 "$notfound"
run_case hash-mismatch-alone 1 1 4 "$mismatch"
run_case hash-mismatch-beside-dns 1 1 4 "$(printf '%s\n%s' "$dns" "$mismatch")"
run_case compile-error-beside-dns 1 1 4 "$(printf '%s\n%s' "$dns" "$compile")"
run_case compile-error 1 1 4 "$compile"
run_case no-error-line 1 1 4 "Segmentation fault"
run_case uri-missing-host 1 1 4 "$nohost"
run_case unsupported-uri-scheme 1 1 4 "$badscheme"
run_case tls-bad-record-mac 1 1 4 "$badmac"
run_case dns-beside-tls-bad-record-mac 1 1 4 "$(printf '%s\n%s' "$dns" "$badmac")"

# A later attempt that turns fatal stops the loop there.
run_case dns-then-hash-mismatch 1 2 4 "$dns" "$(printf '%s\n%s' "$refused" "$mismatch")"

# The step log names a fatal cause as not a fetch failure.
if grep -q '^::error::.*not retried.*hash mismatch' "$work/hash-mismatch-beside-dns/step.log"; then
  ok hash-mismatch-reported
else
  ko hash-mismatch-reported "step log lacks the hash mismatch as a not-retried cause"
fi

# Workflows: the Zig cache cleanup must precede the prefetch in every job that has both.
if [ -d "$workflows" ]; then
  order=$(python3 - "$workflows" <<'EOF'
import glob, os, sys, yaml
bad, both = [], 0
for path in sorted(glob.glob(os.path.join(sys.argv[1], "fork-*.yml"))):
    with open(path) as fh:
        doc = yaml.safe_load(fh)
    for job_id, job in (doc.get("jobs") or {}).items():
        steps = job.get("steps") or []
        fetch = [i for i, s in enumerate(steps) if "zig-fetch-retry" in str(s.get("uses", ""))]
        clean = [i for i, s in enumerate(steps) if s.get("name") == "Remove Zig caches"]
        if fetch and clean:
            both += 1
            if max(clean) > min(fetch):
                bad.append(f"{os.path.basename(path)}:{job_id}")
print(both, " ".join(bad))
EOF
  )
  both=${order%% *}
  bad=${order#* }
  if [ "$both" = 6 ]; then ok "cleanup-jobs-count"; else ko cleanup-jobs-count "jobs with cleanup and prefetch: '$both', want 6"; fi
  if [ -z "${bad// /}" ]; then ok cleanup-before-prefetch; else ko cleanup-before-prefetch "cleanup after prefetch in: $bad"; fi
else
  ko workflows-dir "not found: $workflows"
fi

echo "test-fetch: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
