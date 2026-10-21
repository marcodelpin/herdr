# herdr-pttw r4 + herdr-ir4j: measurement summary (2026-10-21, dcc)

Evidence only, not for landing. Harness: harness/sigterm_inflight_r4.py (r3b harness plus per-request
detail, refused-connect counts, startup mode). Private servers only: environment built from scratch,
own HOME, XDG_* dirs, HERDR_SOCKET_PATH and client socket per trial. Release builds:
head = df0bc3c3 (code of e5237abb), base = 2380eadd. 32 clients, workspace.list, 12 trials per run.

## Load: SIGTERM 0.3 s after the first reply, under load

| run | sent | reply | error:server_unavailable | empty | reset | partial | send_failed | hang | bad trials |
|---|---|---|---|---|---|---|---|---|---|
| head run 1 | 23700 | 18848 | 4852 | 0 | 0 | 0 | 0 | 0 | 0/12 |
| head run 2 | 22982 | 18303 | 4679 | 0 | 0 | 0 | 0 | 0 | 0/12 |
| base 2380eadd | 27619 | 21154 | 6109 | 0 | 356 | 0 | 0 | 0 | 12/12 |

## Startup window (herdr-ir4j): SIGTERM right after the API socket accepted its first connection

Every trial of both runs has term_before_first_reply=True: the signal landed before the event loop
answered anything. On the release build the window is under 50 ms on head (delay probes 0.05/0.1 s
already saw replies), so delay 0.0 is used; base was still inside it at 0.1 s.

| run | sent | error:server_unavailable | empty | reset | send_failed:empty | server exit | bad trials |
|---|---|---|---|---|---|---|---|
| head | 5445 | 5445 | 0 | 0 | 0 | rc=0 x12 | 0/12 |
| base 2380eadd | 387 | 0 | 245 | 139 | 3 | rc=-15 (killed) x12 | 12/12 |

## send_failed verdict

- head: 0 send_failed in 36 trials (24 load, 12 startup), plus 8 in r3b's partial run 4 with the same
  residual-reading send path. Connects that failed after admission closed are counted apart, never as
  sent: head ECONNREFUSED 1 + 2 + 2 (the listener's SHUT_RD: Linux refuses a connect to a listener with
  RCV_SHUTDOWN under the listener lock, so nothing queues behind the last accept), ENOENT for the rest
  (socket file removed). Both are refused at connect after admission closed: acceptable.
- base startup: 3 send_failed:empty, e.g. trial 03 c30-2 conn=+0.0028 s, send EPIPE, read ECONNRESET,
  0 bytes: a connect accepted by the dying process 2.8 ms after SIGTERM, then the process was killed
  (rc=-15). Accepted request that lost its reply: the defect herdr-ir4j fixes.
- r3 head run 1's single send_failed (a013cb9f, 0.9.3 line) came from the earlier harness, which did
  not read the reply left after a failed send; its trial dir was on tmpfs and is gone, so it cannot be
  re-read. With the residual-reading send path the class has not recurred in 44 head trials.

## Tests

- drain filter (32 tests incl. both SIGTERM integration tests) x5: 32/32 each (drain-tests-x5-*.log).
- full suite `cargo nextest run --locked --no-fail-fast` x2 on a36ea808: 4235/4235 both, no red
  (full-suite-head-1.log, -2.log). Known flakes herdr-ltyz, herdr-6p4w, herdr-9exs did not fire.
- mutant battery: control 32/32 green, 18/18 mutants red, 0 compile errors (battery/).
- herdr-ir4j test red on d0bb7e25 (test only) and on 2380eadd + test; green on e5237abb.
