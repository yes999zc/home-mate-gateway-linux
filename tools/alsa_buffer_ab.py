#!/usr/bin/env python3
"""D/T5 firmware debt #4: A/B the ALSA playback buffer (170 ms vs 85 ms).

The 170 ms buffer (8192 frames @48k) is a latency-vs-underrun trade-off,
NOT a defect. Shrinking it is only allowed if an A/B shows a real gain in
first-sound / stop latency WITHOUT an underrun regression. This tool runs
both arms on ONE binary: it writes a frame count to
`/data/local/tmp/hm-direct/play-buffer`, restarts the supervised firmware
(which adds `--play-buffer`), then triggers the core's own audible
voice-chain probe and reads back the receipt + underrun count.

Usage:
  alsa_buffer_ab.py [--port /dev/ttyUSB0] [--rounds 3] [--out ab.tsv]
  alsa_buffer_ab.py --buffers 8192,4096 --rounds 2

Constraints (user 2026-11-09): volume <= 80%, no music playback without
asking. This tool uses the CORE's voice diagnostic probe (short TTS-ish
tone, voice channel), never the media/music channel. If the core's volume
is above 80% at start, it refuses to run rather than silently changing it.

It does NOT decide. It prints a matrix; the go/no-go lives in the T5 record
in docs/plan.md + HANDOFF, signed by the user after reading the numbers.
"""
import argparse
import json
import os
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fw_restart import Serial  # noqa: E402
from ch8_repro import restart, core_online  # noqa: E402

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DIR = "/data/local/tmp/hm-direct"


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def post(path, body=None, timeout=60):
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def volume_ok():
    """Refuse if any device is above the 80% ceiling."""
    try:
        for d in get("/api/v0/mode")["devices"]:
            vol = d.get("volume")
            if isinstance(vol, (int, float)) and vol > 0.8:
                return False, f"{d.get('device')}={vol}"
    except Exception as e:
        return False, f"mode unreadable: {e}"
    return True, ""


def events_since(n):
    try:
        return get("/api/v0/voice/log")["events"][-n:]
    except Exception:
        return []


def one_round(ser, frames, idx):
    ser.cmd(f"echo {frames} > {DIR}/play-buffer", 2)
    if not restart(ser):
        return {"frames": frames, "round": idx, "error": "restart_timeout"}
    if not core_online():
        return {"frames": frames, "round": idx, "error": "core_offline"}
    time.sleep(2)
    # Confirm the knob actually took effect BEFORE measuring (never infer
    # from the file we wrote).
    boot = ser.cmd("grep 'alsa. playback buffer' /run/hm-direct.log | tail -1", 2)
    before = len(events_since(500))
    t0 = time.time()
    try:
        receipt = post("/api/v0/diagnostics/tests/voice_play", {}, timeout=90)
    except Exception as e:
        return {"frames": frames, "round": idx, "error": f"probe: {e}"}
    elapsed = (time.time() - t0) * 1000
    # Give StreamDone time to arrive, then read the underrun count from the
    # firmware log (the StreamDone event carries it northbound as a receipt).
    time.sleep(2)
    evs = events_since(500)[before:] if before < 500 else events_since(500)
    underruns = []
    for e in evs:
        if isinstance(e, dict) and "underruns" in e:
            underruns.append(e["underruns"])
    log = ser.cmd("grep -E 'underrun|EPIPE|StreamDone|stream done' /run/hm-direct.log | tail -5", 2)
    return {
        "frames": frames,
        "round": idx,
        "boot_line": boot.strip().splitlines()[-1] if boot.strip() else "?",
        "probe_ms": round(elapsed),
        "receipt": json.dumps(receipt, ensure_ascii=False)[:180],
        "underruns": underruns[-1] if underruns else "n/a",
        "log_tail": log.strip().replace("\r", " ")[-160:],
        "error": "",
    }


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("--buffers", default="8192,4096")
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    ok, why = volume_ok()
    if not ok:
        print(f"REFUSE: volume ceiling 80% not met ({why}) — lower it first", file=sys.stderr)
        return 2
    frames_list = [int(x) for x in a.buffers.split(",")]
    for f in frames_list:
        if f < 4096 or f % 2048:
            print(f"REFUSE: {f} must be a multiple of 2048 and >= 4096", file=sys.stderr)
            return 2

    ser = Serial(a.port)
    rows = []
    try:
        for frames in frames_list:
            for i in range(a.rounds):
                r = one_round(ser, frames, i + 1)
                rows.append(r)
                print(
                    f"buffer={frames} round={i+1} probe_ms={r.get('probe_ms')} "
                    f"underruns={r.get('underruns')} err={r.get('error') or '-'}"
                )
    finally:
        # Leave the device on the DEFAULT buffer. Never park it on an
        # experimental knob (a later reboot would silently use it).
        ser.cmd(f"rm -f {DIR}/play-buffer", 2)
        ser.close()

    if a.out:
        keys = ["frames", "round", "boot_line", "probe_ms", "underruns", "receipt", "log_tail", "error"]
        with open(a.out, "w") as f:
            f.write("\t".join(keys) + "\n")
            for r in rows:
                f.write("\t".join(str(r.get(k, "")).replace("\t", " ") for k in keys) + "\n")
        print(f"matrix -> {a.out}")
    print("\nNOTE: default buffer restored (play-buffer file removed, restart to apply).")
    print("Decision rule: keep 8192 unless 4096 shows a clear latency win with underruns == 0 on BOTH arms.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
