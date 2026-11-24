from pathlib import Path
# R1d local-stop acceptance: a long answer plays, real speech lands next to
# the device, and the DEVICE must cut the audio itself (<=300 ms fire→flush)
# instead of waiting for the core's Stop (3.078 s fact + 1.9 s stop measured
# on 2026-11-12). Evidence comes from three places that cannot fake each
# other: the firmware log (LOCAL STOP + fire→flush ms), the core event log
# (stopped_locally + wire_ms/pump_ms), and the device's own played_ms.
# Usage: local_stop_probe.py [--wav-command F] [--wav-wake F] [--delay-ms N]
import argparse
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
PHRASES = os.path.join(MAIN, "data/acoustic/phrases")
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def mode():
    return get("/api/v0/mode")


def dev():
    return mode()["devices"][0]


def play(wav):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False, stdout=subprocess.DEVNULL)
    subprocess.run(["afplay", wav], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False,
                   stdout=subprocess.DEVNULL)


def voice_play_live():
    """A voice-channel output that has actually started (not accepted-only)."""
    for o in dev().get("outputs") or []:
        # A streamed answer reports accepted -> streaming -> device_confirmed;
        # waiting for the last one missed every long answer (2026-11-12).
        if o.get("channel") == "voice" and o.get("stage") in (
            "accepted", "streaming", "device_confirmed", "playing"
        ):
            return o
    return None


def quiet(wait_s=12.0):
    deadline = time.time() + wait_s
    while time.time() < deadline:
        m = dev()["mic"]
        mean = (m.get("levels") or {}).get("mean_db")
        if not m.get("in_speech") and mean is not None and mean < -50:
            return True
        time.sleep(0.5)
    return False


def events_since(ts_ms):
    return [e for e in get("/api/v0/voice/log")["events"] if e.get("ts_ms", 0) > ts_ms]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--wav-command", default=os.path.join(PHRASES, "wake_command.wav"))
    ap.add_argument("--wav-wake", default=os.path.join(PHRASES, "wake_bare.wav"))
    ap.add_argument("--delay-ms", type=int, default=1500, help="answer age before the barge")
    ap.add_argument("--wait-play-ms", type=int, default=20000)
    args = ap.parse_args()

    if not quiet():
        print("room not quiet — refusing to play")
        return 2
    t0 = int(time.time() * 1000)
    before = dev()["mic"]["counters"]

    print(f"stimulus: {args.wav_command}")
    play(args.wav_command)

    # Wait for the answer's voice channel to become live.
    started = None
    deadline = time.time() + args.wait_play_ms / 1000
    while time.time() < deadline:
        o = voice_play_live()
        if o:
            started = o
            break
        time.sleep(0.2)
    if not started:
        print("no voice playback started — record the failure and stop")
        print(json.dumps(events_since(t0), ensure_ascii=False)[:2000])
        return 3
    print(f"answer live: {json.dumps(started, ensure_ascii=False)}")
    time.sleep(args.delay_ms / 1000)

    print(f"near-end stimulus at answer age {args.delay_ms} ms: {args.wav_wake}")
    t_barge = int(time.time() * 1000)
    play(args.wav_wake)
    time.sleep(7.0)

    after = dev()["mic"]["counters"]
    evs = events_since(t0)
    kinds = [e.get("kind") for e in evs]
    print(json.dumps({
        "answer_play": started,
        "counters_delta": {k: after[k] - before[k] for k in after},
        "kinds": kinds,
        "barge_events": [e for e in evs if e.get("kind") in
                         ("barge_in", "barge_in_stop", "barge_in_ignored",
                          "stopped_locally", "stop_result", "play_finished",
                          "mic_utterance")],
        "t0": t0,
        "t_barge": t_barge,
    }, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
