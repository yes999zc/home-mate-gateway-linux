<div align="center">
  <h1>Home Mate Linux Device Gateway</h1>
  <p>
    <strong>自适应多板卡 Linux 硬件语音网关</strong> — 以统一 Rust 架构与特征门控 HAL 驱动嵌入式智能音箱硬件
  </p>
  <p>
    <em>Unified, feature-gated Rust Linux Device Gateway bridging embedded smart speaker hardware (ALSA, multi-mic AEC, local KWS, LED rings, buttons) to Home Mate Core over Northbound v2 LDJSON.</em>
  </p>
  <p>
    <img src="https://img.shields.io/badge/Gateway-Rust%20(musl%20static)-DEA584?style=flat-square&logo=rust&logoColor=white" alt="Rust Gateway" />
    <img src="https://img.shields.io/badge/Boards-Echo%20Dot%202%20%7C%20Xiaomi%20LX06-2563EB?style=flat-square" alt="Boards" />
    <img src="https://img.shields.io/badge/Protocol-Northbound%20v0%2Fv2-16A34A?style=flat-square" alt="Northbound Protocol" />
    <img src="https://img.shields.io/badge/License-AGPL--3.0--only%20%2B%20Commercial-0F766E?style=flat-square" alt="License" />
  </p>
</div>

---

## 生态定位与架构边界

本仓库是 **Home Mate** 开源生态三大核心仓库中的 **Linux 嵌入式多板卡硬件网关**（合并原 Dot2 与 LX06 两条网关代码线，由单一仓统一维护）：

| 仓库 | 定位 | 关系 |
|---|---|---|
| **[`yes999zc/home-mate`](https://github.com/yes999zc/home-mate)** | 核心大脑（`hm-core` + Vue 3 控制台） | 与本网关通过本机回环 `127.0.0.1:8932` Northbound v2 协议通信 |
| **`yes999zc/home-mate-gateway-linux`**（本仓） | Linux 多板卡硬件语音网关 | 直占 ALSA、多麦 SpeexDSP AEC、离线 KWS 侧车、灯环、实体按键与监督器 |
| **[`yes999zc/home-mate-gateway-android`](https://github.com/yes999zc/home-mate-gateway-android)** | Android 通用自适应 APK | 适用于安卓手机、平板、带屏音箱、86 墙面屏与斐讯 R1 |

**硬件隔离原则**：核心 `hm-core` 不包含任何品牌或硬件驱动分支；新 Linux 音箱通过本仓的 `Hardware` Trait + `DeviceProfile` + Cargo Feature 门控独立适配，不同板卡编译期零冗余、声学参数按单台实测独立保存（不跨机复制）。

---

## 支持硬件矩阵 (Supported Hardware Matrix)

| 硬件型号 | Cargo Feature | 目标架构 | 音频与外设能力 | 部署形态与前提条件 |
|---|---|---|---|---|
| **Amazon Echo Dot 2 (`AEOBC` / `RS03QR`)** | `board-dot2`（默认） | `armv7-unknown-linux-musleabihf`（硬浮点静态 + `clock32` 防 3.18 内核 time64 trap） | 9ch S24_3LE 16kHz 采集（7 路麦克风 + 回环参考）、SpeexDSP MIMO AEC、12 颗 IS31FL3236 RGB 灯环、实体音量/静音/动作键、本地 Sherpa-ONNX KWS 侧车 | 需提前解锁 Bootrom 并刷入自定义 emOS（参考社区工具 `firebreak` / `amonet`）；与全量 `hm-core` 共驻本机 A/B 槽 |
| **小米小爱音箱 Pro (`LX06`, 红外版)** | `board-lx06`（`--no-default-features --features board-lx06`） | `aarch64-unknown-linux-musl`（原生 64 位静态，绕开系统 32 位 glibc） | 8ch S32_LE 48kHz 采集（6 路 PDM 麦 + 2 路数字回环参考）、48kHz 立体声直出 TAS5805、18 颗 AW20054 RGB 灯环、4 键 `adc_keypad`、Scheme A 独立完整模式 | 需提前开启 Root SSH（如通过 `open-xiaoai`）；**推荐使用 Scheme A 独立模式**（停用原厂冲突守护进程，仅占 `/data` ~5 MB、RAM 充裕，彻底解决原厂小爱双唤醒导致的 ALSA 死锁与 OOM） |
| **其他 Linux 音箱（如小米 OH2P 等社区移植目标）** | 自定义 `board-*` | `armv7` / `aarch64` / `x86_64` musl | 实现 `crates/gateway/src/firmware/mod.rs` 中的 `Hardware` Trait 与 `DeviceProfile` | 暂未内置官方预编固件；例如小米 OH2P（aarch64 内核 + armhf 用户态）可参考 `docs/lx06/feasibility-and-design.md` 移植 |

> **关于小米非 Pro 版小爱音箱**：目前仅验证并内置 **LX06（小米小爱音箱 Pro）** 硬件适配，非 Pro 型号（硬件平台、声卡通道与内存规格不同）尚未支持。

---

## 本地构建与测试 (Build & Verify)

> **关键构建规则**：所有发布二进制必须通过 `tools/` 构建脚本生成（使用 `zig cc` 编译 C 依赖、`rust-lld` + Rust 自带 musl 最终链接），**切勿直接手打 `cargo zigbuild`**（Zig 自带 musl 的 `SmpAllocator` 会导致设备端 RSS 随播放时长单调增长并触发 OOM）。

### 1. 主机三矩阵校验与测试（需要 Rust 1.100+）

```bash
# 行号门禁、体积门禁自测、LX06 监督器与 A/B 槽决策测试
python3 tools/line_gate.py
python3 tools/size_gate.py --self-test
python3 tools/test_lx06_supervisor.py
bash tools/slot_decision_test.sh

# 三种特性组合 Clippy + 全量测试（Dot2 默认 / LX06 独立 / 双板卡全开）
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --workspace --no-default-features --features board-lx06 --all-targets --locked -- -D warnings
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings

cargo test --workspace --all-targets --locked -- --test-threads=1
cargo test --workspace --no-default-features --features board-lx06 --all-targets --locked -- --test-threads=1
cargo test --workspace --all-features --all-targets --locked -- --test-threads=1
```

### 2. 交叉编译设备固件与打包

```bash
# Echo Dot 2 (armv7-unknown-linux-musleabihf，内含 ELF 符号与 16 MiB 体积断言)
./tools/build_firmware.sh

# 小米小爱 Pro LX06 (aarch64-unknown-linux-musl，仅启用 board-lx06 特性，约 1.67 MB)
./tools/build_firmware_lx06.sh

# 打包 LX06 Scheme A 一体化 /data 压缩包（含 slim hm-core + lx06 hm-gateway + silero_vad + unxz + init.sh，严格 ≤ 6.0 MiB 门禁）
./tools/pack_lx06_bundle.sh
```

### 3. 开发调试用虚拟终端 (`virtual`)

```bash
# 在开发机上连接本机或局域网 hm-core（支持 --auth-token 或 HM_GATEWAY_AUTH_TOKEN）
cargo run -- virtual --core 127.0.0.1:8932 --auth-token "<your-token>" --afplay
```

---

## 移植新 Linux 音箱指南

1. 在 `crates/gateway/Cargo.toml` 中新增 `board-<name>` Feature。
2. 在 `crates/gateway/src/firmware/` 下新增板卡模块，实现 `Hardware` Trait（ALSA 采集/播放、LED 灯环、实体按键、Codec 初始化）。
3. 在 `crates/gateway/src/firmware/profile.rs` 中补充对应的 `DeviceProfile` 预设，并在 `FirmwareConfig::apply_profile` 与 `open_hardware` 中接入分发。
4. 添加专属交叉编译脚本并通过三矩阵 `clippy` / `test` 与体积门禁。

---

## 许可协议 (License)

本项目与 [`yes999zc/home-mate`](https://github.com/yes999zc/home-mate) 保持统一双许可策略：

- **开源使用**：遵循 **GNU Affero General Public License v3.0 (`AGPL-3.0-only`)**（见 [`LICENSE`](LICENSE)），面向个人、家庭自托管、学术与开源社区免费开放。
- **Northbound v2 协议例外**：线协议规范与契约定义另行适用 **Apache License 2.0**（见 [`LICENSE-PROTOCOL-APACHE-2.0`](LICENSE-PROTOCOL-APACHE-2.0)），允许第三方硬件网关自由实现而不受 AGPL 传染约束。
- **商业授权 / OEM 预装**：如需商业硬件预装、OEM 分发或闭源衍生，请联系版权方：`zhangcong1985@gmail.com` / `yes999zc@163.com`。
- 第三方依赖许可致谢见 [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md)。
