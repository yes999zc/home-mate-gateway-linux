#!/usr/bin/env bash
# Build and pack the Scheme A compressed runtime bundles for Xiaomi AI Speaker Pro (LX06).
#
# Modes:
#   Default (256 MB RAM LX06, full on-device runtime):
#     - /data/hm/hm-bundle.tar.xz (<= 6.0 MiB):
#         - hm-core (aarch64-unknown-linux-musl, --slim / --no-default-features)
#         - hm-gateway (aarch64-unknown-linux-musl, static musl + rust-lld)
#         - silero_vad.onnx (pinned SHA-256: 9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6)
#         - web/ (compiled console assets from hm-core web/dist)
#     - /data/hm/hm-bundle.sha256
#     - /data/hm/unxz (static aarch64 xz decompressor)
#     - /data/hm/kws.sha256 + hm-kws-lx06.tar.xz (unpacked to /tmp/hm-kws via kws.url)
#         - hm-kws (aarch64 glibc sidecar with #63/#65/#66 Silero VAD fixes)
#         - deploy/lx06/kws-run.sh -> run.sh (--vad-model /tmp/hm/silero_vad.onnx --vad-gain 3.0)
#         - Sherpa-ONNX KWS model + runtime shared libraries
#
#   --thin (128 MB RAM LX06 Thin Gateway, pairs with LAN Full Docker hm-core):
#     - hm-lx06-thin.tar.xz (<= 1.5 MiB) + hm-lx06-thin.sha256:
#         - hm-gateway ONLY (no hm-core, no web/, no silero_vad.onnx, no hm-kws)
#     - Avoids unpacking 12.6 MiB hm-core into 59 MiB /tmp tmpfs completely.
set -euo pipefail

HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT_DIR="${OUT_DIR:-$HERE/build/lx06}"
PINNED_VAD_SHA256="9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"
MAX_MAIN_BUNDLE_BYTES=$((6 * 1024 * 1024))  # 6.0 MiB hard gate for 13.3 MiB /data UBIFS
MAX_THIN_BUNDLE_BYTES=$((1536 * 1024))      # 1.5 MiB hard gate for 128 MB thin gateway bundle

SKIP_BUILD=0
SKIP_WEB_BUILD=0
THIN_MODE=0
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        --skip-web-build) SKIP_WEB_BUILD=1 ;;
        --thin) THIN_MODE=1 ;;
        -h|--help)
            echo "Usage: $0 [--thin] [--skip-build] [--skip-web-build]"
            exit 0
            ;;
        *)
            echo "pack_lx06_bundle: unknown option '$arg' (supported: --thin, --skip-build, --skip-web-build)" >&2
            exit 2
            ;;
    esac
done

GW_BIN="$HERE/target/aarch64-unknown-linux-musl/release/home-mate-device-gateway"

copy_unxz_if_available() {
    if [[ -n "${UNXZ_BIN:-}" ]]; then
        if [[ ! -s "$UNXZ_BIN" ]]; then
            echo "pack_lx06_bundle: UNXZ_BIN is missing or empty: $UNXZ_BIN" >&2
            exit 1
        fi
        if [[ "$UNXZ_BIN" != "$OUT_DIR/unxz" ]]; then
            cp "$UNXZ_BIN" "$OUT_DIR/unxz"
        fi
        chmod 0755 "$OUT_DIR/unxz"
        echo "PASS  unxz (aarch64)     : $UNXZ_BIN"
        return 0
    fi
    for cand in \
        "$OUT_DIR/unxz" \
        "$HERE/out/lx06/unxz" \
        "/private/tmp/lx06-bundle/unxz"; do
        if [[ -s "$cand" ]]; then
            if [[ "$cand" != "$OUT_DIR/unxz" ]]; then
                cp "$cand" "$OUT_DIR/unxz"
            fi
            chmod 0755 "$OUT_DIR/unxz"
            echo "PASS  unxz (aarch64)     : $cand"
            return 0
        fi
    done
    echo "WARN  static aarch64 unxz not bundled (set UNXZ_BIN=/path/to/unxz to bundle; install-lx06.sh will fall back to speaker unxz)" >&2
}

if [[ "$THIN_MODE" -eq 1 ]]; then
    if [[ "$SKIP_BUILD" -eq 0 ]]; then
        echo "=== [1/2] Building LX06 hm-gateway (aarch64-unknown-linux-musl, thin mode) ==="
        (cd "$HERE" && ./tools/build_firmware_lx06.sh)
    fi

    if [[ ! -s "$GW_BIN" ]]; then
        echo "pack_lx06_bundle: required input missing or empty: $GW_BIN" >&2
        exit 1
    fi

    mkdir -p "$OUT_DIR"

    echo "=== [2/2] Packing 128 MB thin gateway bundle: $OUT_DIR/hm-lx06-thin.tar.xz ==="
    python3 - "$GW_BIN" "$OUT_DIR" "$MAX_THIN_BUNDLE_BYTES" <<'PY'
import hashlib
import io
from pathlib import Path
import subprocess
import sys
import tarfile

gw_bin = Path(sys.argv[1])
out_dir = Path(sys.argv[2])
max_bytes = int(sys.argv[3])

gw_bytes = gw_bin.read_bytes()
bundle_path = out_dir / "hm-lx06-thin.tar.xz"
stage_path = out_dir / "hm-lx06-thin.tar.xz.part"

raw_tar = io.BytesIO()
with tarfile.open(fileobj=raw_tar, mode="w", format=tarfile.PAX_FORMAT) as tf:
    info = tarfile.TarInfo("hm-gateway")
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = 1700000000
    info.type = tarfile.REGTYPE
    info.mode = 0o755
    info.size = len(gw_bytes)
    tf.addfile(info, io.BytesIO(gw_bytes))

compressed = subprocess.run(
    ["xz", "--arm64", "--lzma2=preset=9e,dict=8MiB", "--check=crc64", "-c"],
    input=raw_tar.getvalue(),
    capture_output=True,
    check=True,
).stdout
stage_path.write_bytes(compressed)
stage_path.replace(bundle_path)

bundle_size = bundle_path.stat().st_size
bundle_sha = hashlib.sha256(compressed).hexdigest()
(out_dir / "hm-lx06-thin.sha256").write_text(f"{bundle_sha}\n")

if bundle_size > max_bytes:
    sys.exit(f"FAIL: hm-lx06-thin.tar.xz is {bundle_size} bytes (> {max_bytes} byte 1.5 MiB cap)")

print(f"PASS  hm-gateway (lx06)  : {len(gw_bytes):>10,} B  sha256={hashlib.sha256(gw_bytes).hexdigest()}")
print(f"PASS  hm-lx06-thin.tar.xz: {bundle_size:>10,} B ({bundle_size * 100.0 / max_bytes:.1f}% of 1.5 MiB thin gate)")
print(f"PASS  hm-lx06-thin.sha256: {bundle_sha}")
PY

    copy_unxz_if_available
    cp "$HERE/deploy/lx06/init.sh" "$OUT_DIR/init.sh"
    chmod 0755 "$OUT_DIR/init.sh"

    echo "=== LX06 128 MB thin gateway artifacts ready in $OUT_DIR ==="
    ls -lh "$OUT_DIR"
    exit 0
fi

CORE_REPO="${CORE_REPO:-$(cd "$HERE/../home mate b" && pwd)}"
if [[ -z "${VAD_MODEL:-}" ]]; then
    for cand in \
        "$CORE_REPO/models/silero_vad.onnx" \
        "$HOME/AI_Workspace/03_Data/Models/sherpa-onnx/silero_vad.onnx" \
        "$HERE/models/silero_vad.onnx"; do
        if [[ -s "$cand" ]]; then
            VAD_MODEL="$cand"
            break
        fi
    done
fi
VAD_MODEL="${VAD_MODEL:-$CORE_REPO/models/silero_vad.onnx}"

web_dist_needs_rebuild() {
    python3 - "$CORE_REPO/web" <<'PY'
from pathlib import Path
import sys

web = Path(sys.argv[1])
dist_index = web / "dist" / "index.html"
if not dist_index.is_file() or dist_index.stat().st_size == 0:
    sys.exit(0)
dist_mtime = dist_index.stat().st_mtime_ns
watch_files = [
    web / "package.json",
    web / "package-lock.json",
    web / "index.html",
    web / "vite.config.ts",
    web / "vite.config.js",
    web / "tsconfig.json",
]
for f in watch_files:
    if f.is_file() and f.stat().st_mtime_ns > dist_mtime:
        sys.exit(0)
for sub in ("src", "public"):
    root = web / sub
    if root.is_dir():
        for p in root.rglob("*"):
            if p.is_file() and p.stat().st_mtime_ns > dist_mtime:
                sys.exit(0)
sys.exit(1)
PY
}

if [[ "$SKIP_BUILD" -eq 0 ]]; then
    echo "=== [1/4] Building slim hm-core (--no-default-features, aarch64-unknown-linux-musl) ==="
    (cd "$CORE_REPO" && ./tools/build_core.sh --slim)
    if [[ "$SKIP_WEB_BUILD" -eq 0 ]] && web_dist_needs_rebuild; then
        echo "=== [1b/4] Building web console (clean dist + npm run build) ==="
        (cd "$CORE_REPO/web" && rm -rf dist && npm run build)
    fi

    echo "=== [2/4] Building LX06 hm-gateway (aarch64-unknown-linux-musl) ==="
    (cd "$HERE" && ./tools/build_firmware_lx06.sh)

    if [[ -d "${SHERPA_PKG:-/private/tmp/sherpa-pkg}" ]]; then
        echo "=== [2b/4] Rebuilding aarch64 hm-kws sidecar ==="
        SHERPA_PKG="${SHERPA_PKG:-/private/tmp/sherpa-pkg}" "$HERE/kws/build.sh"
    fi
fi

CORE_BIN="$CORE_REPO/target/aarch64-unknown-linux-musl/release/hm-core"
WEB_DIST="$CORE_REPO/web/dist"
KWS_BIN="$HERE/kws/hm-kws"
KWS_RUN="$HERE/deploy/lx06/kws-run.sh"

for f in "$CORE_BIN" "$GW_BIN" "$VAD_MODEL" "$WEB_DIST/index.html" "$KWS_BIN" "$KWS_RUN"; do
    if [[ ! -s "$f" ]]; then
        echo "pack_lx06_bundle: required input missing or empty: $f" >&2
        exit 1
    fi
done

mkdir -p "$OUT_DIR"

echo "=== [3/4] Packing main bundle: $OUT_DIR/hm-bundle.tar.xz ==="
python3 - "$CORE_BIN" "$GW_BIN" "$VAD_MODEL" "$WEB_DIST" "$OUT_DIR" "$PINNED_VAD_SHA256" "$MAX_MAIN_BUNDLE_BYTES" <<'PY'
import hashlib
import io
import lzma
import os
from pathlib import Path
import sys
import tarfile

core_bin = Path(sys.argv[1])
gw_bin = Path(sys.argv[2])
vad_model = Path(sys.argv[3])
web_dist = Path(sys.argv[4])
out_dir = Path(sys.argv[5])
pinned_vad_sha = sys.argv[6]
max_bytes = int(sys.argv[7])

def sha256_file(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()

vad_sha = sha256_file(vad_model)
if vad_sha != pinned_vad_sha:
    sys.exit(f"VAD model SHA-256 mismatch: got {vad_sha}, expected {pinned_vad_sha}")

# Ensure slim hm-core does NOT carry the fat in-process tract-onnx marker
core_bytes = core_bin.read_bytes()
if b"hm-core-vad:silero" in core_bytes:
    sys.exit("hm-core contains fat 'hm-core-vad:silero' marker; LX06 requires --slim build!")

bundle_path = out_dir / "hm-bundle.tar.xz"
stage_path = out_dir / "hm-bundle.tar.xz.part"

def add_entry(tf: tarfile.TarFile, arcname: str, data: bytes | None, mode: int, is_dir: bool = False):
    info = tarfile.TarInfo(arcname)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = 1700000000
    if is_dir:
        info.type = tarfile.DIRTYPE
        info.mode = mode
        tf.addfile(info)
    else:
        assert data is not None
        info.type = tarfile.REGTYPE
        info.mode = mode
        info.size = len(data)
        tf.addfile(info, io.BytesIO(data))

raw_tar = io.BytesIO()
with tarfile.open(fileobj=raw_tar, mode="w", format=tarfile.PAX_FORMAT) as tf:
    add_entry(tf, "hm-core", core_bytes, 0o755)
    add_entry(tf, "hm-gateway", gw_bin.read_bytes(), 0o755)
    add_entry(tf, "silero_vad.onnx", vad_model.read_bytes(), 0o644)
    add_entry(tf, "web", None, 0o755, is_dir=True)
    for root, dirs, files in os.walk(web_dist):
        dirs.sort()
        files.sort()
        rel_root = Path(root).relative_to(web_dist)
        for d in dirs:
            arc = (Path("web") / rel_root / d).as_posix()
            add_entry(tf, arc, None, 0o755, is_dir=True)
        for f in files:
            p = Path(root) / f
            arc = (Path("web") / rel_root / f).as_posix()
            add_entry(tf, arc, p.read_bytes(), 0o644)

# Compress with ARM64 BCJ pre-filter + LZMA2 preset=9e (8 MiB dictionary) + CRC64
# Verified compatible with on-device static /data/hm/unxz (xz-embedded with XZ_DEC_BCJ / BCJ_ARM64).
import subprocess
compressed = subprocess.run(
    ["xz", "--arm64", "--lzma2=preset=9e,dict=8MiB", "--check=crc64", "-c"],
    input=raw_tar.getvalue(),
    capture_output=True,
    check=True,
).stdout
stage_path.write_bytes(compressed)
stage_path.replace(bundle_path)

bundle_size = bundle_path.stat().st_size
bundle_sha = hashlib.sha256(compressed).hexdigest()
(out_dir / "hm-bundle.sha256").write_text(f"{bundle_sha}\n")

if bundle_size > max_bytes:
    sys.exit(f"FAIL: hm-bundle.tar.xz is {bundle_size} bytes (> {max_bytes} byte /data cap)")

print(f"PASS  hm-core (slim)   : {len(core_bytes):>10,} B  sha256={hashlib.sha256(core_bytes).hexdigest()}")
print(f"PASS  hm-gateway (lx06): {gw_bin.stat().st_size:>10,} B  sha256={sha256_file(gw_bin)}")
print(f"PASS  silero_vad.onnx  : {vad_model.stat().st_size:>10,} B  sha256={vad_sha}")
print(f"PASS  hm-bundle.tar.xz : {bundle_size:>10,} B ({bundle_size * 100.0 / max_bytes:.1f}% of 6.0 MiB /data gate)")
print(f"PASS  hm-bundle.sha256 : {bundle_sha}")
PY

# Copy static aarch64 unxz and init.sh if available
copy_unxz_if_available
cp "$HERE/deploy/lx06/init.sh" "$OUT_DIR/init.sh"
chmod 0755 "$OUT_DIR/init.sh"

# Pack hm-kws-lx06.tar.xz if base runtime directory or archive is available
KWS_BASE_DIR="${KWS_BASE_DIR:-/private/tmp/lx06-bundle/kws-extract}"
if [[ -d "$KWS_BASE_DIR" && -f "$KWS_BASE_DIR/ld-linux-aarch64.so.1" ]]; then
    echo "=== [4/4] Packing KWS sidecar bundle: $OUT_DIR/hm-kws-lx06.tar.xz ==="
    python3 - "$KWS_BASE_DIR" "$KWS_BIN" "$KWS_RUN" "$OUT_DIR" <<'PY'
import hashlib
import io
import lzma
import os
from pathlib import Path
import sys
import tarfile

base_dir = Path(sys.argv[1])
kws_bin = Path(sys.argv[2])
kws_run = Path(sys.argv[3])
out_dir = Path(sys.argv[4])

EXECUTABLE_NAMES = {
    "run.sh",
    "hm-kws",
    "ld-linux-aarch64.so.1",
    "libc.so.6",
    "libsherpa-onnx-c-api.so",
}

raw_tar = io.BytesIO()
with tarfile.open(fileobj=raw_tar, mode="w", format=tarfile.PAX_FORMAT) as tf:
    for root, dirs, files in os.walk(base_dir):
        dirs.sort()
        files.sort()
        rel_root = Path(root).relative_to(base_dir)
        for d in dirs:
            arc = (rel_root / d).as_posix()
            info = tarfile.TarInfo(arc)
            info.type = tarfile.DIRTYPE
            info.mode = 0o755
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 1700000000
            tf.addfile(info)
        for f in files:
            p = Path(root) / f
            arc = (rel_root / f).as_posix()
            if arc == "hm-kws":
                data = kws_bin.read_bytes()
            elif arc == "run.sh":
                data = kws_run.read_bytes()
            else:
                data = p.read_bytes()
            info = tarfile.TarInfo(arc)
            info.type = tarfile.REGTYPE
            info.mode = 0o755 if f in EXECUTABLE_NAMES else 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = 1700000000
            info.size = len(data)
            tf.addfile(info, io.BytesIO(data))

import subprocess
compressed = subprocess.run(
    ["xz", "--arm64", "--lzma2=preset=9e,dict=8MiB", "--check=crc64", "-c"],
    input=raw_tar.getvalue(),
    capture_output=True,
    check=True,
).stdout
kws_tar_path = out_dir / "hm-kws-lx06.tar.xz"
kws_tar_path.write_bytes(compressed)
kws_sha = hashlib.sha256(compressed).hexdigest()
(out_dir / "kws.sha256").write_text(f"{kws_sha}\n")
print(f"PASS  hm-kws (aarch64) : {kws_bin.stat().st_size:>10,} B  sha256={hashlib.sha256(kws_bin.read_bytes()).hexdigest()}")
print(f"PASS  hm-kws-lx06.tar.xz: {len(compressed):>9,} B  sha256={kws_sha}")
PY
else
    echo "=== [4/4] Skipping hm-kws-lx06.tar.xz repack (KWS_BASE_DIR not found at $KWS_BASE_DIR) ==="
fi

echo "=== LX06 Scheme A bundle artifacts ready in $OUT_DIR ==="
ls -lh "$OUT_DIR"
