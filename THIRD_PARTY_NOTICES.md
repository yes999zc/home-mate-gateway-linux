# Third-Party Licenses & Notices

Home Mate Linux Device Gateway (`home-mate-gateway-linux`) incorporates or interoperates with the following open-source software components, DSP libraries, and bundled release utilities. We gratefully acknowledge their authors and maintainers.

## 1. Rust Crates (`hm-gateway`)

| Component | License | Usage & Notes |
| :--- | :--- | :--- |
| `tokio` | `MIT` | Async runtime for Northbound TCP LDJSON transport and supervision |
| `serde` / `serde_json` | `MIT OR Apache-2.0` | Northbound v0/v2 LDJSON frame serialization |
| `thiserror` | `MIT OR Apache-2.0` | Structured error types |
| `tracing` / `tracing-subscriber` | `MIT` | Diagnostics & structured logging (`tracing-subscriber` optional under `board-dot2`) |
| `base64` | `MIT OR Apache-2.0` | Inline WAV/PCM payload framing |
| `libc` | `MIT OR Apache-2.0` | Linux ALSA ioctl, evdev input, wake_lock, and armv7 `clock32` bindings |
| `cc` (build-only) | `MIT OR Apache-2.0` | Compiling embedded SpeexDSP C sources |

## 2. DSP, Keyword Spotting, Bundled Release Assets & Hardware Unlock Prerequisites

| Component | License | Source & Attribution |
| :--- | :--- | :--- |
| `speexdsp` | `BSD-3-Clause` | Multi-channel Acoustic Echo Cancellation (AEC) MDF & preprocessing DSP primitives (https://github.com/xiph/speexdsp) — full copyright & disclaimer below |
| `unxz` (`xzminidec` from XZ Embedded + `musl`) | `0BSD / Public Domain` + `MIT` | Standalone static aarch64 `.xz` decompressor shipped in LX06 Releases (https://tukaani.org/xz/embedded.html, https://musl.libc.org/) |
| `silero_vad.onnx` (Silero VAD v4, in `hm-bundle.tar.xz`) | `MIT` | Copyright (c) Silero Team (https://github.com/snakers4/silero-vad) |
| `hm-core` + `web/` (bundled in `hm-bundle.tar.xz`) | `AGPL-3.0-only` (+ third-party crates) | Incorporates `symphonia` (`MPL-2.0`, unmodified upstream source at https://github.com/pdeljanov/Symphonia), `ring` (`Apache-2.0 AND ISC`), `webpki-roots` (`CDLA-Permissive-2.0`), `icu_*`/`unicode-*` (`Unicode-3.0`), `rusqlite`/SQLite (`MIT`/`Public Domain`), and `vue` / `vue-router` (`MIT`) / `lucide-vue-next` (`ISC`). See `yes999zc/home-mate/THIRD_PARTY_NOTICES.md`. |
| `sherpa-onnx` | `Apache-2.0` | Local offline Zipformer KWS sidecar (`hm-kws`) — https://github.com/k2-fsa/sherpa-onnx |
| `onnxruntime` | `MIT` | ONNX Runtime shared library used by `hm-kws` — https://onnxruntime.ai/ |
| `open-xiaoai` (External prerequisite on LX06) | `MIT` | Independently used by end users to obtain root SSH on Xiaomi LX06 speakers — https://github.com/idootop/open-xiaoai |
| `firebreak` / `amonet` (External prerequisite on Echo Dot 2) | `MIT` / `GPL` | Independently used by end users to unlock Echo Dot 2 Bootrom over USB — https://github.com/yes999zc/firebreak |

## 3. Verbatim BSD-3-Clause Notice (`speexdsp`)

```text
Copyright 2002-2008 	Xiph.org Foundation
Copyright 2002-2008 	Jean-Marc Valin
Copyright 2005-2007	Analog Devices Inc.
Copyright 2005-2008	Commonwealth Scientific and Industrial Research
                        Organisation (CSIRO)
Copyright 1993, 2002, 2006 David Rowe
Copyright 2003 		EpicGames
Copyright 1992-1994	Jutta Degener, Carsten Bormann

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

- Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.

- Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

- Neither the name of the Xiph.org Foundation nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION OR
CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
