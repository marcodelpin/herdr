#!/usr/bin/env python3
"""Guard (bd herdr-zs55): every workflow job that installs Zig and then runs cargo or just
must run the zig-fetch-retry action after Install Zig and before the first cargo/just step,
so no build can die on a one-off package download failure. Run: python3 test-workflows.py"""
import glob
import re
import sys

import yaml

# A cargo or just invocation at the start of a command line (optionally after VAR=value words).
CMD = re.compile(r"^\s*(?:[A-Za-z_]+=\S+\s+)*(?:cargo|just)\s", re.M)
bad = []
checked = 0
for path in sorted(glob.glob(".github/workflows/*.yml")):
    jobs = (yaml.safe_load(open(path)) or {}).get("jobs") or {}
    for name, job in jobs.items():
        steps = job.get("steps") or []
        zig = [i for i, s in enumerate(steps) if "setup-zig" in str(s.get("uses", ""))]
        build = [i for i, s in enumerate(steps) if CMD.search(str(s.get("run", "")))]
        if not zig or not build:
            continue
        checked += 1
        retry = [i for i, s in enumerate(steps) if "zig-fetch-retry" in str(s.get("uses", ""))]
        if not any(zig[0] < r < build[0] for r in retry):
            bad.append(f"{path}: job {name}")
for b in bad:
    print("MISSING zig-fetch-retry before first build step:", b)
print(f"checked {checked} jobs, {len(bad)} missing")
sys.exit(1 if bad else 0)
