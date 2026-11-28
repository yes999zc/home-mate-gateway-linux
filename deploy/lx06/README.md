# LX06 Deployment & Idle Wake Selection (256 MB Full vs 128 MB Thin Gateway & Standalone Pure-ASR)

Xiaomi AI Speaker Pro (`LX06`) shipped in **two RAM hardware batches**: **256 MB** (`MemTotal ≈ 241 MiB`, `/tmp ≈ 120 MiB`) and **128 MB** (`MemTotal ≈ 118,760 kB`, `/tmp ≈ 59 MiB`). Check `grep ^MemTotal /proc/meminfo` over SSH before choosing a packaging mode. All three modes use non-invasive **Storage Scheme A** (`/data/hm` -> `/tmp/hm`) and never touch stock `mtd4` / `mtd5` SquashFS partitions.

## Mode 1: 256 MB RAM Full On-Device Dual Wake (`lx06-256m`)

In the default 256 MB mode, “小爱同学” wakes the stock assistant and the wake word configured in Home Mate starts a Home Mate turn. Say the command immediately after the wake word. Each assistant keeps its own conversation, follow-up and playback. A long hold of the Play/Pause key (1.5 seconds) pins the native assistant (`xiaoai-pinned`). Hold it again to return to dual wake (`homemate`). An old saved `xiaoai` selection migrates to dual wake on first boot.

The stock microphone alias `noop` and speaker alias `plug:dmixer` on the tested LX06 firmware allow the stock assistant and Gateway to share the hardware. Gateway forwards idle keyword candidates through the existing Core protocol (`127.0.0.1:8932`). Once Core accepts a Home wake, Gateway pauses the native wake service for that turn. A native dialog, ongoing playback, an unknown native state or a microphone mute prevents a new Home claim. The native touchpad continues to handle the physical mute and volume keys. An interrupted Home playback can leave audio already submitted to the shared mixer; Gateway reports the resulting flush as unconfirmed.

### Packaging (256 MB Full Mode)

Build and pack with `./tools/pack_lx06_bundle.sh` (which runs `./tools/build_core.sh --slim`, `./tools/build_firmware_lx06.sh`, and enforces the `<= 6.0 MiB` `/data` UBIFS cap for `hm-bundle.tar.xz`). Pair the AArch64 `hm-gateway` with the `--slim` `hm-core`, compiled `web/` console assets, pinned `silero_vad.onnx`, and the separately hosted `hm-kws-lx06.tar.xz` archive (with `deploy/lx06/kws-run.sh` packed as `run.sh`). Store the KWS URL in `/data/hm/kws.url`, its lowercase SHA-256 in `/data/hm/kws.sha256`, and the main archive SHA-256 in `/data/hm/hm-bundle.sha256`. Preserve the board's static `/data/hm/unxz` decoder.

Install in one command:

```sh
./deploy/lx06/install-lx06.sh \
  --device-ip 192.168.1.10 \
  --kws-url http://192.168.1.50:18089/hm-kws-lx06.tar.xz
```

After boot, verify `devices[].mic.vad.endpoint.kind` is `gateway_vad` through `/api/v0/mode`. The supervisor checks the unpacked gateway's shared-audio contract (`lx06-idle-wake-v1`) before activation.

## Mode 2: 128 MB RAM Thin Gateway (`lx06-128m` — Experimental)

> [!WARNING]
> **Experimental / Unverified on 128 MB Hardware — Lower Expectations**:
> The maintainer currently only owns and hardware-tests the **256 MB LX06 batch**. All 128 MB modes below (`--thin` and `--standalone-asr`) are **Experimental** and have not yet undergone sustained 128 MB physical-device stress testing. On 128 MB units (`MemTotal ≈ 118,760 kB`), stock XiaoAi leaves only **~51 MiB** of available RAM; community 128 MB testers have reported **severe stutter, high CPU load, memory pressure, and degraded wake reliability (even for stock "XiaoAi")** when coexisting with native XiaoAi:
> - **Mode 3 (`--standalone-asr`) is NOT recommended for daily use**: unpacking `hm-core` (~13.6 MiB) into RAM-backed `/tmp` plus runtime RSS leaves very little page cache, triggering flash paging and high load.
> - **Mode 2 (`--thin` + LAN NAS)** uses far less RAM, but `hm-gateway` currently still runs continuous `arecord -D noop` + `SCHED_FIFO` capture + SpeexDSP AEC alongside stock `mipns-xiaomi` without local KWS, so CPU contention and slower cloud-ASR wake latency remain possible until 128 MB hardware tuning is completed.
> - **Recommended alternative**: For reliable dual-wake LX06 operation, use a **256 MB batch** unit; for 128 MB owners seeking hassle-free daily use, consider a repurposed Android phone/tablet/86 wall panel with NAS Docker Core instead. You can always restore a 128 MB LX06 to 100% stock with `rm -rf /data/hm /data/init.sh && reboot`.

> **Community credit**: Thanks to **@jiantongzh** for validating the 128 MB LX06 batch (`MemTotal ≈ 118,760 kB`, `/tmp tmpfs ≈ 59 MB`) and contributing the initial Thin Mode design.

`deploy/lx06/init.sh` enforces a strict 4-tier mode precedence:
1. **Explicit Thin Mode**: `HM_THIN_MODE=1` or `/data/hm/thin-mode` exists -> always Thin Mode.
2. **Explicit Standalone Pure-ASR Mode**: `HM_STANDALONE_ASR=1` or `/data/hm/standalone-asr` exists -> overrides `< 180000` kB auto-thin and runs Mode 3 below.
3. **Auto Thin Mode**: `/proc/meminfo` reports `MemTotal < 180000` kB (~180 MB) without explicit standalone opt-in -> safe default Thin Mode.
4. **256 MB Full On-Device Dual Wake**: `MemTotal >= 180000` kB without explicit thin/standalone flags.

### Prerequisite: Full Docker `hm-core` on a LAN NAS / Server

Thin Mode does **not** run `hm-core` or `hm-kws` on the speaker. You must run the **standard FULL Docker `hm-core`** image from [`yes999zc/home-mate`](https://github.com/yes999zc/home-mate) (`docker compose up -d --build`, `HM_GATEWAY_ALLOW_LAN=1`) on an always-on LAN NAS, server, or mini-PC. **Do NOT use the `--slim` Core build on your NAS/server** — the host has plenty of RAM/disk and supports in-process Silero VAD directly (place optional `./data/models/silero_vad.onnx`, otherwise Energy VAD fallback is used).

### Packaging & Installation (128 MB Thin Mode)

Pack the dedicated gateway-only archive (`hm-lx06-thin.tar.xz`, hard-capped `<= 1.5 MiB`, typically ~550 KB compressed / ~1.6 MiB unpacked — zero 12.6 MiB `hm-core` unpack spike on 59 MiB `/tmp`):

```sh
./tools/pack_lx06_bundle.sh --thin
./deploy/lx06/install-lx06.sh \
  --thin \
  --device-ip 192.168.1.10 \
  --core-addr 192.168.1.50:8932 \
  --device-id lx06-room1
```

Configuration files under `/data/hm` in Thin Mode:
- `/data/hm/thin-mode`: forces Thin Mode explicitly (takes precedence over `/data/hm/standalone-asr` and auto-detection).
- `/data/hm/core.addr`: optional `<LAN_CORE_IP>:8932` (e.g. `192.168.1.50:8932`). When non-loopback, `init.sh` passes `--allow-lan-core` to `hm-gateway` (strictly limited to loopback + RFC 1918 + Tailscale `100.64/10` + IPv6 ULA `fc00::/7`; public internet IPs are always rejected). When absent, defaults to `127.0.0.1:8932` for an SSH reverse tunnel.
- `/data/hm/core.token`: optional Core bearer token (`chmod 600`), passed via `HM_CORE_AUTH_TOKEN` environment variable into `Hello.auth_token` (never exposed in `/proc/<pid>/cmdline`). Omit for RFC 1918 / Tailscale IPv4 when `lan_gateway_exempt` is enabled in the Core Web Console; **required** for IPv6 ULA (`fc00::/7`) connections.
- If a full `hm-bundle.tar.xz` is placed on a 128 MB unit in Thin Mode instead of `hm-lx06-thin.tar.xz`, `init.sh` extracts only `hm-gateway` (never unpacking `hm-core`, `web/`, or `silero_vad.onnx` into 59 MiB `/tmp`).

### Known Trade-offs of Thin Mode

1. Depends on the LAN NAS/server and LAN/Wi-Fi stability (cannot operate autonomously when the host is offline).
2. Local `hm-kws` is disabled to save ~50–68 MiB RAM/tmpfs; wake runs via Core server-side ASR wake (`proactive_asr_wake`), adding slight wake latency plus continuous LAN PCM uplink traffic and host CPU/VAD/ASR load.
3. In dual-wake coexistence with stock XiaoAi, local KWS cannot preempt stock `mipns-xiaomi` at the speaker edge.
4. Audio and control traverse the LAN (direct `:8932` or SSH tunnel); Wi-Fi jitter can affect interrupt/stop responsiveness.

## Mode 3: 128 MB RAM Standalone Pure-ASR (`--standalone-asr`, No NAS Required)

For 128 MB LX06 owners without a LAN NAS/server, Standalone Pure-ASR skips local `hm-kws` (~50–68 MiB savings) and runs `hm-core --slim` + `hm-gateway` + `:8930` Web Console directly on the speaker (~26–31 MiB net RAM, leaving ~22–30 MiB free headroom):

```sh
# Download prebuilt v0.0.54+ hm-bundle.tar.xz, hm-bundle.sha256, init.sh, unxz into build/lx06/
# (or build locally via `./tools/pack_lx06_bundle.sh --standalone-asr`), then install:
./deploy/lx06/install-lx06.sh \
  --standalone-asr \
  --device-ip 192.168.1.10 \
  --device-id lx06-standalone
```

Behavior & trade-offs in Standalone Pure-ASR mode:
- `/data/hm/standalone-asr` (or `HM_STANDALONE_ASR=1`) overrides `< 180000` kB auto-thin (while explicit `/data/hm/thin-mode` / `HM_THIN_MODE=1` still wins).
- `init.sh` launches `hm-core` with **`HM_VOICE_EVENTS=0`** (preventing `/tmp/hm-voice-events` from filling the 58 MiB `/tmp` tmpfs) and omits `HM_SILERO_VAD` / `KWS_VAD_MODEL` / `--kws` / `fetch_kws_if_needed`.
- `hm-core --slim` automatically falls back to **energy VAD + cloud `proactive_asr_wake`**: louder background TV/noise can trigger extra cloud ASR calls, wake latency is ~0.5–1.5 s slower than local KWS, and mid-music voice barge-in is weak (control playback via button, Web console, or Android app).
- **Optional DIY Tip (Wake Stock XiaoAi to Interrupt Playback)**: By default, `VendorMonitor` (`crates/gateway/src/firmware/lx06_vendor.rs`) pauses stock `pns` during Home Mate playback to keep 256 MB dual-wake clean. In 128 MB non-KWS modes (`!local_kws_enabled`), users who want *"say '小爱同学' to interrupt Home Mate playback"* can ask their AI agent to patch `wants_pns` + native-busy playback stop in `lx06_vendor.rs` locally and rebuild via `./tools/build_firmware_lx06.sh`.

## Verification & Device Acceptance

### Full-mode KWS cold-boot availability & zero-restart hot recovery

In 256 MB full mode the KWS runtime lives in `/tmp/hm-kws` and is fetched after a reboot. If its archive host is unreachable or SHA-256 verification fails, the supervisor never blocks gateway startup and never loads an unverified archive:
- `init.sh` starts `hm-gateway` immediately with `--kws --kws-dir /tmp/hm-kws` (keeping microphones, playback, buttons, acoustic echo guard, and Core status online) and retries KWS download asynchronously in the background every 60 s without blocking the 1 Hz supervisor loop.
- While `/tmp/hm-kws/run.sh` is absent, `hm-gateway`'s built-in `kws::supervise()` reports `kws.state = "off"` honestly to Core/Console, and `VendorMonitor` dynamically relaxes its local-KWS candidate gate so Core's existing `proactive_asr_wake` policy (if enabled by the user) can wake Home Mate without preempting active native XiaoAi dialogs.
- Once a verified KWS archive is atomically unpacked into `/tmp/hm-kws` (with persisted `kws-persist/*` config restored), `kws::supervise()` automatically hot-starts `hm-kws` in place within seconds and flips `VendorMonitor` back to local-KWS gating — with **zero gateway restart**, zero TCP disconnect, and zero audio interruption.

Run host-side supervisor tests covering 256 MB full mode, 128 MB thin mode, and 128 MB standalone pure-ASR mode (cold boot, cache reuse, mode precedence, `HM_VOICE_EVENTS=0`, thin bundle, full-bundle stripping, `core.addr`/`core.token`, `< 180000 kB` auto-detection, and `install-lx06.sh` CLI conflict checks):

```sh
sh -n deploy/lx06/init.sh && sh -n deploy/lx06/install-lx06.sh && bash -n tools/pack_lx06_bundle.sh
python3 tools/test_lx06_supervisor.py
```

On an owner-authorized LX06, record whether each wake word followed by a command in one breath reaches its intended assistant, including the first request after cold boot and after a manual mode change. Try the other wake word during each assistant's listening, follow-up and playback. Exercise physical and managed mute, volume and the long-press fallback, then check native wake resumes after a Home playback tail. Record cold-start and consecutive-turn latency, free RAM, `/tmp` usage, CPU and shared audio underruns on the target unit before release.
