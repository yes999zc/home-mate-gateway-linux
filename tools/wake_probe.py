from pathlib import Path
# Wake-word probe (R1d acceptance): play one wake phrase N times from the
# camera speaker next to the device and record what the ASR actually heard.
# The point is the TRANSCRIPT, not the acoustic level: a wake that fails
# because SenseVoice heard "高单" looks identical to a quiet room unless the
# raw text is kept. Counters are read from the device, never inferred.
# Usage: wake_probe.py [cycles=5] [gap_s=40] [wav] [out_log]
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
DEFAULT_WAV = os.path.join(MAIN, "data/acoustic/phrases/wake_bare.wav")
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def mic():
    return get("/api/v0/mode")["devices"][0]["mic"]


def counters():
    c = mic()["counters"]
    return {k: c.get(k, 0) for k in ("wakes", "ignored_unwoken", "asr_failed",
                                     "speech_started", "segments")}


def events_since(ts_ms):
    evs = get("/api/v0/voice/log")["events"]
    return [e for e in evs if e.get("ts_ms", 0) > ts_ms]


def play(wav):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False,
                   stdout=subprocess.DEVNULL)
    subprocess.run(["afplay", wav], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False,
                   stdout=subprocess.DEVNULL)


def quiet_enough(wait_s=10.0):
    """Pre-flight (user rule): never play into a room that is talking. The
    device reports mean_db over its last 100 blocks; -55 dBFS is the quiet
    floor seen in this room, so require mean below -50 before any stimulus."""
    deadline = time.time() + wait_s
    while time.time() < deadline:
        m = mic()
        lv = m.get("levels") or {}
        mean = lv.get("mean_db")
        if not m.get("in_speech") and mean is not None and mean < -50:
            return True
        time.sleep(0.5)
    return False


def main() -> int:
    cycles = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    gap = float(sys.argv[2]) if len(sys.argv) > 2 else 40.0
    wav = sys.argv[3] if len(sys.argv) > 3 else DEFAULT_WAV
    out = sys.argv[4] if len(sys.argv) > 4 else "/tmp/hm-r1d/wake-probe.log"
    os.makedirs(os.path.dirname(out), exist_ok=True)

    rows = []
    for c in range(cycles):
        if not quiet_enough():
            print(f"cycle {c+1}: room not quiet — stop (no stimulus played)")
            rows.append({"cycle": c + 1, "skipped": "room_not_quiet"})
            break
        before, t0 = counters(), int(time.time() * 1000)
        play(wav)
        time.sleep(9.0)  # wake ack (~0.3 s) + segment close + ASR round trip
        after = counters()
        evs = events_since(t0)
        kinds = [e.get("kind") for e in evs]
        texts = [e.get("text") for e in evs if e.get("kind") == "mic_utterance"]
        utt = [e for e in evs if e.get("kind") == "mic_utterance"]
        row = {
            "cycle": c + 1,
            "t_ms": t0,
            "delta": {k: after[k] - before[k] for k in after},
            "texts": texts,
            "events": kinds,
            "wake_ack": any(e.get("kind") == "assistant_state" and
                            e.get("state") == "speaking" for e in evs),
            "utterance_meta": [{k: u.get(k) for k in
                                ("text", "duration_ms", "source", "wake", "rest")}
                               for u in utt],
        }
        rows.append(row)
        print(json.dumps(row, ensure_ascii=False))
        if c < cycles - 1:
            time.sleep(gap)
    with open(out, "a") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
