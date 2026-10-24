#!/usr/bin/env python3
"""Guard (bd herdr-zs55): every workflow job that installs Zig and then runs cargo or just
must run the zig-fetch-retry action after Install Zig and before the first cargo/just step,
so no build can die on a one-off package download failure. When Install Zig and the first
build step share one `if` (the job skips its toolchain and its build together, as the
preview preflight does when should_publish is false), the prefetch must carry that `if`
too, so a skipped toolchain never meets a prefetch. Otherwise (an Install Zig gated on a
cache hit, with an unconditional build) the prefetch must run whenever the first build step
runs: no `if`, or exactly the first build step's `if`; any other condition (`if: false`)
fails. In a matrix job (`strategy.matrix`) the prefetch must carry no `if` at all, whatever
the other rules allow: an `if` matching one leg's first build step skips the prefetch on the
other legs (ADR-0020 amendment 1, codex zs55-r3fix P3). Paths resolve from this file, and
a scan that checks no job fails. Run: python3 test-workflows.py [workflows dir]
Self-test: test-workflows-guard.sh."""
import pathlib
import re
import sys

import yaml

HERE = pathlib.Path(__file__).resolve().parent
workflows = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else HERE.parents[1] / "workflows"
if not workflows.is_dir():
    print(f"INFRA: workflows dir not found: {workflows}")
    sys.exit(2)

# A cargo or just invocation at the start of a command line (optionally after VAR=value words).
CMD = re.compile(r"^\s*(?:[A-Za-z_]+=\S+\s+)*(?:cargo|just)\s", re.M)
bad = []
checked = 0
for path in sorted(workflows.glob("*.yml")):
    jobs = (yaml.safe_load(path.read_text()) or {}).get("jobs") or {}
    for name, job in jobs.items():
        steps = job.get("steps") or []
        zig = [i for i, s in enumerate(steps) if "setup-zig" in str(s.get("uses", ""))]
        build = [i for i, s in enumerate(steps) if CMD.search(str(s.get("run", "")))]
        if not zig or not build:
            continue
        checked += 1
        retry = [i for i, s in enumerate(steps) if "zig-fetch-retry" in str(s.get("uses", ""))]
        placed = [r for r in retry if zig[0] < r < build[0]]
        if not placed:
            bad.append(f"MISSING zig-fetch-retry before first build step: {path.name}: job {name}")
            continue
        zig_if = steps[zig[0]].get("if")
        build_if = steps[build[0]].get("if")
        shared = zig_if is not None and build_if == zig_if
        matrix = "matrix" in (job.get("strategy") or {})
        for r in placed:
            r_if = steps[r].get("if", None)
            if matrix:
                if "if" in steps[r]:
                    bad.append(f"CONDITIONED zig-fetch-retry in a matrix job (if={r_if!r}, must be unconditional): {path.name}: job {name}")
            elif shared:
                if r_if != zig_if:
                    bad.append(f"UNGUARDED zig-fetch-retry (if={r_if!r}, Install Zig if={zig_if!r}): {path.name}: job {name}")
            elif "if" in steps[r] and r_if != build_if:
                bad.append(f"CONDITIONED zig-fetch-retry (if={r_if!r}, first build step if={build_if!r}): {path.name}: job {name}")
for b in bad:
    print(b)
print(f"checked {checked} jobs, {len(bad)} missing")
if checked == 0:
    print(f"FAIL: no job checked under {workflows}")
    sys.exit(1)
sys.exit(1 if bad else 0)
