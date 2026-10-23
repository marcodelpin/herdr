#!/usr/bin/env bash
# Self-test for test-workflows.py (bd herdr-zs55): the coverage guard must check the real
# workflows from any working directory, refuse an empty scan, and FAIL on a mutated copy
# of the workflows that lacks the retry step or runs it outside the Install Zig guard.
# Usage: test-workflows-guard.sh [test-workflows.py subject] [workflows dir]
set -uo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
subject=${1:-$here/test-workflows.py}
workflows=${2:-$here/../../workflows}
[ -f "$subject" ] || { echo "INFRA: subject not found: $subject"; exit 2; }
[ -d "$workflows" ] || { echo "INFRA: workflows not found: $workflows"; exit 2; }
python3 -I -c 'import yaml' 2>/dev/null || { echo "INFRA: python3 yaml module not found"; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pass=0
fail=0
ok() { pass=$((pass + 1)); echo "PASS $1"; }
ko() { fail=$((fail + 1)); echo "FAIL $1: $2"; }

# expect NAME WANT_RC(0|1) CWD [ARG]: run the guard from CWD; on rc 0 it must have checked
# at least one job.
expect() {
  local name=$1 want=$2 cwd=$3
  shift 3
  local out rc
  out=$(cd "$cwd" && python3 -I "$subject" "$@" 2>&1)
  rc=$?
  if [ "$want" = 0 ]; then
    [ "$rc" -eq 0 ] || { ko "$name" "rc=$rc want 0: $(printf '%s' "$out" | tail -n 3)"; return; }
    printf '%s\n' "$out" | grep -qE '^checked [1-9][0-9]* jobs, 0 missing' || { ko "$name" "no job checked: $out"; return; }
  else
    [ "$rc" -ne 0 ] || { ko "$name" "rc=0 want nonzero: $(printf '%s' "$out" | tail -n 1)"; return; }
  fi
  ok "$name"
}

# mutate NAME MODE: copy the workflows and apply MODE to the first job that runs the retry.
#   drop    remove that job's zig-fetch-retry step
#   unguard remove the 'if' of the first retry step in a job whose Install Zig and first
#           build step share one 'if'
#   disable set 'if: false' on the first retry step in a job whose Install Zig and first
#           build step do NOT share one 'if' (codex zs55-r2 P3: a disabled prefetch)
mutate() {
  local dst="$work/$1"
  mkdir -p "$dst"
  python3 -I - "$workflows" "$dst" "$2" <<'PY'
import pathlib, re, sys
import yaml
src, dst, mode = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3]
done = False
for path in sorted(src.glob("*.yml")):
    doc = yaml.safe_load(path.read_text())
    for job in (doc.get("jobs") or {}).values():
        steps = job.get("steps") or []
        zig = [s for s in steps if "setup-zig" in str(s.get("uses", ""))]
        build = [s for s in steps if re.search(r"^\s*(?:[A-Za-z_]+=\S+\s+)*(?:cargo|just)\s", str(s.get("run", "")), re.M)]
        gated = bool(zig and build and "if" in zig[0] and build[0].get("if") == zig[0]["if"])
        for i, s in enumerate(steps):
            if done or "zig-fetch-retry" not in str(s.get("uses", "")):
                continue
            if mode == "drop":
                del steps[i]
                done = True
            elif mode == "unguard" and gated and "if" in s:
                del s["if"]
                done = True
            elif mode == "disable" and zig and build and not gated:
                s["if"] = False
                done = True
            break
    (dst / path.name).write_text(yaml.safe_dump(doc, sort_keys=False))
sys.exit(0 if done else 3)
PY
}

expect real-from-repo-root 0 "$here/../../.." "$workflows"
expect real-from-script-dir-default 0 "$here"
expect real-from-tmp-default 0 "$work"
mkdir -p "$work/empty"
expect empty-scan-refused 1 "$work" "$work/empty"
if mutate dropped drop; then expect dropped-retry-detected 1 "$work" "$work/dropped"; else ko dropped-retry-detected "mutation found no retry step"; fi
if mutate unguarded unguard; then expect unguarded-retry-detected 1 "$work" "$work/unguarded"; else ko unguarded-retry-detected "mutation found no guarded retry step"; fi
if mutate disabled disable; then expect disabled-retry-detected 1 "$work" "$work/disabled"; else ko disabled-retry-detected "mutation found no ungated retry step"; fi

# Codex zs55-r1 P2: in preview.yml the prefetch must carry the same 'if' as Install Zig,
# else a should_publish=false no-op runs a Zig build without the toolchain.
if python3 -I - "$workflows/preview.yml" <<'PY'
import sys
import yaml
jobs = yaml.safe_load(open(sys.argv[1]))["jobs"]
bad = 0
for name, job in jobs.items():
    steps = job.get("steps") or []
    zig = [s for s in steps if "setup-zig" in str(s.get("uses", ""))]
    for s in steps:
        if zig and "zig-fetch-retry" in str(s.get("uses", "")) and s.get("if") != zig[0].get("if"):
            print(f"{name}: prefetch if={s.get('if')!r}, Install Zig if={zig[0].get('if')!r}")
            bad += 1
sys.exit(1 if bad else 0)
PY
then ok preview-prefetch-guarded; else ko preview-prefetch-guarded "prefetch runs outside the Install Zig guard"; fi

echo "test-workflows-guard: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
