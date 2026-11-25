<div align="center">
  <h1>Home Mate Linux Device Gateway</h1>
  <p>
    <strong>Unified Feature-Gated Rust Hardware Voice Gateway for Embedded Linux Smart Speakers</strong>
  </p>
  <p>
    <em>Consolidates Echo Dot 2 (<code>board-dot2</code>) and Xiaomi AI Speaker Pro LX06 (<code>board-lx06</code>, adaptive 256 MB on-speaker / 128 MB thin mode) into one clean Rust hardware gateway over Northbound Protocol v2.</em>
  </p>
  <p>
    <a href="README.md"><strong>English</strong></a> ·
    <a href="README.zh-CN.md">简体中文</a> ·
    <a href="https://mymate.homes">Official Website (mymate.homes)</a>
  </p>
  <p>
    <img src="https://img.shields.io/badge/Gateway-Rust_(musl_static)-DEA584?style=flat-square&logo=rust&logoColor=white" alt="Rust Gateway" />
    <img src="https://img.shields.io/badge/Targets-Dot2_%7C_LX06--256M_%7C_LX06--128M-2563EB?style=flat-square" alt="Targets" />
    <img src="https://img.shields.io/badge/Protocol-Northbound_v0%2Fv2-16A34A?style=flat-square" alt="Northbound Protocol" />
    <img src="https://img.shields.io/badge/License-AGPL--3.0--only_%2B_Commercial-0F766E?style=flat-square" alt="License" />
  </p>
</div>

> [!NOTE]
> **Unified Repository Announcement**: This repository officially merges and supersedes the earlier separate `home-mate-gateway-dot2` and `home-mate-gateway-lx06` repositories. All Echo Dot 2 and Xiaomi LX06 (256 MB & 128 MB) builds, acoustic fixes, and AI deployment scripts now live here.

---

## Ecosystem & Architecture Boundary

<p align="center">
  <img src="docs/assets/architecture-v3-en-light.png" alt="Home Mate Unified 3-Repo Architecture" width="100%" />
</p>

This repository is the **Linux Embedded Multi-Board Hardware Gateway** in the unified 3-repository **Home Mate** ecosystem:

| Repository | Role | Connection |
| :--- | :--- | :--- |
| **[`yes999zc/home-mate`](https://github.com/yes999zc/home-mate)** | Sovereign Edge Brain (`hm-core`, full & `--slim` builds) + Vue 3 Web Console + Master Brain Agent delegation + Docker/NAS | Listens on Northbound v2 TCP `:8932` (loopback on device, or private LAN in Docker/NAS mode) |
| **`yes999zc/home-mate-gateway-linux`** *(this repo)* | Unified Linux multi-board hardware voice gateway | Owns ALSA capture/playback, multi-mic SpeexDSP AEC, local KWS sidecar (`wake_dur_ms` oneshot + local exit), LED rings, buttons, and non-invasive supervisors |
| **[`yes999zc/home-mate-gateway-android`](https://github.com/yes999zc/home-mate-gateway-android)** | Universal adaptive Android APK client & voice gateway | Connects smart displays, 4-inch 86 wall panels, tablets, phones & spare old phones over LAN/Tailscale |

**Strict Hardware & Acoustic Isolation**:
- `hm-core` never embeds board drivers; Linux speakers live behind the `Hardware` trait, `DeviceProfile`, and Cargo board feature flags (`board-dot2`, `board-lx06`) with zero cross-board binary bloat.
- **Per-family KWS isolation & `config_apply_v2`**: Hardware families are isolated cleanly so tuning one room never breaks another — **LX06** (`0.20 / 2.50`) and **Android** (`0.22 / 3.00`) carry dedicated hardware-class defaults, while **Dot2** ships with code/sidecar default `0.25 / 3.00` (and calibrated Dot2 field baseline `0.32 / 2.50`). Runtime KWS/EQ/LED/acoustics updates flow over Northbound **`config_apply_v2`** with deferred KWS reload receipts (`Persisted` -> `Active` only after `hm-kws` reports `ready`, 20 s monotonic timeout). Physical unit acoustics (volume cap, mix levels, VAD thresholds) are calibrated per unit and never copied across devices.
- **Responsive Voice Pipeline**: Supports oneshot `wake_dur_ms` command slicing, wake-ack tail bridging, deterministic local reflex exit (`退下吧` / `停止播放` — emits `LocalExitFlushed` strictly before the retreat `Keyword` frame when playback is active), and mid-playback wake-word interruption (everyday default is half-duplex; Full-Duplex is in active development).

---

## Supported Device Targets (3 Adaptive Deployment Modes)

Before deploying to a Xiaomi AI Speaker Pro (`LX06`), always run `free -m` or `grep ^MemTotal /proc/meminfo` over SSH first — Xiaomi shipped **both 256 MB and 128 MB RAM hardware batches** under the same `LX06` model name.

| Target | Board / RAM | Architecture & Cargo Feature | Runtime Topology | Core Location & Link |
| :--- | :--- | :--- | :--- | :--- |
| **1. Amazon Echo Dot 2 (`dot2`)** | Echo Dot 2 (`AEOBC` / `RS03QR`), 512 MB RAM | `armv7-unknown-linux-musleabihf`<br/>`board-dot2` *(default)* + `clock32` | Full on-device sovereign runtime: Full `hm-core` + `hm-gateway` + local `hm-kws` (code default `0.25/3.00`; calibrated Dot2 baseline `0.32/2.50`), 7-mic + ch8 loopback AEC, 12-LED IS31FL3236 ring, A/B slot OTA | **On-device** full `hm-core` over loopback `127.0.0.1:8932` |
| **2. Xiaomi AI Speaker Pro `LX06` 256 MB (`lx06-256m`)** | `LX06` (`MemTotal >= 180,000 kB`, ~241 MiB RAM, `/tmp` ~120 MiB) | `aarch64-unknown-linux-musl`<br/>`--no-default-features --features board-lx06` | Full on-device dual-wake runtime: `hm-core --slim` + `hm-gateway` + `hm-kws` (`0.20/2.50`, `gateway_vad`), Storage Scheme A (`/data` -> `/tmp`, zero SquashFS edits), coexists with stock XiaoAi | **On-device** `hm-core --slim` over loopback `127.0.0.1:8932` |
| **3. Xiaomi AI Speaker Pro `LX06` 128 MB Thin Gateway (`lx06-128m`)** | `LX06` 128 MB batch (`MemTotal < 180,000 kB`, ~118 MB RAM, `/tmp` ~59 MiB) | `aarch64-unknown-linux-musl`<br/>`--no-default-features --features board-lx06` | Gateway-only thin client: `hm-lx06-thin.tar.xz` (~550 KB compressed / ~1.6 MiB unpacked), no local `hm-core`, no local `hm-kws`, zero SquashFS edits | **LAN NAS / Server running Full Docker `hm-core`** (direct LAN `:8932` via `--allow-lan-core`, or SSH reverse tunnel `127.0.0.1:8932`) |

> **Other Xiaomi non-Pro models**: Only **LX06 (Xiaomi AI Speaker Pro)** is built-in and verified. See [Porting a New Linux Speaker](#porting-a-new-linux-speaker) for community ports.

---

## Target-by-Target Deployment Guide

### Target 1: Amazon Echo Dot 2 (`dot2`)

- **Prerequisites**: Unlocked bootrom running custom emOS (via `firebreak` / `amonet`).
- **Audio & Peripherals**: 9ch `S24_3LE` 16 kHz capture (7 mics + ch8 hardware loopback reference), SpeexDSP MIMO AEC, 12× IS31FL3236 RGB ring, hardware buttons, local Sherpa-ONNX KWS sidecar (code default `0.25 / 3.00`; calibrated Dot2 field baseline `0.32 / 2.50`).
- **Build**:
  ```bash
  ./tools/build_firmware.sh
  ```
  Produces a static `armv7-unknown-linux-musleabihf` binary linked with `rust-lld` + Rust's musl `mallocng`, verified by `tools/elf_assert.py` and the 16 MiB OTA `tools/size_gate.py`.

### Target 2: Xiaomi AI Speaker Pro `LX06` — 256 MB RAM (`lx06-256m`)

- **Prerequisites**: Root SSH enabled (via [idootop/open-xiaoai](https://github.com/idootop/open-xiaoai)), `MemTotal >= 180,000 kB`, and `>= 6.0 MiB` free on `/data` (`df -h /data`).
- **Non-Invasive Storage Scheme A**: Stock `mtd4` / `mtd5` SquashFS partitions are **never touched** (zero brick risk; uninstall anytime with `rm -rf /data/hm /data/init.sh && reboot`).
- **Build & Install**:
  ```bash
  # 1. Build slim hm-core + LX06 hm-gateway + pack <= 6.0 MiB Scheme A bundle
  ./tools/pack_lx06_bundle.sh

  # 2. Serve build/lx06/hm-kws-lx06.tar.xz on a LAN HTTP server (e.g. 192.168.1.50:18089), then install:
  ./deploy/lx06/install-lx06.sh \
    --device-ip 192.168.1.10 \
    --kws-url http://192.168.1.50:18089/hm-kws-lx06.tar.xz
  ```
- **Console & Mode Switching**: Open `http://192.168.1.10:8930` (runs directly on the speaker). Long-press the Play/Pause button for 1.5 s (`lx06-switch-watch`) to toggle between dual-wake (`homemate`) and pinned stock XiaoAi (`xiaoai-pinned`).

### Target 3: Xiaomi AI Speaker Pro `LX06` — 128 MB RAM Thin Gateway (`lx06-128m`)

> **Community Credit**: Huge thanks to **[@jiantongzh](https://github.com/jiantongzh)** for hardware-validating the 128 MB LX06 batch (`MemTotal ≈ 118,760 kB`, `/tmp tmpfs ≈ 59 MB`) and contributing the initial Thin Mode design!

#### Mandatory Prerequisite: Full Docker `hm-core` on a LAN NAS / Server

To use the 128 MB Thin Gateway, you **must** run **Full Docker `hm-core`** from [`yes999zc/home-mate`](https://github.com/yes999zc/home-mate) on an always-on LAN NAS, Linux server, Mac, or mini-PC:

```bash
git clone https://github.com/yes999zc/home-mate.git
cd home-mate
docker compose up -d
```

> **IMPORTANT — Use FULL `hm-core`, NOT `--slim`!**
> Always deploy the **standard FULL `hm-core` Docker image** on your NAS/server so Silero VAD runs in-process on the server. Pair a household admin token immediately at `http://<NAS_IP>:8930` (`HM_GATEWAY_ALLOW_LAN=1`), then either pass `--core-token` (`chmod 600` in `/data/hm/core.token`, injected via `HM_CORE_AUTH_TOKEN`, never visible in `argv`) or opt into `lan_gateway_exempt` for trusted RFC 1918 / Tailscale IPv4 gateways.

#### Honest Trade-offs of Thin Gateway (`lx06-128m`)
1. **Depends on your LAN NAS/server and Wi-Fi stability** (unlike 256 MB on-speaker mode or Dot2, which work independently).
2. **No local on-device KWS (`hm-kws` disabled)** to fit in 59 MiB `/tmp` and 128 MB RAM; wake runs server-side via ASR wake (`proactive_asr_wake`).
3. **No edge-level KWS preemption of stock XiaoAi** before audio reaches the LAN Core.
4. **LAN Wi-Fi jitter** can affect barge-in/stop latency compared to loopback `127.0.0.1:8932`.

#### Adaptive Activation & Install
`deploy/lx06/init.sh` automatically enters Thin Mode when `/proc/meminfo` reports `MemTotal < 180000` kB (or `/data/hm/thin-mode` exists, or `HM_THIN_MODE=1`):

```bash
# 1. Pack dedicated <= 1.5 MiB thin gateway bundle (hm-gateway only; no hm-core repo required)
./tools/pack_lx06_bundle.sh --thin

# 2. Install onto 128 MB LX06 pointing directly at your LAN Full Docker hm-core
./deploy/lx06/install-lx06.sh \
  --thin \
  --device-ip 192.168.1.10 \
  --core-addr 192.168.1.50:8932 \
  --device-id lx06-livingroom
# (Add --core-token "<TOKEN>" when paired auth is enforced)
```
*(Even with `--allow-lan-core`, `hm-gateway` strictly permits only loopback + RFC 1918 + Tailscale `100.64/10` + IPv6 ULA `fc00::/7` — public IPs are always rejected. Omit `--core-addr` to use an SSH reverse tunnel on `127.0.0.1:8932`.)*

---

## Copy-Paste AI Assistant Deployment Prompts

### Prompt A — Xiaomi AI Speaker Pro (`LX06`, Adaptive 256 MB vs 128 MB)
```text
Help me deploy Home Mate onto my Xiaomi AI Speaker Pro (LX06) at 192.168.1.10 (root SSH already enabled via open-xiaoai). My LAN NAS/server is at 192.168.1.50.

Follow these strict safety and architecture rules:
1. NEVER modify stock SquashFS partitions (mtd4 / mtd5). Use only non-invasive Storage Scheme A under /data/hm and /data/init.sh.
2. First SSH into root@192.168.1.10 and inspect `free -m`, `grep ^MemTotal /proc/meminfo`, and `df -h /data /tmp` to determine whether this LX06 is the 256 MB RAM batch (MemTotal >= 180000 kB) or the 128 MB RAM batch (MemTotal < 180000 kB).
3. If MemTotal >= 180000 kB (256 MB batch -> Target 2 On-Speaker Dual-Wake):
   - Confirm /data has >= 6 MiB free, pack with `./tools/pack_lx06_bundle.sh` (<= 6.0 MiB), and serve `build/lx06/hm-kws-lx06.tar.xz` on 192.168.1.50:18089.
   - Install using `./deploy/lx06/install-lx06.sh --device-ip 192.168.1.10 --kws-url http://192.168.1.50:18089/hm-kws-lx06.tar.xz`.
   - Verify `hm-core`, `hm-gateway`, and `hm-kws` are running, `http://192.168.1.10:8930` loads HTTP 200, and `/api/v0/mode` shows LX06 KWS `0.20 / 2.50`.
4. If MemTotal < 180000 kB (128 MB batch -> Target 3 Thin Gateway):
   - First verify or deploy standard FULL Docker `hm-core` from `https://github.com/yes999zc/home-mate` on 192.168.1.50 (`docker compose up -d`, `HM_GATEWAY_ALLOW_LAN=1`, paired admin token). NEVER use `--slim` on the NAS!
   - Pack the thin bundle with `./tools/pack_lx06_bundle.sh --thin` (hm-lx06-thin.tar.xz <= 1.5 MiB).
   - Install with `./deploy/lx06/install-lx06.sh --thin --device-ip 192.168.1.10 --core-addr 192.168.1.50:8932` (pass `--core-token` if `lan_gateway_exempt` is off).
   - Verify ONLY `hm-gateway` runs on the speaker (no local `hm-core`/`hm-kws`) and the speaker appears online at `http://192.168.1.50:8930`.
5. Never commit real IPs, tokens, or credentials to Git. Ask before rebooting my NAS or modifying unrelated services.
```

### Prompt B — Known 128 MB `LX06` (NAS Full Docker Core + Thin Gateway)
```text
My Xiaomi AI Speaker Pro (LX06) at 192.168.1.10 is the 128 MB RAM batch (MemTotal ~118 MB, /tmp ~59 MB). Help me set up Target 3 (Thin Gateway + LAN Full Docker hm-core on my NAS at 192.168.1.50):

1. On the NAS (192.168.1.50), deploy `yes999zc/home-mate` standard FULL Docker `hm-core` (`docker compose up -d`, `HM_GATEWAY_ALLOW_LAN=1`, ports 8930/8932/8933-udp) and pair an admin Bearer token immediately.
2. In `yes999zc/home-mate-gateway-linux`, run `./tools/pack_lx06_bundle.sh --thin` to produce `build/lx06/hm-lx06-thin.tar.xz` (<= 1.5 MiB).
3. Run `./deploy/lx06/install-lx06.sh --thin --device-ip 192.168.1.10 --core-addr 192.168.1.50:8932 --device-id lx06-room1 --core-token "<TOKEN>"` (do NOT touch stock SquashFS `mtd4`/`mtd5`).
4. Verify `/tmp/hm-gateway.log` shows a clean Northbound v2 handshake, `/tmp` usage stays well below 50%, and the speaker appears online in `http://192.168.1.50:8930`.
```

### Prompt C — Amazon Echo Dot 2 (`dot2`) Build & Local Verification
```text
Help me build and verify Home Mate Linux Device Gateway for Amazon Echo Dot 2 (Target 1, `board-dot2`, `armv7-unknown-linux-musleabihf`):

1. Do NOT run raw `cargo zigbuild` manually (Zig's musl SmpAllocator leaks RSS on-device). Always use `./tools/build_firmware.sh`, which links with `rust-lld` + Rust musl `mallocng` and enforces `tools/elf_assert.py` + the 16 MiB `tools/size_gate.py`.
2. Run host checks first: `python3 tools/line_gate.py`, `cargo fmt --all -- --check`, 3-matrix `cargo clippy -- -D warnings`, and `cargo test --workspace --locked -- --test-threads=1`.
3. Pair with full `aarch64-unknown-linux-musl` `hm-core` from `yes999zc/home-mate` (`./tools/build_core.sh`, loopback `127.0.0.1:8932`) without copying acoustic calibration parameters from another Dot2 unit.
```

---

## Local Build & Verification

> **Critical Build Rule**: Always build device release binaries using `tools/build_firmware*.sh` (`zig cc` for C deps + `rust-lld` with Rust's musl `mallocng` for final linking). **Never run raw `cargo zigbuild`** for production speaker binaries — Zig's bundled musl `SmpAllocator` leaks RSS monotonically during sustained audio playback and causes OOM on embedded speakers.

```bash
# 1. Host 3-Matrix Checks & Tests (Rust 1.100+)
python3 tools/line_gate.py
python3 tools/size_gate.py --self-test
sh -n deploy/lx06/init.sh && sh -n deploy/lx06/install-lx06.sh && bash -n tools/pack_lx06_bundle.sh
python3 tools/test_lx06_supervisor.py
bash tools/slot_decision_test.sh

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --no-default-features --features board-lx06 --all-targets --locked -- -D warnings
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings

cargo test --workspace --all-targets --locked -- --test-threads=1
cargo test --workspace --no-default-features --features board-lx06 --all-targets --locked -- --test-threads=1
cargo test --workspace --all-features --all-targets --locked -- --test-threads=1

# 2. Cross-Compile Device Firmware & Bundles
./tools/build_firmware.sh          # Target 1: Echo Dot 2 (armv7 + 16 MiB gate)
./tools/build_firmware_lx06.sh     # Target 2/3: LX06 gateway binary (aarch64)
./tools/pack_lx06_bundle.sh        # Target 2: LX06 256 MB full Scheme A bundle (<= 6.0 MiB)
./tools/pack_lx06_bundle.sh --thin # Target 3: LX06 128 MB thin gateway bundle (<= 1.5 MiB)
```

---

## Porting a New Linux Speaker

1. Add a `board-<name>` Cargo feature in `crates/gateway/Cargo.toml`.
2. Add a board module under `crates/gateway/src/firmware/` implementing the `Hardware` trait (ALSA capture/playback, LED ring, buttons, codec initialization).
3. Add a `DeviceProfile` preset in `crates/gateway/src/firmware/profile.rs` and wire dispatch in `FirmwareConfig::apply_profile` and `open_hardware`.
4. Add a `rust-lld` + Rust musl cross-compile script and verify all 3 feature matrices + size gate.

---

## License

Aligned with [`yes999zc/home-mate`](https://github.com/yes999zc/home-mate):
- **Open Source**: **GNU Affero General Public License v3.0 (`AGPL-3.0-only`)** ([`LICENSE`](LICENSE)).
- **Northbound v2 Protocol Exception**: **Apache License 2.0** ([`LICENSE-PROTOCOL-APACHE-2.0`](LICENSE-PROTOCOL-APACHE-2.0)).
- **Commercial / OEM Licensing**: Contact `zhangcong1985@gmail.com` / `yes999zc@163.com` ([https://mymate.homes](https://mymate.homes)).
- Third-party dependency notices: [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md).
