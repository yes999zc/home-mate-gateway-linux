# LX06 (Xiaomi AI Speaker Pro) Board Reference & Handoff

Updated: 2026-11-24  
Status: **Unified into `home-mate-device-gateway-b` (`#71`, feature-gated multi-board HAL)**  
Related Roadmap: `yes999zc/home-mate#274` (Phase 1)

---

## 1. Unified Multi-Board Architecture (Owner Decision 2026-11-24)

Per `yes999zc/home-mate-gateway-linux#71` and `yes999zc/home-mate#274`, Echo Dot 2 and Xiaomi AI Speaker Pro (`LX06`) are maintained in this single unified Linux Device Gateway repository behind explicit Cargo board features:

- `default = ["board-dot2"]`: Echo Dot 2 (`armv7-unknown-linux-musleabihf`) build with zero LX06 code or binary bloat (`./tools/build_firmware.sh`).
- `--no-default-features --features board-lx06`: Xiaomi AI Speaker Pro (`aarch64-unknown-linux-musl`) build (`./tools/build_firmware_lx06.sh` and `./tools/pack_lx06_bundle.sh`), pulling in `lx06.rs`, `lx06_pcm.rs`, and `lx06_vendor.rs` while excluding Dot2-only `DeviceHardware` and `tracing-subscriber`.
- `--all-features`: compiles and tests both board paths simultaneously to guarantee zero feature conflicts.
- Shared runtime (~90% of codebase): Northbound v2 transport, `kws/driver.c` + `kws.rs` (including XiaoAi 3-tier wake `--l2-min-tokens 7` and sidecar Silero VAD), SpeexDSP AEC, playback/barge-in engine, loudness, and config journal.
- Legacy standalone private repo `yes999zc/home-mate-device-gateway-lx06` (`d831504`, including PR `#1` dark-ring idle `/bin/shut_led 4` and PR `#2` 3-tier wake) is superseded by this unified repository and scheduled for archival during Phase 4 public launch.

---

## 2. Storage Scheme A & Slim VAD Topology

1. **Non-Invasive Storage Scheme A (`/data` -> `/tmp` on Boot)**
   - Stock `mtd4` (`system0`) and `mtd5` (`system1`) SquashFS partitions remain untouched (zero flash-brick risk).
   - Main compressed runtime bundle lives in `/data/hm/hm-bundle.tar.xz` (`<= 6.0 MiB` hard gate for 13.3 MiB `/data` UBIFS) containing `--slim` `hm-core`, `hm-gateway` (`aarch64-unknown-linux-musl`), pinned `silero_vad.onnx`, and `web/`, verified by `/data/hm/hm-bundle.sha256` and unpacked via static `/data/hm/unxz` into `/tmp/hm`.
   - `hm-kws` sidecar archive (`hm-kws-lx06.tar.xz`) is fetched into `/tmp/hm-kws` via `/data/hm/kws.url` + `/data/hm/kws.sha256`.
   - Persistent mutable state (`home-mate.db`, `device-id`, `state.json`, KWS tuning/phrase persistence, config journals) lives in `/data/hm/state`.
2. **Self-Contained Slim Scheme (`--slim` Core + Sidecar `gateway_vad`)**
   - `hm-core` is built with `--no-default-features` (`./tools/build_core.sh --slim`), dropping in-process `tract-onnx`.
   - Silero VAD runs in the `hm-kws` sidecar (`deploy/lx06/kws-run.sh` with `--vad-model /tmp/hm/silero_vad.onnx --vad-gain 3.0`) and streams frame decisions over loopback (`127.0.0.1:8932`) via `GatewayFrame::AudioChunk.{vad_speech, vad_prob, vad_gen, vad_decision_sample, vad_capture_sample}`.

---

## 3. LX06 Hardware & Audio Quick Facts (Verified Pitfalls — Do Not Regress)

- **SoC & Kernel**: Amlogic A113X (4× Cortex-A53 `aarch64`), Linux `4.9.61 aarch64`, ~241 MiB RAM. Runs static `aarch64-unknown-linux-musl` binaries linked with `rust-lld`.
- **Partitions**: `/data` (UBIFS, ~13.3 MiB total, ~9.5 MiB usable) and `/tmp` (`tmpfs`, ~120.9 MiB).
- **ALSA Audio (`AMLAXGSOUND`, Card 0)**:
  - Capture: `hw:0,3` (`/dev/snd/pcmC0D3c`, `PDM-dummy`), `S32_LE`, 8ch, 48,000 Hz (`ch0..5` = 6 circular PDM mics `>> 12`; `ch6..7` = 2-ch digital playback loopback reference `>> 16`, routed via `amixer -c 0 cset numid=7 1`, `numid=8 4`, `numid=9 5` `TDMIN_C`), or shared `noop` dsnoop in dual-wake mode.
  - Playback: `hw:0,2` (`/dev/snd/pcmC0D2p`, `TDM-C-tas5805`), `S16_LE`, 2ch, 48,000 Hz (`mysoftvol` `numid=177` `0..255`), or shared `plug:dmixer` in dual-wake mode.
- **LED Ring & Keys**:
  - 18× RGB LEDs on **AW20054** (`/sys/devices/i2c-0/0-003a/led_rgb`, write `"<0..17> 0xBBGGRR\n"`, open `led_rgb` per write so sysfs `ppos` stays 0; set `led_imax=0x07` and `led_fade="r/g/b 0xff"` only in exclusive mode; **never write `led_hwen`** which resets AW20054 into hardware shutdown; apply 2.2× diffuser brightness boost with floor `4` for 6-bit `val >> 2` PWM; in dual-wake mode `Indicator` only writes LEDs when `IndicatorOwner::Home` and clears on `Idle`).
  - Boot dark-ring idle (`deploy/lx06/init.sh`): skip restarting already-running `ledserver` and run `/bin/shut_led 4` to dismiss the boot blue ring while preserving hardware mute (`L=7`).
  - 4 physical buttons on `/dev/input/event0` (`adc_keypad` non-standard codes): `102` (`KEY_HOME` = Mic Mute, short press mute, 1.5s hold Wi-Fi provisioning), `114` (`KEY_VOLUMEDOWN` = Play/Pause Action, 1.5s hold toggles `xiaoai-pinned` / `homemate`), `115` (`KEY_VOLUMEUP` = Vol-), `139` (`KEY_MENU` = Vol+). In dual-wake (`HM_LX06_DUAL_WAKE=1`), open evdev with `grab = false` so stock `touchpad` keeps volume/mute keys.
- **Audio / EQ / AEC Pitfalls**:
  - Never use 16 kHz ALSA capture rate conversion (`speexrate_medium` loses ~15 dB speech energy); always capture 8ch `S32_LE` at native **48,000 Hz**, apply per-channel shifts (`ch0..5 >> 12`, `ch6..7 >> 16`, encode ref with `<< 8` into `S24_3LE` `ch8`), 3:1 anti-alias decimate to 16 kHz, and run 6-channel SpeexDSP MDF with `direct_clean_ref = true`.
  - Bypass stock `default` playback LADSPA `dtsaudio` compressor by playing to `hw:0,2` (direct) or `plug:dmixer` (shared, `F_SETPIPE_SZ = 8192`, `PR_SET_PDEATHSIG = SIGKILL`, lazy `aplay` spawn on non-zero audio) with `--loudness-bass 0.0`.
  - Stock BusyBox `tr` lacks POSIX classes (`[:space:]`), so `deploy/lx06/init.sh` normalizes JSON whitespace with `tr '\011\012\015' '   '`.

---

## 4. Build, Verification & Packaging Commands

```sh
# Local CI across both board features + supervisor tests
cargo fmt --all --check
python3 tools/line_gate.py
python3 tools/size_gate.py --self-test
python3 tools/test_lx06_supervisor.py
cargo clippy --workspace --all-targets --locked --no-default-features --features board-lx06 -- -D warnings
cargo test --workspace --all-targets --locked --no-default-features --features board-lx06 -- --test-threads=1

# Cross-compile LX06 aarch64 static musl binary + pack Scheme A bundle (<= 6.0 MiB)
./tools/build_firmware_lx06.sh
./tools/pack_lx06_bundle.sh
```
