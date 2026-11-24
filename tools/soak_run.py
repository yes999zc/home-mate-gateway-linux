from pathlib import Path
# T8 30-minute self-hear soak (full-duplex-plan §4): ACK / answer / music
# plays over 30 minutes at the steady config (blind window as set), with
# the only legitimate mic segment per cycle being the camera stimulus
# (wake+question). Everything else that opens a segment during a play or
# a silent gap is self-hear or a false open — counted, timestamped, and
# attributed from the voice log.
# Usage: soak_run.py [minutes=30] [out_log]
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
STIM = os.path.join(MAIN, "data/acoustic/phrases/wake_command.wav")
CAM = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def get(p):
    with urllib.request.urlopen(CORE + p, timeout=6) as r:
        return json.load(r)


def post(p, body=None, timeout=90):
    req = urllib.request.Request(
        CORE + p, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def mic():
    return get("/api/v0/mode")["devices"][0]["mic"]


def speech_events_since(ts_ms):
    evs = get("/api/v0/voice/log")["events"]
    return [e for e in evs if e.get("kind") == "mic_speech_started" and e.get("ts_ms", 0) > ts_ms]


def camera_stimulus():
    subprocess.run([SWITCH, "-s", CAM], check=False)
    subprocess.run(["afplay", STIM], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False)


def main() -> int:
    minutes = int(sys.argv[1]) if len(sys.argv) > 1 else 30
    out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/hm-g6/results/soak.log"
    os.makedirs(os.path.dirname(out), exist_ok=True)
    log = open(out, "a", buffering=1)
    mode = get("/api/v0/mode")
    log.write(
        f"# soak {time.strftime('%H:%M:%S')} {minutes}min "
        f"tail_blind_ms={mode.get('tail_blind_ms')} volume=100/127\n"
    )
    post("/api/v0/devices/dot2-d/volume", {"value": round(100 / 127, 6)})

    t_end = time.time() + minutes * 60
    cycle = 0
    bad = 0
    while time.time() < t_end:
        cycle += 1
        c0 = mic()["counters"]

        # A. LLM answer (voice channel): the stimulus is the ONLY
        # legitimate segment of this cycle.
        t_seg = time.time() * 1000
        camera_stimulus()
        time.sleep(22)

        # B. broadcast-class speech on the voice channel (28s).
        t_b = time.time() * 1000
        post("/api/v0/diagnostics/tests/play_asset",
             {"asset": "erle-stimulus.wav", "channel": "voice"})
        time.sleep(32)

        # C. music on the media channel (30s, mic open during play).
        t_c = time.time() * 1000
        post("/api/v0/diagnostics/tests/play_asset",
             {"asset": "bargein-track.wav", "channel": "media"})
        time.sleep(36)

        c1 = mic()["counters"]
        segs = speech_events_since(t_seg - 1500)
        # 1 legitimate (the stimulus) + anything else is suspect.
        suspects = len(segs) - 1
        bad += max(0, suspects)
        log.write(
            f"cycle {cycle}: speech_started {c0['speech_started']}->{c1['speech_started']} "
            f"segments_in_window={len(segs)} suspects={suspects} "
            f"wakes {c0['wakes']}->{c1['wakes']} "
            f"tail_suppressed {c0['tail_suppressed']}->{c1['tail_suppressed']} "
            f"self_echo {c0['self_echo_suppressed']}->{c1['self_echo_suppressed']}\n"
        )
        # spacing: let the participation window expire before the next
        # stimulus so the wake stays a fresh wake.
        gap = max(0.0, 40 - (time.time() - (t_end - minutes * 60)) % 1)
        time.sleep(35)

    log.write(f"RESULT suspect_segments={bad} over {cycle} cycles / {minutes}min\n")
    print(f"soak: {bad} suspect segments over {cycle} cycles")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
