# T4 keyword-spotter bundle — device deployment

Everything the firmware needs to run `hm-kws` lives in `/data/local/tmp/hm-kws/`
on the speaker. The bundle is assembled from three sources; only the launcher
and the keyword line are tracked here.

| Part | Source | Tracked? |
|---|---|---|
| `run.sh`, `kw.txt` | this directory | yes |
| `hm-kws` | `kws/build.sh` (this repo) | build artifact |
| `libsherpa-onnx-c-api.so`, `libonnxruntime.so` | sherpa-onnx v1.12.15 `linux-aarch64-shared-cpu` release | no (third-party) |
| `ld-linux-aarch64.so.1`, `libc.so.6`, `libm.so.6`, `libpthread.so.0`, `libdl.so.2`, `librt.so.1` | Debian 12 arm64 `libc6` (the device has no glibc loader) | no (third-party) |
| `libstdc++.so.6`, `libgcc_s.so.1` | Debian 12 arm64 `libstdc++6`, `libgcc-s1` | no (third-party) |
| `model/*.onnx`, `model/tokens.txt` | sherpa-onnx streaming zipformer zh-en 3M, chunk-16 int8 | no (third-party) |

**Third-party sources (verified 2026-11-13; both URLs answer 200):**

- runtime: `https://github.com/k2-fsa/sherpa-onnx/releases/download/v1.12.15/sherpa-onnx-v1.12.15-linux-aarch64-shared-cpu.tar.bz2` (22,814,988 B, sha256 `97d61fe8527f94d835d065dbfe9ea1e8a175acb22da89e8f00902e7d60aa459c`)
- model: `https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20.tar.bz2` (32,885,699 B, sha256 `68447f4fbc67e70eee3a93961f36e81e98f47aef73ce7e7ca00885c6cd3616a6`)
  — files `encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx`, `decoder-…chunk-16….onnx`,
  `joiner-…chunk-16….int8.onnx`, `tokens.txt`
- loader libs: Debian 12 arm64 `libc6`, `libstdc++6`, `libgcc-s1` (extract `ld-linux-aarch64.so.1`,
  `libc.so.6`, `libm.so.6`, `libpthread.so.0`, `libdl.so.2`, `librt.so.1`, `libstdc++.so.6`, `libgcc_s.so.1`)

`kw.txt` was lost once (device wipe 2026-11-12) while this table claimed it was tracked; it is now
committed here (one line: `n ǐ h ǎo g āo d á @你好高达`). Score/threshold/trailing-blanks live in
`run.sh` (`0.07 / 4.0 / 0`), so the keyword line carries no `:score #threshold` suffix.

## Install (Mac → device, HTTP pull path)

The device pulls; the Mac serves. `busybox wget` is the transfer tool that
works around the Mac's inbound firewall (the adb/nc path was blocked, and
`/data` writes are eMMC-heavy — pull straight to the final path).

```bash
# on the Mac, from a directory holding the assembled files
python3 -m http.server 8950
```

```sh
# on the device (serial console)
mkdir -p /data/local/tmp/hm-kws/model
cd /data/local/tmp/hm-kws
for f in hm-kws ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libpthread.so.0 \
         libdl.so.2 librt.so.1 libgcc_s.so.1 libstdc++.so.6 \
         libsherpa-onnx-c-api.so libonnxruntime.so run.sh kw.txt; do
  busybox wget -q -O "$f" "http://<mac>:8950/$f"
done
for f in encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
         decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
         joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx tokens.txt; do
  busybox wget -q -O "model/$f" "http://<mac>:8950/model/$f"
done
chmod 755 hm-kws ld-linux-aarch64.so.1 run.sh
```

emOS's busybox `chmod` rejects the symbolic `+x` form ("Bad mode") — use the
numeric mode.

## Enable / disable

`/data/local/bin/start_server.sh` passes the flag set to the firmware:

```sh
KWS_FLAG=${KWS_FLAG---kws --kws-dir /data/local/tmp/hm-kws --kws-cpu 3}
$DIR/current firmware --core $CORE --state-dir $DIR $AEC_FLAG $KWS_FLAG >> $LOG 2>&1 &
```

`KWS_FLAG=""` runs the whole system without the spotter (keyword interruption
then simply reports "unavailable" in the console; the action button still
stops playback). Restart the supervisor for a flag change to take effect
(`tools/fw_restart.py`).

## Verify

```sh
grep -E '\[kws\]' /run/hm-direct.log      # enabled → pinned → ready → HIT lines
```

Core side: `GET /api/v0/mode` → `devices[].kws.state` should be `running`,
`keyword_interrupt` is the user-facing switch, `keyword_phrase` is what the
bundle is deployed with (changing it needs a re-tuned keyword line, so it is a
fact, not a setting).

## Rollback

Set `KWS_FLAG=""` and restart the supervisor, or remove the flag from the
script. The sidecar is a separate process — nothing else depends on it.

## Vocabulary (R1j/P1, 2026-11-13)

`kw.txt` is now COMPILED by the core from the `reflex` configuration block and
deployed over the wire (`CoreFrame::KwsDeploy` → this repo's firmware validates,
backs up, installs and restarts the spotter). Hand-editing the file on the
device works for experiments but the next deploy overwrites it — that is the
intended single source.

Measured with `tools/kws_offline.sh` on the production TTS voice. The
2026-11-12 run put several spelling variants in ONE file; the 2026-11-13 S1
run is controlled (one target + the wake phrase as a control, so a dead file
is distinguishable):

| phrase | spelling | tokens | result |
|---|---|---|---|
| 你好高达 (control) | `n ǐ h ǎo g āo d á` | 8 | **HIT 560 ms** |
| 暂停播放 | `z àn t íng b ō f àng` | 8 | **HIT 360 ms** |
| 退下 | `t uì x ià` (toned) | 4 | **HIT 320 ms** (tb 0/1) |
| 退下 | `t ui x ia` (neutral) | 4 | 0 |
| 退下 | `t uì x ia` (mixed) | 4 | HIT 360 ms |
| 继续 | `j ì x ù` (toned) | 4 | **HIT 440 ms** |
| 继续 | `j i x u` (neutral) | 4 | 0 |
| 暂停 / 拜拜 | toned or neutral | 4 | 0 |
| 打开 / 停止 / 关闭 | toned | 4–5 | 0/1 each in the 2026-11-12 run |

Two findings worth keeping:

1. **A single unknown token kills the whole file.** sherpa logs
   `Encode keywords failed` and then spots NOTHING, while the driver still
   prints `ready`. `tools/kws_offline.sh` now pre-validates every token
   against `model/tokens.txt` (exit 3 with the offending line); the deploy
   path does the same and reports a named rejection.
2. **Tone marks matter; there is no 8-token wall.** The first run said short
   words never fire, but the controlled S1 re-test shows the TONED spelling
   of 退下 (4 tokens) hits at the default 0.07/4.0, and 继续 does too; the
   neutral/mixed spellings and 暂停/拜拜 do not. `trailing-blanks 0/1` made
   no difference. Whether a word fires is per-word and per-spelling — watch
   that word's `hits` after deploying. The transcript remains the ~1 s
   fallback.

`tools/kws_offline.sh <kw-file> <pcm> [threshold] [score] [trailing-blanks] [model-dir]`
runs the driver on a PCM file at real time and prints HIT lines — use it to
check a new phrase BEFORE deploying it.
