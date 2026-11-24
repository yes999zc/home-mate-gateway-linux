from pathlib import Path
#!/usr/bin/env python3
"""D/T5 guard calibration: collect corr distributions per acoustic condition.

Why this exists (2026-11-09 night): the first T5 acceptance calibrated the
near-end vs DAC-reference guard on TWO single-point samples (echo 0.652,
speech 0.214). On device the echo then measured 0.509 -- 0.009 above the
0.50 threshold -- while the "near-end" value also covered our own TTS onset
residual (0.12-0.40). A threshold cannot be chosen from single points; it
needs the DISTRIBUTION of each condition:

  echo    voice playback, nobody speaking, no stimulus: every stat sample
          is echo. The `voice_ms < settle` subset is the TTS-onset
          transient group (the one that must never fire).
  near    same playback, the camera speaker carries a NON-wake speech
          stimulus from t=2 s (speaker next to the device, the established
          unattended stimulus source). Labelled as camera-speaker speech,
          NOT a human: the human sample joins at the in-person round.

The firmware logs one `[barge-in] stat corr=… energy=… ref=… voice_ms=…
settle=…` line per `--barge-in-stats` interval while the voice channel
plays (fw 0.0.18, opt-in). Rounds are separated by `[T5CAL …]` markers
echoed into the same RAM log, so per-round attribution needs no clock sync.

Usage:
  t5_bargein_calibrate.py --group echo --rounds 6
  t5_bargein_calibrate.py --group near --rounds 6
  t5_bargein_calibrate.py --analyze docs/d/evidence/t5-corr-*.tsv

Constraints: voice channel only (no music), device volume <= 80%.
"""
import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
PORT = os.environ.get("HM_SERIAL", "/dev/ttyUSB0")
LOG = "/run/hm-direct.log"
ASSET = "erle-stimulus.wav"
CAMERA_OUT = "2K USB Camera-Audio"
MAC_OUT = "Mac Studio扬声器"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
STIMULI = [
    os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat1.boost.wav"),
    os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat2.boost.wav"),
    os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat3.boost.wav"),
]

STAT_RE = re.compile(
    r"\[barge-in\] stat corr=(?P<corr>-?[0-9.]+) energy=(?P<energy>-?[0-9.]+) "
    r"ref=(?P<ref>-?[0-9.]+) voice_ms=(?P<voice_ms>[0-9]+) settle=(?P<settle>[01])"
)
MARK_RE = re.compile(r"\[T5CAL round=(?P<round>[0-9]+) group=(?P<group>[a-z]+)\]")


def post(path, body=None, timeout=60):
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def device():
    return get("/api/v0/mode")["devices"][0]


def serial_cmd(ser, cmd, wait=1.5):
    return ser.cmd(cmd, wait)


# The three phrases are 1.2-2.0 s each; two passes (~8.7 s) give the
# 500 ms stat sampler enough near-end samples per round.
NEAR_WIN = (2000, 11500)


def play_stimulus():
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False)
    for _ in range(2):
        for p in STIMULI:
            subprocess.run(["afplay", p], check=False)
    subprocess.run([SWITCH, "-s", MAC_OUT], check=False)


def run_round(ser, i, group, stim_at):
    stat = lambda body: serial_cmd(ser, f"printf '\\n{body}\\n' >> {LOG}", 1.5)
    stat(f"[T5CAL round={i} group={group}]")
    result = {}

    def _play():
        try:
            result.update(
                post("/api/v0/diagnostics/tests/play_asset",
                     {"asset": ASSET, "channel": "voice"}, timeout=120)
            )
        except Exception as e:  # noqa: BLE001 - evidence, never crash the round
            result["error"] = str(e)

    th = threading.Thread(target=_play, daemon=True)
    t0 = time.time()
    th.start()
    level = None
    if group == "near":
        time.sleep(stim_at)
        # Capture the mic level WHILE the stimulus plays (proves it reached
        # the device at speech-like level, not a whisper).
        stim_thread = threading.Thread(target=play_stimulus, daemon=True)
        stim_thread.start()
        time.sleep(1.0)
        try:
            level = (device().get("mic") or {}).get("levels")
        except Exception:  # noqa: BLE001
            level = None
        stim_thread.join(timeout=15)
    th.join(timeout=40)
    time.sleep(1.0)
    print(
        f"round {i} [{group}] play={result.get('stage') or result.get('error')} "
        f"duration={result.get('duration_ms')}ms elapsed={round(time.time()-t0,1)}s "
        f"levels={level}"
    )
    return result


def analyze(paths):
    rows = []
    for p in paths:
        cur = ("?", "?")
        for line in open(p, encoding="utf-8", errors="replace"):
            m = MARK_RE.search(line)
            if m:
                cur = (m.group("round"), m.group("group"))
                continue
            s = STAT_RE.search(line)
            if s:
                rows.append({
                    "round": cur[0], "group": cur[1],
                    "corr": float(s.group("corr")),
                    "energy": float(s.group("energy")),
                    "ref": float(s.group("ref")),
                    "voice_ms": int(s.group("voice_ms")),
                    "settle": s.group("settle") == "1",
                })
    if not rows:
        print("no stat lines parsed", file=sys.stderr)
        return 1

    def pct(v, q):
        v = sorted(v)
        return v[min(len(v) - 1, int(len(v) * q))]

    def block(name, vals, energies):
        if not vals:
            return
        print(
            f"{name:<28} n={len(vals):4d}  min={min(vals):.3f}  p05={pct(vals,0.05):.3f}  "
            f"p50={pct(vals,0.50):.3f}  p95={pct(vals,0.95):.3f}  max={max(vals):.3f}  "
            f"energy p50={pct(energies,0.50):.1f}dBFS"
        )

    echo_all = [r["corr"] for r in rows if r["group"] == "echo"]
    echo_e = [r["corr"] for r in rows if r["group"] == "echo" and not r["settle"]]
    echo_s = [r["corr"] for r in rows if r["group"] == "echo" and r["settle"]]
    near = [r for r in rows if r["group"] == "near"]
    # Near-end label is approximate by construction: the stimulus starts at
    # --stim-at into the play; samples from that point until +6 s are the
    # near-end window, earlier ones are echo (documented, not hidden).
    near_win = [r["corr"] for r in near if NEAR_WIN[0] <= r["voice_ms"] <= NEAR_WIN[1]]
    near_pre = [r["corr"] for r in near if r["voice_ms"] < 1800]
    print(f"\n=== T5 corr distributions ({len(rows)} stat samples) ===")
    block("echo (all)", echo_all, [r["energy"] for r in rows if r["group"] == "echo"])
    block("echo steady (settle=0)", echo_e, [r["energy"] for r in rows if r["group"] == "echo" and not r["settle"]])
    block("TTS onset (settle=1)", echo_s, [r["energy"] for r in rows if r["group"] == "echo" and r["settle"]])
    block("near-end (camera window)", near_win,
          [r["energy"] for r in near if NEAR_WIN[0] <= r["voice_ms"] <= NEAR_WIN[1]])
    block("near round pre-stim", near_pre, [r["energy"] for r in near if r["voice_ms"] < 1800])
    if echo_e and near_win:
        gap_lo, gap_hi = max(near_win), min(echo_e)
        print(
            f"\nseparation: max(near)={gap_lo:.3f} vs min(echo steady)={gap_hi:.3f} "
            f"-> gap={gap_hi-gap_lo:.3f} (threshold at midpoint = {(gap_lo+gap_hi)/2:.3f})"
        )
        print(f"margin each side = {(gap_hi-gap_lo)/2:.3f} (target >= 0.1)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--group", choices=["echo", "near"], help="acoustic condition for this run")
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--stim-at", type=float, default=2.0, help="seconds into the play to start the camera stimulus")
    ap.add_argument("--out", default=None)
    ap.add_argument("--analyze", nargs="*", default=None)
    a = ap.parse_args()
    if a.analyze is not None:
        return analyze(a.analyze)
    if not a.group:
        ap.error("--group or --analyze required")

    d = device()
    vol = d.get("volume")
    if isinstance(vol, (int, float)) and vol > 0.8:
        print(f"REFUSE: device volume {vol} > 80%", file=sys.stderr)
        return 2
    out = a.out or f"/tmp/hm-t5/corr-{a.group}-{time.strftime('%Y%m%d-%H%M%S')}.tsv"
    os.makedirs(os.path.dirname(out), exist_ok=True)

    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from fw_restart import Serial  # noqa: E402

    ser = Serial(PORT)
    try:
        print(f"group={a.group} rounds={a.rounds} volume={vol} out={out}")
        for i in range(1, a.rounds + 1):
            run_round(ser, i, a.group, a.stim_at)
        reply = serial_cmd(ser, f"grep -E '\\[T5CAL|\\[barge-in\\] stat' {LOG}", 15)
    finally:
        ser.close()
    lines = [ln for ln in reply.splitlines() if "[T5CAL" in ln or "[barge-in] stat" in ln]
    with open(out, "w", encoding="utf-8") as f:
        f.write(f"# T5 calibration group={a.group} rounds={a.rounds} volume={vol} "
                f"{time.strftime('%Y-%m-%d %H:%M:%S')}\n")
        for ln in lines:
            f.write(ln.strip() + "\n")
    print(f"captured {len(lines)} lines -> {out}")
    return analyze([out])


if __name__ == "__main__":
    sys.exit(main())
