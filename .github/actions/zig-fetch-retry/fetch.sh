#!/usr/bin/env bash
# Fork-only CI helper (bd herdr-5bka): prefetch the Zig packages that the vendored
# libghostty-vt build needs, retrying ONLY this network fetch.
#
# crates/ghostty-vt/build.rs runs `zig build` inside vendor/libghostty-vt, and that build
# downloads its lazy packages (codeberg.org, deps.files.ghostty.org) on first use. A runner
# DNS failure there (build.zig.zon:14:20 NameServerFailure) used to kill the job before
# any test ran, and the red run read like a code verdict. This script runs the same
# `zig build` configuration with `--fetch`, so the packages land in the Zig global cache
# that build.rs then reads, and retries that fetch a bounded number of times with a
# growing backoff. It never builds, tests or judges anything: the verdict steps that
# follow it run exactly once.
#
# Environment:
#   ZIG_FETCH_ATTEMPTS         total attempts (default 4)
#   ZIG_FETCH_BACKOFF_SECONDS  backoff unit; attempt N waits N * unit before retrying (default 15)
#   ZIG_FETCH_RUST_TARGET      Rust target triple the job builds (default: rustc's host)
#   ZIG                        zig binary (default: zig), the same variable build.rs reads
#   LIBGHOSTTY_VT_OPTIMIZE / LIBGHOSTTY_VT_SIMD   read as build.rs reads them
set -uo pipefail

attempts=${ZIG_FETCH_ATTEMPTS:-4}
backoff=${ZIG_FETCH_BACKOFF_SECONDS:-15}
zig=${ZIG:-zig}
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=${ZIG_FETCH_ROOT:-$(cd -- "$script_dir/../../.." && pwd)}
vendored="$root/vendor/libghostty-vt"

case "$attempts" in ''|*[!0-9]*|0) echo "::error::ZIG_FETCH_ATTEMPTS must be a positive integer, got '$attempts'"; exit 2 ;; esac
case "$backoff" in ''|*[!0-9]*) echo "::error::ZIG_FETCH_BACKOFF_SECONDS must be a non-negative integer, got '$backoff'"; exit 2 ;; esac
[ -f "$vendored/build.zig.zon" ] || { echo "::error::vendored libghostty-vt not found at $vendored"; exit 2; }

rust_target=${ZIG_FETCH_RUST_TARGET:-}
if [ -z "$rust_target" ]; then
  rust_target=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p' | tr -d '\r')
  [ -n "$rust_target" ] || { echo "::error::cannot read the rustc host triple; set ZIG_FETCH_RUST_TARGET"; exit 2; }
fi

# Same table as zig_target() in crates/ghostty-vt/build.rs.
case "$rust_target" in
  x86_64-unknown-linux-gnu) zig_target=x86_64-linux-gnu ;;
  aarch64-unknown-linux-gnu) zig_target=aarch64-linux-gnu ;;
  x86_64-unknown-linux-musl) zig_target=x86_64-linux-musl ;;
  aarch64-unknown-linux-musl) zig_target=aarch64-linux-musl ;;
  x86_64-apple-darwin) zig_target=x86_64-macos ;;
  aarch64-apple-darwin) zig_target=aarch64-macos ;;
  aarch64-apple-ios) zig_target=aarch64-ios ;;
  aarch64-apple-ios-sim) zig_target=aarch64-ios-simulator ;;
  x86_64-pc-windows-msvc) zig_target=x86_64-windows-msvc ;;
  aarch64-pc-windows-msvc) zig_target=aarch64-windows-msvc ;;
  *) echo "::error::unsupported target for the libghostty-vt fetch: $rust_target"; exit 2 ;;
esac

case "$(printf '%s' "${LIBGHOSTTY_VT_SIMD:-true}" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes|on) simd=true ;;
  0|false|no|off) simd=false ;;
  *) echo "::error::invalid boolean value for LIBGHOSTTY_VT_SIMD: $LIBGHOSTTY_VT_SIMD"; exit 2 ;;
esac
version_string=$(tr -d '[:space:]' < "$vendored/VERSION")

# The build.rs argument list minus --prefix (the fetch installs nothing), so the fetch
# resolves the same lazy packages the build will ask for.
args=(build --fetch
  -Demit-lib-vt
  "-Doptimize=${LIBGHOSTTY_VT_OPTIMIZE:-ReleaseFast}"
  "-Dsimd=$simd"
  "-Dtarget=$zig_target"
  "-Dversion-string=$version_string"
  -Demit-xcframework=false)
if [ -n "${LIBGHOSTTY_VT_ZIG_SYSTEM_DIR:-}" ]; then
  args+=(--system "$LIBGHOSTTY_VT_ZIG_SYSTEM_DIR")
fi

log=$(mktemp)
trap 'rm -f "$log"' EXIT

echo "zig-fetch-retry: prefetching libghostty-vt packages for $zig_target (up to $attempts attempts)"
attempt=1
while :; do
  (cd "$vendored" && "$zig" "${args[@]}") >"$log" 2>&1
  rc=$?
  cat "$log"
  if [ "$rc" -eq 0 ]; then
    echo "zig-fetch-retry: fetch succeeded on attempt $attempt/$attempts"
    exit 0
  fi
  cause=$(grep -m1 'error:' "$log" | tr -d '\r')
  [ -n "$cause" ] || cause="zig exited $rc with no error line"
  if [ "$attempt" -ge "$attempts" ]; then
    echo "::error::zig-fetch-retry: fetch failed on all $attempts attempts; no test ran. Last cause: $cause"
    exit 1
  fi
  wait_s=$((attempt * backoff))
  echo "::warning::zig-fetch-retry: fetch attempt $attempt/$attempts failed (cause: $cause); retrying in ${wait_s}s"
  sleep "$wait_s"
  attempt=$((attempt + 1))
done
