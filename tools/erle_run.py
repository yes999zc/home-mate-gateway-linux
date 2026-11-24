# G6 serial helper (emOS has no adbd; this is the only device channel).
# Port name can change on USB re-enumeration: list /dev/cu.usbmodem* and
# pick the one that appears when the device boots (baseline the others).
# Usage: erle_run.py <run_dir> [port]
#!/usr/bin/env python3
"""One ERLE measurement run (acceptance §0, G6 pre-freeze sequence).

Per run, in order:
  1. firmware restart with measurement tracks fresh (the caller keeps the
     supervisor in `--aec --aec-measure` mode for the whole campaign);
  2. wait for reconnect + settle, then device volume -> 100/127 (software
     law, level=100 condition);
  3. play the archived stimulus via the core's play_asset diagnostic
     (the real Play path; ~28s);
  4. capture dealias lock state across the playback window from
     /run/hm-direct.log (an unlocked reference invalidates the run);
  5. pull pre/post/ref tracks off the device over LAN (busybox httpd on
     the ACM console to start it, curl from the Mac) into <run_dir>.

The acoustic placement (speaker on-axis 1m, level marking, still room) is
the OPERATOR's part — this script only runs the machine side.
"""
import json
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from fw_restart import Serial, first_pid  # noqa: E402

# Stage D: the core lives ON the speaker; override with HM_CORE when the
# Mac dev instance is the target again.
CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DEVICE_IP = os.environ.get("HM_DEVICE_IP", "192.168.1.73")
HTTPD_PORT = 8943
STATE = "/data/local/tmp/hm-direct"
FIND_FW = (
    "for p in /proc/[0-9]*; do "
    '[ -r $p/cmdline ] || continue; '
    'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
    'case "$c" in *"current firmware"*) basename $p;; esac; '
    "done | head -1"
)
# The supervisor is the FIRMWARE's parent — not "the pid-1 shell with
# start_server.sh in cmdline": after a supervisor death its log-trim child
# shell is re-parented to init and keeps that cmdline, so pid order picks
# the orphan and the TERM goes nowhere (lesson D-17, re-hit 2026-11-10).
FIND_SUP = (
    "FW=$(" + FIND_FW + "); "
    '[ -n "$FW" ] && [ -r /proc/$FW/stat ] && awk \'{print $4}\' /proc/$FW/stat'
)


def sh(cmd: str, wait: float = 5.0) -> str:
    return subprocess.run(
        ["python3", os.path.join(os.path.dirname(__file__), "serial_cmd.py"), cmd, str(wait)],
        capture_output=True, text=True,
    ).stdout


def core_post(path: str, body: dict, timeout: float = 60.0) -> dict:
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def chunks_ok() -> float:
    with urllib.request.urlopen(CORE + "/api/v0/mode", timeout=5) as r:
        return json.load(r)["devices"][0]["mic"]["chunks_per_sec"]


def main() -> int:
    run_dir = sys.argv[1]
    port = sys.argv[2] if len(sys.argv) > 2 else "/dev/ttyUSB0"
    os.makedirs(run_dir, exist_ok=True)
    ser = Serial(port)

    # 1. fresh firmware boot = fresh measure tracks (files truncate).
    old = first_pid(ser.cmd(FIND_FW, 3)) or "?"
    sup_out = ser.cmd(FIND_SUP, 3)
    sup = first_pid(sup_out)
    if not sup:
        print("ERROR: supervisor not found")
        return 1
    ser.cmd(f"kill -TERM {sup}", 2)
    deadline = time.time() + 45
    new_fw = ""
    while time.time() < deadline:
        cand = first_pid(ser.cmd(FIND_FW, 3))
        if cand and cand != old:
            new_fw = cand
            break
        time.sleep(1)
    if not new_fw:
        print("ERROR: firmware did not come back")
        return 1
    print(f"firmware {old} -> {new_fw}")
    # Self-guard: measure tracks only open when the supervisor actually
    # passed --aec-measure (the steady config is --aec only). Without it
    # this run would record nothing and the numbers would be a lie.
    cmdline = ser.cmd(f"tr '\\0' ' ' < /proc/{new_fw}/cmdline", 3)
    if "--aec-measure" not in cmdline:
        print("ERROR: firmware runs WITHOUT --aec-measure — sed the supervisor first:")
        print('  sed -i \'s/AEC_FLAG="--aec"/AEC_FLAG="--aec --aec-measure"/\' /data/local/bin/start_server.sh')
        return 1
    # Grep the whole log, not the tail: with the KWS sidecar down the
    # supervisor's backoff lines churn the last few lines within seconds and
    # pushed this marker out of `tail -5` (R0, 2026-11-12).
    if "measure tracks open" not in ser.cmd("grep 'measure tracks open' /run/hm-direct.log", 4):
        print("ERROR: no 'measure tracks open' in the boot log")
        return 1

    # 2. reconnect + settle (speex states warm, chunks flowing).
    for _ in range(20):
        try:
            c = chunks_ok()
            if c >= 11.5:
                break
        except Exception:
            pass
        time.sleep(2)
    time.sleep(3)
    vol = core_post("/api/v0/devices/dot2-d/volume", {"value": round(100 / 127, 6)})
    print("volume:", vol.get("state"), "chunks/s:", round(chunks_ok(), 2))
    lock_before = ser.cmd("grep 'dealias locked' /run/hm-direct.log | tail -1", 4)
    print("lock line before play:", [l for l in lock_before.splitlines() if "locked" in l][-1:])

    # 3. the stimulus, through the real play path.
    t0 = time.time()
    play = core_post(
        "/api/v0/diagnostics/tests/play_asset",
        {"asset": "erle-stimulus.wav"},
        timeout=90,
    )
    print("play:", json.dumps(play, ensure_ascii=False))
    if not play.get("ok"):
        return 1
    time.sleep(2)
    lock_after = ser.cmd("grep 'dealias locked' /run/hm-direct.log | tail -2", 4)
    lock_lines = [l.strip() for l in lock_after.splitlines() if "locked" in l]
    print("lock lines after play:", lock_lines)

    # 4. pull the three tracks over LAN. emOS's firewall drops all inbound
    #    (INPUT policy DROP, first ACCEPT is -i lo): open ONE rule for the
    #    measurement port from this Mac only, idempotent per boot.
    MAC_IP = os.environ.get("HM_HOST_IP", "192.168.1.27")
    ser.cmd(
        f"iptables -C INPUT -p tcp -s {MAC_IP} --dport {HTTPD_PORT} -j ACCEPT 2>/dev/null || "
        f"iptables -I INPUT 1 -p tcp -s {MAC_IP} --dport {HTTPD_PORT} -j ACCEPT",
        3,
    )
    # kill any stale httpd from an earlier run (busybox applet name)
    ser.cmd(
        "for p in /proc/[0-9]*; do [ -r $p/cmdline ] || continue; "
        "c=$(tr '\\0' ' ' < $p/cmdline); "
        'case "$c" in *httpd*) kill $(basename $p) 2>/dev/null;; esac; done; true',
        3,
    )
    ser.cmd(f"busybox httpd -f -p {HTTPD_PORT} -h {STATE}/aec-measure &", 2)
    time.sleep(1)
    ok = True
    for name in ("pre.pcm", "post.pcm", "ref.pcm"):
        r = subprocess.run(
            ["curl", "-s", "-m", "30", "-o", os.path.join(run_dir, name),
             f"http://{DEVICE_IP}:{HTTPD_PORT}/{name}"],
        )
        size = os.path.getsize(os.path.join(run_dir, name)) if r.returncode == 0 else 0
        print(f"pulled {name}: {size} bytes")
        ok = ok and size > 800_000
    # stop httpd (leave the console clean); the iptables rule is runtime-only
    ser.cmd(
        "for p in /proc/[0-9]*; do [ -r $p/cmdline ] || continue; "
        "c=$(tr '\\0' ' ' < $p/cmdline); "
        'case "$c" in *httpd*) kill $(basename $p) 2>/dev/null;; esac; done; true',
        3,
    )
    with open(os.path.join(run_dir, "meta.json"), "w") as f:
        json.dump({
            "played_at": time.strftime("%Y-%m-%d %H:%M:%S"),
            "wall_play_start": t0,
            "play": play,
            "lock_lines_after": lock_lines,
            "firmware_pid": new_fw,
        }, f, ensure_ascii=False, indent=1)
    print("run complete:", run_dir, "pull_ok:", ok)
    ser.close()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
