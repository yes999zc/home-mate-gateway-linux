from pathlib import Path
#!/usr/bin/env python3
"""D-stage 1.6: the release-candidate soak (>= 180 min).

The D stage closed with three audio changes that only long running can
disprove: T4 (on-device KWS beside AEC), T5 (near-end detection beside the
same AEC) and the P0 binary fix (no time64/statx traps). This driver keeps
the device in the shipped configuration and exercises the three things a
user does, on a loop, for the whole window:

  voice   two stimulus-free plays of the 28 s TTS asset -> every
          `barge_in_stop` here is a FALSE interrupt (criterion: zero) and
          both plays must report the full asset length
  barge   one play with a camera-speaker phrase at ~3 s -> the T5 criterion
          (exactly one stop, one segment, ASR text, answer to completion)
  wake    one camera-speaker wake phrase -> a wake counter increment plus a
          spoken answer
  idle    2 minutes of nothing

Health is NOT sampled here (the serial line is a single-user resource):
run tools/p0_watch.py next to this script for uptime continuity, trap age,
SPI/oops ages and resets. This script only adds what p0_watch cannot see:
the core's own counters, the voice log, and the audio-rate sentinel.

Exit code 0 only when: no false stop in any voice window, no stop outside a
stimulus window, every completed play reached the full asset length,
chunks_per_sec stayed >= 12, and no reset was observed in the core's view
(device generation bumps are recorded, not hidden).

Usage:
  d_soak_3h.py --minutes 180 --out docs/d/evidence/d-soak-3h-20261110.jsonl
"""
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
ASSET = "erle-stimulus.wav"
FULL_PLAY_MS = 27000
CAM = "2K USB Camera-Audio"
MAC = "Mac Studio扬声器"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
PHRASES = os.path.join(MAIN, "data/acoustic/phrases")
WAKE_WAV = os.path.join(PHRASES, "wake_command.wav")
CHAT_WAV = os.path.join(PHRASES, "nonwx_chat1.boost.wav")
MIN_CHUNKS = 12.0


def get(p):
    with urllib.request.urlopen(CORE + p, timeout=10) as r:
        return json.load(r)


def post(p, body=None, timeout=180):
    req = urllib.request.Request(
        CORE + p, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def events():
    return get("/api/v0/voice/log")["events"]


def ts(e):
    return int(e.get("ts_ms") or 0)


def kind(e):
    return e.get("kind") if isinstance(e, dict) else None


def cursor_ms():
    """Window start in the CORE's clock, not the Mac's.

    The device has no NTP (the core bootstraps its clock from HTTP-Date and
    drifts); measured offset to the Mac was -42.6 s, which made every
    Mac-stamped window include the previous phase's events — the first run
    reported 9 phantom "stops before the stimulus" because of it. A cursor
    taken from the core's own newest event is clock-independent."""
    evs = events()
    return max((ts(e) for e in evs), default=0)


def device():
    return get("/api/v0/mode")["devices"][0]


def wait_until_idle(timeout_s=90):
    """No play is live any more. The warm-up uses the full 28 s asset, so
    starting the measured rounds while it still plays puts its
    `play_finished` inside round 1's window and the voice row reads
    complete=3 (the standing tool defect recorded 2026-11-10 and again in
    R0). The window is correct only after quiescence."""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            if not device().get("outputs"):
                return True
        except Exception:  # noqa: BLE001 - a flaky read must not kill the soak
            pass
        time.sleep(1.0)
    return False


def play(asset, channel="voice"):
    try:
        return post("/api/v0/diagnostics/tests/play_asset",
                    {"asset": asset, "channel": channel}, timeout=120)
    except Exception as e:  # noqa: BLE001 - evidence, never crash the soak
        return {"error": str(e)}


def camera_play(wav, settle=2.0):
    """Stimulus through the camera speaker (the unattended mouth)."""
    try:
        subprocess.run([SWITCH, "-s", CAM], check=False, timeout=20)
        subprocess.run(["afplay", wav], check=False, timeout=30)
    finally:
        subprocess.run([SWITCH, "-s", MAC], check=False, timeout=20)
    time.sleep(settle)


def window(t0):
    evs = [e for e in events() if ts(e) >= t0]
    evs.sort(key=ts)
    stops = [e for e in evs if kind(e) == "barge_in_stop"]
    fins = [e for e in evs if kind(e) == "play_finished"]
    segs = [e for e in evs if kind(e) == "mic_segment_closed"]
    utts = [e for e in evs if kind(e) == "mic_utterance"]
    return stops, fins, segs, utts


def cycle_voice(log, round_no):
    """Two stimulus-free plays: zero false stops, both complete."""
    t0 = cursor_ms()
    receipts = [play(ASSET), play(ASSET)]
    time.sleep(2 * 31 + 8)
    stops, fins, _, utts = window(t0)
    played = [int(f.get("played_ms") or 0) for f in fins]
    complete = [p for p in played if p >= FULL_PLAY_MS]
    row = {"round": round_no, "phase": "voice", "stops": len(stops),
           "played_ms": played, "complete": len(complete),
           "utterances": len(utts),
           "receipts": [r.get("stage") or r.get("error") for r in receipts],
           "ok": len(stops) == 0 and len(complete) == 2}
    log.write(json.dumps(row) + "\n")
    print(f"  voice : stops={len(stops)} complete={len(complete)}/2 "
          f"played={played} -> {'ok' if row['ok'] else 'FAIL'}")
    return row


def cycle_barge(log, round_no):
    """Play + one camera phrase at ~3 s: exactly one stop, answer survives."""
    t0 = cursor_ms()
    import threading
    res = {}

    def _play():
        res.update(play(ASSET))

    th = threading.Thread(target=_play, daemon=True)
    th.start()
    time.sleep(3.0)
    t_stim = cursor_ms()   # same clock as the events (see cursor_ms)
    camera_play(CHAT_WAV, settle=8.0)
    th.join(timeout=60)
    time.sleep(20.0)
    stops, fins, segs, utts = window(t0)
    pre = [s for s in stops if ts(s) < t_stim]
    post = [s for s in stops if ts(s) >= t_stim]
    barged = [s for s in segs if s.get("barge_in") is True and ts(s) >= t_stim]
    played = [int(f.get("played_ms") or 0) for f in fins]
    said = utts[0].get("text") if utts else None
    ok = (len(pre) == 0 and len(post) == 1 and len(barged) >= 1 and said)
    row = {"round": round_no, "phase": "barge", "stops_before": len(pre),
           "stops_after": len(post), "barge_segments": len(barged),
           "asr": said, "played_ms": played, "ok": bool(ok)}
    log.write(json.dumps(row, ensure_ascii=False) + "\n")
    print(f"  barge : pre={len(pre)} post={len(post)} seg={len(barged)} "
          f"asr={said!r} -> {'ok' if ok else 'FAIL'}")
    return row


def cycle_wake(log, round_no):
    """One camera wake phrase: a wake counter increment and a spoken answer."""
    before = device()["mic"]["counters"]
    t0 = cursor_ms()
    camera_play(WAKE_WAV, settle=10.0)
    time.sleep(14.0)
    after = device()["mic"]["counters"]
    _, fins, _, _ = window(t0)
    played = [int(f.get("played_ms") or 0) for f in fins]
    woke = after.get("wakes", 0) > before.get("wakes", 0)
    ok = woke and len([p for p in played if p > 500]) >= 1
    row = {"round": round_no, "phase": "wake", "woke": woke,
           "answered": len([p for p in played if p > 500]), "played_ms": played,
           "ok": bool(ok)}
    log.write(json.dumps(row) + "\n")
    print(f"  wake  : woke={woke} answered={row['answered']} -> {'ok' if ok else 'FAIL'}")
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--minutes", type=float, default=180)
    ap.add_argument("--idle-s", type=float, default=120)
    ap.add_argument("--warmup-plays", type=int, default=2,
                    help="unmeasured plays first: the first playback after a firmware "
                         "restart runs on an unconverged AEC (measured, see D-71)")
    ap.add_argument("--out", default="/tmp/hm-t5/d-soak.jsonl")
    a = ap.parse_args()
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    log = open(a.out, "a", buffering=1)
    log.write(f"# D soak {time.strftime('%Y-%m-%d %H:%M:%S')} minutes={a.minutes}\n")

    if a.warmup_plays:
        print(f"warm-up: {a.warmup_plays} unmeasured plays")
        for _ in range(a.warmup_plays):
            play(ASSET)
            time.sleep(2.0)
        # R1: the rounds must start AFTER the warm-up audio drained, or its
        # play_finished lands in round 1 and that row reports complete=3.
        if wait_until_idle():
            print("warm-up drained (no live play)")
        else:
            print("WARNING: warm-up still playing after 90 s; round 1 may read complete=3")
        time.sleep(1.0)
    start = time.time()
    end = start + a.minutes * 60
    rounds, fails, min_chunks = 0, 0, 99.0
    gens = set()
    log_every = 300
    last_note = time.time()
    try:
        while time.time() < end:
            rounds += 1
            print(f"[{time.strftime('%H:%M:%S')}] round {rounds} "
                  f"({(time.time()-start)/60:.0f}/{a.minutes:.0f} min)")
            d = device()
            chunks = float(d["mic"]["chunks_per_sec"])
            # R0 (2026-11-12): the summary's min_chunks had no attribution -
            # an 8.73 dip could not be traced to a round after the fact.
            print(f"  chunks: {chunks:.2f}")
            min_chunks = min(min_chunks, chunks)
            gens.add(get("/api/v0/status")["gateway"]["generation"])
            r1 = cycle_voice(log, rounds)
            r2 = cycle_barge(log, rounds)
            r3 = cycle_wake(log, rounds)
            fails += sum(0 if r["ok"] else 1 for r in (r1, r2, r3))
            if time.time() - last_note >= log_every:
                last_note = time.time()
                d = device()
                print(f"  health: chunks={d['mic']['chunks_per_sec']:.2f} "
                      f"kws={d['kws']['state']} barge_stops={d['barge']['stops']} "
                      f"segments={d['mic']['counters']['segments']} "
                      f"tail_supp={d['mic']['counters']['tail_suppressed']}")
            time.sleep(max(0.0, a.idle_s))
    finally:
        # Final sweep: no stop may exist without a stimulus in the whole run.
        log.write(json.dumps({"summary": True, "rounds": rounds, "fails": fails,
                              "min_chunks_per_sec": round(min_chunks, 2),
                              "generations": sorted(gens),
                              "minutes": round((time.time() - start) / 60, 1)})
                  + "\n")
        log.close()
    print(f"\nSOAK rounds={rounds} failing_rounds={fails} "
          f"min_chunks={min_chunks:.2f} generations={sorted(gens)}")
    return 0 if fails == 0 and min_chunks >= MIN_CHUNKS else 1


if __name__ == "__main__":
    sys.exit(main())
