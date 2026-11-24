#!/system/bin/sh
# Home Mate direct-occupation supervisor — G6 emOS-aware edition (2026-11-08).
# One script, both platforms:
#   - FireOS: exactly the validated G5-2 behavior (P1–P5 fixes included).
#   - emOS:   emOS init (PID1) execs this after /run/net-up; differences are
#             branched on /etc/os-release — no ledcontroller (no Android),
#             logs on the /run tmpfs (no /tmp), wake_lock direct.
# The codec bring-up itself now lives INSIDE the firmware (direct mixer
# ioctls, the complete EchoMuse codec sequence — user direction 2026-11-08:
# the old script only carried part of it and emOS has no HAL to fill in).
# The tinymix chain below is a deliberately idempotent belt on BOTH paths
# until the firmware-internal sequence is validated on-device.
#
# A/B slots + fast-exit swap. A failed new slot rolls back to the other
# firmware slot. If that slot also burns its fast-exit budget, stay there,
# log, and paint the error ring. Never exec EchoMuse.

# /data/hm/tools (last) is the seed's own applet dir: an older FireOS base has
# no toybox/busybox applets (no tail, wc, awk — measured on dot2-c 2026-11-14),
# so the bootstrap installer links the node's busybox there. Empty/absent on
# emOS, where /system already provides them: the entry is inert.
export PATH=/sbin:/system/bin:/system/xbin:/data/hm/tools
# emOS spawn() passes no PATH — without this line every tool below is
# "not found" on emOS while working fine on FireOS (which sets it in rc).

# ── CPU: MTK HPS keeps exactly ONE core online at steady state (measured
#    2026-11-10: /proc/hps/enabled=1, dmesg "CPU1: shutdown" every ~5 s,
#    37–40 °C so not thermal). All four A53s are ours: stop the policy and
#    online cpu1–3. Idempotent and re-applied on every boot, which is what
#    makes it survive reboots (proc/sys state itself does not).
#    Rollback (one line): echo 1 > /proc/hps/enabled; for c in 1 2 3; do
#    echo 0 > /sys/devices/system/cpu/cpu$c/online; done
#    Pinning (below, env vars) degrades to unpinned if any write was refused.
if [ -w /proc/hps/enabled ]; then
    echo 0 > /proc/hps/enabled 2>/dev/null
fi
for c in 1 2 3; do
    if [ -w /sys/devices/system/cpu/cpu$c/online ]; then
        echo 1 > /sys/devices/system/cpu/cpu$c/online 2>/dev/null
    fi
done
# AEC/capture/playback → cpu1; hm-core supervisor reads HM_CORE_CPU=2.
export HM_FW_AUDIO_CPU=1

DIR=/data/local/tmp/hm-direct
EMO=0
if [ -r /etc/os-release ] && grep -q 'ID=emos' /etc/os-release 2>/dev/null; then
    EMO=1
fi
if [ "$EMO" = "1" ]; then
    LOG=/run/hm-direct.log          # /run is a 4MB tmpfs on emOS
    # 2026-11-13 (device hang): the log shares /run with the core's files and
    # the diagnostic probe tracks. A full /run used to make `println!` write a
    # PARTIAL line and panic the capture thread (the log ended mid-word:
    # "[barge-in] guard held: e"), which wedged the audio path. The firmware
    # log is non-blocking now; this cap + the 30 s trim + the free-space guard
    # below keep /run from filling in the first place.
    MAX_LOG=1048576                 # 1MB cap: a quarter of /run, never /data
else
    LOG=/tmp/hm-direct.log
    MAX_LOG=5242880
fi
KEEP_LOG=524288
SUP_LOG=$DIR/supervisor.log
SUP_MAX=65536
SUP_KEEP=32768
MIN_RUNTIME=15
MAX_ATTEMPTS=3
CORE=127.0.0.1:8932
POLICY=$(cd "$(dirname "$0")" && pwd)/slot_decision.sh
if [ ! -r "$POLICY" ]; then
    POLICY=/data/local/bin/slot_decision.sh
fi
if [ ! -r "$POLICY" ]; then
    echo "slot_decision.sh missing; refusing to start (will not fall back to EchoMuse)" >&2
    exit 1
fi
# shellcheck disable=SC1090
. "$POLICY"

# ── Wait for the codec node (FireOS: EchoMuse waited for echoaudioservice;
#    emOS: init creates the node by hand before services start — belt).
i=0
while [ $i -lt 120 ]; do
    [ -e /dev/snd/controlC0 ] && break
    sleep 2
    i=$(( i + 2 ))
done

# ── Mixer belt: EchoMuse's script-level settings. Idempotent with the
#    firmware's internal direct-ioctl bring-up (which also closes the ten
#    DAPM routes this chain never carried — the "partial copy" gap).
if [ -x /system/bin/tinymix ]; then
    tinymix -D 0 "Audio_I2S1_Setting" On
    tinymix -D 0 "HP DAC Playback Switch" 1 1
    tinymix -D 0 "MFP Gpio Mute" On
    tinymix -D 0 "PCM Playback Volume" 100 100
    for adc in A B C D; do
        tinymix -D 0 "ADC_$adc Digital Volume Control" 88 88
        tinymix -D 0 "ADC_$adc MICPGA Volume Ctrl" 40 40
    done
fi

echo hm-direct > /sys/power/wake_lock
if [ "$EMO" = "0" ]; then
    # FireOS only: ledcontroller is an INIT service — killing it makes init
    # respawn it; `stop` is the clean takeover. emOS has no ledcontroller
    # (nothing else writes the LED ring after init's boot progress child
    # exits) and no property service for `stop` to talk to.
    stop ledcontroller 2>/dev/null
fi
mkdir -p $DIR
# Stage C1: on-speaker hm-core service (same supervision primitives).
# Port check guards a double spawn when init re-execs this script.
netstat -lnt 2>/dev/null | grep -q ":8930 " || /data/hm/hm-core-supervisor.sh &

# ── Supervisor log: the ONLY chatty-on-boot artifact on /data (eMMC-light);
#    trimmed before every append so a crash loop can never grow past the cap.
sup_log() {
    if [ -f "$SUP_LOG" ]; then
        SIZE=$(wc -c < "$SUP_LOG" 2>/dev/null)
    else
        SIZE=0
    fi
    if [ "$SIZE" -gt "$SUP_MAX" ]; then
        tail -c $SUP_KEEP "$SUP_LOG" > "${SUP_LOG}.tmp" 2>/dev/null
        mv "${SUP_LOG}.tmp" "$SUP_LOG" 2>/dev/null
    fi
    UP=""
    if [ -r /proc/uptime ]; then
        read UP _REST < /proc/uptime 2>/dev/null
        UP=${UP%.*}
    fi
    echo "up=${UP}s wall=$(date '+%Y-%m-%d %H:%M:%S') os=$([ $EMO = 1 ] && echo emos || echo fireos) $*" >> "$SUP_LOG"
}

amp_off() {
    if [ -x /system/bin/tinymix ]; then
        tinymix -D 0 "PCM Playback Volume" 0 0 2>/dev/null
        tinymix -D 0 "Ext_Speaker_Amp_Switch" Off 2>/dev/null
    fi
}

# ── 2026-11-13: shutdown awareness. The device hang ended with a
#    `busybox reboot`: init TERM'd the firmware child first (clean exit 0),
#    this loop saw the exit before its own TERM and restarted into a system
#    that was already tearing down — three panic-fast exits per slot, an
#    A/B swap, and the EchoMuse fallback, all during shutdown. The device
#    then booted the FALLBACK slot (0.0.37), which looked like "0.0.38 is
#    broken". init writes `stage=shutdown` into /run/messages at the start of
#    its shutdown; that is the marker this loop checks before every restart.
#    2026-11-13 (M4): the same-SHA init writes its trail to /run/boot-trail
#    (init.c note() -> /run/boot-trail), NOT /run/messages. Checking only
#    /run/messages meant the guard never fired on a real shutdown. Check
#    both: boot-trail for this image, messages for older images.
system_stopping() {
    { [ -r /run/boot-trail ] && grep -q 'stage=shutdown' /run/boot-trail 2>/dev/null; }         || { [ -r /run/messages ] && grep -q 'stage=shutdown' /run/messages 2>/dev/null; }
}

# Free KB on /run (emOS). Empty/unknown on other platforms.
run_free_kb() {
    [ "$EMO" = "1" ] || return 0
    df /run 2>/dev/null | awk 'NR==2 {v=$4; m=(v ~ /M/); gsub(/[MK]/,"",v); if(m) v=v*1024; print int(v)}'
}

# Keep the log from starting a child into a full filesystem.
make_room() {
    [ "$EMO" = "1" ] || return 0
    FREE=$(run_free_kb)
    [ -n "$FREE" ] || return 0
    if [ "$FREE" -lt 384 ]; then
        SIZE=$(wc -c < "$LOG" 2>/dev/null)
        tail -c 65536 "$LOG" > "${LOG}.1" 2>/dev/null
        : > "$LOG"
        sup_log "low /run (${FREE}KB free, log ${SIZE}B) — kept the log tail, starting with a clean file"
        FREE=$(run_free_kb)
        if [ -n "$FREE" ] && [ "$FREE" -lt 128 ]; then
            sup_log "still only ${FREE}KB free on /run — waiting before starting the child"
            sleep 5
        fi
    fi
}

# ── RAM log cap (chatty firmware output stays on tmpfs, wiped at boot).
#    30 s, not 300: the cap is a hard ceiling only if it is enforced often
#    enough; a 5-minute window could still fill /run before the first trim.
(
    while true; do
        sleep 30
        SIZE=$(wc -c < "$LOG" 2>/dev/null)
        if [ -n "$SIZE" ] && [ "$SIZE" -gt "$MAX_LOG" ]; then
            tail -c $KEEP_LOG "$LOG" > "${LOG}.1" 2>/dev/null
            : > "$LOG"
            echo "[supervisor] log trimmed: ${SIZE} bytes" >> "$LOG"
        fi
    done
) &
TRIM_PID=$!

CHILD=0
trap 'sup_log "term signalled — supervisor exiting"; kill $CHILD $TRIM_PID 2>/dev/null; wait $CHILD 2>/dev/null; amp_off; exit 0' TERM INT

# ── A/B slots + fast-exit swap. No EchoMuse fallback.
[ -e $DIR/current ] || ln -sf $DIR/audio_a $DIR/current
attempt=0
round=0

# ── T5 firmware debt #2: boot counter (the first-run occasional death
#    needs a repro matrix — every supervisor line below is now tied to a
#    boot number, so "which boot died" is answerable from supervisor.log).
BOOT_COUNT_FILE=$DIR/boot-count
BOOT_COUNT=0
[ -r "$BOOT_COUNT_FILE" ] && BOOT_COUNT=$(cat "$BOOT_COUNT_FILE" 2>/dev/null)
case "$BOOT_COUNT" in ''|*[!0-9]*) BOOT_COUNT=0 ;; esac
BOOT_COUNT=$(( BOOT_COUNT + 1 ))
echo "$BOOT_COUNT" > "$BOOT_COUNT_FILE" 2>/dev/null

sup_log "boot=#$BOOT_COUNT slot=$(readlink $DIR/current 2>/dev/null) core=$CORE"

# G6 device-side AEC runs on emOS (the validated FireOS form stays as-is).
# --loopback-probe: DAC-side + ch8 tracks on /run tmpfs for the core's
# playback_loopback diagnostic (task B). Bounded (960KB/track) and reset at
# every play start; no /data writes, no effect on the audio path.
if [ "$EMO" = "1" ]; then
    AEC_FLAG="--aec --loopback-probe"
else
    AEC_FLAG="--aec --loopback-probe"
fi

# D/T4 keyword interruption: the firmware feeds its post-AEC block to the
# local spotter (own process, cpu3) and reports detections north. The core
# decides whether to stop playback. Set KWS_FLAG="" to run without it.
KWS_FLAG=${KWS_FLAG---kws --kws-dir /data/local/tmp/hm-kws --kws-cpu 3}

# D/T5 debt #4: playback buffer A/B without rebuilding the binary. Write a
# frame count to $DIR/play-buffer (multiple of 2048, >= 4096) and the next
# supervisor start adds --play-buffer. Empty/absent = firmware default 8192
# (170 ms). tools/alsa_buffer_ab.py drives the matrix over the serial line.
T5_BUFFER_FLAG=""
if [ -r "$DIR/play-buffer" ]; then
    PB=$(cat "$DIR/play-buffer" 2>/dev/null)
    case "$PB" in
        ''|*[!0-9]*) sup_log "ignoring invalid play-buffer value '$PB'" ;;
        *) T5_BUFFER_FLAG="--play-buffer $PB" ;;
    esac
fi
if [ -n "$T5_BUFFER_FLAG" ]; then
    sup_log "T5 A/B: $T5_BUFFER_FLAG"
fi

# D/T5 calibration knobs without a rebuild: $DIR/barge-args may hold extra
# barge-in flags, e.g. "--barge-in-stats 500" (corr distribution telemetry)
# or "--barge-in-settle 800" (playback settle A/B). Absent = firmware
# defaults. Logged on every start so the applied set is never a guess.
BARGE_FLAG=""
if [ -r "$DIR/barge-args" ]; then
    BARGE_FLAG=$(cat "$DIR/barge-args" 2>/dev/null)
    sup_log "barge args: $BARGE_FLAG"
fi

# Node identity (2026-11-14, provisioning): one supervisor script for every
# device. A bootstrap seed writes $DIR/device-id; absent = the firmware
# default (dot2-d), which is what every pre-provisioning install relies on.
# Kept as a FILE rather than a patched script line so the seed never ships a
# forked copy of this file and dot2-d stays byte-identical to the repo.
DEVICE_FLAG=""
if [ -r "$DIR/device-id" ]; then
    DEVICE_ID=$(cat "$DIR/device-id" 2>/dev/null)
    case "$DEVICE_ID" in
        ''|*[!a-z0-9-]*) sup_log "ignoring invalid device-id '$DEVICE_ID'" ;;
        *) DEVICE_FLAG="--device-id $DEVICE_ID" ;;
    esac
fi

while true; do
    make_room
    START_TIME=$(date +%s)
    $DIR/current firmware --core $CORE --state-dir $DIR $AEC_FLAG $KWS_FLAG $T5_BUFFER_FLAG $BARGE_FLAG $DEVICE_FLAG >> $LOG 2>&1 &
    CHILD=$!
    wait $CHILD
    EXIT_CODE=$?
    amp_off
    END_TIME=$(date +%s)
    RUNTIME=$(( END_TIME - START_TIME ))
    # P3 forensics: each exit carries its code + the LAST 12 lines of the
    # RAM log into the persistent supervisor log. D/T5 debt #2: every line
    # is tagged with the boot number, so the "which boot died first" repro
    # matrix is answerable from supervisor.log alone.
    LOG_TAIL=$(tail -12 $LOG 2>/dev/null)
    sup_log "boot=#$BOOT_COUNT exit code=$EXIT_CODE ran=${RUNTIME}s; log tail: $LOG_TAIL"

    # A shutdown is not a crash. init terminates the firmware before this
    # loop gets its own TERM; restarting then panics (files are going away)
    # and the old code responded by swapping slots — the device came back on
    # the fallback firmware and 0.0.38 was blamed for a teardown artifact.
    if system_stopping; then
        sup_log "boot=#$BOOT_COUNT system is shutting down — supervisor exiting"
        exit 0
    fi
    # Exit code 0 is the firmware's TERM handler: an intentional stop. Do not
    # restart and never treat it as a failed start (2026-11-13).
    if [ "$EXIT_CODE" = "0" ]; then
        sup_log "boot=#$BOOT_COUNT clean exit (code 0) — intentional stop, supervisor exiting"
        exit 0
    fi
    # Exit code 75 is the firmware's unrecoverable-audio-fault exit (M4): the
    # DAC write gate or the capture path is wedged, the owed `failed`
    # receipts are already on the wire, and the only recovery is a fresh
    # codec bring-up. Name it in the supervisor log (the RAM log dies with
    # the process), then let the normal policy decide: a fault that keeps
    # happening inside MIN_RUNTIME counts as a fast exit and can still fall
    # back — the point is not to mask a firmware regression, only to restart
    # a transient one.
    if [ "$EXIT_CODE" = "75" ]; then
        sup_log "boot=#$BOOT_COUNT audio fault (code 75) — restart for a clean codec bring-up"
    fi

    if [ $RUNTIME -ge $MIN_RUNTIME ]; then
        attempt=0
        sup_log "boot=#$BOOT_COUNT exit code=$EXIT_CODE ran=${RUNTIME}s — restarting"
        sleep 2
        continue
    fi

    attempt=$(( attempt + 1 ))
    sup_log "boot=#$BOOT_COUNT fast-exit ${attempt}/${MAX_ATTEMPTS} code=$EXIT_CODE ran=${RUNTIME}s"
    if [ $attempt -lt $MAX_ATTEMPTS ]; then
        sleep 3
        continue
    fi

    attempt=0
    round=$(( round + 1 ))
    CURRENT=$(readlink $DIR/current 2>/dev/null)
    CURRENT=${CURRENT:-audio_a}
    DECISION=$(slot_decision "$CURRENT" "$round")
    set -- $DECISION
    action=$1
    target=$2
    if [ "$action" = "stay-error" ]; then
        sup_log "ERROR firmware slots exhausted fast exits; staying on $target; not starting EchoMuse"
        paint_supervisor_error_ring
        sleep 30
        continue
    fi
    if [ ! -x "$DIR/$target" ]; then
        sup_log "ERROR rollback slot $target is not executable; staying on $CURRENT; not starting EchoMuse"
        paint_supervisor_error_ring
        sleep 30
        continue
    fi
    ln -sf "$target" $DIR/current
    sup_log "slot rollback $CURRENT -> $target after $MAX_ATTEMPTS fast exits (not EchoMuse)"
done
