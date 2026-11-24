# Contributing to Home Mate · 小瑞

Thank you for your interest in contributing to **Home Mate · 小瑞** (https://mymate.homes).

## Repository Matrix

- **[yes999zc/home-mate](https://github.com/yes999zc/home-mate)** — Core voice brain (`hm-core`, full & `--slim` builds), signed release manager (`hm-release`), Vue 3 web console (`web/`), Docker/NAS deployment, and Northbound v2 protocol contract.
- **[yes999zc/home-mate-gateway-linux](https://github.com/yes999zc/home-mate-gateway-linux)** — Unified multi-board Linux Device Gateway (Rust, feature-gated HAL for Amazon Echo Dot 2 `board-dot2` & Xiaomi AI Speaker Pro `board-lx06`).
- **[yes999zc/home-mate-gateway-android](https://github.com/yes999zc/home-mate-gateway-android)** — Universal adaptive Android client & voice gateway APK (phones, tablets, smart displays, 4-inch 86 wall panels, and Phicomm R1; Android 5–16).

## Architectural Boundary Rules

1. **Core vs. Gateway Separation**:
   - `hm-core` never contains hardware-model branches (no `if model == "LX06"` in Core).
   - Gateways own ALSA/Android audio capture & playback, AEC, KWS, LED rings, physical buttons, and hardware voicing EQ, communicating with `hm-core` strictly over Northbound Protocol v2 (`:8932`).
2. **Zero Credentials or Personal Data in Git**:
   - Never commit real LAN/Tailscale IPs, Wi-Fi SSIDs, device serials, API keys, `.token` files, `.db` files, `.pem` certificates, or `.wav`/`.pcm` voice recordings. Use RFC 5737 / `192.168.1.x` placeholders in tests and examples.
3. **Quality Gates**:
   - Run `cargo fmt --all -- --check`, `python3 tools/line_gate.py`, and 3-matrix `cargo clippy --workspace --all-targets --locked -- -D warnings` + `cargo test --workspace --all-targets --locked -- --test-threads=1` (`default`, `--no-default-features --features board-lx06`, `--all-features`).

## Contributor License Agreement (CLA)

By submitting a Pull Request or patch to any Home Mate repository, you agree that:
1. Your contribution is your original work and you have the legal right to grant this license.
2. You grant the Project Maintainer (Bakey Zhang / `@yes999zc`) a perpetual, worldwide, non-exclusive, royalty-free, irrevocable license to use, modify, sublicense, and re-distribute your contribution as part of Home Mate under both the open-source license (`AGPL-3.0-only` / `Apache-2.0`) and commercial licensing terms.

## Contact

- **Email**: `zhangcong1985@gmail.com` / `yes999zc@163.com`
- **Website**: [https://mymate.homes](https://mymate.homes)
