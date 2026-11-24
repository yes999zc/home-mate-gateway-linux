#!/system/bin/sh
# hm-kws launcher (D/T4). emOS has no /lib/ld-linux-aarch64.so.1, so the
# glibc-built driver runs under the bundled Debian loader (the KWS spike
# proved this path on 2026-11-10). Everything resolves next to this script;
# the firmware spawns it with KWS_CPU (default cpu3) and feeds raw s16le
# 16 kHz mono blocks on stdin.
#
# Model: sherpa-onnx streaming zipformer zh-en 3M, chunk-16 int8
# (real-speech phrase check: 5/5, 2026-11-10). Keyword line lives in kw.txt;
# threshold/score are the RUI production values for this model.
cd /data/local/tmp/hm-kws || exit 1
exec ./ld-linux-aarch64.so.1 --library-path . ./hm-kws \
  --tokens model/tokens.txt \
  --encoder model/encoder-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
  --decoder model/decoder-epoch-13-avg-2-chunk-16-left-64.onnx \
  --joiner model/joiner-epoch-13-avg-2-chunk-16-left-64.int8.onnx \
  --keywords-file kw.txt \
  --threshold 0.07 --score 4.0 --trailing-blanks 0 \
  --cpu "${KWS_CPU:-3}" --name dot2-d
