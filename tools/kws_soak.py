# T4 zero-self-trigger soak: keep a channel playing continuously and count
# keyword detections. The acceptance risk is that the spotter hears OUR OWN
# playback (post-AEC residual) and fires; a detection while playing is the
# failure, a detection while idle is a different (room) question and is
# reported separately so the two cannot be confused.
#
# Usage: kws_soak.py [seconds=1800] [--channel voice|media] [--asset NAME]
#        [--idle-control]   # run the same window with NO playback
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
OUT = os.environ.get("HM_OUT", "/tmp/hm-t4/kws-soak.log")


def post(path, body=None, timeout=120):
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


def playing() -> bool:
    return device().get("activity") in ("speaking", "playing")


def keyword_events(since_ms, kinds=("keyword", "keyword_stop", "keyword_ignored")):
    out = []
    for e in get("/api/v0/voice/log").get("events", []):
        if e.get("ts_ms", 0) >= since_ms and e.get("kind") in kinds:
            out.append(e)
    return out


def main() -> int:
    seconds = int(sys.argv[1]) if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else 1800
    channel = "voice"
    if "--channel" in sys.argv:
        channel = sys.argv[sys.argv.index("--channel") + 1]
    asset = os.environ.get("HM_ASSET", "erle-stimulus.wav")
    idle = "--idle-control" in sys.argv
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    log = open(OUT, "a", buffering=1)
    start = time.time()
    start_ms = int(time.time() * 1000)
    log.write(
        f"# soak {time.strftime('%Y-%m-%d %H:%M:%S')} secs={seconds} "
        f"channel={channel if not idle else 'IDLE'} asset={asset}\n"
    )
    plays = 0
    last_note = 0
    while time.time() - start < seconds:
        if not idle and not playing():
            threading.Thread(
                target=lambda: post("/api/v0/diagnostics/tests/play_asset",
                                    {"asset": asset, "channel": channel}, timeout=120),
                daemon=True,
            ).start()
            plays += 1
        time.sleep(2)
        if time.time() - last_note >= 60:
            last_note = time.time()
            evs = keyword_events(start_ms)
            log.write(f"t={int(time.time()-start)}s plays={plays} keyword_events={len(evs)}\n")
    evs = keyword_events(start_ms)
    log.write(f"# done plays={plays} keyword_events={len(evs)}\n")
    for e in evs:
        log.write("  " + json.dumps({k: e.get(k) for k in ("kind", "ts_ms", "reason", "keyword")}, ensure_ascii=False) + "\n")
    print(f"soak done: plays={plays} keyword_events={len(evs)} -> {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
