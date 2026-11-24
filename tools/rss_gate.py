#!/usr/bin/env python3
"""R0 five-minute gate (2026-11-13): does the firmware RSS sawtooth back?

The OOM blocker was an allocator that never returned freed blocks to the OS:
RSS grew ~0.33 MB per second of played audio and the kernel killed the
firmware after ~57 min (exit 137). The fix (tools/build_firmware.sh: rust-lld
final link + Rust's musl) must show a sawtooth instead: every play returns
RSS to the baseline line, and the `[mem] play end` telegram (live =
allocated - freed) stays in the hundreds-of-KB range.

Method: restart the supervised firmware (fresh baseline), then play the 28 s
stimulus N times through the real core play path and read, between plays:
  * /proc/<fw>/VmRSS          (what the kernel gave the process)
  * the last `[mem] play end` line  (live/alloc/free: the program's own bytes)
  * MemAvailable              (whole-device pressure)
The serial line is the only device channel (emOS has no adbd).

Criteria (hard-coded, from the R0 task):
  * PASS = every post-play sample within +/-2 MB of the baseline AND
           (max - min) <= 5 MB AND not strictly increasing.
  * FAIL otherwise; a failure means the three-hour soak must NOT be started
    and the next knob is dlmalloc -> large-buffer reuse -> short-lived media
    process (docs/roadmap-20261113 §2).

Usage: rss_gate.py --plays 10 [--asset erle-stimulus.wav] [--out <tsv>]
                   [--port /dev/ttyUSB0] [--core http://192.168.1.73:8930]
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from fw_restart import FIND_FW, Serial, first_pid  # noqa: E402

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
TOL_KB = 2048          # +/-2 MB against the baseline
SPREAD_KB = 5 * 1024   # max - min across the run
RSS_WARN_KB = 16_384   # 16 MiB warning threshold (matches firmware RSS_WARN_KB)
RSS_LIMIT_KB = 32_768  # 32 MiB hard ceiling on 512 MB Dot2 (matches RSS_LIMIT_KB)


def sh(cmd: str, wait: float = 5.0, port: str = "/dev/ttyUSB0") -> str:
    out = subprocess.run([sys.executable, os.path.join(HERE, "serial_cmd.py"), cmd, str(wait), port],
                         capture_output=True, text=True)
    return out.stdout


def core_post(path: str, body: dict, timeout: float = 120.0) -> dict:
    req = urllib.request.Request(CORE + path, data=json.dumps(body).encode(),
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def fw_pid(port: str) -> str:
    return first_pid(sh(FIND_FW, 3, port))


def read_rss(pid: str, port: str):
    """(rss_kb, live_last_mem_line) — a missing value is returned as None."""
    out = sh(f"grep VmRSS /proc/{pid}/status; echo ---; "
             f"grep '\\[mem\\]' /run/hm-direct.log | tail -1", 5, port)
    rss = None
    mem = ""
    for line in out.splitlines():
        if "VmRSS" in line:
            for tok in line.replace("VmRSS:", " ").split():
                if tok.isdigit():
                    rss = int(tok)
                    break
        elif "[mem]" in line:
            mem = line.strip()
    return rss, mem


def mem_available(port: str) -> int:
    out = sh("grep MemAvailable /proc/meminfo", 4, port)
    for tok in out.replace("MemAvailable:", " ").split():
        if tok.isdigit():
            return int(tok)
    return -1


def restart_firmware(ser: Serial, port: str) -> str:
    old = first_pid(ser.cmd(FIND_FW, 3)) or "?"
    sup = first_pid(ser.cmd(f"[ -d /proc/{old} ] && awk '{{print $4}}' /proc/{old}/stat", 3))
    if not sup or sup == "1":
        print("ERROR: cannot find the firmware's owning supervisor", file=sys.stderr)
        sys.exit(1)
    ser.cmd(f"kill -TERM {sup}", 2)
    deadline = time.time() + 60
    while time.time() < deadline:
        cand = first_pid(ser.cmd(FIND_FW, 3))
        if cand and cand != old:
            return cand
        time.sleep(1)
    print("ERROR: firmware did not come back after the supervisor restart", file=sys.stderr)
    sys.exit(1)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--plays", type=int, default=10)
    ap.add_argument("--asset", default="erle-stimulus.wav")
    ap.add_argument("--out", default="")
    ap.add_argument("--port", default="/dev/ttyUSB0")
    a = ap.parse_args()

    ser = Serial(a.port)
    try:
        pid = restart_firmware(ser, a.port)
        print(f"firmware pid={pid} (fresh boot = fresh baseline)")
        # Let the core reconnect and the mic/AEC chain settle before the first play.
        time.sleep(12)
        base, mem = read_rss(pid, a.port)
        if base is None:
            print("ERROR: no VmRSS after restart", file=sys.stderr)
            return 1
        print(f"baseline rss={base} kB  avail={mem_available(a.port)} kB  {mem}")

        rows = []
        fh = open(a.out, "w", buffering=1) if a.out else None
        if fh:
            fh.write("# play\tmac_time\trss_kb\tdelta_kb\tavail_kb\tmem_line\n")
        t0 = time.time()
        for i in range(1, a.plays + 1):
            play = core_post("/api/v0/diagnostics/tests/play_asset", {"asset": a.asset})
            ok = play.get("ok")
            time.sleep(1.0)
            rss, mem = read_rss(pid, a.port)
            avail = mem_available(a.port)
            delta = (rss - base) if rss is not None else None
            elapsed = time.time() - t0
            print(f"play {i:2d}/{a.plays} {play.get('duration_ms')}ms ok={ok} "
                  f"stage={play.get('stage')} rss={rss} kB ({delta:+d}) avail={avail} kB "
                  f"[{elapsed:.0f}s] {mem}", flush=True)
            if fh:
                fh.write(f"{i}\t{time.strftime('%H:%M:%S')}\t{rss}\t{delta}\t{avail}\t{mem}\n")
            rows.append(rss)
            if not ok:
                print(f"FAIL: play {i} did not confirm (stage={play.get('stage')}) — "
                      "the gate needs real playback, not a receipt", flush=True)
                if fh:
                    fh.close()
                return 1
        if fh:
            fh.close()

        post = [r for r in rows[1:] if r is not None]   # the first play warms pages
        spread = max(post) - min(post)
        outside = [(i + 2, r - base) for i, r in enumerate(post) if abs(r - base) > TOL_KB]
        over_limit = [(i + 2, r) for i, r in enumerate(post) if r >= RSS_LIMIT_KB]
        over_warn = [(i + 2, r) for i, r in enumerate(post) if r >= RSS_WARN_KB]
        rising = all(b > a_ for a_, b in zip(rows, rows[1:]) if a_ and b)
        print()
        print(f"baseline {base} kB; post-play samples {post}")
        print(f"spread(max-min) = {spread} kB (limit {SPREAD_KB})")
        print(f"outside baseline +/-{TOL_KB} kB: {outside or 'none'}")
        if over_warn:
            print(f"WARNING: samples >= RSS_WARN_KB ({RSS_WARN_KB} kB): {over_warn}")
        print(f"over RSS_LIMIT_KB ({RSS_LIMIT_KB} kB): {over_limit or 'none'}")
        print(f"strictly increasing: {rising}")
        passed = not outside and not over_limit and spread <= SPREAD_KB and not rising
        print("PASS" if passed else "FAIL")
        return 0 if passed else 1
    finally:
        ser.close()


if __name__ == "__main__":
    sys.exit(main())
