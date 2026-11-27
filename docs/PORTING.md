# Porting Home Mate Linux Gateway to a New Speaker Board

Community contributors frequently ask about porting `home-mate-gateway-linux` to additional ARM64/ARM32 Linux smart speakers — such as **Xiaomi Smart Speaker Pro (`OH2P`)** (`aarch64` kernel, ~242 MiB RAM, ~117 MiB `/data`, 4-ch PDM mic, `acm8625p` amplifier).

This guide summarizes the minimal surface you need to add a new board family cleanly without touching `hm-core` (which stays 100% board-agnostic via Northbound Protocol v2).

## 1. Reuse the Static `aarch64` Binaries First

If your board runs an `aarch64` Linux kernel with `>= 200 MiB` RAM (like `OH2P`):
- `hm-core` (`--slim`) and `unxz` from the LX06 bundle **already execute unmodified** on `aarch64` kernels (static musl, zero glibc dependency).
- Only the **hardware audio/LED/button HAL** inside `crates/gateway/src/firmware/` needs a board adapter.

## 2. Board Adapter Checklist (`crates/gateway/src/firmware/`)

1. **Cargo Feature Flag (`crates/gateway/Cargo.toml`)**:
   - Add `board-<name> = []` alongside `board-dot2` and `board-lx06`.
2. **Audio Capture & Playback (`lx06.rs` / `lx06_pcm.rs` as reference)**:
   - Verify ALSA PCM capture & playback devices (`arecord -l`, `aplay -l`, `/proc/asound/pcm`), sample rate (`16000` Hz capture, `48000` Hz playback), channel count (e.g., 4-ch PDM on `OH2P`), and whether stock vendor audio shares capture via ALSA `noop` / `dsnoop` or requires exclusive access.
   - Verify hardware/software reference channel for SpeexDSP AEC (or `barge-in off` initially while bringing up basic wake + playback).
3. **Volume, Mute & Buttons**:
   - Map `amixer` / ALSA mixer control for the board's amplifier (e.g. `acm8625p` DAC gain) and `evdev` `/dev/input/event*` keycodes for Play/Pause, Vol+/-, and Mic Mute.
4. **LEDs & Vendor Coexistence**:
   - Wrap LED ring / indicator writes and optional vendor pause hooks (e.g., `ubus` on Xiaomi boards) behind your board module.
5. **Non-Invasive Storage Layout (`deploy/<board>/init.sh`)**:
   - Follow **Storage Scheme A**: never modify read-only SquashFS `mtd` partitions; keep compressed archives under `/data/hm` and unpack to `/tmp/hm` on boot with SHA-256 verification and memory headroom guards.

## 3. Verification Before Opening a PR

1. Run `cargo test --workspace --locked` and `cargo clippy --workspace --locked --all-targets`.
2. Verify `hm-gateway` negotiates Northbound Protocol v2 (`protocol_operating: 2`) with `hm-core` and passes cold-boot audio capture, playback, volume receipt, and clean shutdown.
3. Never commit personal IPs, tokens, or proprietary vendor firmware blobs.
