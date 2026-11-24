# Third-Party Licenses & Notices

Home Mate Linux Device Gateway (`home-mate-gateway-linux`) incorporates or interoperates with the following open-source software components and DSP/KWS libraries. We gratefully acknowledge their authors and maintainers.

## 1. Rust Crates

| Component | License | Usage & Notes |
| :--- | :--- | :--- |
| `tokio` | `MIT` | Async runtime for Northbound TCP LDJSON transport and supervision |
| `serde` / `serde_json` | `MIT OR Apache-2.0` | Northbound v0/v2 LDJSON frame serialization |
| `thiserror` | `MIT OR Apache-2.0` | Structured error types |
| `tracing` / `tracing-subscriber` | `MIT` | Diagnostics & structured logging (`tracing-subscriber` optional under `board-dot2`) |
| `base64` | `MIT OR Apache-2.0` | Inline WAV/PCM payload framing |
| `libc` | `MIT OR Apache-2.0` | Linux ALSA ioctl, evdev input, wake_lock, and armv7 `clock32` bindings |
| `cc` (build-only) | `MIT OR Apache-2.0` | Compiling embedded SpeexDSP C sources |

## 2. DSP, Keyword Spotting & Hardware Unlock Prerequisites

| Component | License | Source & Attribution |
| :--- | :--- | :--- |
| `speexdsp` | `BSD-3-Clause` | Multi-channel Acoustic Echo Cancellation (AEC) MDF & preprocessing DSP primitives |
| `sherpa-onnx` | `Apache-2.0` | Local offline Zipformer KWS sidecar (`hm-kws`) — https://github.com/k2-fsa/sherpa-onnx |
| `onnxruntime` | `MIT` | ONNX Runtime shared library used by `hm-kws` — https://github.com/microsoft/onnxruntime |
| `open-xiaoai` (External prerequisite on LX06) | `MIT` | Independently used by end users to obtain root SSH on Xiaomi LX06 speakers — https://github.com/idootop/open-xiaoai |
| `firebreak` / `amonet` (External prerequisite on Echo Dot 2) | `MIT` / `GPL` | Independently used by end users to unlock Echo Dot 2 Bootrom over USB — https://github.com/yes999zc/firebreak |
