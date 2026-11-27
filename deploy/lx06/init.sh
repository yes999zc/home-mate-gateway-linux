#!/bin/sh
# Home Mate on-device supervisor for Xiaomi AI Speaker Pro (LX06).
#
# Keep the main bundle compressed in flash and unpack it into tmpfs on boot.
# The independently fetched keyword spotter also lives in tmpfs. Both caches
# survive a change between normal dual-wake operation and pinned native mode.
# Mutable Core and gateway state lives in /data/hm/state.

export PATH=/usr/sbin:/usr/bin:/sbin:/bin

HM_FLASH_DIR=/data/hm
HM_STATE_DIR=/data/hm/state
HM_RUN_DIR=/tmp/hm
KWS_DIR=/tmp/hm-kws
NATIVE_MUTE=/tmp/mipns/mute
NATIVE_LOG_DIR=/tmp/mico_aivs_lab
CORE_LOG=/tmp/hm-core.log
GW_LOG=/tmp/hm-gateway.log
WD_LOG=/tmp/hm-watchdog.log
INIT_D_DIR=/etc/init.d
SHUT_LED_BIN=/bin/shut_led
MEMINFO_FILE=/proc/meminfo
HM_THIN_MEM_KB=180000
HM_FLASH_MIN_FREE_KB=64
KWS_PERSIST_FILES="kw.txt kw.exit.json kw.tuning.json kw.bundle.json"
KWS_SYNC_BACKOFF_MAX=60
kws_sync_fail_streak=0
kws_sync_cooldown=0
kws_retry_at=0

# 128 MB RAM LX06 batch (MemTotal ~118,760 kB, /tmp tmpfs ~59 MB) cannot host
# on-device hm-core + hm-kws without OOM.
# Mode precedence:
#   1. Explicit Thin Mode (HM_THIN_MODE=1 or /data/hm/thin-mode): gateway-only
#      thin client dialing a LAN Full Docker hm-core (recommended for 128 MB + NAS).
#   2. Explicit Standalone Pure-ASR Mode (HM_STANDALONE_ASR=1 or /data/hm/standalone-asr):
#      on-device hm-core --slim (HM_VOICE_EVENTS=0, energy VAD + cloud ASR wake) +
#      hm-gateway without local hm-kws (~26-31 MiB RAM, no NAS required).
#   3. Auto Thin Mode when MemTotal < 180,000 kB (safe default for 128 MB units).
#   4. 256 MB full on-device hm-core --slim + hm-gateway + local hm-kws dual-wake.
read_mem_total_kb() {
    [ -r "$MEMINFO_FILE" ] || return 1
    while IFS= read -r line; do
        case "$line" in
            MemTotal:*)
                val=$(
                    set -f
                    set -- $line
                    printf '%s' "${2:-}"
                )
                case "$val" in
                    ''|*[!0-9]*) return 1 ;;
                    *)
                        printf '%s\n' "$val"
                        return 0
                        ;;
                esac
                ;;
        esac
    done < "$MEMINFO_FILE"
    return 1
}

is_standalone_asr_mode() {
    if [ "${HM_THIN_MODE:-0}" = 1 ] || [ -f "$HM_FLASH_DIR/thin-mode" ]; then
        return 1
    fi
    [ "${HM_STANDALONE_ASR:-0}" = 1 ] || [ -f "$HM_FLASH_DIR/standalone-asr" ]
}

is_thin_mode() {
    if [ "${HM_THIN_MODE:-0}" = 1 ] || [ -f "$HM_FLASH_DIR/thin-mode" ]; then
        return 0
    fi
    if is_standalone_asr_mode; then
        return 1
    fi
    mem_kb=$(read_mem_total_kb) || return 1
    [ "$mem_kb" -lt "$HM_THIN_MEM_KB" ]
}

# Only the dual-wake gateway coordinates pns while it is running. A pinned
# native session has all of its original services, including the wake engine.
# Stock /etc/init.d/led's service_started() always triggers `/bin/show_led 4`
# (the boot blue ring) on every `start` call. Skip restarting ledserver when
# it is already live and dismiss L=4 (boot blue ring only, preserving L=7 mute).
start_native_services() {
    for svc in led touchpad sound_effect mediaplayer quickplayer mico_aivs_lab mibrain_service messagingagent "$@"; do
        if [ "$svc" = led ]; then
            if ! pidof ledserver >/dev/null 2>&1 && [ -x "$INIT_D_DIR/$svc" ]; then
                "$INIT_D_DIR/$svc" start >> "$WD_LOG" 2>&1 || true
            fi
            if [ -x "$SHUT_LED_BIN" ]; then
                "$SHUT_LED_BIN" 4 >> "$WD_LOG" 2>&1 || true
            fi
            continue
        fi
        if [ -x "$INIT_D_DIR/$svc" ]; then
            "$INIT_D_DIR/$svc" start >> "$WD_LOG" 2>&1 || true
        fi
    done
}

# The native daemon creates its instruction journal on the first instruction.
# Prepare its empty starting state after the daemon creates its directory.
# Retry on later ticks if necessary while the gateway continues to serve buttons.
prepare_native_journal() {
    [ "${journal_initialized:-0}" = 0 ] || return 0
    [ -d "$NATIVE_LOG_DIR" ] || return 1
    journal="$NATIVE_LOG_DIR/instruction.log"
    if [ ! -e "$journal" ]; then
        (umask 022; set -C; : > "$journal") || [ -f "$journal" ] || return 1
    fi
    [ -f "$journal" ] || return 1
    journal_initialized=1
}

pns_service() {
    /etc/init.d/pns "$1" >> "$WD_LOG" 2>&1
}

restore_native_wake() {
    # The existing gateway journal has exactly two fields, in either order.
    # Keep pns stopped on an unreadable/corrupt latch even if hm-core or the
    # flash bundle is unavailable. Native hardware mute has its own latch.
    # This BusyBox tr treats POSIX classes as literal bytes. Normalize JSON
    # whitespace to spaces and allow it only between tokens.
    if tr '\011\012\015' '   ' < "$HM_STATE_DIR/state.json" |
        grep -Eq '^ *\{ *("muted" *: *false *, *"volume" *: *[0-9]+|"volume" *: *[0-9]+ *, *"muted" *: *false) *\} *$'; then
        if [ ! -e "$NATIVE_MUTE" ] && ! pidof mipns-xiaomi >/dev/null 2>&1; then
            pns_service start || return 1
        fi
    elif pidof mipns-xiaomi >/dev/null 2>&1; then
        pns_service stop || return 1
    fi
}

runtime_pid() {
    binary=$1
    subcommand=$2
    for pid in $(pidof "$binary" 2>/dev/null); do
        # Match argv rather than ps display width. The button watcher and
        # the firmware have the same executable.
        if [ "$(tr '\000' '\n' < "/proc/$pid/cmdline" 2>/dev/null | sed -n '2p')" = "$subcommand" ]; then
            echo "$pid"
            return 0
        fi
    done
    return 1
}

stop_runtime() {
    old_pid=$(runtime_pid hm-gateway "$1") || return 0
    kill "$old_pid" 2>/dev/null || true
    for attempt in 1 2 3; do
        kill -0 "$old_pid" 2>/dev/null || return 0
        sleep 1
    done
    if [ "$(runtime_pid hm-gateway "$1")" = "$old_pid" ]; then
        kill -KILL "$old_pid" 2>/dev/null || true
    fi
}

stop_core() {
    for old_pid in $(pidof hm-core 2>/dev/null); do
        kill "$old_pid" 2>/dev/null || true
        for attempt in 1 2 3; do
            kill -0 "$old_pid" 2>/dev/null || break
            sleep 1
        done
        if kill -0 "$old_pid" 2>/dev/null; then
            kill -KILL "$old_pid" 2>/dev/null || true
        fi
    done
}

unpack_bundle_if_needed() {
    thin=0
    if is_thin_mode; then
        thin=1
    fi
    main_digest=""
    if [ -f "$HM_FLASH_DIR/hm-bundle.sha256" ]; then
        main_digest=$(cat "$HM_FLASH_DIR/hm-bundle.sha256")
        case "$main_digest" in *[!0-9a-f]*) return 1 ;; esac
        [ "${#main_digest}" -eq 64 ] || return 1
    fi
    if [ "$thin" = 1 ]; then
        if [ -f "$HM_RUN_DIR/.lx06-idle-wake-v1" ] && [ -x "$HM_RUN_DIR/hm-gateway" ] &&
            [ ! -e "$HM_RUN_DIR/hm-core" ] && [ ! -e "$HM_RUN_DIR/web" ] &&
            [ "$(cat "$HM_RUN_DIR/.main-sha256" 2>/dev/null)" = "$main_digest" ]; then
            return 0
        fi
    else
        if [ -f "$HM_RUN_DIR/.lx06-idle-wake-v1" ] && [ -x "$HM_RUN_DIR/hm-core" ] &&
            [ -x "$HM_RUN_DIR/hm-gateway" ] &&
            [ "$(cat "$HM_RUN_DIR/.main-sha256" 2>/dev/null)" = "$main_digest" ]; then
            if is_standalone_asr_mode && [ -f "$HM_RUN_DIR/silero_vad.onnx" ]; then
                rm -f "$HM_RUN_DIR/silero_vad.onnx"
            fi
            return 0
        fi
    fi
    if [ -n "$main_digest" ]; then
        [ "$(sha256sum "$HM_FLASH_DIR/hm-bundle.tar.xz" | cut -d ' ' -f 1)" = "$main_digest" ] || return 1
    fi
    mkdir -p "$HM_STATE_DIR/models"
    echo "$(date) unpacking $HM_FLASH_DIR/hm-bundle.tar.xz -> $HM_RUN_DIR (thin=$thin)" >> "$WD_LOG"
    stage="$HM_RUN_DIR.part"
    rm -rf "$stage"
    mkdir -p "$stage" || return 1
    # Check both stages on BusyBox sh. Stream the xz directly from flash;
    # there is no room for another copy of the compressed archive in /data.
    if [ "$thin" = 1 ]; then
        # Extract ONLY hm-gateway (never hm-core, web/, or silero_vad.onnx) so
        # a full bundle never spikes 59 MB /tmp tmpfs with the 12.6 MB hm-core.
        ( "$HM_FLASH_DIR/unxz" < "$HM_FLASH_DIR/hm-bundle.tar.xz" && touch "$stage/.unpacked" ) |
            tar -xf - -C "$stage" hm-gateway &&
            [ -f "$stage/.unpacked" ] && [ -s "$stage/hm-gateway" ] &&
            chmod +x "$stage/hm-gateway" || { rm -rf "$stage"; return 1; }
    else
        ( "$HM_FLASH_DIR/unxz" < "$HM_FLASH_DIR/hm-bundle.tar.xz" && touch "$stage/.unpacked" ) |
            tar -xf - -C "$stage" &&
            [ -f "$stage/.unpacked" ] && [ -s "$stage/hm-core" ] && [ -s "$stage/hm-gateway" ] &&
            chmod +x "$stage/hm-core" "$stage/hm-gateway" || { rm -rf "$stage"; return 1; }
    fi
    # Old LX06 binaries take exclusive ownership of audio. Only activate a
    # freshly unpacked archive after its gateway confirms the shared contract.
    [ "$("$stage/hm-gateway" lx06-wake-contract 2>> "$WD_LOG")" = lx06-idle-wake-v1 ] &&
        touch "$stage/.lx06-idle-wake-v1" || { rm -rf "$stage"; return 1; }
    echo "$main_digest" > "$stage/.main-sha256" || { rm -rf "$stage"; return 1; }
    # The live directory is a disposable tmpfs cache, never persistent state.
    rm -rf "$HM_RUN_DIR"
    mv "$stage" "$HM_RUN_DIR" || return 1
    if [ "$thin" = 0 ] && ! is_standalone_asr_mode && [ -s "$HM_RUN_DIR/silero_vad.onnx" ]; then
        ln -sf "$HM_RUN_DIR/silero_vad.onnx" "$HM_STATE_DIR/models/silero_vad.onnx"
    elif is_standalone_asr_mode && [ -f "$HM_RUN_DIR/silero_vad.onnx" ]; then
        # Free ~2.3 MiB of 58 MB RAM-backed /tmp tmpfs in 128 MB standalone-asr mode.
        rm -f "$HM_RUN_DIR/silero_vad.onnx"
    fi
}

flash_free_kb() {
    df -k "$1" 2>/dev/null | sed -n '$p' | (
        read -r _a _b c d _rest || exit 1
        case "${d:-}" in
            *%) val="$c" ;;
            *) val="$d" ;;
        esac
        case "$val" in
            ''|*[!0-9]*) exit 1 ;;
            *) printf '%s\n' "$val" ;;
        esac
    )
}

# Initial restore only (before first gateway start on a fresh or pre-cached
# tmpfs KWS dir). Steady-state ticks never resurrect deleted live files from
# flash back into tmpfs.
restore_kws_config() {
    [ -d "$KWS_DIR" ] || return 0
    [ ! -f "$KWS_DIR/.persist-restored" ] || return 0
    for f in $KWS_PERSIST_FILES; do
        disk="$HM_STATE_DIR/kws-persist/$f"
        live="$KWS_DIR/$f"
        if [ ! -e "$live" ] && [ -s "$disk" ]; then
            if cp "$disk" "$live.tmp" 2>/dev/null && mv -f "$live.tmp" "$live" 2>/dev/null; then
                :
            else
                rm -f "$live.tmp" 2>/dev/null || true
                return 1
            fi
        fi
    done
    : > "$KWS_DIR/.persist-restored" 2>/dev/null || true
}

# Steady-state flash backup: atomic write via "$disk.tmp" + mv only when content
# actually changed, guarded by free-space check and exponential backoff on error
# so ENOSPC / flash faults never hammer UBIFS at 1 Hz or leave truncated files.
sync_kws_config() {
    if [ "${kws_sync_cooldown:-0}" -gt 0 ]; then
        kws_sync_cooldown=$((kws_sync_cooldown - 1))
        return 0
    fi
    [ -d "$KWS_DIR" ] || return 0
    sync_failed=0
    for f in $KWS_PERSIST_FILES; do
        disk="$HM_STATE_DIR/kws-persist/$f"
        live="$KWS_DIR/$f"
        if [ -s "$live" ]; then
            if cmp -s "$live" "$disk" 2>/dev/null; then
                continue
            fi
            if ! mkdir -p "$HM_STATE_DIR/kws-persist" 2>/dev/null; then
                sync_failed=1
                break
            fi
            avail_kb=$(flash_free_kb "$HM_STATE_DIR" 2>/dev/null || echo "")
            if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "${HM_FLASH_MIN_FREE_KB:-64}" ]; then
                sync_failed=1
                break
            fi
            tmp="$disk.tmp"
            if cp "$live" "$tmp" 2>/dev/null && mv -f "$tmp" "$disk" 2>/dev/null; then
                :
            else
                rm -f "$tmp" 2>/dev/null || true
                sync_failed=1
                break
            fi
        elif [ ! -e "$live" ] && [ -e "$disk" ]; then
            # Intentionally deleted in live tmpfs (e.g. tuning reset): remove
            # the flash copy instead of resurrecting it back into tmpfs.
            if ! rm -f "$disk" 2>/dev/null; then
                sync_failed=1
                break
            fi
        fi
    done
    if [ "$sync_failed" = 1 ]; then
        kws_sync_fail_streak=$(( ${kws_sync_fail_streak:-0} + 1 ))
        backoff=5
        step=1
        while [ "$step" -lt "$kws_sync_fail_streak" ] && [ "$backoff" -lt "$KWS_SYNC_BACKOFF_MAX" ]; do
            backoff=$((backoff * 2))
            step=$((step + 1))
        done
        if [ "$backoff" -gt "$KWS_SYNC_BACKOFF_MAX" ]; then
            backoff=$KWS_SYNC_BACKOFF_MAX
        fi
        kws_sync_cooldown=$backoff
        echo "$(date) kws persist sync failed (streak=$kws_sync_fail_streak, backoff=${backoff}s)" >> "$WD_LOG"
        return 1
    fi
    kws_sync_fail_streak=0
    kws_sync_cooldown=0
    return 0
}

kws_runtime_ready() {
    [ -s "$HM_FLASH_DIR/kws.sha256" ] || return 1
    digest=$(cat "$HM_FLASH_DIR/kws.sha256")
    case "$digest" in *[!0-9a-f]*) return 1 ;; esac
    [ "${#digest}" -eq 64 ] || return 1
    [ -x "$KWS_DIR/run.sh" ] && [ -x "$KWS_DIR/hm-kws" ] &&
        [ "$(cat "$KWS_DIR/.archive-sha256" 2>/dev/null)" = "$digest" ]
}

# Fetch and verify the KWS archive without ever blocking the 1 Hz supervisor
# tick. Uses an atomic lock directory created before fork and removed by the
# child on exit (avoiding parent/child PID-file races and Linux zombie kill -0
# false positives). Once atomically unpacked into $KWS_DIR, hm-gateway's
# built-in kws::supervise() detects run.sh and hot-starts local KWS in place
# (zero gateway restart, zero audio interruption, zero Core HTTP polling).
ensure_kws_fetch_async() {
    if kws_runtime_ready; then
        restore_kws_config || true
        return 0
    fi
    lock_dir="$HM_RUN_DIR/.kws-fetch.lock"
    now_s=$(date +%s)
    # Guard against backward NTP jumps on RTC-less LX06 boots.
    if [ "${kws_retry_at:-0}" -gt $((now_s + 60)) ]; then
        kws_retry_at=0
    fi
    if [ "$now_s" -lt "${kws_retry_at:-0}" ]; then
        return 0
    fi
    if ! mkdir "$lock_dir" 2>/dev/null; then
        return 0
    fi
    kws_retry_at=$((now_s + 60))
    (
        fetch_kws_if_needed || true
        rmdir "$lock_dir" 2>/dev/null || true
    ) &
    return 0
}

fetch_kws_if_needed() {
    [ -s "$HM_FLASH_DIR/kws.sha256" ] || return 1
    digest=$(cat "$HM_FLASH_DIR/kws.sha256")
    case "$digest" in *[!0-9a-f]*) return 1 ;; esac
    [ "${#digest}" -eq 64 ] || return 1
    if [ -x "$KWS_DIR/run.sh" ] && [ -x "$KWS_DIR/hm-kws" ] &&
        [ "$(cat "$KWS_DIR/.archive-sha256" 2>/dev/null)" = "$digest" ]; then
        restore_kws_config || return 1
        return 0
    fi
    [ -s "$HM_FLASH_DIR/kws.url" ] || return 1
    url=$(tr -d '\r\n' < "$HM_FLASH_DIR/kws.url")
    [ -n "$url" ] || return 1
    stage="$KWS_DIR.part"
    rm -rf "$stage"
    mkdir -p "$stage" || return 1
    archive="$KWS_DIR.download"
    if curl -fsSL --connect-timeout 5 --max-time 60 -o "$archive" "$url" &&
        [ "$(sha256sum "$archive" | cut -d ' ' -f 1)" = "$digest" ] &&
        ( "$HM_FLASH_DIR/unxz" < "$archive" && touch "$stage/.unpacked" ) |
        tar -xf - -C "$stage" &&
        [ -f "$stage/.unpacked" ] &&
        [ -s "$stage/run.sh" ] && [ -s "$stage/hm-kws" ] &&
        [ -s "$stage/ld-linux-aarch64.so.1" ]; then
        rm -f "$archive"
        echo "$digest" > "$stage/.archive-sha256" || return 1
        chmod +x "$stage/run.sh" "$stage/hm-kws" "$stage/ld-linux-aarch64.so.1" || return 1
        for f in $KWS_PERSIST_FILES; do
            if [ -s "$HM_STATE_DIR/kws-persist/$f" ]; then
                cp "$HM_STATE_DIR/kws-persist/$f" "$stage/$f" || return 1
            fi
        done
        : > "$stage/.persist-restored" || return 1
        rm -rf "$KWS_DIR"
        mv "$stage" "$KWS_DIR" || return 1
        echo "$(date) keyword spotter ready" >> "$WD_LOG"
    else
        rm -f "$archive"
        rm -rf "$stage"
        echo "$(date) keyword spotter fetch or unpack failed" >> "$WD_LOG"
        return 1
    fi
}

init_state_if_needed() {
    mkdir -p "$HM_STATE_DIR" /tmp/hm-voice-events
    if [ ! -L "$HM_STATE_DIR/voice-events" ]; then
        rm -rf "$HM_STATE_DIR/voice-events"
        ln -s /tmp/hm-voice-events "$HM_STATE_DIR/voice-events"
    fi
    if [ ! -s "$HM_STATE_DIR/device-id" ]; then
        printf 'lx06\n' > "$HM_STATE_DIR/device-id"
    fi
    if [ ! -e "$HM_STATE_DIR/state.json" ]; then
        printf '{"volume":100,"muted":false}\n' > "$HM_STATE_DIR/state.json"
    fi
    # Older supervisors saved whichever assistant the user had last selected.
    # Both old modes now start with both wake words available. Only a new
    # explicit hardware pin survives a reboot as native-only.
    if [ ! -s "$HM_FLASH_DIR/mode" ] || [ "$(cat "$HM_FLASH_DIR/mode")" = xiaoai ]; then
        printf 'homemate\n' > "$HM_FLASH_DIR/mode.new" &&
            mv "$HM_FLASH_DIR/mode.new" "$HM_FLASH_DIR/mode"
    fi
}

# One iteration is shared by boot, hot selection and supervisor recovery.
supervise_tick() {
    mode=$(tr -d '\r\n ' < "$HM_FLASH_DIR/mode" 2>/dev/null)
    case "$mode" in
        xiaoai-pinned)
            if [ "$last_mode" != xiaoai ]; then
                stop_runtime firmware
                stop_core
                start_native_services
                last_mode=xiaoai
            fi
            restore_native_wake || return 1
            unpack_bundle_if_needed || return 1
            if ! runtime_pid hm-gateway lx06-switch-watch >/dev/null; then
                "$HM_RUN_DIR/hm-gateway" lx06-switch-watch /dev/input/event0 "$HM_FLASH_DIR/mode" >> "$WD_LOG" 2>&1 &
            fi
            return 0
            ;;
        homemate) ;;
        *) return 1 ;;
    esac

    if [ "$last_mode" != homemate ]; then
        stop_runtime lx06-switch-watch
        start_native_services
        last_mode=homemate
    fi

    prepare_native_journal || true
    if ! runtime_pid hm-gateway firmware >/dev/null; then
        restore_native_wake || return 1
    fi
    unpack_bundle_if_needed || return 1
    if is_thin_mode; then
        # 128 MB Thin Gateway mode: skip local hm-core and local hm-kws; connect
        # directly to a LAN Full Docker hm-core (via /data/hm/core.addr + --allow-lan-core)
        # or to an SSH reverse tunnel on 127.0.0.1:8932.
        if ! runtime_pid hm-gateway firmware >/dev/null; then
            core_addr="127.0.0.1:8932"
            if [ -s "$HM_FLASH_DIR/core.addr" ]; then
                addr_val=$(tr -d '\r\n ' < "$HM_FLASH_DIR/core.addr")
                [ -z "$addr_val" ] || core_addr="$addr_val"
            fi
            core_token=""
            if [ -s "$HM_FLASH_DIR/core.token" ]; then
                chmod 600 "$HM_FLASH_DIR/core.token" 2>/dev/null || true
                core_token=$(tr -d '\r\n' < "$HM_FLASH_DIR/core.token")
            fi
            dev_id="lx06"
            if [ -s "$HM_STATE_DIR/device-id" ]; then
                id_val=$(tr -d '\r\n ' < "$HM_STATE_DIR/device-id")
                [ -z "$id_val" ] || dev_id="$id_val"
            fi
            vol_cap=1.0
            [ ! -f "$HM_FLASH_DIR/silent" ] || vol_cap=0.0
            case "$core_addr" in
                127.*|localhost:*|"[::1]":*|"[::ffff:127."*)
                    HM_FW_AUDIO_CPU=1 HM_FW_PLAY_CPU=0 HM_LOOP_DIR=/tmp/hm-loop \
                        HM_LX06_DUAL_WAKE=1 HM_CORE_AUTH_TOKEN="$core_token" \
                        "$HM_RUN_DIR/hm-gateway" firmware --core "$core_addr" \
                        --device-id "$dev_id" --model LX06 \
                        --state-dir "$HM_STATE_DIR" --aec --barge-in off \
                        --loudness-bass 0.0 --loopback-probe \
                        --volume-cap "$vol_cap" >> "$GW_LOG" 2>&1 &
                    ;;
                *)
                    HM_FW_AUDIO_CPU=1 HM_FW_PLAY_CPU=0 HM_LOOP_DIR=/tmp/hm-loop \
                        HM_LX06_DUAL_WAKE=1 HM_CORE_AUTH_TOKEN="$core_token" \
                        "$HM_RUN_DIR/hm-gateway" firmware --core "$core_addr" --allow-lan-core \
                        --device-id "$dev_id" --model LX06 \
                        --state-dir "$HM_STATE_DIR" --aec --barge-in off \
                        --loudness-bass 0.0 --loopback-probe \
                        --volume-cap "$vol_cap" >> "$GW_LOG" 2>&1 &
                    ;;
            esac
        fi
        return 0
    fi

    if is_standalone_asr_mode; then
        # 128 MB Standalone Pure-ASR mode (no NAS required): run on-device
        # hm-core --slim (HM_VOICE_EVENTS=0 to protect 58 MB /tmp, energy VAD +
        # cloud proactive_asr_wake) + hm-gateway over loopback without local hm-kws.
        if ! pidof hm-core >/dev/null 2>&1; then
            HM_DATA_DIR="$HM_STATE_DIR" HM_BIND="0.0.0.0:8930" HM_GATEWAY_BIND="127.0.0.1:8932" \
                HM_WEB_DIR="$HM_RUN_DIR/web" HM_VOICE_EVENTS=0 \
                HM_LOOP_DIR=/tmp/hm-loop "$HM_RUN_DIR/hm-core" >> "$CORE_LOG" 2>&1 &
        fi
        if ! runtime_pid hm-gateway firmware >/dev/null; then
            dev_id="lx06"
            if [ -s "$HM_STATE_DIR/device-id" ]; then
                id_val=$(tr -d '\r\n ' < "$HM_STATE_DIR/device-id")
                [ -z "$id_val" ] || dev_id="$id_val"
            fi
            vol_cap=1.0
            [ ! -f "$HM_FLASH_DIR/silent" ] || vol_cap=0.0
            HM_FW_AUDIO_CPU=1 HM_FW_PLAY_CPU=0 HM_LOOP_DIR=/tmp/hm-loop \
                HM_LX06_DUAL_WAKE=1 \
                "$HM_RUN_DIR/hm-gateway" firmware --core 127.0.0.1:8932 \
                --device-id "$dev_id" --model LX06 \
                --state-dir "$HM_STATE_DIR" --aec --barge-in off \
                --loudness-bass 0.0 --loopback-probe \
                --volume-cap "$vol_cap" >> "$GW_LOG" 2>&1 &
        fi
        return 0
    fi

    if ! pidof hm-core >/dev/null 2>&1; then
        HM_DATA_DIR="$HM_STATE_DIR" HM_BIND="0.0.0.0:8930" HM_GATEWAY_BIND="127.0.0.1:8932" \
            HM_WEB_DIR="$HM_RUN_DIR/web" HM_SILERO_VAD="$HM_RUN_DIR/silero_vad.onnx" \
            HM_LOOP_DIR=/tmp/hm-loop "$HM_RUN_DIR/hm-core" >> "$CORE_LOG" 2>&1 &
    fi

    # Keep KWS archive acquisition completely non-blocking and decoupled from
    # gateway startup: hm-gateway runs with --kws even when /tmp/hm-kws is not
    # yet downloaded, reporting kws.state="off" and allowing Core ASR wake
    # fallback until hm-kws appears and hot-starts in place.
    ensure_kws_fetch_async

    if ! runtime_pid hm-gateway firmware >/dev/null; then
        dev_id="lx06"
        if [ -s "$HM_STATE_DIR/device-id" ]; then
            id_val=$(tr -d '\r\n ' < "$HM_STATE_DIR/device-id")
            [ -z "$id_val" ] || dev_id="$id_val"
        fi
        vol_cap=1.0
        [ ! -f "$HM_FLASH_DIR/silent" ] || vol_cap=0.0
        HM_FW_AUDIO_CPU=1 HM_FW_PLAY_CPU=0 HM_LOOP_DIR=/tmp/hm-loop \
            KWS_VAD_MODEL="$HM_RUN_DIR/silero_vad.onnx" HM_LX06_DUAL_WAKE=1 \
            "$HM_RUN_DIR/hm-gateway" firmware --core 127.0.0.1:8932 \
            --device-id "$dev_id" --model LX06 \
            --state-dir "$HM_STATE_DIR" --aec --barge-in off --loudness-bass 0.0 \
            --loopback-probe --kws --kws-dir "$KWS_DIR" --kws-cpu 3 \
            --volume-cap "$vol_cap" >> "$GW_LOG" 2>&1 &
    else
        if kws_runtime_ready; then
            sync_kws_config || true
        fi
    fi
}

run_supervisor() {
    sleep 5
    init_state_if_needed
    last_mode=""
    while true; do
        supervise_tick
        # Keep runtime logs and loopback dumps from crowding the RAM-backed KWS and main bundles.
        use=$(df -Pk /tmp 2>/dev/null | awk 'NR==2 {gsub(/%/,"",$5); print $5}')
        if [ "${use:-0}" -ge 85 ]; then
            : > "$CORE_LOG"
            : > "$GW_LOG"
            : > "$WD_LOG"
            rm -f /tmp/hm-voice-events/voice-events-*.jsonl 2>/dev/null || true
            rm -f /tmp/hm-loop/*.pcm 2>/dev/null || true
        fi
        sleep 1
    done
}

run_supervisor &
