# A/B slot policy for the on-device supervisor. Sourced, not executed.
#
# EchoMuse is not an outcome. After one slot burns its fast-exit budget,
# roll back to the other firmware slot. After the second slot does too,
# stay on the current slot, paint the error ring if the sysfs node is
# there, and let the caller log. Never exec start_server.echomuse.sh.

slot_decision() {
    _cur=$(basename "$1")
    _round=$2
    if [ "$_round" -ge 2 ]; then
        printf 'stay-error %s\n' "$_cur"
        return 0
    fi
    case "$_cur" in
        audio_a) _other=audio_b ;;
        audio_b) _other=audio_a ;;
        *) _other=audio_a ;;
    esac
    printf 'swap %s\n' "$_other"
}

# leds.rs state "error" (#AA0000) on the default sequential 12-LED map.
# 12 * "aa0000" = 72 hex chars, the sysfs frame line. No-op off the board.
paint_supervisor_error_ring() {
    _frame=/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame
    [ -w "$_frame" ] || return 0
    printf 'aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000aa0000\r\n' > "$_frame" 2>/dev/null || true
}
