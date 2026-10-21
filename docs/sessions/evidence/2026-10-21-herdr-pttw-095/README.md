# herdr-pttw round 4 + herdr-ir4j evidence (2026-10-21)

Evidence only, not for landing. The fork ignores /docs/*; these files are force-added.

- drain-tests-port-1.log: drain test filter on the cherry-picked port (7af4ec43), 31/31.
- ir4j-test-red-before-fix.log: sigterm_during_startup_answers_requests_already_accepted on dc1919bc (test + hook only): request 0 got Ok(0) "".
- ir4j-test-green-after-fix.log: drain + signal + startup tests on 513fde36, 45/45.
- Toolchain: the brief's zig16 dir has no zig binary (herdr-wkgq); a checksum-verified Zig 0.16.0 (sha256 70e49664...3d00) under /srv/scratch/pttw-r4/zig16 was used.
