from pathlib import Path
#!/usr/bin/env python3
"""D/T5 acceptance: SEMANTIC barge-in, judged by the fixed criterion.

The 2026-11-09 first acceptance reported "3/3, P50 120 ms" — FALSE. The
script counted `stops` deltas, and the implementation was self-interrupting
in a loop, so the count could only grow. The user rejected that run on the
spot. The criterion below is therefore written INTO the tool and is the
only thing that decides PASS:

    ONE human utterance during playback
      -> EXACTLY ONE stop
      -> ONE complete mic segment (>= MIN_SEG_BLOCKS blocks)
      -> ONE ASR utterance after that stop, consistent with what was said
      -> the follow-up answer plays to COMPLETION
      -> TEN SECONDS with ZERO new stops

Any of these fails the round, and the failure names the condition:
  * any stop BEFORE the stimulus        -> false trigger (self-echo)
  * more than one stop                  -> loop / episode not closed
  * no segment / no ASR after the stop   -> stop without a reason
  * answer cut (played_ms < MIN_PLAY_MS) -> the answer did not survive
  * a stop inside the quiet window       -> the loop is still alive
Fragmented ASR (short "嗯/哦" style transcripts) is reported as text and
also fails the coherence check.

Constraints (user 2026-11-09/10): volume <= 80%; voice channel only; NO
music without asking; the camera speaker carries the unattended stimulus.

Usage:
  t5_bargein_verify.py --rounds 3                 # human in the loop (default)
  t5_bargein_verify.py --rounds 3 --camera        # unattended camera stimulus
  t5_bargein_verify.py --immersion-min 4          # zero-false-trigger soak
"""
import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time
import urllib.request

CORE = os.environ.get("HM_CORE", "http://192.168.1.73:8930")
MAIN = os.environ.get("HM_CORE_REPO", str(Path(__file__).resolve().parents[2] / "home-mate"))
STIMULI = [
    (os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat1.boost.wav"), "他说要是有肉有菜有大米饭"),
    (os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat2.boost.wav"), "他说马亮特别不爱吃淡水"),
    (os.path.join(MAIN, "data/acoustic/phrases/nonwx_chat3.boost.wav"), "星期天天气不错"),
]
CAMERA_OUT = "2K USB Camera-Audio"
MAC_OUT = "Mac Studio扬声器"
SWITCH = "/opt/homebrew/bin/SwitchAudioSource"
ASSET = "erle-stimulus.wav"
HUMAN_WINDOW_S = 10.0
QUIET_S = 10.0
MIN_SEG_BLOCKS = 6      # >= 480 ms of speech after the stop
MIN_PLAY_MS = 800       # the follow-up answer actually played
COHERENCE = 0.30        # camera mode: char overlap with the known phrase


def post(path, body=None, timeout=120):
    req = urllib.request.Request(
        CORE + path, data=json.dumps(body or {}).encode(),
        headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def get(path):
    with urllib.request.urlopen(CORE + path, timeout=8) as r:
        return json.load(r)


def events():
    return get("/api/v0/voice/log")["events"]


def device():
    return get("/api/v0/mode")["devices"][0]


def kind(e):
    return e.get("kind") if isinstance(e, dict) else None


def ts(e):
    return int(e.get("ts_ms") or 0)


def newest_ts():
    evs = events()
    return max((ts(e) for e in evs), default=0)


def play_stimulus():
    subprocess.run([SWITCH, "-s", CAMERA_OUT], check=False)
    phrase, text = STIMULI[int(time.time()) % len(STIMULI)]
    subprocess.run(["afplay", phrase], check=False)
    subprocess.run([SWITCH, "-s", MAC_OUT], check=False)
    return text


def coherence(said, expected):
    """Crude char-overlap: the ASR of a 1-2 s phrase is never exact."""
    if not said or not expected:
        return 0.0
    s = set(re.sub(r"[^\u4e00-\u9fff\w]", "", said))
    e = set(re.sub(r"[^\u4e00-\u9fff\w]", "", expected))
    return len(s & e) / max(1, len(e))


def run_round(i, log, camera):
    t0 = newest_ts()
    stim_text = None
    result = {}

    def _play():
        try:
            result.update(post("/api/v0/diagnostics/tests/play_asset",
                               {"asset": ASSET, "channel": "voice"}, timeout=120))
        except Exception as e:  # noqa: BLE001 - evidence, never crash the round
            result["error"] = str(e)

    th = threading.Thread(target=_play, daemon=True)
    th.start()
    # Wait until the answer is audibly live: the play receipt is sent when
    # the device accepts the stream; 3 s also fills the guard rings.
    time.sleep(3)
    t_stim = newest_ts()
    if camera:
        stim_text = play_stimulus()
    else:
        print("  >>> 请在音箱说话时正常说一句中文（不要唤醒词），Then stop. <<<", flush=True)
        time.sleep(HUMAN_WINDOW_S)
    th.join(timeout=45)
    # Quiet window: no new stop may appear after the answer.
    time.sleep(QUIET_S)

    evs = [e for e in events() if ts(e) >= t0]
    evs.sort(key=ts)
    stops = [e for e in evs if kind(e) == "barge_in_stop"]
    pre = [e for e in stops if ts(e) < t_stim]
    post = [e for e in stops if ts(e) >= t_stim]
    segs = [e for e in evs if kind(e) == "mic_segment_closed" and e.get("barge_in") is True
            and ts(e) >= t_stim]
    utts = [e for e in evs if kind(e) == "mic_utterance" and ts(e) >= t_stim]
    utt_ts = ts(utts[0]) if utts else None
    plays = [e for e in evs if kind(e) == "play_stage" and e.get("stage") == "accepted"]
    fins = [e for e in evs if kind(e) == "play_finished"]
    last_play = max((ts(e) for e in plays), default=0)
    answer_fin = [e for e in fins if last_play and ts(e) >= last_play]
    answer_ms = max((int(e.get("played_ms") or 0) for e in answer_fin), default=0)
    tail_stops = [e for e in stops if utt_ts and ts(e) > utt_ts]

    said = utts[0].get("text") if utts else None
    fails = []
    if pre:
        fails.append(f"false_trigger_before_speech={len(pre)}")
    if len(post) != 1:
        fails.append(f"stops_after_speech={len(post)} (want exactly 1)")
    if len(segs) != 1:
        fails.append(f"barge_segments={len(segs)} (want exactly 1)")
    elif int(segs[0].get("blocks") or 0) < MIN_SEG_BLOCKS:
        fails.append(f"segment_blocks={segs[0].get('blocks')} (<{MIN_SEG_BLOCKS})")
    if len(utts) != 1:
        fails.append(f"utterances_after_stop={len(utts)} (want exactly 1)")
    if said is not None:
        if camera and stim_text and coherence(said, stim_text) < COHERENCE:
            fails.append(f"asr_not_consistent(coherence={coherence(said, stim_text):.2f})")
        if len(re.sub(r"\s", "", said or "")) < 2:
            fails.append("asr_empty_or_fragment")
    if answer_ms < MIN_PLAY_MS:
        fails.append(f"answer_played_ms={answer_ms} (<{MIN_PLAY_MS})")
    if tail_stops:
        fails.append(f"stops_after_utterance={len(tail_stops)}")

    row = {
        "round": i, "mode": "camera" if camera else "human",
        "verdict": "PASS" if not fails else "FAIL",
        "fails": fails,
        "stops_before_speech": len(pre),
        "stops_after_speech": len(post),
        "segment_blocks": segs[0].get("blocks") if segs else None,
        "asr_text": said,
        "asr_coherence": round(coherence(said, stim_text), 2) if (said and stim_text) else None,
        "answer_played_ms": answer_ms,
        "corr": post[0].get("corr") if post else None,
        "stimulus": os.path.basename(STIMULI[int(time.time()) % len(STIMULI)][0]) if camera else "human",
    }
    log.write(json.dumps(row, ensure_ascii=False) + "\n")
    print(f"round {i}: {row['verdict']} stops(pre/post)={len(pre)}/{len(post)} "
          f"seg={row['segment_blocks']} asr={said!r} answer_ms={answer_ms} "
          f"corr={row['corr']} fails={fails or '-'}")
    time.sleep(3)  # let the follow window settle before the next round
    return row


def immersion(minutes, log):
    """Continuous VOICE playback, NO stimulus: every barge_in_stop is a
    false interrupt. This is the zero-false-trigger criterion."""
    print(f"immersion {minutes} min: voice-only, no stimulus")
    t0 = newest_ts()
    plays = 0
    end = time.time() + minutes * 60
    while time.time() < end:
        p = post("/api/v0/diagnostics/tests/play_asset",
                 {"asset": ASSET, "channel": "voice"}, timeout=120)
        plays += 1
        time.sleep(2)
    evs = [e for e in events() if ts(e) >= t0]
    stops = [e for e in evs if kind(e) == "barge_in_stop"]
    row = {"immersion_minutes": minutes, "plays": plays,
           "barge_stops_during_playback": len(stops)}
    log.write("# IMMERSION " + json.dumps(row, ensure_ascii=False) + "\n")
    print("IMMERSION", json.dumps(row, ensure_ascii=False),
          "->", "PASS" if not stops else "FAIL")
    return not stops


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--camera", action="store_true", help="unattended camera-speaker stimulus")
    ap.add_argument("--immersion-min", type=float, default=0.0)
    ap.add_argument("--out", default="/tmp/hm-t5/bargein-verify.jsonl")
    a = ap.parse_args()
    os.makedirs(os.path.dirname(a.out), exist_ok=True)
    d = device()
    vol = d.get("volume")
    if isinstance(vol, (int, float)) and vol > 0.8:
        print(f"REFUSE: device volume {vol} > 80%", file=sys.stderr)
        return 2
    log = open(a.out, "a", buffering=1)
    log.write(f"# T5 verify {time.strftime('%Y-%m-%d %H:%M:%S')} "
              f"barge_in={get('/api/v0/mode').get('barge_in')} volume={vol}\n")
    rows, imm_ok = [], True
    try:
        for i in range(1, a.rounds + 1):
            rows.append(run_round(i, log, a.camera))
        if a.immersion_min > 0:
            imm_ok = immersion(a.immersion_min, log)
    finally:
        log.close()
    passed = sum(1 for r in rows if r["verdict"] == "PASS")
    print(f"\nRESULT {passed}/{len(rows)} rounds PASS (criterion is per-round, not a counter)")
    for r in rows:
        if r["fails"]:
            print(f"  round {r['round']} FAIL: {', '.join(r['fails'])}")
    print(f"evidence -> {a.out}")
    return 0 if (passed == len(rows) and imm_ok) else 1


if __name__ == "__main__":
    sys.exit(main())
