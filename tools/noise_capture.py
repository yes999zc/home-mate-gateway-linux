# Tail-pop forensic capture (full-duplex-plan §8 steps 1+2, 2026-11-10).
#
# Evidence-first: restart the firmware with `--aec-measure` (dual tracks:
# pre = pre-AEC 7-mic acoustic, ref = dealias reference / raw ch8 playback
# loopback, both 16k S16 mono, time-aligned), then play a fixed schedule
# through the REAL Play path:
#   tone ×3   follow-window-end.wav (275 ms pure tone — user-reported pop)
#   zeros ×1  3 s of pure silence -> EOS (§8 step 2: content-independent?)
#   speech ×1 erle-stimulus.wav (28 s TTS voice, real answer tail)
# then pull the three tracks + logs over LAN, RESTORE the supervisor flag,
# and restart the firmware clean. Every step is reversible (sed back).
# Usage: noise_capture.py <run_dir> [port]
import json
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from fw_restart import Serial, first_pid  # noqa: E402

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DEVICE_IP = os.environ.get("HM_DEVICE_IP", "192.168.1.73")
HTTPD_PORT = 8943
STATE = "/data/local/tmp/hm-direct"
SUP = "/data/local/bin/start_server.sh"
ASSETS = "/data/hm/data/assets"
MAC_IP = os.environ.get("HM_HOST_IP", "192.168.1.27")
FIND_FW = (
    "for p in /proc/[0-9]*; do "
    '[ -r $p/cmdline ] || continue; '
    'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
    'case "$c" in *"current firmware"*) basename $p;; esac; '
    "done | head -1"
)


def sh(cmd: str, wait: float = 5.0) -> str:
    return subprocess.run(
        ["python3", os.path.join(os.path.dirname(__file__), "serial_cmd.py"), cmd, str(wait)],
        capture_output=True, text=True,
    ).stdout


def core_post(path: str, body: dict, timeout: float = 90.0) -> dict:
    req = urllib.request.Request(
        CORE + path,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def core_get(path: str) -> dict:
    with urllib.request.urlopen(CORE + path, timeout=6) as r:
        return json.load(r)


def chunks_ok() -> float:
    return core_get("/api/v0/mode")["devices"][0]["mic"]["chunks_per_sec"]


def find_sup_pid(ser) -> str:
    out = ser.cmd(
        "for p in /proc/[0-9]*; do "
        '[ -r $p/cmdline ] || continue; '
        'pp=$(awk "{print \\$4}" $p/stat 2>/dev/null); '
        'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
        'case "$c" in *start_server.sh*) [ "$pp" = "1" ] && basename $p;; esac; '
        "done | head -1",
        3,
    )
    return first_pid(out) or ""


def restart_firmware(ser, old_pid: str) -> str:
    sup = find_sup_pid(ser)
    if not sup:
        raise SystemExit("ERROR: supervisor not found")
    ser.cmd(f"kill -TERM {sup}", 2)
    deadline = time.time() + 45
    new_fw = ""
    while time.time() < deadline:
        cand = first_pid(ser.cmd(FIND_FW, 3))
        if cand and cand != old_pid:
            new_fw = cand
            break
        time.sleep(1)
    if not new_fw:
        raise SystemExit("ERROR: firmware did not come back")
    return new_fw


def main() -> int:
    run_dir = sys.argv[1]
    port = sys.argv[2] if len(sys.argv) > 2 else "/dev/ttyUSB0"
    os.makedirs(run_dir, exist_ok=True)
    ser = Serial(port)

    # 0. push the zeros asset (Mac http.server -> device busybox wget).
    os.chdir("/tmp/hm-noise-forensic")
    srv = subprocess.Popen(["python3", "-m", "http.server", "8944", "--bind", MAC_IP])
    time.sleep(1)
    out = sh(
        f"cd {ASSETS} && busybox wget -q http://{MAC_IP}:8944/zeros-3s.wav "
        f"&& md5sum zeros-3s.wav erle-stimulus.wav follow-window-end.wav", 6,
    )
    print("asset push:", out.strip())
    srv.terminate()

    # 1. supervisor flag -> measure mode (backup first, reversible).
    print(sh(f"cp {SUP} {SUP}.bak-noisecap", 3).strip())
    print(sh(
        f"sed -i 's/AEC_FLAG=\"--aec\"/AEC_FLAG=\"--aec --aec-measure\"/' {SUP} "
        f"&& grep -n AEC_FLAG {SUP}", 4,
    ).strip())

    # 2. fresh firmware boot = fresh measure tracks.
    old = first_pid(sh(FIND_FW, 3)) or "?"
    new_fw = restart_firmware(ser, old)
    print(f"firmware {old} -> {new_fw}")
    cmdline = sh(f"tr '\\0' ' ' < /proc/{new_fw}/cmdline", 3)
    if "--aec-measure" not in cmdline:
        sh(f"cp {SUP}.bak-noisecap {SUP}", 3)
        raise SystemExit("ERROR: firmware WITHOUT --aec-measure — supervisor restored, aborting")
    if "measure tracks open" not in sh("tail -5 /run/hm-direct.log", 3):
        sh(f"cp {SUP}.bak-noisecap {SUP}", 3)
        raise SystemExit("ERROR: no 'measure tracks open' — supervisor restored, aborting")

    # 3. settle + volume 100/127 (<=80% rule).
    for _ in range(20):
        try:
            if chunks_ok() >= 11.5:
                break
        except Exception:
            pass
        time.sleep(2)
    time.sleep(4)
    vol = core_post("/api/v0/devices/dot2-d/volume", {"value": round(100 / 127, 6)})
    print("volume:", vol.get("state"), "chunks/s:", round(chunks_ok(), 2))

    # 4. the stimulus schedule (real Play path, voice channel).
    schedule = []
    t0 = time.time()
    evs = []
    for i in range(3):
        schedule.append(("tone", "follow-window-end.wav", t0 + i * 6.0))
    schedule.append(("zeros", "zeros-3s.wav", t0 + 20.0))
    schedule.append(("speech", "erle-stimulus.wav", t0 + 28.0))
    for label, asset, at in schedule:
        while time.time() < at:
            time.sleep(0.05)
        r = core_post(
            "/api/v0/diagnostics/tests/play_asset",
            {"asset": asset, "channel": "voice"},
            timeout=90,
        )
        evs.append({"label": label, "asset": asset, "at_s": round(time.time() - t0, 2),
                    "ok": r.get("ok"), "stage": r.get("stage"),
                    "duration_ms": r.get("duration_ms")})
        print(f"play {label}: ok={r.get('ok')} stage={r.get('stage')}")
    # post-roll so the last tail lands inside the 120s cap.
    time.sleep(6)

    # 5. pull the tracks over LAN (busybox httpd, Mac-only rule, idempotent).
    ser.cmd(
        f"iptables -C INPUT -p tcp -s {MAC_IP} --dport {HTTPD_PORT} -j ACCEPT 2>/dev/null || "
        f"iptables -I INPUT 1 -p tcp -s {MAC_IP} --dport {HTTPD_PORT} -j ACCEPT",
        3,
    )
    sh(
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
        ok = ok and size > 100_000
    sh(
        "for p in /proc/[0-9]*; do [ -r $p/cmdline ] || continue; "
        "c=$(tr '\\0' ' ' < $p/cmdline); "
        'case "$c" in *httpd*) kill $(basename $p) 2>/dev/null;; esac; done; true',
        3,
    )
    log_tail = sh("tail -25 /run/hm-direct.log", 4)
    with open(os.path.join(run_dir, "boot_log_tail.txt"), "w") as f:
        f.write(log_tail)
    with open(os.path.join(run_dir, "meta.json"), "w") as f:
        json.dump({
            "played_at": time.strftime("%Y-%m-%d %H:%M:%S"),
            "firmware_pid": new_fw,
            "schedule": evs,
            "volume_level": 100,
        }, f, ensure_ascii=False, indent=1)

    # 6. RESTORE the supervisor and restart clean (measure off).
    print(sh(f"cp {SUP}.bak-noisecap {SUP} && grep -n AEC_FLAG {SUP}", 4).strip())
    old2 = first_pid(sh(FIND_FW, 3)) or "?"
    new2 = restart_firmware(ser, old2)
    cmdline2 = sh(f"tr '\\0' ' ' < /proc/{new2}/cmdline", 3)
    restored = "--aec-measure" not in cmdline2 and "--aec" in cmdline2
    print(f"restore restart {old2} -> {new2}, measure off: {restored}")
    ser.close()
    print("run complete:", run_dir, "pull_ok:", ok)
    return 0 if ok and restored else 1


if __name__ == "__main__":
    sys.exit(main())
