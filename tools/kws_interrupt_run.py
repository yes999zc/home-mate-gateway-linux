from pathlib import Path
# T4 acceptance runner: keyword interruption during voice playback.
#
# One attempt = start a long VOICE-channel playback on the device, let it run,
# fire the wake phrase from the USB camera speaker, then observe whether the
# playback actually stopped. The physical latency (phrase end -> speaker
# silence) is computed afterwards from the firmware's dual-track measurement
# by tools/kws_latency_report.py — this runner only drives stimulus and
# records the outcome, so the report stays the single source of numbers.
#
# Prereqs:
#   * device firmware in measure mode: --aec-measure-secs >= attempts*20
#     (patched into /data/local/bin/start_server.sh AEC_FLAG for the run)
#   * core reachable (HM_CORE), asset erle-stimulus.wav in data/assets
#   * the 2K USB Camera speaker connected (unattended stimulus path, D-13)
#
# Usage: kws_interrupt_run.py [attempts=10] [--gap 4] [--asset NAME]
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
PHRASE = os.path.join(MAIN, "data/acoustic/phrases/wake_command.wav")
CAMERA_OUT = "2K USB Camera-Audio"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
OUT = os.environ.get("HM_OUT", "/tmp/hm-t4/kws-interrupt.log")


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


def device_state():
    dev = get("/api/v0/mode")["devices"][0]
    return dev, dev.get("kws") or {}


def voice_live() -> bool:
    """A diagnostic play is a HUB play: it never appears in outputs[], but
    the presentation activity does read "speaking" while it is live."""
    dev, _ = device_state()
    return dev.get("activity") == "speaking"


def play_phrase() -> None:
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False)
    subprocess.run(["afplay", PHRASE], check=False)
    subprocess.run([SWITCH, "-s", "Mac Studio扬声器"], check=False)


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    attempts = int(args[0]) if args else 10
    gap = 4.0
    if "--gap" in sys.argv:
        gap = float(sys.argv[sys.argv.index("--gap") + 1])
    asset = os.environ.get("HM_ASSET", "erle-stimulus.wav")
    human = "--human" in sys.argv

    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    log = open(OUT, "a", buffering=1)
    log.write(f"# t4 keyword-interrupt {time.strftime('%Y-%m-%d %H:%M:%S')} attempts={attempts} asset={asset}\n")

    # Daytime volume cap: 100/127 = 78.7% (user rule: <=80%).
    post("/api/v0/devices/dot2-d/volume", {"value": round(100 / 127, 6)})
    log.write("volume set to 100/127\n")

    hits = 0
    for i in range(attempts):
        # One play in flight at a time: a new play supersedes the previous
        # one, which would make a "did it stop?" judgement meaningless.
        for _ in range(100):
            if not voice_live():
                break
            time.sleep(0.2)
        if voice_live():
            post("/api/v0/devices/dot2-d/stop", {})
            time.sleep(2)
        _, before = device_state()
        threading.Thread(
            target=lambda: post("/api/v0/diagnostics/tests/play_asset",
                                {"asset": asset, "channel": "voice"}, timeout=120),
            daemon=True,
        ).start()
        started = False
        for _ in range(60):
            time.sleep(0.1)
            if voice_live():
                started = True
                break
        if not started:
            log.write(f"attempt={i} SKIP: playback never started\n")
            continue
        t_start = time.time()
        if human:
            # The person speaks whenever they like while the story plays; the
            # attempt ends when the playback stops (or the story runs out).
            t_stim = time.time()
        else:
            time.sleep(4.0 + (i % 3) * 0.5)
            t_stim = time.time()
            play_phrase()
        stopped = False
        for _ in range(60 if not human else 250):
            time.sleep(0.1)
            if not voice_live():
                stopped = True
                break
        t_seen = time.time()
        _, after = device_state()
        if stopped:
            hits += 1
        else:
            # Recover: an unstopped attempt must not bleed into the next one.
            post("/api/v0/devices/dot2-d/stop", {})
        log.write(
            f"attempt={i} stopped={stopped} play_to_phrase={t_stim-t_start:.2f}s "
            f"phrase_to_silence={t_seen-t_stim:.2f}s "
            f"stops={after.get('stops')}(+{after.get('stops',0)-before.get('stops',0)}) "
            f"ignored={after.get('ignored')}(+{after.get('ignored',0)-before.get('ignored',0)})\n"
        )
        time.sleep(max(0.0, gap + 8.0 - (time.time() - t_start)))

    log.write(f"# result {hits}/{attempts} stopped\n")
    print(f"{hits}/{attempts} stopped; log {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
