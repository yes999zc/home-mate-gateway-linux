from pathlib import Path
# T8 self-hear battery (full-duplex-plan §4, echo root-fix acceptance):
# with the post-play blind window REMOVED (tail_blind_ms=0) and the device
# AEC at tail=2560, our own playback must not open mic segments — during
# media playback (mic open) or after any playback (no window). Stimulus
# comes from the USB camera speaker next to the device (user directive
# 2026-11-09: unattended voice testing rides the camera + TTS phrases).
# Usage: selfhear_run.py <cycles> [out_log]
import json
import os
import subprocess
import sys
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
PHRASES = os.path.join(MAIN, "data/acoustic/phrases")
WAKE_BARE = os.path.join(PHRASES, "wake_bare.wav")        # "你好，高达"
WAKE_COMMAND = os.path.join(PHRASES, "wake_command.wav")  # wake + a command
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=6) as r:
        return json.load(r)


def post(path, body=None, timeout=90):
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def mic():
    return get("/api/v0/mode")["devices"][0]["mic"]


def counters():
    return mic()["counters"]


def voice_events(since_ms):
    evs = get("/api/v0/voice/log")["events"]
    return [e for e in evs if e.get("ts_ms", 0) > since_ms]


def camera_play(wav):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False)
    subprocess.run(["afplay", wav], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False)


def main() -> int:
    cycles = int(sys.argv[1]) if len(sys.argv) > 1 else 3
    out = sys.argv[2] if len(sys.argv) > 2 else "/tmp/hm-g6/results/selfhear.log"
    os.makedirs(os.path.dirname(out), exist_ok=True)
    log = open(out, "a", buffering=1)

    mode = get("/api/v0/mode")
    blind = mode.get("tail_blind_ms")
    log.write(
        f"# selfhear battery {time.strftime('%H:%M:%S')} cycles={cycles} "
        f"tail_blind_ms={blind} chunks/s={mic().get('chunks_per_sec')}\n"
    )
    # R1f A/B: with --allow-window the same counters are valid at a nonzero
    # window too (the window only suppresses MORE; it is logged so the two
    # runs cannot be confused).
    if blind != 0 and "--allow-window" not in sys.argv:
        log.write("ERROR: tail_blind_ms != 0 — the battery is only valid with the blind window retired\n")
        print("tail_blind_ms must be 0 (PUT /api/v0/mode/dot2-d), or pass --allow-window")
        return 1

    selfhear = 0
    for c in range(cycles):
        # --- A. local ack: bare wake → device ACK (~300ms local wav) -------
        t0 = time.time() * 1000
        before = counters()
        camera_play(WAKE_BARE)
        time.sleep(8)  # ack + immediate aftermath, room silent
        after = counters()
        evs = voice_events(t0 - 2000)
        kinds = [e["kind"] for e in evs]
        # Expected: exactly the wake segment itself; the ACK replay must
        # not open another one.
        seg = after["speech_started"] - before["speech_started"]
        ok = seg == 1 and after["wakes"] - before["wakes"] == 1
        log.write(
            f"cycle {c+1}A ack: speech_started +{seg} wakes "
            f"+{after['wakes']-before['wakes']} events={kinds} "
            f"{'OK' if ok else 'SELF-HEAR?'}\n"
        )
        if not ok:
            selfhear += max(0, seg - 1)

        # --- B. llm answer: wake+command → cloud answer plays (voice ch) ---
        t0 = time.time() * 1000
        before = counters()
        camera_play(WAKE_COMMAND)
        time.sleep(22)  # full chain: asr + llm + tts play + aftermath
        after = counters()
        evs = voice_events(t0 - 2000)
        kinds = [e["kind"] for e in evs]
        seg = after["speech_started"] - before["speech_started"]
        ok = seg == 1
        log.write(
            f"cycle {c+1}B answer: speech_started +{seg} events={kinds} "
            f"{'OK' if ok else 'SELF-HEAR?'}\n"
        )
        if not ok:
            selfhear += max(0, seg - 1)

        # --- C. music: media channel, mic OPEN during play, no window ------
        t0 = time.time() * 1000
        before = counters()
        play = post(
            "/api/v0/diagnostics/tests/play_asset",
            {"asset": "bargein-track.wav", "channel": "media"},
        )
        time.sleep(34)  # 30s track + tail, room silent throughout
        after = counters()
        evs = voice_events(t0 - 2000)
        kinds = [e["kind"] for e in evs]
        seg = after["speech_started"] - before["speech_started"]
        ok = seg == 0 and play.get("ok")
        log.write(
            f"cycle {c+1}C music(ok={play.get('ok')}): speech_started +{seg} "
            f"events={kinds} {'OK' if ok else 'SELF-HEAR?'}\n"
        )
        selfhear += max(0, seg)

    log.write(f"RESULT selfhear_segments={selfhear} over {cycles} cycles\n")
    print(f"self-hear segments: {selfhear} over {cycles} cycles")
    return 0 if selfhear == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
