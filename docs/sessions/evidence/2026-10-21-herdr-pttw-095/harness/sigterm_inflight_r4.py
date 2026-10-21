#!/usr/bin/env python3
"""herdr-pttw r4: what in-flight API requests get when SIGTERM ends an isolated herdr server.

Usage: sigterm_inflight_r4.py <herdr-binary> <trials> <clients> <mode> [delay_s]
  mode load:    wait for a first reply, run <clients> back-to-back workspace.list clients,
                SIGTERM after delay_s (default 0.3) under load.
  mode startup: start the clients as soon as the API socket accepts a connection and send
                SIGTERM delay_s (default 0.3) after that first accepted connection, i.e. in
                the window between the API bind and the event loop (herdr-ir4j).

Every trial starts its own server: environment built from scratch (no inherited variable),
private HOME, XDG dirs, HERDR_SOCKET_PATH and client socket under one trial dir.
Each request whose connect succeeded is classified:
  reply (result) | error:<code> | empty (EOF, zero bytes) | partial (bytes, no newline) |
  reset (ECONNRESET/EPIPE while reading) | hang (no answer in 10 s) |
  send_failed:<empty|partial|garbled> (the send failed and nothing complete was left to read).
A connect that fails is not a request: counted as refused (with its errno), never as sent.
Based on the r3b harness (r3b-salvage/sigterm_inflight.py); additions: per-request detail for
every non-reply class, refused-connect counts, the startup mode, trial dirs off tmpfs.
"""
import collections
import errno
import glob
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

BIN, TRIALS, CLIENTS, MODE = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
DELAY = float(sys.argv[5]) if len(sys.argv) > 5 else 0.3
METHOD = "workspace.list"
HANG_S = 10.0
TRIAL_ROOT = os.environ.get("PTTW_TRIAL_ROOT", "/srv/scratch/pttw-r4/trials")
GOOD = ("reply",)


def errname(err):
    return errno.errorcode.get(err.errno, str(err.errno)) if getattr(err, "errno", None) else type(err).__name__


def read_line(s):
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        buf += chunk
    return buf


def classify_line(buf):
    try:
        msg = json.loads(buf)
    except ValueError:
        return "garbled"
    if "error" in msg:
        return "error:" + msg["error"].get("code", "?")
    return "reply"


def one_request(path, rid):
    """Returns (class, detail) or (None, refused-errno) when the connect failed."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(HANG_S)
    t_conn = time.monotonic()
    try:
        s.connect(path)
    except OSError as err:
        s.close()
        return None, errname(err)
    detail = {"rid": rid, "t_conn": t_conn}
    try:
        s.sendall((json.dumps({"id": rid, "method": METHOD, "params": {}}) + "\n").encode())
    except OSError as err:
        detail["send_err"] = errname(err)
        # The server may have answered and closed before the request was fully
        # sent; read what it left before classifying.
        buf = b""
        try:
            buf = read_line(s)
        except OSError as rerr:
            detail["read_err"] = errname(rerr)
        s.close()
        detail["t_end"] = time.monotonic()
        detail["bytes"] = len(buf)
        if buf.endswith(b"\n"):
            c = classify_line(buf)
            return ("send_failed:garbled" if c == "garbled" else c), detail
        return ("send_failed:empty" if not buf else "send_failed:partial"), detail
    buf = b""
    try:
        buf = read_line(s)
    except socket.timeout:
        s.close()
        detail["t_end"] = time.monotonic()
        return "hang", detail
    except (ConnectionResetError, BrokenPipeError) as err:
        s.close()
        detail["t_end"] = time.monotonic()
        detail["read_err"] = errname(err)
        detail["bytes"] = len(buf)
        return ("reset" if not buf else "partial"), detail
    s.close()
    detail["t_end"] = time.monotonic()
    detail["bytes"] = len(buf)
    if not buf:
        return "empty", detail
    if not buf.endswith(b"\n"):
        return "partial", detail
    return classify_line(buf), detail


def connectable(path):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(path)
        return True
    except OSError:
        return False
    finally:
        s.close()


def trial(n):
    os.makedirs(TRIAL_ROOT, exist_ok=True)
    root = tempfile.mkdtemp(prefix=f"{MODE}-", dir=TRIAL_ROOT)
    os.makedirs(os.path.join(root, "home"), mode=0o700)
    env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": root + "/home",
           "TERM": "xterm-256color", "LANG": "C.UTF-8"}
    for d in ("config", "runtime", "state", "data", "cache"):
        os.makedirs(os.path.join(root, d), mode=0o700)
    api = os.path.join(root, "runtime", "herdr.sock")
    env.update(XDG_CONFIG_HOME=root + "/config", XDG_RUNTIME_DIR=root + "/runtime",
               XDG_STATE_HOME=root + "/state", XDG_DATA_HOME=root + "/data",
               XDG_CACHE_HOME=root + "/cache", HERDR_SOCKET_PATH=api,
               HERDR_CLIENT_SOCKET_PATH=root + "/runtime/herdr-client.sock", SHELL="/bin/sh",
               HERDR_LOG=os.environ.get("PTTW_LOG", "herdr=info"))
    master, slave = os.openpty()
    log = open(os.path.join(root, "server.out"), "wb")
    proc = subprocess.Popen([BIN, "server"], env=env, stdin=slave, stdout=log, stderr=log,
                            start_new_session=True)
    os.close(slave)
    deadline = time.time() + 30
    if MODE == "load":
        while time.time() < deadline and one_request(api, "warm")[0] != "reply":
            time.sleep(0.05)
    else:
        while time.time() < deadline and not connectable(api):
            time.sleep(0.002)
    t_first = time.monotonic()

    counts = collections.Counter()
    refused = collections.Counter()
    bad = []
    first_reply = [None]
    lock = threading.Lock()
    stop = threading.Event()
    term = {"t": None}

    def worker(i):
        k = 0
        while not stop.is_set():
            k += 1
            c, d = one_request(api, f"c{i}-{k}")
            with lock:
                after = term["t"] is not None
                if c is None:
                    refused[("after_term:" if after else "before_term:") + d] += 1
                else:
                    counts[c] += 1
                    if c == "reply" and first_reply[0] is None:
                        first_reply[0] = d["t_end"]
                    if c not in GOOD and not c.startswith("error:"):
                        bad.append((c, d))
            if c is None:
                time.sleep(0.002)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(CLIENTS)]
    for t in threads:
        t.start()
    time.sleep(max(0.0, t_first + DELAY - time.monotonic()))
    with lock:
        term["t"] = time.monotonic()
    proc.send_signal(signal.SIGTERM)
    try:
        rc = proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        rc = "TIMEOUT"
        os.killpg(proc.pid, signal.SIGKILL)
        proc.wait()
    time.sleep(0.3)
    stop.set()
    for t in threads:
        t.join()
    os.close(master)
    log.close()
    t_term = term["t"]
    in_window = first_reply[0] is None or first_reply[0] > t_term
    details = []
    for c, d in bad[:20]:
        details.append(
            f"{c} {d['rid']} conn={d['t_conn'] - t_term:+.4f}s end={d.get('t_end', t_term) - t_term:+.4f}s "
            f"send_err={d.get('send_err', '-')} read_err={d.get('read_err', '-')} bytes={d.get('bytes', '-')}")
    logs = glob.glob(os.path.join(root, "config", "*", "herdr-server.log"))
    logtxt = open(logs[0], errors="replace").read() if logs else ""
    seen = sum(1 for _, d in bad if f'"{d["rid"]}"' in logtxt or f"={d['rid']}" in logtxt)
    keep = bool(bad) or rc == "TIMEOUT"
    if not keep:
        shutil.rmtree(root, ignore_errors=True)
    return rc, counts, refused, details, seen, in_window, root if keep else "-"


total = collections.Counter()
total_refused = collections.Counter()
bad_trials = 0
print(f"# bin={BIN} mode={MODE} trials={TRIALS} clients={CLIENTS} delay={DELAY}s", flush=True)
for n in range(TRIALS):
    rc, c, r, details, seen, in_window, root = trial(n)
    total.update(c)
    total_refused.update(r)
    nbad = sum(v for k, v in c.items() if k not in GOOD and not k.startswith("error:"))
    if nbad or rc != 0:
        bad_trials += 1
    extra = f" term_before_first_reply={in_window}" if MODE == "startup" else ""
    print(f"trial {n:02d} rc={rc} sent={sum(c.values())} {dict(c)} refused={dict(r)}"
          f" bad_ids_in_server_log={seen}{extra} kept={root}", flush=True)
    for line in details:
        print(f"    {line}", flush=True)
print(f"TOTAL sent={sum(total.values())} {dict(total)}")
print(f"REFUSED_AT_CONNECT {dict(total_refused)}")
print(f"TRIALS_WITH_LOSS_OR_BAD_EXIT {bad_trials}/{TRIALS}")
