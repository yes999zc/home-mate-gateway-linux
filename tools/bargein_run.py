from pathlib import Path
# G6 barge-in test (acceptance §5), stage-D edition: heavy track at
# level=100 on the media channel, stimulus from the camera speaker next
# to the device. The D acceptance wants REAL interruptions caught >=4/5.
#
# Verdict honesty (2026-11-10 rewrite): three mechanisms used to blur as
# "miss" — (a) the takeover landed as a FOLLOW-UP turn (30s window still
# open from the previous round; the user's voice DID take over but the
# wakes counter never moves), (b) an ASR spike delayed the match past the
# old 12s window, (c) a genuine wake miss. The verdict now requires the
# FUNCTIONAL outcome — music stopped AND a new turn (wake or follow-up)
# — with a 20s window and 35s drains so rounds never overlap the
# participation window.
#
# Usage: bargein_run.py <rounds> [out_log] [--command]
#   --command: stimulus = wake + a question (真打断类: new command must
#   be answered), else bare wake + greeting (stop + ack).
#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time
import urllib.request

# Stage D: core lives ON the speaker; override with HM_CORE for a Mac dev
# instance. Stimulus plays on the USB camera speaker (user directive
# 2026-11-09: unattended voice testing rides the camera's speaker+mic).
CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
PHRASES = {
    "bare": os.path.join(MAIN, "data/acoustic/phrases/wake_greeting.wav"),
    "command": os.path.join(MAIN, "data/acoustic/phrases/wake_command.wav"),
}
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"


def post(path, body=None, timeout=30):
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=5) as r:
        return json.load(r)


def counters():
    return get("/api/v0/mode")["devices"][0]["mic"]["counters"]


def outputs():
    return get("/api/v0/mode")["devices"][0].get("outputs") or []


def play_stimulus(mode):
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False)
    subprocess.run(["afplay", PHRASES[mode]], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False)


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    mode = "command" if "--command" in sys.argv else "bare"
    rounds = int(args[0]) if args else 5
    out = args[1] if len(args) > 1 else f"/tmp/hm-g6/results/bargein-{mode}.log"
    os.makedirs(os.path.dirname(out), exist_ok=True)
    log = open(out, "a", buffering=1)
    log.write(f"# bargein start {time.strftime('%H:%M:%S')} rounds={rounds} mode={mode}\n")

    # Conditions per acceptance: level=100 software law (=78.7% <= the
    # user's 80% daytime cap).
    post("/api/v0/devices/dot2-d/volume", {"value": round(100 / 127, 6)})
    log.write("volume set to 100/127\n")

    hits = 0
    for r in range(rounds):
        c0 = counters()
        t0 = time.time() * 1000
        play = post(
            "/api/v0/diagnostics/tests/play_asset",
            {"asset": "bargein-track.wav", "channel": "media"},
            timeout=90,
        )
        if not play.get("ok"):
            log.write(f"round {r+1}: music FAILED {play.get('error')}\n")
            continue
        play_id = play.get("play_id")
        time.sleep(4)
        log.write(f"round {r+1}: music on (play_id={str(play_id)[-8:]}), stimulus {mode}\n")
        play_stimulus(mode)

        # Functional verdict, 20s window: the music play must DISAPPEAR
        # from outputs AND a new turn counter must move (wake = fresh
        # session, followup = takeover inside a still-open window; both
        # are the user's voice winning).
        deadline = time.time() + 20
        music_gone = False
        turn_taken = False
        while time.time() < deadline:
            c = counters()
            if c["wakes"] > c0["wakes"] or c["followup_turns"] > c0["followup_turns"]:
                turn_taken = True
            if not any(o.get("play_id") == play_id for o in outputs()):
                music_gone = True
            if turn_taken and music_gone:
                break
            time.sleep(0.5)
        kind = (
            "WAKE" if c0 is not None and counters()["wakes"] > c0["wakes"]
            else ("FOLLOWUP" if turn_taken else "-")
        )
        hit = turn_taken and music_gone
        log.write(
            f"round {r+1}: {'HIT' if hit else 'MISS'} turn={kind} "
            f"music_stopped={music_gone} "
            f"wakes {c0['wakes']}->{counters()['wakes']} "
            f"followups {c0['followup_turns']}->{counters()['followup_turns']}\n"
        )
        if hit:
            hits += 1
        # Drain 35s: answer finishes AND the participation window closes
        # so the next round cannot land as a follow-up by accident.
        time.sleep(35)

    log.write(f"RESULT {hits}/{rounds} (mode={mode})\n")
    print(f"barge-in {mode}: {hits}/{rounds}")
    return 0 if hits * 5 >= rounds * 4 else 1


if __name__ == "__main__":
    sys.exit(main())
