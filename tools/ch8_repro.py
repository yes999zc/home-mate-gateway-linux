#!/usr/bin/env python3
"""D/T5 firmware debt #1 + #2: the cross-boot ch8 / first-run death repro matrix.

The two bugs were "cannot reproduce reliably". This script makes them
measurable instead of remembered: for N consecutive supervised firmware
restarts it records

  * the `[boot-health]` line from /run/hm-direct.log (mic alive? ch8 alive?
    did ch8 track the DAC reference on first playback?),
  * the boot counter and exit lines from supervisor.log (which boot died,
    with what code and runtime),
  * the core's reported firmware version / generation.

Usage:
  ch8_repro.py <rounds> [--port /dev/ttyUSB0] [--out matrix.tsv]
  ch8_repro.py --report supervisor.log      # offline: summarise a pulled log

It restarts the firmware via the supervisor's TERM path (never kills the
firmware directly) and waits for the core to see the device online again.
No playback is triggered by this script: `first_play_corr=n/a` on a quiet
boot is an honest result, not a failure (see boot_health.rs).

To force the correlation path, run one TTS answer between rounds with the
console or `HM_CORE` API — the tool reports only what it observes.
"""
import argparse
import os
import re
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fw_restart import Serial, FIND_FW, first_pid  # noqa: E402

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
DIR = "/data/local/tmp/hm-direct"
BOOT = re.compile(r"boot=#(\d+)")
EXIT = re.compile(r"boot=#(\d+) exit code=(\d+) ran=(\d+)s")
HEALTH = re.compile(
    r"\[boot-health\] blocks=(\d+) mic_nonzero=(\d+)/7 ch8_nonzero=(\w+) "
    r"first_play_corr=(\S+) verdict=(\S+)"
)


def core_online(timeout=40):
    """Wait until the core reports the device connected again."""
    end = time.time() + timeout
    while time.time() < end:
        try:
            with urllib.request.urlopen(CORE + "/api/v0/status", timeout=3) as r:
                import json
                st = json.load(r)
            devs = st.get("gateway", {}).get("devices", [])
            if devs and devs[0].get("online"):
                return devs[0].get("firmware", "?")
        except Exception:
            pass
        time.sleep(1)
    return None


def restart(ser, timeout=45):
    old = first_pid(ser.cmd(FIND_FW, 3)) or "?"
    sup = ""
    if old != "?":
        sup = first_pid(ser.cmd(
            f"[ -d /proc/{old} ] && awk '{{print $4}}' /proc/{old}/stat", 3
        ))
    if not sup or sup == "1":
        # Same owning-parent concern as fw_restart.py: avoid orphaned trim
        # subshells that inherited the script name but own no firmware.
        sup = first_pid(ser.cmd(
            "for p in /proc/[0-9]*; do "
            '[ -r $p/cmdline ] || continue; '
            'pp=$(awk "{print \\$4}" $p/stat 2>/dev/null); '
            'c=$(tr "\\0" " " < $p/cmdline 2>/dev/null); '
            'case "$c" in *start_server.sh*) [ "$pp" = "1" ] && basename $p;; esac; '
            "done | head -1", 3
        ))
    if not sup:
        return False
    ser.cmd(f"kill -TERM {sup}", 2)
    end = time.time() + timeout
    while time.time() < end:
        new = first_pid(ser.cmd(FIND_FW, 3))
        if new and new != old:
            return True
        time.sleep(1)
    return False


def play_once():
    """One short voice-channel play through the core's diagnostic path."""
    import json
    import urllib.request
    req = urllib.request.Request(
        CORE + "/api/v0/diagnostics/tests/play_asset",
        data=json.dumps({"asset": "follow-window-end.wav", "channel": "voice"}).encode(),
        headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            r.read()
    except Exception:  # noqa: BLE001 - evidence only
        pass


def collect(ser):
    log = ser.cmd(f"grep -E 'boot-health|alsa. playback buffer' /run/hm-direct.log | tail -8", 3)
    sup = ser.cmd(f"grep -E 'boot=#|fast-exit|slot swap' {DIR}/supervisor.log | tail -12", 3)
    health = HEALTH.search(log)
    boot = BOOT.findall(sup)
    exits = EXIT.findall(sup)
    return {
        "boot": boot[-1] if boot else "?",
        "verdict": health.group(5) if health else "no_line",
        "mic": health.group(2) if health else "?",
        "ch8": health.group(3) if health else "?",
        "corr": health.group(4) if health else "?",
        "exit_code": exits[-1][1] if exits else "?",
        "runtime": exits[-1][2] if exits else "?",
        "fast_exits": sup.count("fast-exit"),
    }


def offline_report(path):
    text = open(path, errors="replace").read()
    boots = [int(x) for x in BOOT.findall(text)] or [0]
    exits = EXIT.findall(text)
    deaths = [e for e in exits if int(e[2]) < 15]
    print(f"boots observed : {max(boots)}")
    print(f"exit events    : {len(exits)}")
    print(f"fast exits     : {len(deaths)}")
    print(f"first boot     : {'DIED' if any(int(e[0]) == 1 for e in deaths) else 'clean'}")
    for e in deaths[:10]:
        print(f"  boot=#{e[0]} code={e[1]} ran={e[2]}s")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("rounds", type=int, nargs="?", default=5)
    ap.add_argument("--port", default="/dev/ttyUSB0")
    ap.add_argument("--out", default="")
    ap.add_argument("--report", default="", help="offline: summarise a pulled supervisor.log")
    ap.add_argument("--play", action="store_true",
                    help="trigger one short voice play right after each restart so "
                         "boot_health can judge ch8 against the DAC (the ch8 race "
                         "is invisible on a quiet boot: first_play_corr=n/a)")
    a = ap.parse_args()
    if a.report:
        return offline_report(a.report)

    ser = Serial(a.port)
    rows = []
    try:
        for i in range(a.rounds):
            ok = restart(ser)
            fw = core_online()
            if a.play:
                # The sentinel keeps collecting up to 75 blocks (~6 s), so a
                # play fired the moment the core sees the device lands inside
                # the window and lets it compare ch8 with the DAC reference.
                play_once()
                time.sleep(8)
            # Give the 2 s warm-up window time to print its line.
            time.sleep(3)
            r = collect(ser)
            r["round"] = i + 1
            r["restart"] = "ok" if ok else "timeout"
            r["core"] = fw or "offline"
            rows.append(r)
            print(
                f"[{i+1}/{a.rounds}] boot=#{r['boot']} restart={r['restart']} "
                f"health={r['verdict']} mic={r['mic']}/7 ch8={r['ch8']} "
                f"corr={r['corr']} exit={r['exit_code']} ran={r['runtime']}s core={r['core']}"
            )
    finally:
        ser.close()

    bad = [r for r in rows if r["verdict"] in ("mic_all_zero", "ch8_silent", "ch8_uncorrelated")]
    quick = [r for r in rows if r["exit_code"] != "?" and r["runtime"] != "?" and int(r["runtime"]) < 15]
    print(f"\nrounds={len(rows)} health_failures={len(bad)} fast_exits={len(quick)}")
    if a.out:
        with open(a.out, "w") as f:
            keys = ["round", "boot", "restart", "verdict", "mic", "ch8", "corr",
                    "exit_code", "runtime", "fast_exits", "core"]
            f.write("\t".join(keys) + "\n")
            for r in rows:
                f.write("\t".join(str(r.get(k, "")) for k in keys) + "\n")
        print(f"matrix -> {a.out}")
    return 1 if (bad or quick) else 0


if __name__ == "__main__":
    sys.exit(main())
