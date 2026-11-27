#!/bin/sh
# Home Mate 1-Command Non-Invasive Installer for Xiaomi AI Speaker Pro (LX06)
#
# Storage Scheme A (256 MB full on-device, 128 MB thin gateway, OR 128 MB standalone pure-ASR):
#   - Stock mtd4/mtd5 SquashFS partitions remain 100% untouched (zero brick risk).
#   - 256 MB full mode: uploads /data/hm/{hm-bundle.tar.xz, hm-bundle.sha256, init.sh, unxz, kws.url, kws.sha256}
#   - 128 MB thin mode (--thin): uploads hm-lx06-thin.tar.xz -> /data/hm/hm-bundle.tar.xz (~606 KB),
#     writes /data/hm/thin-mode + optional /data/hm/core.addr & /data/hm/core.token, skips KWS.
#   - 128 MB standalone pure-ASR mode (--standalone-asr): uploads hm-bundle.tar.xz (~5.0 MiB),
#     writes /data/hm/standalone-asr, runs local hm-core --slim (HM_VOICE_EVENTS=0, energy VAD +
#     cloud ASR wake) + hm-gateway without hm-kws (~26-31 MiB RAM, no NAS required).
#   - Hooks /data/init.sh (open-xiaoai boot hook) -> /data/hm/init.sh
#
# Usage:
#   256 MB full on-device mode:
#     ./deploy/lx06/install-lx06.sh --device-ip <LX06_IP> --kws-url <HTTP_URL_TO_HM_KWS_TAR_XZ> [--bundle-dir build/lx06]
#   128 MB thin gateway mode (pairs with LAN Full Docker hm-core):
#     ./deploy/lx06/install-lx06.sh --thin --device-ip <LX06_IP> [--core-addr <LAN_CORE_IP:8932>] [--core-token <TOKEN>] [--device-id <ID>] [--bundle-dir build/lx06]
#   128 MB standalone pure-ASR mode (no NAS required; energy VAD + cloud ASR wake, no local KWS):
#     ./deploy/lx06/install-lx06.sh --standalone-asr --device-ip <LX06_IP> [--device-id <ID>] [--bundle-dir build/lx06]

set -eu

DEVICE_IP=""
KWS_URL=""
BUNDLE_DIR="build/lx06"
SSH_USER="root"
THIN_MODE=0
STANDALONE_ASR_MODE=0
CORE_ADDR=""
CORE_TOKEN=""
DEVICE_ID=""

usage() {
    cat <<EOF
Usage:
  256 MB on-device mode:
    $0 --device-ip <LX06_IP> --kws-url <HTTP_URL_TO_HM_KWS_TAR_XZ> [--bundle-dir build/lx06]
  128 MB thin gateway mode (connects to LAN Full Docker hm-core):
    $0 --thin --device-ip <LX06_IP> [--core-addr <LAN_CORE_IP:8932>] [--core-token <TOKEN>] [--device-id <ID>] [--bundle-dir build/lx06]
  128 MB standalone pure-ASR mode (no NAS required; on-device hm-core --slim + hm-gateway, no local KWS):
    $0 --standalone-asr --device-ip <LX06_IP> [--device-id <ID>] [--bundle-dir build/lx06]

Options:
  --device-ip <IP>        IPv4 address of your Xiaomi AI Speaker Pro (LX06) with open-xiaoai SSH enabled
  --kws-url <URL>         (256 MB mode) LAN HTTP URL serving hm-kws-lx06.tar.xz (e.g. http://192.168.1.50:18089/hm-kws-lx06.tar.xz)
  --thin                  Install 128 MB thin gateway bundle (hm-lx06-thin.tar.xz); skips local hm-core & hm-kws
  --standalone-asr        Install 128 MB standalone pure-ASR mode (hm-bundle.tar.xz, HM_VOICE_EVENTS=0, no local hm-kws, no NAS)
  --core-addr <IP:8932>   (--thin only) LAN Full Docker hm-core address (e.g. 192.168.1.50:8932; default: 127.0.0.1:8932 for SSH tunnel)
  --core-token <TOKEN>    (--thin only) Optional Core pairing/admin bearer token (written chmod 600 to /data/hm/core.token)
  --device-id <ID>        Optional stable device ID written to /data/hm/state/device-id (default: lx06)
  --bundle-dir <DIR>      Directory containing built LX06 artifacts (default: build/lx06)

To uninstall cleanly and return to 100% stock XiaoAi:
  ssh root@<LX06_IP> 'rm -rf /data/hm /data/init.sh && reboot'
EOF
}

require_arg() {
    if [ "$2" -lt 2 ]; then
        echo "Error: $1 requires a value." >&2
        usage >&2
        exit 2
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --device-ip) require_arg "$1" "$#"; DEVICE_IP="$2"; shift 2 ;;
        --kws-url) require_arg "$1" "$#"; KWS_URL="$2"; shift 2 ;;
        --thin) THIN_MODE=1; shift ;;
        --standalone-asr) STANDALONE_ASR_MODE=1; shift ;;
        --core-addr) require_arg "$1" "$#"; CORE_ADDR="$2"; shift 2 ;;
        --core-token) require_arg "$1" "$#"; CORE_TOKEN="$2"; shift 2 ;;
        --device-id) require_arg "$1" "$#"; DEVICE_ID="$2"; shift 2 ;;
        --bundle-dir) require_arg "$1" "$#"; BUNDLE_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ "$THIN_MODE" = 1 ] && [ "$STANDALONE_ASR_MODE" = 1 ]; then
    echo "Error: --thin and --standalone-asr are mutually exclusive." >&2
    usage >&2
    exit 2
fi

if [ "$STANDALONE_ASR_MODE" = 1 ]; then
    if [ -n "$CORE_ADDR" ] || [ -n "$CORE_TOKEN" ] || [ -n "$KWS_URL" ]; then
        echo "Error: --standalone-asr runs hm-core locally without KWS and cannot be combined with --core-addr, --core-token, or --kws-url." >&2
        usage >&2
        exit 2
    fi
fi

if [ -z "$DEVICE_IP" ]; then
    echo "Error: --device-ip is required." >&2
    usage >&2
    exit 2
fi

if [ "$THIN_MODE" = 0 ] && [ "$STANDALONE_ASR_MODE" = 0 ] && [ -z "$KWS_URL" ]; then
    echo "Error: --kws-url is required in 256 MB full mode (or pass --thin / --standalone-asr for 128 MB modes)." >&2
    usage >&2
    exit 2
fi

file_size_kb() {
    bytes=$(wc -c < "$1" | tr -d ' ')
    printf '%s\n' $(( (bytes + 1023) / 1024 ))
}

check_remote_data_space() {
    bundle_file=$1
    mode_label=$2
    need_kb=$(file_size_kb "$bundle_file")
    need_kb=$((need_kb + $(file_size_kb "$BUNDLE_DIR/init.sh") + 256))
    upload_unxz=0
    if [ -f "$BUNDLE_DIR/unxz" ]; then
        need_kb=$((need_kb + $(file_size_kb "$BUNDLE_DIR/unxz")))
        upload_unxz=1
    fi
    ssh "$SSH_USER@$DEVICE_IP" "NEED_KB=$need_kb UPLOAD_UNXZ=$upload_unxz MODE_LABEL=$mode_label sh -s" <<'REMOTE'
set -e
mkdir -p /data/hm/state /tmp/hm-voice-events
df -k /data /tmp
grep -E '^MemTotal:' /proc/meminfo || true
avail_kb=$(df -k /data | sed -n '$p' | awk '{if ($4 ~ /%/) print $3; else print $4}')
reclaim_kb=0
if [ -f /data/hm/hm-bundle.tar.xz ]; then
    b=$(wc -c < /data/hm/hm-bundle.tar.xz | tr -d ' ')
    reclaim_kb=$((reclaim_kb + (b + 1023) / 1024))
fi
if [ "$UPLOAD_UNXZ" = 1 ] && [ -f /data/hm/unxz ] && [ ! -L /data/hm/unxz ]; then
    u=$(wc -c < /data/hm/unxz | tr -d ' ')
    reclaim_kb=$((reclaim_kb + (u + 1023) / 1024))
fi
effective_kb=$((avail_kb + reclaim_kb))
if [ "$effective_kb" -lt "$NEED_KB" ]; then
    echo "Error: insufficient /data free space for $MODE_LABEL install: need ${NEED_KB} KiB (including 256 KiB UBIFS margin), available ${avail_kb} KiB + ${reclaim_kb} KiB reclaimable = ${effective_kb} KiB." >&2
    echo "Hint : Free space under /data (e.g. old logs/recordings) or install 128 MB thin mode (--thin, ~550 KiB bundle)." >&2
    exit 1
fi
REMOTE
}

ensure_remote_unxz() {
    if [ -f "$BUNDLE_DIR/unxz" ]; then
        scp -O "$BUNDLE_DIR/unxz" "$SSH_USER@$DEVICE_IP:/data/hm/unxz"
        return 0
    fi
    if ! ssh "$SSH_USER@$DEVICE_IP" '
        if [ -x /data/hm/unxz ]; then
            exit 0
        fi
        sys_unxz=$(command -v unxz 2>/dev/null || true)
        if [ -n "$sys_unxz" ] && [ -x "$sys_unxz" ]; then
            ln -sf "$sys_unxz" /data/hm/unxz
            exit 0
        fi
        exit 1
    '; then
        echo "Error: unxz not found in '$BUNDLE_DIR/unxz' or on $DEVICE_IP (/data/hm/unxz or PATH)." >&2
        echo "       Re-run ./tools/pack_lx06_bundle.sh with UNXZ_BIN=/path/to/aarch64-unxz." >&2
        exit 1
    fi
}

if [ "$THIN_MODE" = 1 ]; then
    for f in hm-lx06-thin.tar.xz hm-lx06-thin.sha256 init.sh; do
        if [ ! -f "$BUNDLE_DIR/$f" ]; then
            echo "Error: missing artifact '$BUNDLE_DIR/$f'. Run ./tools/pack_lx06_bundle.sh --thin first." >&2
            exit 1
        fi
    done

    echo "==> [1/4] Checking /data & /tmp space and memory on $SSH_USER@$DEVICE_IP ..."
    check_remote_data_space "$BUNDLE_DIR/hm-lx06-thin.tar.xz" "thin"

    echo "==> [2/4] Uploading 128 MB thin gateway bundle to $SSH_USER@$DEVICE_IP:/data/hm/ ..."
    scp -O "$BUNDLE_DIR/hm-lx06-thin.tar.xz" "$SSH_USER@$DEVICE_IP:/data/hm/hm-bundle.tar.xz"
    scp -O "$BUNDLE_DIR/hm-lx06-thin.sha256" "$SSH_USER@$DEVICE_IP:/data/hm/hm-bundle.sha256"
    scp -O "$BUNDLE_DIR/init.sh" "$SSH_USER@$DEVICE_IP:/data/hm/"
    ensure_remote_unxz

    echo "==> [3/4] Writing thin-mode config and /data/init.sh boot hook ..."
    ssh "$SSH_USER@$DEVICE_IP" '
set -e
: > /data/hm/thin-mode
rm -f /data/hm/standalone-asr /data/hm/kws.url /data/hm/kws.sha256 /data/hm/state/models/silero_vad.onnx
chmod +x /data/hm/init.sh
[ -L /data/hm/unxz ] || chmod +x /data/hm/unxz
rm -rf /tmp/hm /tmp/hm-kws /tmp/hm-voice-events
cat << '\''HOOK'\'' > /data/init.sh
#!/bin/sh
exec /data/hm/init.sh "$@"
HOOK
chmod +x /data/init.sh
sync
'
    if [ -n "$CORE_ADDR" ]; then
        printf '%s\n' "$CORE_ADDR" | ssh "$SSH_USER@$DEVICE_IP" 'cat > /data/hm/core.addr && sync'
    else
        ssh "$SSH_USER@$DEVICE_IP" 'rm -f /data/hm/core.addr && sync'
    fi
    if [ -n "$CORE_TOKEN" ]; then
        printf '%s\n' "$CORE_TOKEN" | ssh "$SSH_USER@$DEVICE_IP" 'umask 077 && cat > /data/hm/core.token && chmod 600 /data/hm/core.token && sync'
    fi
    if [ -n "$DEVICE_ID" ]; then
        printf '%s\n' "$DEVICE_ID" | ssh "$SSH_USER@$DEVICE_IP" 'mkdir -p /data/hm/state && cat > /data/hm/state/device-id && sync'
    fi

    echo "==> [4/4] Starting Home Mate thin gateway supervisor on $DEVICE_IP ..."
    ssh "$SSH_USER@$DEVICE_IP" '
for p in $(ps | grep "/data/hm/init.sh" | grep -v grep | awk "{print \$1}"); do
    kill -9 "$p" 2>/dev/null || true
done
killall hm-gateway hm-core hm-kws 2>/dev/null || true
/data/hm/init.sh </dev/null >/dev/null 2>&1 &
sleep 10
ps | grep -E "hm-core|hm-gateway|hm-kws" | grep -v grep || true
'

    TARGET_CORE="${CORE_ADDR:-127.0.0.1:8932}"
    echo ""
    echo "✓ Home Mate LX06 128 MB Thin Gateway deployment complete!"
    echo "  Core target : $TARGET_CORE (run Full Docker hm-core on your LAN NAS/server, NOT --slim)"
    if [ -z "$CORE_ADDR" ]; then
        echo "  Note        : No --core-addr set; cleared /data/hm/core.addr so gateway defaults to 127.0.0.1:8932 (SSH reverse tunnel)."
    fi
    echo "  10-Min Beginner Guide: https://mymate.homes/#guideline"
    exit 0
fi

if [ "$STANDALONE_ASR_MODE" = 1 ]; then
    for f in hm-bundle.tar.xz hm-bundle.sha256 init.sh; do
        if [ ! -f "$BUNDLE_DIR/$f" ]; then
            echo "Error: missing artifact '$BUNDLE_DIR/$f'. Download the release assets or run ./tools/pack_lx06_bundle.sh --standalone-asr first." >&2
            exit 1
        fi
    done

    echo "==> [1/4] Checking /data & /tmp space and memory on $SSH_USER@$DEVICE_IP ..."
    check_remote_data_space "$BUNDLE_DIR/hm-bundle.tar.xz" "standalone-asr"

    echo "==> [2/4] Uploading 128 MB standalone pure-ASR bundle to $SSH_USER@$DEVICE_IP:/data/hm/ ..."
    scp -O \
        "$BUNDLE_DIR/hm-bundle.tar.xz" \
        "$BUNDLE_DIR/hm-bundle.sha256" \
        "$BUNDLE_DIR/init.sh" \
        "$SSH_USER@$DEVICE_IP:/data/hm/"
    ensure_remote_unxz

    echo "==> [3/4] Writing standalone-asr config and /data/init.sh boot hook ..."
    ssh "$SSH_USER@$DEVICE_IP" '
set -e
: > /data/hm/standalone-asr
rm -f /data/hm/thin-mode /data/hm/core.addr /data/hm/core.token /data/hm/kws.url /data/hm/kws.sha256 /data/hm/state/models/silero_vad.onnx
chmod +x /data/hm/init.sh
[ -L /data/hm/unxz ] || chmod +x /data/hm/unxz
rm -rf /tmp/hm /tmp/hm-kws /tmp/hm-voice-events
cat << '\''HOOK'\'' > /data/init.sh
#!/bin/sh
exec /data/hm/init.sh "$@"
HOOK
chmod +x /data/init.sh
sync
'
    if [ -n "$DEVICE_ID" ]; then
        printf '%s\n' "$DEVICE_ID" | ssh "$SSH_USER@$DEVICE_IP" 'mkdir -p /data/hm/state && cat > /data/hm/state/device-id && sync'
    fi

    echo "==> [4/4] Starting Home Mate standalone pure-ASR supervisor on $DEVICE_IP ..."
    ssh "$SSH_USER@$DEVICE_IP" '
for p in $(ps | grep "/data/hm/init.sh" | grep -v grep | awk "{print \$1}"); do
    kill -9 "$p" 2>/dev/null || true
done
killall hm-gateway hm-core hm-kws 2>/dev/null || true
/data/hm/init.sh </dev/null >/dev/null 2>&1 &
sleep 12
ps | grep -E "hm-core|hm-gateway|hm-kws" | grep -v grep || true
'

    echo ""
    echo "✓ Home Mate LX06 128 MB Standalone Pure-ASR deployment complete!"
    echo "  Web Console : http://$DEVICE_IP:8930  (runs directly on the speaker, no NAS required)"
    echo "  Mode Notes  : Local hm-kws is disabled (saves ~50-68 MiB RAM); wake runs via energy VAD + cloud ASR."
    echo "                Voice event journaling is disabled (HM_VOICE_EVENTS=0) to protect 58 MB /tmp."
    echo "  10-Min Beginner Guide: https://mymate.homes/#guideline"
    exit 0
fi

for f in hm-bundle.tar.xz hm-bundle.sha256 kws.sha256 init.sh; do
    if [ ! -f "$BUNDLE_DIR/$f" ]; then
        echo "Error: missing artifact '$BUNDLE_DIR/$f'. Run ./tools/pack_lx06_bundle.sh first." >&2
        exit 1
    fi
done

echo "==> [1/4] Checking /data free space on $SSH_USER@$DEVICE_IP ..."
check_remote_data_space "$BUNDLE_DIR/hm-bundle.tar.xz" "full"

echo "==> [2/4] Uploading Scheme A bundle artifacts to $SSH_USER@$DEVICE_IP:/data/hm/ ..."
scp -O \
    "$BUNDLE_DIR/hm-bundle.tar.xz" \
    "$BUNDLE_DIR/hm-bundle.sha256" \
    "$BUNDLE_DIR/kws.sha256" \
    "$BUNDLE_DIR/init.sh" \
    "$SSH_USER@$DEVICE_IP:/data/hm/"
ensure_remote_unxz

echo "==> [3/4] Writing kws.url and /data/init.sh boot hook ..."
printf '%s\n' "$KWS_URL" | ssh "$SSH_USER@$DEVICE_IP" 'cat > /data/hm/kws.url && sync'
ssh "$SSH_USER@$DEVICE_IP" '
set -e
rm -f /data/hm/thin-mode /data/hm/standalone-asr /data/hm/core.addr /data/hm/core.token
chmod +x /data/hm/init.sh
[ -L /data/hm/unxz ] || chmod +x /data/hm/unxz
rm -rf /tmp/hm
cat << '\''HOOK'\'' > /data/init.sh
#!/bin/sh
exec /data/hm/init.sh "$@"
HOOK
chmod +x /data/init.sh
sync
'
if [ -n "$DEVICE_ID" ]; then
    printf '%s\n' "$DEVICE_ID" | ssh "$SSH_USER@$DEVICE_IP" 'mkdir -p /data/hm/state && cat > /data/hm/state/device-id && sync'
fi

echo "==> [4/4] Starting Home Mate supervisor on $DEVICE_IP ..."
ssh "$SSH_USER@$DEVICE_IP" '
for p in $(ps | grep "/data/hm/init.sh" | grep -v grep | awk "{print \$1}"); do
    kill -9 "$p" 2>/dev/null || true
done
killall hm-gateway hm-core hm-kws 2>/dev/null || true
/data/hm/init.sh </dev/null >/dev/null 2>&1 &
sleep 14
ps | grep -E "hm-core|hm-gateway|hm-kws" | grep -v grep || true
'

echo ""
echo "✓ Home Mate LX06 deployment complete!"
echo "  Web Console: http://$DEVICE_IP:8930"
echo "  10-Min Beginner Guide: https://mymate.homes/#guideline"
