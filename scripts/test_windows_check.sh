#!/usr/bin/env bash
# Offline suite for scripts/windows_check.ps1 (bd herdr-zs55): the clippy step may be
# retried ONLY when its output carries a transient Zig package fetch error from the
# ADR-0020 allowlist (herdr-AGENT wrapper) and no other error. A fake cargo replays one
# canned output per clippy call; each case asserts on the exit code and on the number of
# clippy calls. Needs pwsh; no network, no real cargo.
#
# Usage: test_windows_check.sh [windows_check.ps1 subject]
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
subject=${1:-$here/windows_check.ps1}
[ -f "$subject" ] || { echo "INFRA: subject not found: $subject"; exit 2; }
command -v pwsh >/dev/null || { echo "INFRA: pwsh not found"; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pass=0
fail=0
ok() { pass=$((pass + 1)); echo "PASS $1"; }
ko() { fail=$((fail + 1)); echo "FAIL $1: $2"; }

# Fake cargo: `cargo fmt` succeeds; clippy call N prints script/N.out to stderr and exits
# with script/N.rc (missing = 0), appending one line per clippy call to calls.log.
mkdir -p "$work/bin"
cat > "$work/bin/cargo" <<'FAKE'
#!/usr/bin/env bash
[ "$1" = fmt ] && exit 0
dir=${FAKE_CARGO_DIR:?}
echo call >> "$dir/calls.log"
n=$(wc -l < "$dir/calls.log" | tr -d ' ')
[ -f "$dir/script/$n.out" ] && cat "$dir/script/$n.out" >&2
if [ -f "$dir/script/$n.rc" ]; then exit "$(cat "$dir/script/$n.rc")"; fi
exit 0
FAKE
chmod +x "$work/bin/cargo"

zon='C:\a\herdr\vendor\libghostty-vt\build.zig.zon'
wrap='error: failed to run custom build command for `ghostty-vt v0.1.0`'
dns="  $zon:14:20: error: unable to connect to server: NameServerFailure"
mismatch="  $zon:9:21: error: hash mismatch: manifest declares N-V-__8AAAA but the fetched package has N-V-__8AAAB"
nohost="  $zon:14:20: error: unable to connect to server: UriMissingHost"
rustc=$'error[E0308]: mismatched types\n --> src/main.rs:1:14\nerror: could not compile `herdr` (bin "herdr") due to 1 previous error'
lint=$'error: unused variable: `x`\n --> src/main.rs:2:9\nerror: could not compile `herdr` (bin "herdr") due to 1 previous error'
zigsrc="  C:\\a\\herdr\\vendor\\libghostty-vt\\src\\lib_vt.zig:10:5: error: use of undeclared identifier 'foo'"

# run_case NAME WANT_RC(0|nonzero) WANT_CALLS OUT1 [OUT2 ...]; '-' = a successful call.
run_case() {
  local name=$1 want=$2 want_calls=$3
  shift 3
  local d="$work/$name" k=1 out
  mkdir -p "$d/script"
  : > "$d/calls.log"
  for out in "$@"; do
    if [ "$out" != - ]; then
      printf '%s\n' "$out" > "$d/script/$k.out"
      echo 101 > "$d/script/$k.rc"
    fi
    k=$((k + 1))
  done
  (cd "$d" && FAKE_CARGO_DIR="$d" PATH="$work/bin:$PATH" pwsh -NoProfile -File "$subject" -Mode lint) > "$d/run.log" 2>&1
  local rc=$? calls
  calls=$(wc -l < "$d/calls.log" | tr -d ' ')
  if [ "$want" = 0 ] && [ "$rc" -ne 0 ]; then ko "$name" "rc=$rc want 0 (calls=$calls)"; return; fi
  if [ "$want" != 0 ] && [ "$rc" -eq 0 ]; then ko "$name" "rc=0 want nonzero (calls=$calls)"; return; fi
  [ "$calls" = "$want_calls" ] || { ko "$name" "clippy calls=$calls want $want_calls (rc=$rc)"; return; }
  ok "$name"
}

run_case clean-pass 0 1 -
run_case dns-then-pass 0 2 "$wrap"$'\n'"$dns" -
run_case dns-twice-fails 1 2 "$wrap"$'\n'"$dns" "$wrap"$'\n'"$dns"
run_case rustc-error-not-retried 1 1 "$rustc" -
run_case lint-error-not-retried 1 1 "$lint" -
run_case dns-plus-mismatch-not-retried 1 1 "$wrap"$'\n'"$dns"$'\n'"$mismatch" -
run_case dns-plus-rustc-not-retried 1 1 "$wrap"$'\n'"$dns"$'\n'"$rustc" -
run_case permanent-cause-not-retried 1 1 "$wrap"$'\n'"$nohost" -
run_case zig-compile-error-not-retried 1 1 "$wrap"$'\n'"$zigsrc" -
run_case no-error-line-not-retried 1 1 "something went wrong" -

echo "test_windows_check: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
