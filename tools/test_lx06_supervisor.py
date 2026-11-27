"""Boot and switch the LX06 supervisor against disposable archives and stub services."""
import hashlib
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


SUPERVISOR = Path(__file__).resolve().parents[1] / "deploy/lx06/init.sh"
STUB = b'#!/bin/sh\nif [ "$1" = lx06-wake-contract ]; then echo lx06-idle-wake-v1; exit; fi\nprintf "%s %s %s\\n" "${0##*/}" "${HM_LX06_DUAL_WAKE:-}" "$*" >> "$FIXTURE/starts"\n'


def archive(path, files):
    with tarfile.open(path, "w:xz") as out:
        for name, data in files.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(data)
            entry.mode = 0o755
            out.addfile(entry, io.BytesIO(data))


class TestSupervisor(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.flash = self.root / "flash"
        self.flash.mkdir()
        self.vendor = self.root / "native-journal"
        self.vendor.mkdir()
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": STUB, "silero_vad.onnx": b"fixture",
        })
        archive(self.root / "kws.tar.xz", {
            name: STUB for name in ("run.sh", "hm-kws", "ld-linux-aarch64.so.1")
        })
        (self.flash / "kws.sha256").write_text(hashlib.sha256((self.root / "kws.tar.xz").read_bytes()).hexdigest())
        (self.flash / "kws.url").write_text((self.root / "kws.tar.xz").as_uri())
        unxz = self.flash / "unxz"
        unxz.write_text('#!/bin/sh\necho unpack >> "$FIXTURE/count"\nexec xz -dc\n')
        unxz.chmod(0o755)
        self.meminfo = self.root / "meminfo"
        # Default 256 MB LX06 unit (~241 MiB MemTotal) so existing tests stay on full mode.
        self.meminfo.write_text("MemTotal:         246784 kB\nMemFree:          120000 kB\n")
        # Exercise the shipped function bodies. Replace just the boot invocation
        # and board service/process discovery at the host fixture boundary.
        body, launch, tail = SUPERVISOR.read_text().rpartition("\nrun_supervisor &")
        assert launch and not tail.strip()
        self.body = body + """
PATH="$HOST_BIN_PATH"
HM_FLASH_DIR="$FIXTURE/flash"
HM_STATE_DIR="$HM_FLASH_DIR/state"
HM_RUN_DIR="$FIXTURE/hm"
KWS_DIR="$FIXTURE/hm-kws"
NATIVE_MUTE="$FIXTURE/mute"
NATIVE_LOG_DIR="$FIXTURE/native-journal"
WD_LOG="$FIXTURE/wd"
CORE_LOG="$FIXTURE/core"
GW_LOG="$FIXTURE/gateway"
MEMINFO_FILE="$FIXTURE/meminfo"
start_native_services() { echo "native $*" >> "$FIXTURE/services"; }
stop_runtime() { echo "stop $*" >> "$FIXTURE/services"; }
stop_core() { echo "stop hm-core" >> "$FIXTURE/services"; }
runtime_pid() { return 1; }
pidof() { [ "$1" = mipns-xiaomi ] && [ -f "$FIXTURE/pns-live" ]; }
pns_service() {
    echo "pns $1" >> "$FIXTURE/services"
    if [ "$1" = start ]; then touch "$FIXTURE/pns-live"; else rm -f "$FIXTURE/pns-live"; fi
}
last_mode=""
init_state_if_needed
"""

    def run_shell(self, steps):
        return subprocess.run(
            ["sh", "-eu"], input=self.body + steps, text=True,
            env={**os.environ, "FIXTURE": str(self.root), "HOST_BIN_PATH": os.environ["PATH"]}, capture_output=True,
            timeout=15,
        )

    def test_cold_dual_pin_and_return_reuse_cache(self):
        r = self.run_shell("""
supervise_tick
wait
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
printf 'homemate\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
""")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"] * 2)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(sum(s.startswith("hm-gateway 1 firmware ") for s in starts), 2)
        self.assertEqual(sum("lx06-switch-watch" in s for s in starts), 1)
        services = (self.root / "services").read_text().splitlines()
        self.assertIn("pns start", services)
        self.assertIn("stop firmware", services)
        self.assertIn("stop hm-core", services)
        self.assertTrue((self.flash / "state/models/silero_vad.onnx").is_file())

    def test_cold_start_enables_the_bundled_gateway_vad_model(self):
        gateway = STUB + b'printf "%s" "${KWS_VAD_MODEL:-}" > "$FIXTURE/kws-vad-model"\n'
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": gateway, "silero_vad.onnx": b"fixture",
        })
        result = self.run_shell("unset KWS_VAD_MODEL; supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "kws-vad-model").read_text(), str(self.root / "hm/silero_vad.onnx"))

    def test_first_home_wake_has_a_journal_before_any_native_instruction(self):
        result = self.run_shell("""
rmdir "$NATIVE_LOG_DIR"
supervise_tick
wait
grep -q "^hm-gateway 1 firmware " "$FIXTURE/starts"
test ! -e "$NATIVE_LOG_DIR/instruction.log"
mkdir "$NATIVE_LOG_DIR"
supervise_tick
wait
test -f "$NATIVE_LOG_DIR/instruction.log"
test ! -s "$NATIVE_LOG_DIR/instruction.log"
""")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())

    def test_start_preserves_vendor_instructions(self):
        instruction = self.vendor / "instruction.log"
        instruction.write_bytes(b"active dialog")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(instruction.read_bytes(), b"active dialog")

    def test_wrong_keyword_archive_starts_asr_fallback_without_loading_it(self):
        (self.flash / "kws.sha256").write_text("0" * 64)
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "hm-kws/run.sh").exists())
        starts = (self.root / "starts").read_text()
        self.assertIn(" 1 firmware ", starts)
        self.assertNotIn("--kws", starts)
        self.assertTrue((self.root / "hm/.kws-asr-fallback").exists())

    def test_missing_keyword_host_starts_asr_fallback(self):
        (self.flash / "kws.url").write_text((self.root / "absent.tar.xz").as_uri())
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        starts = (self.root / "starts").read_text()
        self.assertIn(" 1 firmware ", starts)
        self.assertNotIn("--kws", starts)
        self.assertTrue((self.root / "hm/.kws-asr-fallback").exists())

    def test_legacy_native_selection_starts_dual_but_explicit_pin_persists(self):
        (self.flash / "mode").write_text("xiaoai\n")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())
        (self.flash / "mode").write_text("xiaoai-pinned\n")
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.root / "starts").read_text().splitlines()[-1].startswith(
            "hm-gateway  lx06-switch-watch "
        ))

    def test_old_cached_gateway_is_replaced_before_dual_start(self):
        cache = self.root / "hm"
        cache.mkdir()
        for name in ("hm-core", "hm-gateway"):
            binary = cache / name
            binary.write_text("#!/bin/sh\nexit 2\n")
            binary.chmod(0o755)
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(" 1 firmware ", (self.root / "starts").read_text())
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"] * 2)

    def test_old_flash_gateway_does_not_take_exclusive_audio(self):
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB, "hm-gateway": b"#!/bin/sh\nexit 2\n",
        })
        result = self.run_shell("if supervise_tick; then exit 9; fi")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "hm/hm-gateway").exists())
        self.assertFalse((self.root / "starts").exists())
        self.assertTrue((self.root / "pns-live").exists())

    def test_bad_download_retries_without_replacing_main_bundle(self):
        (self.flash / "kws.url").write_text((self.root / "absent").as_uri())
        r = self.run_shell("""
supervise_tick
wait
test ! -e "$KWS_DIR/run.sh"
printf '%s' "$RECOVER_URL" > "$HM_FLASH_DIR/kws.url"
supervise_tick
wait
""".replace("$RECOVER_URL", (self.root / "kws.tar.xz").as_uri()))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.root / "starts").read_text().count(" 1 firmware "), 2)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"] * 2)
        self.assertFalse((self.root / "hm/.kws-asr-fallback").exists())

    def test_recovered_model_restarts_only_an_idle_fallback_gateway(self):
        (self.flash / "kws.url").write_text((self.root / "absent").as_uri())
        result = self.run_shell("""
supervise_tick
wait
runtime_pid() { [ "$2" = firmware ] && echo 123; }
core_idle_for_kws_recovery() { [ -f "$FIXTURE/idle" ]; }
printf '%s' "$RECOVER_URL" > "$HM_FLASH_DIR/kws.url"
kws_retry_at=0
supervise_tick
wait
test ! -e "$KWS_DIR/run.sh"
touch "$FIXTURE/idle"
kws_retry_at=0
supervise_tick
wait
test -e "$KWS_DIR/run.sh"
grep -qx 'stop firmware' "$FIXTURE/services"
""".replace("$RECOVER_URL", (self.root / "kws.tar.xz").as_uri()))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_kws_recovery_does_not_interrupt_unrecognized_speech(self):
        cases = [
            ({"assistant_state": "idle", "input_window_open": False, "in_speech": False, "asr_inflight": 0}, True),
            ({"assistant_state": "idle", "input_window_open": False, "in_speech": True, "asr_inflight": 0}, False),
            ({"assistant_state": "idle", "input_window_open": False, "in_speech": False, "asr_inflight": 1}, False),
            ({"assistant_state": "idle", "input_window_open": False, "in_speech": False, "asr_inflight": 0, "input_id": 7}, False),
            ({"assistant_state": "speaking", "input_window_open": False, "in_speech": False, "asr_inflight": 0}, False),
            ({"assistant_state": "idle", "input_window_open": True, "in_speech": False, "asr_inflight": 0}, False),
            ({"error": "unauthorized"}, False),
        ]
        import json
        for status, idle in cases:
            with self.subTest(status=status):
                (self.root / "mode.json").write_text(json.dumps(status, separators=(",", ":")))
                steps = """
printf 'fixture-token' > "$HM_STATE_DIR/local-release.token"
curl() { cat "$FIXTURE/mode.json"; }
if core_idle_for_kws_recovery; then echo idle; else echo busy; fi
"""
                result = self.run_shell(steps)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), "idle" if idle else "busy")

    def test_pinned_native_honors_management_mute_even_with_broken_bundle(self):
        (self.flash / "hm-bundle.tar.xz").write_bytes(b"truncated")
        result = self.run_shell("""
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
printf '{"volume":40, "muted":true}' > "$HM_STATE_DIR/state.json"
touch "$FIXTURE/pns-live"
if supervise_tick; then exit 9; fi
test ! -e "$FIXTURE/pns-live"
printf '{"muted":false,"volume":40}' > "$HM_STATE_DIR/state.json"
if supervise_tick; then exit 9; fi
test -e "$FIXTURE/pns-live"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_wake_reads_journal_with_lx06_busybox_tr(self):
        # This ROM builds tr without character classes. Its -d operand then
        # deletes the literal eight bytes in '[:space:]', including the colon.
        result = self.run_shell(r"""
tr() {
    if [ "$1" = -d ] && [ "$2" = '[:space:]' ]; then
        command tr -d '\133\072\163\160\141\143\145\135'
    else
        command tr "$@"
    fi
}
journal="$HM_STATE_DIR/state.json"
printf '%s' '{"muted":false,"volume":92}' > "$journal"
restore_native_wake
test -e "$FIXTURE/pns-live"
printf '{\r\n\t"volume" : 92,  "muted" : true\r\n}' > "$journal"
restore_native_wake
test ! -e "$FIXTURE/pns-live"
printf '{\r\n\t"volume" : 92,  "muted" : false\r\n}' > "$journal"
restore_native_wake
test -e "$FIXTURE/pns-live"
printf '%s' '{"muted":f alse,"volume":92}' > "$journal"
restore_native_wake
test ! -e "$FIXTURE/pns-live"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bad_main_archive_never_starts_partial_binary(self):
        (self.flash / "hm-bundle.tar.xz").write_bytes(b"truncated")
        r = self.run_shell("if supervise_tick; then exit 9; fi\n")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse((self.root / "hm/hm-core").exists())
        self.assertFalse((self.root / "starts").exists())

    def test_start_native_services_skips_live_ledserver_and_dismisses_boot_blue_ring(self):
        body, launch, tail = SUPERVISOR.read_text().rpartition("\nrun_supervisor &")
        assert launch and not tail.strip()
        init_d = self.root / "init.d"
        init_d.mkdir()
        for svc in ("led", "touchpad", "mico_aivs_lab"):
            script = init_d / svc
            script.write_text(f'#!/bin/sh\necho "{svc} $*" >> "$FIXTURE/native.log"\n')
            script.chmod(0o755)
        shut_led = self.root / "shut_led"
        shut_led.write_text('#!/bin/sh\necho "shut_led $*" >> "$FIXTURE/native.log"\n')
        shut_led.chmod(0o755)

        script = body + """
PATH="$HOST_BIN_PATH"
INIT_D_DIR="$FIXTURE/init.d"
SHUT_LED_BIN="$FIXTURE/shut_led"
WD_LOG="$FIXTURE/wd"
pidof() { [ "$1" = ledserver ] && [ -f "$FIXTURE/led-live" ]; }

# 1) When ledserver is already running (redeploy/mode-switch), do NOT re-run
#    `/etc/init.d/led start` (which fires `show_led 4`), and dismiss stale L=4.
touch "$FIXTURE/led-live"
start_native_services

# 2) When ledserver is stopped, start it and immediately dismiss boot L=4.
rm -f "$FIXTURE/led-live"
start_native_services
"""
        r = subprocess.run(
            ["sh", "-eu"], input=script, text=True,
            env={**os.environ, "FIXTURE": str(self.root), "HOST_BIN_PATH": os.environ["PATH"]},
            capture_output=True, timeout=15,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(
            (self.root / "native.log").read_text().splitlines(),
            [
                "shut_led 4",
                "touchpad start",
                "mico_aivs_lab start",
                "led start",
                "shut_led 4",
                "touchpad start",
                "mico_aivs_lab start",
            ],
        )

    def test_thin_mode_flag_with_thin_bundle_skips_core_and_kws(self):
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        gateway = STUB + b'printf "%s|%s" "${KWS_VAD_MODEL:-}" "${HM_CORE_AUTH_TOKEN:-}" > "$FIXTURE/thin-env"\n'
        archive(self.flash / "hm-bundle.tar.xz", {"hm-gateway": gateway})
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(len(starts), 1)
        self.assertTrue(starts[0].startswith("hm-gateway 1 firmware "), starts[0])
        self.assertIn("--core 127.0.0.1:8932", starts[0])
        self.assertNotIn("--allow-lan-core", starts[0])
        self.assertNotIn("--kws", starts[0])
        self.assertFalse(any(s.startswith("hm-core ") for s in starts))
        self.assertFalse((self.root / "hm/hm-core").exists())
        self.assertFalse((self.root / "hm-kws").exists())
        self.assertEqual((self.root / "thin-env").read_text(), "|")

    def test_thin_mode_with_full_bundle_strips_core_and_web_and_reuses_cache(self):
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": STUB,
            "hm-gateway": STUB,
            "silero_vad.onnx": b"fixture",
            "web/index.html": b"<html></html>",
        })
        result = self.run_shell("unset HM_SILERO_VAD KWS_VAD_MODEL; supervise_tick; wait; supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"])
        self.assertTrue((self.root / "hm/hm-gateway").is_file())
        self.assertFalse((self.root / "hm/hm-core").exists())
        self.assertFalse((self.root / "hm/web").exists())
        self.assertFalse((self.root / "hm/silero_vad.onnx").exists())
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(len(starts), 2)
        for line in starts:
            self.assertTrue(line.startswith("hm-gateway 1 firmware "), line)
            self.assertNotIn("--kws", line)

    def test_thin_mode_reads_lan_core_addr_and_token_via_env(self):
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        (self.flash / "core.addr").write_text("192.168.1.50:8932\n")
        token_file = self.flash / "core.token"
        token_file.write_text("hm1_lan_secret_token\n")
        token_file.chmod(0o644)
        gateway = STUB + b'printf "%s" "${HM_CORE_AUTH_TOKEN:-}" > "$FIXTURE/gw-token"\n'
        archive(self.flash / "hm-bundle.tar.xz", {"hm-gateway": gateway})
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(len(starts), 1)
        self.assertIn("--core 192.168.1.50:8932", starts[0])
        self.assertIn("--allow-lan-core", starts[0])
        self.assertNotIn("hm1_lan_secret_token", starts[0])
        self.assertNotIn("--kws", starts[0])
        self.assertEqual((self.root / "gw-token").read_text(), "hm1_lan_secret_token")
        self.assertEqual(token_file.stat().st_mode & 0o777, 0o600)

    def test_thin_mode_auto_detected_from_128mb_meminfo(self):
        # 128 MB LX06 batch measures MemTotal ~ 118,760 kB (< 180,000 kB threshold).
        self.meminfo.write_text("MemTotal:         118760 kB\nMemFree:           41200 kB\n")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {"hm-gateway": STUB})
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(len(starts), 1)
        self.assertTrue(starts[0].startswith("hm-gateway 1 firmware "), starts[0])
        self.assertIn("--core 127.0.0.1:8932", starts[0])
        self.assertNotIn("--kws", starts[0])
        self.assertFalse(any(s.startswith("hm-core ") for s in starts))

    def test_thin_mode_env_override_supports_xiaoai_pin_and_cache_reuse(self):
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {"hm-gateway": STUB})
        result = self.run_shell("""
HM_THIN_MODE=1
export HM_THIN_MODE
supervise_tick
wait
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
printf 'homemate\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
""")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"])
        starts = (self.root / "starts").read_text().splitlines()
        self.assertEqual(sum(s.startswith("hm-gateway 1 firmware ") for s in starts), 2)
        self.assertEqual(sum("lx06-switch-watch" in s for s in starts), 1)
        self.assertFalse(any(s.startswith("hm-core ") for s in starts))

    def test_custom_device_id_honored_in_both_256mb_and_thin_modes(self):
        # 1. 256 MB full mode passes custom device-id + --model LX06 + --kws.
        state_dir = self.flash / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "device-id").write_text("lx06-bedroom\n")
        r_full = self.run_shell("supervise_tick; wait")
        self.assertEqual(r_full.returncode, 0, r_full.stderr)
        full_starts = (self.root / "starts").read_text().splitlines()
        gw_full = [s for s in full_starts if s.startswith("hm-gateway 1 firmware ")]
        self.assertEqual(len(gw_full), 1)
        self.assertIn("--device-id lx06-bedroom --model LX06", gw_full[0])
        self.assertIn("--kws", gw_full[0])

        # 2. 128 MB thin mode passes custom device-id + --model LX06 without --kws.
        (self.root / "starts").unlink()
        (self.flash / "thin-mode").write_text("")
        (state_dir / "device-id").write_text("lx06-study\n")
        r_thin = self.run_shell("supervise_tick; wait")
        self.assertEqual(r_thin.returncode, 0, r_thin.stderr)
        thin_starts = (self.root / "starts").read_text().splitlines()
        gw_thin = [s for s in thin_starts if s.startswith("hm-gateway 1 firmware ")]
        self.assertEqual(len(gw_thin), 1)
        self.assertIn("--device-id lx06-study --model LX06", gw_thin[0])
        self.assertNotIn("--kws", gw_thin[0])

    def test_thin_mode_loopback_vs_lan_and_ipv6_ula_allow_lan_core_flag(self):
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {"hm-gateway": STUB})
        cases = [
            ("127.0.0.1:8932", False),
            ("localhost:8932", False),
            ("[::1]:8932", False),
            ("[::ffff:127.0.0.1]:8932", False),
            ("192.168.1.50:8932", True),
            ("[fd7a:115c:a1e0::1]:8932", True),
        ]
        for addr, expect_lan_flag in cases:
            if (self.root / "starts").exists():
                (self.root / "starts").unlink()
            (self.flash / "core.addr").write_text(f"{addr}\n")
            res = self.run_shell("supervise_tick; wait")
            self.assertEqual(res.returncode, 0, f"{addr}: {res.stderr}")
            starts = (self.root / "starts").read_text().splitlines()
            self.assertEqual(len(starts), 1, f"{addr}: {starts}")
            self.assertIn(f"--core {addr}", starts[0])
            if expect_lan_flag:
                self.assertIn("--allow-lan-core", starts[0], f"{addr} must pass --allow-lan-core")
            else:
                self.assertNotIn("--allow-lan-core", starts[0], f"{addr} must omit --allow-lan-core")

    def test_xiaoai_pinned_stops_running_hm_core_to_reclaim_ram(self):
        body, launch, tail = SUPERVISOR.read_text().rpartition("\nrun_supervisor &")
        assert launch and not tail.strip()
        script = body + """
PATH="$HOST_BIN_PATH"
HM_FLASH_DIR="$FIXTURE/flash"
HM_STATE_DIR="$HM_FLASH_DIR/state"
HM_RUN_DIR="$FIXTURE/hm"
KWS_DIR="$FIXTURE/hm-kws"
NATIVE_MUTE="$FIXTURE/mute"
NATIVE_LOG_DIR="$FIXTURE/native-journal"
WD_LOG="$FIXTURE/wd"
CORE_LOG="$FIXTURE/core"
GW_LOG="$FIXTURE/gateway"
MEMINFO_FILE="$FIXTURE/meminfo"
start_native_services() { :; }
stop_runtime() { :; }
runtime_pid() { return 1; }
pns_service() { :; }
sleep 30 &
CORE_PID=$!
pidof() {
    if [ "$1" = hm-core ] && kill -0 "$CORE_PID" 2>/dev/null; then
        echo "$CORE_PID"
        return 0
    fi
    return 1
}
last_mode="homemate"
init_state_if_needed
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait "$CORE_PID" 2>/dev/null || true
if kill -0 "$CORE_PID" 2>/dev/null; then
    echo "hm-core still alive" >&2
    exit 9
fi
wait 2>/dev/null || true
"""
        r = subprocess.run(
            ["sh", "-eu"], input=script, text=True,
            env={**os.environ, "FIXTURE": str(self.root), "HOST_BIN_PATH": os.environ["PATH"]},
            capture_output=True, timeout=15,
        )
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_thin_mode_extracts_only_hm_gateway_member_without_spiking_tmpfs(self):
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": b"BIG_CORE_PAYLOAD" * 1024,
            "hm-gateway": STUB,
            "silero_vad.onnx": b"VAD_BYTES" * 1024,
            "web/index.html": b"<html></html>",
        })
        result = self.run_shell("""
tar() {
    printf '%s\\n' "$*" >> "$FIXTURE/tar-args"
    command tar "$@"
}
supervise_tick
wait
""")
        self.assertEqual(result.returncode, 0, result.stderr)
        tar_calls = (self.root / "tar-args").read_text().splitlines()
        self.assertEqual(len(tar_calls), 1)
        self.assertTrue(
            tar_calls[0].endswith(" hm-gateway"),
            f"thin mode must extract only hm-gateway member, got: {tar_calls[0]}",
        )
        self.assertTrue((self.root / "hm/hm-gateway").is_file())
        self.assertFalse((self.root / "hm/hm-core").exists())
        self.assertFalse((self.root / "hm/web").exists())
        self.assertFalse((self.root / "hm/silero_vad.onnx").exists())

    def test_sync_kws_config_is_atomic_skips_unchanged_and_does_not_resurrect_deleted(self):
        result = self.run_shell("""
mkdir -p "$KWS_DIR" "$HM_STATE_DIR/kws-persist"
printf 'n i h ao @你好\\n' > "$HM_STATE_DIR/kws-persist/kw.txt"
printf '{"threshold":0.25,"score":3.0}\\n' > "$HM_STATE_DIR/kws-persist/kw.tuning.json"

# 1. Initial restore copies persisted files into fresh KWS_DIR once.
restore_kws_config
cmp -s "$HM_STATE_DIR/kws-persist/kw.txt" "$KWS_DIR/kw.txt"
cmp -s "$HM_STATE_DIR/kws-persist/kw.tuning.json" "$KWS_DIR/kw.tuning.json"

# 2. Steady-state sync skips cp when unchanged (zero flash writes).
cp() {
    printf 'cp %s -> %s\\n' "$1" "$2" >> "$FIXTURE/cp-log"
    command cp "$@"
}
mv() {
    printf 'mv %s\\n' "$*" >> "$FIXTURE/mv-log"
    command mv "$@"
}
sync_kws_config
test ! -e "$FIXTURE/cp-log"
test ! -e "$FIXTURE/mv-log"

# 3. Changed live file writes atomically via .tmp + mv -f.
printf 'n i h ao @你好小智\\n' > "$KWS_DIR/kw.txt"
sync_kws_config
grep -q "kws-persist/kw.txt.tmp$" "$FIXTURE/cp-log"
grep -q "kws-persist/kw.txt.tmp .*kws-persist/kw.txt$" "$FIXTURE/mv-log"
test ! -e "$HM_STATE_DIR/kws-persist/kw.txt.tmp"
cmp -s "$KWS_DIR/kw.txt" "$HM_STATE_DIR/kws-persist/kw.txt"

# 4. Intentionally deleted live file (e.g. tuning reset) is NOT resurrected
#    from flash on steady-state ticks, and its stale flash copy is removed.
rm -f "$KWS_DIR/kw.tuning.json"
sync_kws_config
test ! -e "$KWS_DIR/kw.tuning.json"
test ! -e "$HM_STATE_DIR/kws-persist/kw.tuning.json"

# 5. Subsequent restore_kws_config after initial marker does not overwrite live.
printf 'keep-live\\n' > "$KWS_DIR/kw.txt"
restore_kws_config
grep -qx 'keep-live' "$KWS_DIR/kw.txt"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_sync_kws_config_handles_enospc_and_write_failures_with_backoff(self):
        result = self.run_shell("""
mkdir -p "$KWS_DIR" "$HM_STATE_DIR/kws-persist"
printf 'old-good-kw\\n' > "$HM_STATE_DIR/kws-persist/kw.txt"
printf 'new-kw-v2\\n' > "$KWS_DIR/kw.txt"

# 1. Low free space skips write, preserves existing flash file, and arms backoff (5s).
flash_free_kb() { echo 12; }
cp() {
    echo "cp_called" >> "$FIXTURE/writes"
    command cp "$@"
}
if sync_kws_config; then exit 9; fi
test ! -e "$FIXTURE/writes"
grep -qx 'old-good-kw' "$HM_STATE_DIR/kws-persist/kw.txt"
test "$kws_sync_fail_streak" -eq 1
test "$kws_sync_cooldown" -eq 5

# 2. During cooldown ticks, sync_kws_config returns 0 without retrying flash.
flash_free_kb() { echo 4096; }
for _i in 1 2 3 4 5; do
    sync_kws_config
done
test ! -e "$FIXTURE/writes"
test "$kws_sync_cooldown" -eq 0

# 3. Simulated cp failure mid-write cleans up partial .tmp, keeps old file intact,
#    and doubles backoff (streak=2 -> 10s).
cp() {
    echo "cp_fail" >> "$FIXTURE/writes"
    printf 'partial' > "$2"
    return 1
}
if sync_kws_config; then exit 9; fi
test ! -e "$HM_STATE_DIR/kws-persist/kw.txt.tmp"
grep -qx 'old-good-kw' "$HM_STATE_DIR/kws-persist/kw.txt"
test "$kws_sync_fail_streak" -eq 2
test "$kws_sync_cooldown" -eq 10

# 4. Once cooldown expires and writes succeed, streak resets and new file persists.
kws_sync_cooldown=0
cp() { command cp "$@"; }
sync_kws_config
test "$kws_sync_fail_streak" -eq 0
test "$kws_sync_cooldown" -eq 0
grep -qx 'new-kw-v2' "$HM_STATE_DIR/kws-persist/kw.txt"
""")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_standalone_asr_mode_overrides_128mb_auto_thin_runs_core_with_voice_events_disabled_and_skips_kws(self):
        # 128 MB LX06 batch (MemTotal ~118 MB) opting into standalone pure-ASR mode
        # must override auto-thin, launch on-device hm-core with HM_VOICE_EVENTS=0
        # (no HM_SILERO_VAD), launch hm-gateway against loopback without --kws or
        # KWS_VAD_MODEL, and never fetch or unpack hm-kws.
        self.meminfo.write_text("MemTotal:         118760 kB\nMemFree:           52000 kB\n")
        (self.flash / "standalone-asr").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        core_stub = (
            STUB
            + b'printf "%s|%s|%s" "${HM_VOICE_EVENTS:-}" "${HM_SILERO_VAD:-}" "${HM_BIND:-}" > "$FIXTURE/core-env"\n'
        )
        gw_stub = STUB + b'printf "%s" "${KWS_VAD_MODEL:-}" > "$FIXTURE/gw-vad-env"\n'
        archive(self.flash / "hm-bundle.tar.xz", {
            "hm-core": core_stub,
            "hm-gateway": gw_stub,
            "silero_vad.onnx": b"fixture",
            "web/index.html": b"<html></html>",
        })
        result = self.run_shell("unset HM_SILERO_VAD KWS_VAD_MODEL; supervise_tick; wait; supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        # Main bundle unpacked once and cached; KWS archive never touched.
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"])
        self.assertTrue((self.root / "hm/hm-core").is_file())
        self.assertTrue((self.root / "hm/hm-gateway").is_file())
        self.assertFalse((self.root / "hm-kws").exists())
        self.assertFalse((self.flash / "state/models/silero_vad.onnx").exists())
        self.assertEqual((self.root / "core-env").read_text(), "0||0.0.0.0:8930")
        self.assertEqual((self.root / "gw-vad-env").read_text(), "")
        starts = (self.root / "starts").read_text().splitlines()
        core_starts = [s for s in starts if s.startswith("hm-core ")]
        gw_starts = [s for s in starts if s.startswith("hm-gateway 1 firmware ")]
        self.assertEqual(len(core_starts), 2)
        self.assertEqual(len(gw_starts), 2)
        for line in gw_starts:
            self.assertIn("--core 127.0.0.1:8932", line)
            self.assertIn("--model LX06", line)
            self.assertNotIn("--allow-lan-core", line)
            self.assertNotIn("--kws", line)

    def test_standalone_asr_mode_explicit_thin_flag_still_takes_precedence(self):
        self.meminfo.write_text("MemTotal:         118760 kB\nMemFree:           52000 kB\n")
        (self.flash / "standalone-asr").write_text("")
        (self.flash / "thin-mode").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        archive(self.flash / "hm-bundle.tar.xz", {"hm-core": STUB, "hm-gateway": STUB})
        result = self.run_shell("supervise_tick; wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        starts = (self.root / "starts").read_text().splitlines()
        self.assertFalse(any(s.startswith("hm-core ") for s in starts))
        self.assertEqual(sum(s.startswith("hm-gateway 1 firmware ") for s in starts), 1)
        self.assertFalse((self.root / "hm/hm-core").exists())

    def test_standalone_asr_mode_supports_xiaoai_pin_toggle_and_custom_device_id(self):
        self.meminfo.write_text("MemTotal:         118760 kB\nMemFree:           52000 kB\n")
        (self.flash / "standalone-asr").write_text("")
        (self.flash / "kws.url").unlink()
        (self.flash / "kws.sha256").unlink()
        state_dir = self.flash / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "device-id").write_text("lx06-standalone\n")
        result = self.run_shell("""
supervise_tick
wait
printf 'xiaoai-pinned\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
printf 'homemate\\n' > "$HM_FLASH_DIR/mode"
supervise_tick
wait
""")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "count").read_text().splitlines(), ["unpack"])
        starts = (self.root / "starts").read_text().splitlines()
        gw_starts = [s for s in starts if s.startswith("hm-gateway 1 firmware ")]
        self.assertEqual(len(gw_starts), 2)
        for line in gw_starts:
            self.assertIn("--device-id lx06-standalone --model LX06", line)
            self.assertNotIn("--kws", line)
        self.assertEqual(sum("lx06-switch-watch" in s for s in starts), 1)
        services = (self.root / "services").read_text().splitlines()
        self.assertIn("stop hm-core", services)
        self.assertIn("stop firmware", services)

    def test_install_lx06_cli_rejects_conflicting_standalone_asr_flags(self):
        installer = SUPERVISOR.parent / "install-lx06.sh"
        help_res = subprocess.run(["sh", str(installer), "--help"], text=True, capture_output=True, timeout=10)
        self.assertEqual(help_res.returncode, 0, help_res.stderr)
        self.assertIn("--standalone-asr", help_res.stdout)

        both = subprocess.run(
            ["sh", str(installer), "--thin", "--standalone-asr", "--device-ip", "192.168.1.10"],
            text=True, capture_output=True, timeout=10,
        )
        self.assertEqual(both.returncode, 2)
        self.assertIn("mutually exclusive", both.stderr)

        with_core = subprocess.run(
            ["sh", str(installer), "--standalone-asr", "--device-ip", "192.168.1.10", "--core-addr", "192.168.1.50:8932"],
            text=True, capture_output=True, timeout=10,
        )
        self.assertEqual(with_core.returncode, 2)
        self.assertIn("--standalone-asr", with_core.stderr)

        for flag, val in (("--core-token", "tok_test"), ("--kws-url", "http://192.168.1.50:18089/kws.tar.xz")):
            conflict = subprocess.run(
                ["sh", str(installer), "--standalone-asr", "--device-ip", "192.168.1.10", flag, val],
                text=True, capture_output=True, timeout=10,
            )
            self.assertEqual(conflict.returncode, 2, f"{flag} must conflict with --standalone-asr")
            self.assertIn("--standalone-asr", conflict.stderr)


if __name__ == "__main__":
    unittest.main()
