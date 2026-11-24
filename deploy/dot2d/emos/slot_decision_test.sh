#!/bin/sh
# Reproducible A/B drill for the supervisor policy and kws/run.sh VAD flag gating.
# A failed new slot rolls back to the other firmware slot. A second
# failure stays there. The string "echomuse" is not a decision.
set -eu
cd "$(dirname "$0")"
# shellcheck disable=SC1091
. ./slot_decision.sh

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[ "$(slot_decision audio_b 1)" = "swap audio_a" ] || fail "audio_b round 1 must roll back to audio_a"
[ "$(slot_decision audio_a 1)" = "swap audio_b" ] || fail "audio_a round 1 must roll to audio_b"
second=$(slot_decision audio_a 2)
[ "$second" = "stay-error audio_a" ] || fail "second exhaustion must stay on audio_a, got $second"
printf '%s\n' "$second" | grep -qi echomuse && fail "EchoMuse leaked into the decision"

for script in start_server.sh ../direct/start_server.sh; do
    [ -f "$script" ] || continue
    if grep -n -E '^[^#]*ECHOMUSE_FALLBACK|^[^#]*exec[[:space:]].*(echomuse|EchoMuse)' "$script"; then
        fail "$script still execs or names the EchoMuse fallback"
    fi
done

# Regression check (#219 Slice 2): kws/run.sh must NOT mount --vad-model by default
# on Dot2 even when model/silero_vad.onnx is present on disk, unless KWS_VAD_MODEL
# is explicitly set (!= "off") to an existing file.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
mkdir -p "$tmp/hm-kws/model"
: > "$tmp/hm-kws/model/silero_vad.onnx"
printf 'dot2-d\n' > "$tmp/device-id"
cat > "$tmp/hm-kws/ld-linux-aarch64.so.1" <<'EOF'
#!/bin/sh
printf '%s\n' "$*"
EOF
chmod +x "$tmp/hm-kws/ld-linux-aarch64.so.1"
sed "s|/data/local/tmp/hm-kws|$tmp/hm-kws|g; s|/data/local/tmp/hm-direct/device-id|$tmp/device-id|g" \
    ../../../kws/run.sh > "$tmp/hm-kws/run_under_test.sh"
chmod +x "$tmp/hm-kws/run_under_test.sh"

out_default=$(env -u KWS_VAD_MODEL /bin/sh "$tmp/hm-kws/run_under_test.sh")
printf '%s\n' "$out_default" | grep -q -- '--vad-model' && \
    fail "kws/run.sh must not pass --vad-model when KWS_VAD_MODEL is unset: $out_default"

out_off=$(KWS_VAD_MODEL=off /bin/sh "$tmp/hm-kws/run_under_test.sh")
printf '%s\n' "$out_off" | grep -q -- '--vad-model' && \
    fail "kws/run.sh must not pass --vad-model when KWS_VAD_MODEL=off: $out_off"

out_on=$(KWS_VAD_MODEL=model/silero_vad.onnx /bin/sh "$tmp/hm-kws/run_under_test.sh")
printf '%s\n' "$out_on" | grep -q -- '--vad-model model/silero_vad.onnx' || \
    fail "kws/run.sh must pass --vad-model when KWS_VAD_MODEL names an existing file: $out_on"

echo "slot decision drill ok"
