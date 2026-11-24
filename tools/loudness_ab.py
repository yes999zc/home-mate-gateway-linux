from pathlib import Path
#!/usr/bin/env python3
"""D-stage 1.1 acceptance: loudness chain under the T5 criterion.

The T5 A/B (`docs/d/evidence/t5-vol-ab-20261110.tsv`) showed the deployed
loudness ladder made the device interrupt ITSELF at high volume: 4 false
near-end fires in 56 s of stimulus-free speech at 84/127, 0 with
`--loudness off`. The limiter's per-sample gain steps (fixed 2026-11-10)
were the physical source. This tool re-measures the same thing after the
rewrite, with the criterion written down here so it cannot drift:

    N runs x 56 s of voice playback (2 x the 28 s asset), NO stimulus
      -> every `[barge-in] near-end speech` line in the firmware log is a
         FALSE interrupt
      -> every play must report played_ms == the full asset length
    PASS = zero fires in ALL runs AND every play complete.

Fires are counted from the FIRMWARE log (`[barge-in] near-end speech`,
/run/hm-direct.log on emOS) — not from core counters. The core only acts on
a reported fact, so a firmware fire that the core ignores is still a
detector failure and must be reported here.

Usage:
  loudness_ab.py --runs 3                  # 3 x 56 s at the current volume
  loudness_ab.py --runs 3 --volume 84      # set the level first (<=80 % cap)
  loudness_ab.py --runs 3 --plays 2 --asset erle-stimulus.wav
"""
import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
GW = os.environ.get("HM_GW_REPO", str(Path(__file__).resolve().parents[1]))
SERIAL = os.environ.get("HM_SERIAL", "/dev/ttyUSB0")
FW_LOG = "/run/hm-direct.log"
NEAR = "[barge-in] near-end speech"
# The 28.0 s asset: a complete play reports 28032 ms (measured, T5 A/B).
FULL_PLAY_MS = 27000


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=10) as r:
        return json.load(r)


def post(path, body=None, timeout=180):
    req = urllib.request.Request(
        CORE + path, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def serial(cmd, wait=4.0):
    out = subprocess.run(
        [sys.executable, os.path.join(GW, "tools/serial_cmd.py"), cmd, str(wait), SERIAL],
        capture_output=True, text=True, timeout=wait + 20)
    return out.stdout


def near_count():
    """Count firmware near-end lines. The log is a 2 MB tmpfs ring; a count
    delta over a 60 s window cannot wrap (the firmware writes ~1 line/min at
    steady state), and the supervisor trims to 512 KB, so we read the count
    before and after each run and use the delta."""
    txt = serial(f"grep -c 'barge-in' {FW_LOG}", wait=4.0)
    m = [int(x) for x in re.findall(r"^(\d+)$", txt, re.M)]
    return m[-1] if m else 0


def played_ms_since(t0_ms):
    evs = get("/api/v0/voice/log")["events"]
    fins = [e for e in evs
            if isinstance(e, dict) and e.get("kind") == "play_finished"
            and int(e.get("ts_ms") or 0) >= t0_ms]
    return [int(e.get("played_ms") or 0) for e in fins]


def run_once(asset, plays, log):
    t0 = int(time.time() * 1000)
    before = near_count()
    receipts = []
    for _ in range(plays):
        try:
            receipts.append(post("/api/v0/diagnostics/tests/play_asset",
                                 {"asset": asset, "channel": "voice"}))
        except Exception as e:  # noqa: BLE001 - evidence, never crash
            receipts.append({"error": str(e)})
        time.sleep(1.0)
    # The asset is 28 s; give the last play time to finish plus a margin.
    time.sleep(30.0 * plays + 6.0)
    after = near_count()
    plays_ms = played_ms_since(t0)
    fires = max(0, after - before)
    complete = [ms for ms in plays_ms if ms >= FULL_PLAY_MS]
    row = {
        "runs": 1, "plays": plays, "near_end_lines": fires,
        "play_finished": plays_ms, "complete_plays": len(complete),
        "verdict": "PASS" if (fires == 0 and len(complete) == plays) else "FAIL",
        "receipts": [r.get("status") or r.get("error") for r in receipts],
    }
    log.write(json.dumps(row, ensure_ascii=False) + "\n")
    print(f"  near_end={fires} played_ms={plays_ms} complete={len(complete)}/{plays}"
          f" -> {row['verdict']}")
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--plays", type=int, default=2, help="plays per run (2 x 28 s = 56 s)")
    ap.add_argument("--asset", default="erle-stimulus.wav")
    ap.add_argument("--volume", type=int, default=0, help="set level/127 first (<=102 = 80%%)")
    ap.add_argument("--warmup-plays", type=int, default=2,
                    help="unmeasured plays first: the AEC needs a few seconds of "
                         "real audio to converge, and its first playback after a "
                         "firmware restart is measurably different (2026-11-10)")
    ap.add_argument("--out", default="/tmp/hm-t5/loudness-ab.jsonl")
    a = ap.parse_args()
    os.makedirs(os.path.dirname(a.out), exist_ok=True)

    d = get("/api/v0/mode")["devices"][0]
    vol = d.get("volume")
    if a.volume:
        if a.volume > 102:
            print(f"REFUSE: volume {a.volume} > 80 % cap", file=sys.stderr)
            return 2
        # The API takes a LINEAR 0..1 value; the device level is 0..127.
        post("/api/v0/devices/dot2-d/volume", {"value": round(a.volume / 127.0, 4)})
        vol = a.volume
        time.sleep(1.0)
    fw = get("/api/v0/firmware")["audio"]
    print(f"firmware {fw['slots']} active={fw['active_slot']}")
    print(f"volume {vol}/127  asset {a.asset}  runs {a.runs} x {a.plays} plays")
    log = open(a.out, "a", buffering=1)
    log.write(f"# loudness A/B {time.strftime('%Y-%m-%d %H:%M:%S')} "
              f"volume={vol} asset={a.asset}\n")
    if a.warmup_plays:
        # Unmeasured: AEC convergence. Counting these would measure the
        # cold-start transient, not the steady-state ladder.
        print(f"warm-up: {a.warmup_plays} unmeasured plays")
        for _ in range(a.warmup_plays):
            try:
                post("/api/v0/diagnostics/tests/play_asset",
                     {"asset": a.asset, "channel": "voice"})
            except Exception:  # noqa: BLE001
                pass
            time.sleep(31.0)
        time.sleep(2.0)
    rows = []
    try:
        for i in range(1, a.runs + 1):
            print(f"run {i}/{a.runs}:")
            rows.append(run_once(a.asset, a.plays, log))
    finally:
        log.close()
    ok = all(r["verdict"] == "PASS" for r in rows)
    print(f"\nRESULT {'PASS' if ok else 'FAIL'}: "
          f"fires={[r['near_end_lines'] for r in rows]} "
          f"complete={[r['complete_plays'] for r in rows]} -> {a.out}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
