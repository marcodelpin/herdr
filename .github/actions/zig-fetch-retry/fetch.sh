#!/usr/bin/env bash
# Fork-only CI helper (bd herdr-5bka): prefetch the Zig packages that the vendored
# libghostty-vt build needs, retrying ONLY this network fetch.
#
# crates/ghostty-vt/build.rs runs `zig build` inside vendor/libghostty-vt, and that build
# downloads its lazy packages (codeberg.org, deps.files.ghostty.org) on first use. A runner
# DNS failure there (build.zig.zon:14:20 NameServerFailure) used to kill the job before
# any test ran, and the red run read like a code verdict.
#
# `zig build --fetch` cannot do this job: it fetches only eager dependencies, and every
# package here is lazy (measured with zig 0.14.1 on a lazy file:// dependency, and on
# CI where the 0.16.0 `--fetch` finished in 1s on a cold runner). So this script runs
# the same `zig build` that build.rs runs (same arguments, a throwaway --prefix): the
# packages land in the Zig global cache and the compiled library in the vendored
# .zig-cache, both of which the later build.rs run reuses. An attempt is retried ONLY
# when its error is located in a build.zig.zon, which is where zig reports a failed
# package download (DNS, connect, HTTP status); a compile error, a hash mismatch or any
# other failure fails the step at once. No cargo build and no test runs here: the
# verdict steps that follow run exactly once.
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

# The build.rs argument list; only --prefix differs (a throwaway directory), so this
# build asks for exactly the lazy packages the build.rs run will need.
prefix=$(mktemp -d)
log=$(mktemp)
trap 'rm -rf "$log" "$prefix"' EXIT
args=(build
  --prefix "$prefix"
  -Demit-lib-vt
  "-Doptimize=${LIBGHOSTTY_VT_OPTIMIZE:-ReleaseFast}"
  "-Dsimd=$simd"
  "-Dtarget=$zig_target"
  "-Dversion-string=$version_string"
  -Demit-xcframework=false)
if [ -n "${LIBGHOSTTY_VT_ZIG_SYSTEM_DIR:-}" ]; then
  args+=(--system "$LIBGHOSTTY_VT_ZIG_SYSTEM_DIR")
fi

echo "zig-fetch-retry: fetching libghostty-vt packages for $zig_target via zig build (up to $attempts attempts)"
attempt=1
while :; do
  if [ "${ZIG_FETCH_FAULT_FIRST_ATTEMPT:-}" = 1 ] && [ "$attempt" -eq 1 ]; then
    # CI proof branch ci/5bka-retry-r2 only: fail the first attempt as a runner DNS failure would.
    echo "$vendored/build.zig.zon:14:20: error: unable to connect to server: NameServerFailure (injected by ZIG_FETCH_FAULT_FIRST_ATTEMPT)" >"$log"
    rc=1
  else
    (cd "$vendored" && "$zig" "${args[@]}") >"$log" 2>&1
    rc=$?
  fi
  cat "$log"
  if [ "$rc" -eq 0 ]; then
    echo "zig-fetch-retry: packages fetched on attempt $attempt/$attempts"
    exit 0
  fi
  # zig reports a failed package download at the dependency's entry in a build.zig.zon.
  cause=$(grep -E 'build\.zig\.zon:[0-9]+:[0-9]+: error:' "$log" | grep -v 'hash mismatch' | head -n 1 | tr -d '\r')
  if [ -z "$cause" ]; then
    other=$(grep -m1 'error:' "$log" | tr -d '\r')
    echo "::error::zig-fetch-retry: zig build failed (rc=$rc) for a reason other than a package fetch; not retried. ${other:-no error line}"
    exit 1
  fi
  if [ "$attempt" -ge "$attempts" ]; then
    echo "::error::zig-fetch-retry: package fetch failed on all $attempts attempts; no test ran. Last cause: $cause"
    exit 1
  fi
  wait_s=$((attempt * backoff))
  echo "::warning::zig-fetch-retry: fetch attempt $attempt/$attempts failed (cause: $cause); retrying in ${wait_s}s"
  sleep "$wait_s"
  attempt=$((attempt + 1))
done
