//! Local keyword-spotting sidecar (D/T4, 2026-11-10).
//!
//! Why this lives in the firmware: the keyword spotter is a reflex over the
//! SAME post-AEC stream the mic uplink carries, and it must keep working
//! while the core's half-duplex gate is (correctly) dropping that stream
//! during voice playback — a core-side detector could never see the audio it
//! is supposed to interrupt. The device gateway therefore runs the spotter
//! (`kws/hm-kws`: sherpa-onnx streaming zipformer, pinned to its own core)
//! and reports detections north as FACTS. The decision to stop playback
//! stays in the core ([`crate::...`] voice pipeline) for every word except
//! the deployed retreat set: those cut the audio locally FIRST (v0.5 local
//! exit, contract-tested) and close the session north. A false wake-word
//! positive can only ever be acted on by the core's rules; a false retreat
//! positive costs one silence, never a semantics execution.
//!
//! Failure policy: the sidecar is supervised here with backoff, its state is
//! reported north (`kws_report`) so the console can say "keyword interrupt
//! unavailable", and nothing else in the audio path depends on it. If it
//! dies, the action button still stops playback — a missing reflex is
//! reported, never silently pretended.

use crate::fwlog;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::protocol::GatewayFrame;

/// Queued blocks ahead of the spotter: 8 × 80 ms = 640 ms of slack. When
/// the sidecar stalls (restart, CPU starvation) blocks are DROPPED and
/// counted — the audio path must never block on a diagnostic/reflex helper.
pub const QUEUE_BLOCKS: usize = 8;

/// Restart backoff schedule in seconds; a run longer than `HEALTHY_SECS`
/// resets the schedule so a flaky model load cannot hammer the CPU.
const BACKOFF_SECS: [u64; 5] = [1, 2, 5, 10, 30];
/// How often a MISSING sidecar bundle is re-checked (access(2), no trap).
/// Slow enough to stay invisible in the trap log, fast enough that restoring
/// the bundle after a cold-boot download hot-starts local KWS within seconds.
const KWS_MISSING_POLL_SECS: u64 = 5;
const HEALTHY_SECS: u64 = 60;

/// Grace period between a deploy's SIGTERM and the SIGKILL escalation:
/// long enough for the driver to finish the block it is decoding, short
/// enough that a deploy is not felt as a stall. A sidecar that cannot be
/// stopped must not be able to keep the device on an old vocabulary.
const KWS_KILL_GRACE_MS: u64 = 1500;
/// How often the supervisor checks for a dead child while waiting. Small
/// enough to reap promptly, cheap enough for an otherwise idle thread.
const KWS_WAIT_POLL_MS: u64 = 50;

#[derive(Debug, Clone, Default)]
pub struct KwsStatus {
    /// "off" | "starting" | "running" | "down"
    pub state: String,
    /// Human-readable last fact: model load time, exit status, last hit.
    pub detail: String,
    pub restarts: u64,
    pub detections: u64,
    /// Hits suppressed by the echo guard (D/T5, RUI Local defense ①):
    /// the spotter heard the speakers, not the user. Counted and visible;
    /// never reported north as a fact.
    pub guarded: u64,
    pub last_keyword_ms: u64,
    pub dropped_blocks: u64,
    /// R1g/P7: this PROCESS's epoch. The lifetime counters above survive a
    /// core restart while the core's own counters do not, so a number is only
    /// comparable inside one epoch — the core reports both.
    pub epoch_ms: u64,
    /// M6b (2026-11-13): the FNV-1a of the keyword file this sidecar actually
    /// opened (computed at spawn, reported only after its `ready` line).
    /// "The device installed hash X" (deploy receipt) and "the spotter loaded
    /// hash X" are different facts; this is the second one.
    pub loaded_hash: String,
    /// When that load was confirmed ready (ms epoch, 0 = never).
    pub loaded_ms: u64,
    /// Contract v2 §2.5 (#5, 2026-11-16): how many retreat words this device
    /// currently ARMS for a local exit (`kw.exit.json`). A device fact that
    /// survives core restarts — the core's own deploy record does not, so a
    /// console reading only that would report "not armed" after every core
    /// restart, and an activation would re-push for nothing.
    pub local_exit: u32,
    /// Contract v2 #4 (2026-11-16): the sensitivity the LOADED model reported
    /// in its own `ready` line (the driver prints both). 0.0 = that line did
    /// not carry them (an older driver).
    pub threshold: f32,
    pub score: f32,
    /// Active KWS bundle identity (`config_apply_v2` / #112) and monotonic
    /// generation counter, attached to `GatewayFrame::Keyword`.
    pub bundle_id: String,
    pub bundle_generation: u64,
}

impl KwsStatus {
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.state,
            "detail": self.detail,
            "restarts": self.restarts,
            "detections": self.detections,
            "guarded": self.guarded,
            "last_keyword_ms": self.last_keyword_ms,
            "dropped_blocks": self.dropped_blocks,
            "epoch": self.epoch_ms,
            "loaded_hash": self.loaded_hash,
            "loaded_ms": self.loaded_ms,
            "local_exit": self.local_exit,
            "threshold": self.threshold,
            "score": self.score,
            "bundle_id": self.bundle_id,
            "bundle_generation": self.bundle_generation,
        })
    }
}

/// "Is the microphone echo-dominated right now?" — the echo guard the
/// caller supplies (D/T5): playback live AND the near-end vs DAC-reference
/// cross-correlation above threshold means a keyword hit heard THIS. None =
/// no guard (hits pass through, T4 behavior).
pub type Guard = Arc<dyn Fn() -> bool + Send + Sync>;

/// Where the acoustic strength of the phrase comes from (v0.2.9, wake
/// arbitration). The capture thread owns the post-AEC block the spotter is
/// fed, so it owns the measurement; this closure reads the peak over the
/// requested `window_ms` lookback span. None = this firmware configuration
/// does not measure levels.
pub type LevelSource = Arc<dyn Fn(u64) -> Option<f32> + Send + Sync>;

/// Nominal wake-phrase span (ms) used to align `LevelWindow::peak_over_ms`
/// to the utterance rather than the full 1.6 s history buffer (#47).
pub(crate) const WAKE_PHRASE_SPAN_MS: u64 = 720;

/// Compute the lookback window (ms) for a keyword hit at `fed_ms` with
/// optional reporting `lag_ms`, clamped to `[LEVEL_BLOCK_MS, fed_ms]` when
/// `fed_ms > 0`.
pub(crate) fn wake_phrase_window_ms(fed_ms: u64, lag_ms: Option<u64>) -> u64 {
    let target = WAKE_PHRASE_SPAN_MS.saturating_add(lag_ms.unwrap_or(0));
    let clamped = if fed_ms > 0 {
        target.min(fed_ms)
    } else {
        target
    };
    clamped.max(super::wire::LEVEL_BLOCK_MS)
}

pub type SharedStatus = Arc<Mutex<KwsStatus>>;

/// Contract v2 §2.5 (#5, 2026-11-16): what the reader does when the spotted
/// word's intent is `retreat`. The firmware cuts its own audio and defers the
/// `Keyword` frame until `FlushDone` emits `LocalExitFlushed` (returning `None`
/// when deferred, or `Some(frame)` when nothing needed flushing so the caller
/// sends `Keyword` immediately).
pub type RetreatSink = Arc<dyn Fn(&str, u64, GatewayFrame) -> Option<GatewayFrame> + Send + Sync>;

/// Sidecar reload lifecycle facts for deferring `ConfigApply(Kws)` receipts
/// until the newly spawned `hm-kws` child actually reaches `ready` or fails.
#[derive(Debug, Clone)]
pub enum KwsLifecycleEvent {
    Ready {
        reload_gen: u64,
        load_ms: u64,
        threshold: Option<f32>,
        score: Option<f32>,
        loaded_hash: Option<String>,
        local_exit: u32,
    },
    Failed {
        reload_gen: u64,
        error_code: &'static str,
        detail: String,
    },
    /// Sidecar is currently not running (missing bundle or exited unexpectedly).
    /// Allows `VendorMonitor` to permit Core ASR wake fallback without killing
    /// or restarting `hm-gateway`.
    Unavailable,
}

pub type KwsLifecycleSink = Arc<dyn Fn(KwsLifecycleEvent) + Send + Sync>;

/// Contract v2 §2.5 (#5): may this hit be acted on locally? Yes only when
/// the word's intent is retreat AND the echo guard did not suspect the
/// microphone heard our own speakers. Pure, so the two-veto rule (meaning,
/// self-hearing) is testable without a model.
pub fn local_exit_for(
    keyword: &str,
    exit_words: &std::collections::HashSet<String>,
    suspected: bool,
) -> bool {
    !suspected && exit_words.contains(keyword.trim())
}

/// Contract v2 #4 (2026-11-16): the spotter's sensitivity, written by a
/// deploy into `kw.tuning.json` and handed to the launcher as
/// `KWS_THRESHOLD`/`KWS_SCORE` (run.sh reads both). Absent = the bundle's own
/// default (0.25/3.0). The APPLIED numbers come back from the model's `ready`
/// line, so the receipt never claims a value the spotter did not load.
pub fn load_tuning(dir: &std::path::Path) -> Option<(f32, f32)> {
    let raw = std::fs::read_to_string(dir.join("kw.tuning.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let t = v["threshold"].as_f64()? as f32;
    let s = v["score"].as_f64()? as f32;
    (t.is_finite() && s.is_finite()).then_some((t, s))
}

/// Install the sensitivity for the next spawn (atomic; a rejected deploy must
/// not change it). `None` removes any override, so the bundle default returns.
pub fn write_tuning(dir: &std::path::Path, tuning: Option<(f32, f32)>) -> Result<(), String> {
    let path = dir.join("kw.tuning.json");
    match tuning {
        None => {
            let _ = std::fs::remove_file(&path);
            Ok(())
        }
        Some((t, s)) => {
            let tmp = dir.join("kw.tuning.json.tmp");
            let doc = serde_json::json!({ "threshold": t, "score": s });
            std::fs::write(&tmp, doc.to_string())
                .map_err(|e| format!("write {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &path).map_err(|e| format!("install {}: {e}", path.display()))
        }
    }
}

/// Persist the active KWS bundle identity and generation (`kw.bundle.json`).
pub fn write_bundle_meta(
    dir: &std::path::Path,
    bundle_id: &str,
    generation: u64,
) -> Result<(), String> {
    let path = dir.join("kw.bundle.json");
    let tmp = dir.join("kw.bundle.json.tmp");
    let doc = serde_json::json!({
        "bundle_id": bundle_id,
        "generation": generation,
    });
    std::fs::write(&tmp, doc.to_string()).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("install {}: {e}", path.display()))
}

/// Load the persisted KWS bundle identity and generation from `kw.bundle.json`.
pub fn load_bundle_meta(dir: &std::path::Path) -> Option<(String, u64)> {
    let raw = std::fs::read_to_string(dir.join("kw.bundle.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let id = v["bundle_id"].as_str()?.to_string();
    let gen = v["generation"].as_u64()?;
    (!id.is_empty()).then_some((id, gen))
}

/// The local-exit vocabulary: the words whose intent is retreat, loaded from
/// `kw.exit.json` (written by a deploy, never by hand). Read at SPAWN — the
/// deploy restarts the sidecar, so a fresh process always sees the new set —
/// which keeps the reader thread free of shared mutable state.
///
/// A missing/empty file means "no local exit": every hit is then only a fact
/// north, exactly as before #5. An unreadable file is reported as no set
/// rather than failing closed: the CORE is still the authority either way.
fn load_exit_words(dir: &std::path::Path) -> std::collections::HashSet<String> {
    let path = dir.join("kw.exit.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return std::collections::HashSet::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        fwlog!("[kws] {} is not JSON — no local exit words", path.display());
        return std::collections::HashSet::new();
    };
    v["words"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|w| w.as_str())
                .filter(|w| !w.trim().is_empty())
                .map(|w| w.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// R1j/P1: control handle for deploying a new keyword file. Owns nothing
/// beyond the live child's pid: killing it makes the supervisor respawn with
/// the file the deploy just wrote. An INTENTIONAL kill is not a crash — it
/// must not bump `restarts` (that number is the failure budget).
///
/// 2026-11-14: `kill()` succeeding is not proof the process will die. A
/// muted device feeds no PCM, and a sidecar parked in `read()` (glibc
/// SA_RESTART + an EINTR branch that re-read) ignored SIGTERM forever, so
/// `reflex.deploy` was accepted and silently never applied. `signalled_at`
/// is the stamp the supervisor uses to escalate to SIGKILL, making the
/// deploy contract independent of the sidecar's own stop handling.
#[derive(Clone)]
pub struct KwsControl {
    pub dir: PathBuf,
    pid: Arc<AtomicU64>,
    intentional: Arc<std::sync::atomic::AtomicBool>,
    signalled_at: Arc<AtomicU64>,
    reload_gen: Arc<AtomicU64>,
    wake: Arc<(Mutex<u64>, std::sync::Condvar)>,
}

impl KwsControl {
    /// Ask the live sidecar to exit. Returns true when a process was
    /// signalled (false = nothing was running, the file still applies on the
    /// next start).
    pub fn restart(&self) -> bool {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid == 0 {
            return false;
        }
        self.intentional.store(true, Ordering::SeqCst);
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            return false;
        }
        self.signalled_at.store(now_ms(), Ordering::SeqCst);
        true
    }

    /// Advance the reload generation, wake any backoff wait immediately, and
    /// signal any running sidecar to restart. Returns `(target_gen, signalled)`.
    pub fn request_reload(&self) -> (u64, bool) {
        let target = self.reload_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let (lock, cv) = &*self.wake;
        if let Ok(mut g) = lock.lock() {
            *g = target;
            cv.notify_all();
        }
        let signalled = self.restart();
        (target, signalled)
    }
}

/// Lock-free tri-state for sidecar neural VAD (`0 = None/unknown`,
/// `1 = Some(false)`, `2 = Some(true)`). Reset to `None` whenever the sidecar
/// exits or restarts so `hm-core` seamlessly falls back to local/energy VAD.
const VAD_UNKNOWN: u8 = 0;
const VAD_SILENCE: u8 = 1;
const VAD_SPEECH: u8 = 2;

fn decode_vad_state(raw: u8) -> Option<bool> {
    match raw {
        VAD_SILENCE => Some(false),
        VAD_SPEECH => Some(true),
        _ => None,
    }
}

/// Handle the audio thread uses to push blocks at the spotter.
pub struct KwsFeed {
    tx: Arc<Mutex<Option<SyncSender<Vec<u8>>>>>,
    status: SharedStatus,
    control: KwsControl,
    vad_state: Arc<AtomicU8>,
}

impl KwsFeed {
    /// Non-blocking: drops (and counts) when the queue is full or no sidecar
    /// is running. Never blocks the capture thread on the spotter.
    pub fn feed(&self, block: &[i16]) {
        let mut bytes = Vec::with_capacity(block.len() * 2);
        for s in block {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        let outcome = {
            let guard = self.tx.lock().unwrap();
            match guard.as_ref() {
                Some(tx) => tx.try_send(bytes).map_err(|e| match e {
                    TrySendError::Full(_) => "full",
                    TrySendError::Disconnected(_) => "disconnected",
                }),
                None => Err("no sidecar"),
            }
        };
        if outcome.is_err() {
            if let Ok(mut st) = self.status.lock() {
                st.dropped_blocks += 1;
            }
        }
    }

    pub fn status(&self) -> SharedStatus {
        self.status.clone()
    }

    /// R1j/P1: the deploy handle (restart the spotter after a new file).
    pub fn control(&self) -> KwsControl {
        self.control.clone()
    }

    /// Latest neural VAD decision from the sidecar (`None` when the sidecar
    /// has no `--vad-model` or is currently restarting/down).
    pub fn vad_speech(&self) -> Option<bool> {
        decode_vad_state(self.vad_state.load(Ordering::Acquire))
    }
}

/// Optional XiaoAi-style L1+L2 acoustic telemetry emitted by `hm-kws` (#69).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeywordMeta {
    pub tier: Option<String>,
    pub wake_dur_ms: Option<u64>,
    pub max_gap_ms: Option<u64>,
    pub l2_passed: Option<bool>,
}

/// One parsed stdout event from the driver (JSON lines, see kws/driver.c).
#[derive(Debug, Clone, PartialEq)]
pub enum DriverEvent {
    Ready {
        load_ms: u64,
        /// Contract v2 #4: the driver prints the values it actually loaded.
        threshold: Option<f32>,
        score: Option<f32>,
    },
    Keyword {
        keyword: String,
        fed_ms: u64,
        lag_ms: Option<u64>,
        meta: KeywordMeta,
    },
    /// Stage 2 remote neural VAD event from `hm-kws` (`--vad-model`).
    Vad {
        speech: bool,
        prob: Option<f32>,
    },
    Exit {
        fed_ms: u64,
        detections: u64,
    },
    Error {
        stage: String,
    },
    Unknown,
}

/// Parse one driver stdout line. Kept pure so the contract is unit-tested
/// without a device or a model.
pub fn parse_event_line(line: &str) -> DriverEvent {
    let line = line.trim();
    if !line.starts_with('{') {
        return DriverEvent::Unknown;
    }
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return DriverEvent::Unknown,
    };
    match v["event"].as_str().unwrap_or("") {
        "ready" => DriverEvent::Ready {
            load_ms: v["load_ms"].as_u64().unwrap_or(0),
            threshold: v["threshold"].as_f64().map(|f| f as f32),
            score: v["score"].as_f64().map(|f| f as f32),
        },
        "keyword" => DriverEvent::Keyword {
            keyword: v["keyword"].as_str().unwrap_or("").to_string(),
            fed_ms: v["fed_ms"].as_u64().unwrap_or(0),
            // v0.2.9: the driver has always printed it; nobody parsed it, so
            // the north frame could not say when the phrase's last audio
            // arrived (only where in the stream it was). None when a driver
            // build predates the field — never a fabricated 0.
            lag_ms: v["lag_ms"].as_u64(),
            meta: KeywordMeta {
                tier: v["tier"].as_str().map(str::to_string),
                wake_dur_ms: v["wake_dur_ms"].as_u64(),
                max_gap_ms: v["max_gap_ms"].as_u64(),
                l2_passed: v["l2_passed"].as_bool(),
            },
        },
        "vad" => match v["speech"].as_bool() {
            Some(speech) => DriverEvent::Vad {
                speech,
                prob: v["prob"]
                    .as_f64()
                    .map(|f| f as f32)
                    .filter(|f| f.is_finite()),
            },
            None => DriverEvent::Unknown,
        },
        "exit" => DriverEvent::Exit {
            fed_ms: v["fed_ms"]
                .as_u64()
                .unwrap_or_else(|| v["fed_samples"].as_u64().unwrap_or(0) / 16),
            detections: v["detections"].as_u64().unwrap_or(0),
        },
        "error" => DriverEvent::Error {
            stage: v["stage"].as_str().unwrap_or("unknown").to_string(),
        },
        _ => DriverEvent::Unknown,
    }
}

pub(super) type HitAdmission = Arc<dyn Fn(&str, bool, bool) -> bool + Send + Sync>;

/// Spawn the supervised sidecar. Returns the feed handle (always — the
/// audio path doesn't care whether a spotter is running). `guard` decides
/// per hit whether the microphone was hearing the speakers (self-heard
/// hits are suppressed and counted, never reported as facts).
pub fn spawn(
    dir: PathBuf,
    cpu: usize,
    device: String,
    north: NorthSink,
    guard: Option<Guard>,
    level: Option<LevelSource>,
    retreat: Option<RetreatSink>,
) -> KwsFeed {
    spawn_with_admission(
        dir, cpu, device, north, guard, level, retreat, None, None, None,
    )
}

/// Reconcile volatile tmpfs `kw.bundle.json` metadata with the current status
/// and the flash-persisted `config_journal.json` floor so `bundle_generation`
/// never regresses below the last persisted KWS generation across reboots or
/// tmpfs wipes.
fn reconcile_bundle_meta(
    current_id: &str,
    current_gen: u64,
    file_meta: Option<(String, u64)>,
    bundle_floor: Option<&(Option<String>, u64)>,
) -> (String, u64) {
    let (floor_id, floor_gen) = bundle_floor
        .map(|(id, g)| (id.as_deref(), *g))
        .unwrap_or((None, 0));
    let min_gen = current_gen.max(floor_gen);
    match file_meta {
        Some((fid, fgen)) if fgen >= min_gen => (fid, fgen),
        Some((fid, _)) => {
            let id = if !current_id.is_empty() {
                current_id.to_string()
            } else {
                floor_id.map(ToOwned::to_owned).unwrap_or(fid)
            };
            (id, min_gen)
        }
        None => {
            let id = if !current_id.is_empty() {
                current_id.to_string()
            } else {
                floor_id.unwrap_or_default().to_string()
            };
            (id, min_gen)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_with_admission(
    dir: PathBuf,
    cpu: usize,
    device: String,
    north: NorthSink,
    guard: Option<Guard>,
    level: Option<LevelSource>,
    retreat: Option<RetreatSink>,
    admit: Option<HitAdmission>,
    lifecycle: Option<KwsLifecycleSink>,
    bundle_floor: Option<(Option<String>, u64)>,
) -> KwsFeed {
    let (init_id, init_gen) =
        reconcile_bundle_meta("", 0, load_bundle_meta(&dir), bundle_floor.as_ref());
    let status: SharedStatus = Arc::new(Mutex::new(KwsStatus {
        state: "starting".into(),
        epoch_ms: now_ms(),
        bundle_id: init_id,
        bundle_generation: init_gen,
        ..Default::default()
    }));
    let tx: Arc<Mutex<Option<SyncSender<Vec<u8>>>>> = Arc::new(Mutex::new(None));
    let pid = Arc::new(AtomicU64::new(0));
    let intentional = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signalled_at = Arc::new(AtomicU64::new(0));
    let reload_gen = Arc::new(AtomicU64::new(0));
    let wake = Arc::new((Mutex::new(0u64), std::sync::Condvar::new()));
    let vad_state = Arc::new(AtomicU8::new(VAD_UNKNOWN));
    let control = KwsControl {
        dir: dir.clone(),
        pid: pid.clone(),
        intentional: intentional.clone(),
        signalled_at: signalled_at.clone(),
        reload_gen: reload_gen.clone(),
        wake: wake.clone(),
    };
    let feed = KwsFeed {
        tx: tx.clone(),
        status: status.clone(),
        control,
        vad_state: vad_state.clone(),
    };

    let script = dir.join("run.sh");
    let pid_sup = pid.clone();
    let intentional_sup = intentional.clone();
    let signalled_sup = signalled_at.clone();
    std::thread::Builder::new()
        .name("kws-supervisor".into())
        .spawn(move || {
            supervise(
                script,
                dir,
                cpu,
                device,
                north,
                guard,
                level,
                retreat,
                admit,
                lifecycle,
                status,
                tx,
                pid_sup,
                intentional_sup,
                signalled_sup,
                reload_gen,
                wake,
                vad_state,
                bundle_floor,
            )
        })
        .expect("spawn kws supervisor");
    feed
}

/// Where keyword/status frames go. The firmware's north channel is
/// per-connection and lives in `Shared`; this indirection keeps the KWS
/// module independent of that lock while still following reconnects.
pub type NorthSink = Arc<dyn Fn(GatewayFrame) + Send + Sync>;

/// Is the sidecar bundle actually there? Uses `access(2)` on purpose: musl's
/// `Path::exists()`/`metadata()` go through `statx` (397 on this kernel),
/// which the P0 fix removed everywhere else — and a failed
/// `Command::spawn` is exactly what produced one trap dump per retry
/// (R0 gateway §5). With this check a missing bundle costs no syscall the
/// kernel does not know.
pub(super) fn script_ready(script: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(script.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

fn wait_or_wake(wake: &(Mutex<u64>, std::sync::Condvar), seen_gen: u64, dur: std::time::Duration) {
    let (lock, cv) = wake;
    if let Ok(guard) = lock.lock() {
        let _ = cv.wait_timeout_while(guard, dur, |g| *g <= seen_gen);
    }
}

#[allow(clippy::too_many_arguments)]
fn supervise(
    script: PathBuf,
    dir: PathBuf,
    cpu: usize,
    device: String,
    north: NorthSink,
    guard: Option<Guard>,
    level: Option<LevelSource>,
    retreat: Option<RetreatSink>,
    admit: Option<HitAdmission>,
    lifecycle: Option<KwsLifecycleSink>,
    status: SharedStatus,
    tx: Arc<Mutex<Option<SyncSender<Vec<u8>>>>>,
    pid: Arc<AtomicU64>,
    intentional: Arc<std::sync::atomic::AtomicBool>,
    signalled_at: Arc<AtomicU64>,
    reload_gen: Arc<AtomicU64>,
    wake: Arc<(Mutex<u64>, std::sync::Condvar)>,
    vad_state: Arc<AtomicU8>,
    bundle_floor: Option<(Option<String>, u64)>,
) {
    let mut attempt = 0usize;
    let mut reported_off = false;
    loop {
        vad_state.store(VAD_UNKNOWN, Ordering::Release);
        let spawn_gen = reload_gen.load(Ordering::SeqCst);
        // Missing bundle: report once, then poll cheaply. Never call spawn —
        // it would statx per retry and flood the trap log (R0 finding).
        if !script_ready(&script) {
            if let Some(lc) = &lifecycle {
                if spawn_gen > 0 {
                    lc(KwsLifecycleEvent::Failed {
                        reload_gen: spawn_gen,
                        error_code: "kws_sidecar_unavailable",
                        detail: format!("keyword sidecar not installed: {}", script.display()),
                    });
                } else {
                    lc(KwsLifecycleEvent::Unavailable);
                }
            }
            if !reported_off {
                set_status(
                    &status,
                    "off",
                    format!("keyword sidecar not installed: {} (off)", script.display()),
                    false,
                );
                north(status_frame(&device, &status));
                fwlog!(
                    "[kws] {} missing — keyword interruption off",
                    script.display()
                );
                reported_off = true;
            }
            wait_or_wake(
                &wake,
                spawn_gen,
                std::time::Duration::from_secs(KWS_MISSING_POLL_SECS),
            );
            continue;
        }
        reported_off = false;
        let started = std::time::Instant::now();
        // Re-read bundle metadata at every spawn so a hot-downloaded KWS
        // archive (with restored `kw.bundle.json`) picks up its persisted
        // identity/generation, while never regressing below the flash floor.
        {
            let file_meta = load_bundle_meta(&dir);
            let mut st = status.lock().unwrap();
            let (id, gen) = reconcile_bundle_meta(
                &st.bundle_id,
                st.bundle_generation,
                file_meta,
                bundle_floor.as_ref(),
            );
            st.bundle_id = id;
            st.bundle_generation = gen;
        }
        // M6b: hash the keyword file AT SPAWN. The deploy path renames the
        // new file into place before killing the child, so the bytes hashed
        // here are exactly the bytes the sidecar opens; the `ready` line is
        // what turns this from "the file" into "the spotter loaded it".
        let spawn_hash = read_file_hash(&dir.join("kw.txt"));
        // #5: the words this process may act on WITHOUT the core. Loaded at
        // spawn for the same reason the hash is: the deploy restarts us.
        let exit_words = load_exit_words(&dir);
        // #4: the spotter's sensitivity for THIS spawn. run.sh already reads
        // KWS_THRESHOLD/KWS_SCORE; a missing file keeps the bundle default.
        let tuning = load_tuning(&dir);
        {
            let mut st = status.lock().unwrap();
            if let Some((t, s)) = tuning {
                st.threshold = t;
                st.score = s;
            }
        }
        {
            // The armed count is a device fact (it survives a core restart);
            // it rides every status report so the core never has to remember
            // it and the console never reports a false "not armed".
            let mut st = status.lock().unwrap();
            st.local_exit = exit_words.len() as u32;
        }
        if !exit_words.is_empty() {
            fwlog!(
                "[kws] local exit armed for {} word(s): {}",
                exit_words.len(),
                exit_words.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        let mut cmd = Command::new(&script);
        cmd.current_dir(&dir).env("KWS_CPU", cpu.to_string());
        if let Some((t, s)) = tuning {
            cmd.env("KWS_THRESHOLD", format!("{t:.3}"))
                .env("KWS_SCORE", format!("{s:.2}"));
        }
        let child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child: Child = match child {
            Ok(c) => c,
            Err(e) => {
                let detail = format!("spawn {}: {e}", script.display());
                set_status(&status, "down", detail.clone(), false);
                north(status_frame(&device, &status));
                if let Some(lc) = &lifecycle {
                    lc(KwsLifecycleEvent::Failed {
                        reload_gen: spawn_gen,
                        error_code: "kws_spawn_failed",
                        detail,
                    });
                }
                sleep_backoff(&wake, spawn_gen, attempt);
                attempt += 1;
                continue;
            }
        };
        let child_pid = child.id() as u64;
        pid.store(child_pid, Ordering::SeqCst);
        // Fresh process, no stop request outstanding yet.
        signalled_at.store(0, Ordering::SeqCst);
        // Close the TOCTOU window: if a newer ConfigApply(Kws) landed while we
        // were spawning, terminate this stale child right away and respawn.
        if reload_gen.load(Ordering::SeqCst) > spawn_gen {
            intentional.store(true, Ordering::SeqCst);
            if unsafe { libc::kill(child_pid as i32, libc::SIGTERM) } == 0 {
                signalled_at.store(now_ms(), Ordering::SeqCst);
            }
        }
        // Fresh queue per process: stale blocks from a previous run would
        // only waste the new model's first chunk.
        let (ntx, nrx) = sync_channel::<Vec<u8>>(QUEUE_BLOCKS);
        *tx.lock().unwrap() = Some(ntx);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let writer = std::thread::Builder::new()
            .name("kws-writer".into())
            .spawn(move || pump_stdin(stdin, nrx))
            .ok();
        let device_r = device.clone();
        let north_r = north.clone();
        let status_r = status.clone();
        let guard_r = guard.clone();
        let level_r = level.clone();
        let hash_r = spawn_hash.clone();
        let retreat_r = retreat.clone();
        let admit_r = admit.clone();
        let lifecycle_r = lifecycle.clone();
        let exit_r = exit_words.clone();
        let vad_r = vad_state.clone();
        let saw_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_ready_r = saw_ready.clone();
        let reader = std::thread::Builder::new()
            .name("kws-reader".into())
            .spawn(move || {
                if let Some(out) = stdout {
                    read_events(
                        out,
                        &device_r,
                        &north_r,
                        &guard_r,
                        &level_r,
                        &retreat_r,
                        &admit_r,
                        &lifecycle_r,
                        &exit_r,
                        &status_r,
                        hash_r,
                        &vad_r,
                        spawn_gen,
                        &saw_ready_r,
                    );
                }
            })
            .ok();

        let exit = reap(&mut child, &pid, &signalled_at);
        vad_state.store(VAD_UNKNOWN, Ordering::Release);
        *tx.lock().unwrap() = None;
        if let Some(w) = writer {
            let _ = w.join();
        }
        if let Some(r) = reader {
            let _ = r.join();
        }
        pid.store(0, Ordering::SeqCst);
        signalled_at.store(0, Ordering::SeqCst);
        // M6b: the process that loaded that vocabulary is gone; keeping the
        // hash would claim a running spotter that is not there.
        {
            let mut st = status.lock().unwrap();
            st.loaded_hash.clear();
            st.loaded_ms = 0;
        }
        let ran = started.elapsed().as_secs();
        let was_intentional = intentional.swap(false, Ordering::SeqCst);
        let latest_gen = reload_gen.load(Ordering::SeqCst);
        // R1j/P1: a deploy restart is deliberate. It is NOT a sidecar
        // failure, so it must not spend the failure budget (RJ-4 counts
        // restarts as zero) — and it restarts immediately, no backoff.
        if was_intentional || latest_gen > spawn_gen {
            set_status(
                &status,
                "starting",
                "restarting for keyword deploy".into(),
                false,
            );
            north(status_frame(&device, &status));
            attempt = 0;
            continue;
        }
        let exit_detail = match &exit {
            Ok(st) => format!("sidecar exited ({st}) after {ran}s — restarting"),
            Err(e) => format!("wait failed: {e} — restarting"),
        };
        if let Some(lc) = &lifecycle {
            if !saw_ready.load(Ordering::Acquire) {
                lc(KwsLifecycleEvent::Failed {
                    reload_gen: spawn_gen,
                    error_code: "kws_sidecar_exited",
                    detail: exit_detail.clone(),
                });
            } else {
                lc(KwsLifecycleEvent::Unavailable);
            }
        }
        set_status(&status, "down", exit_detail, true);
        north(status_frame(&device, &status));
        if ran >= HEALTHY_SECS {
            attempt = 0;
        }
        sleep_backoff(&wake, spawn_gen, attempt);
        attempt += 1;
    }
}

/// Wait for the sidecar to exit, escalating a stop request to SIGKILL when
/// the process does not honour SIGTERM within [`KWS_KILL_GRACE_MS`].
///
/// Why not `child.wait()`: a deploy signals the child and expects the
/// supervisor to come back around the loop with the new keyword file. If the
/// child cannot be stopped (muted device → no PCM → the driver parked in a
/// restarted read()), an unconditional wait leaves the device on the old
/// vocabulary forever while the management side was told "spotter
/// restarting". The escalation makes that impossible: the vocabulary the
/// deploy wrote IS what runs after the deploy, whatever the sidecar does.
fn reap(
    child: &mut Child,
    pid: &AtomicU64,
    signalled_at: &AtomicU64,
) -> std::io::Result<std::process::ExitStatus> {
    let mut escalated = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        if !escalated {
            let at = signalled_at.load(Ordering::SeqCst);
            if at != 0 && now_ms().saturating_sub(at) >= KWS_KILL_GRACE_MS {
                let p = pid.load(Ordering::SeqCst);
                if p != 0 {
                    fwlog!(
                        "[kws] sidecar ignored SIGTERM for {}ms — SIGKILL ({p})",
                        KWS_KILL_GRACE_MS
                    );
                    unsafe { libc::kill(p as i32, libc::SIGKILL) };
                    escalated = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(KWS_WAIT_POLL_MS));
    }
}

fn sleep_backoff(wake: &(Mutex<u64>, std::sync::Condvar), seen_gen: u64, attempt: usize) {
    let secs = BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)];
    fwlog!("[kws] supervisor backoff {secs}s (attempt {})", attempt + 1);
    wait_or_wake(wake, seen_gen, std::time::Duration::from_secs(secs));
}

fn set_status(status: &SharedStatus, state: &str, detail: String, restart: bool) {
    let mut st = status.lock().unwrap();
    st.state = state.to_string();
    st.detail = detail;
    if restart {
        st.restarts += 1;
    }
}

fn status_frame(device: &str, status: &SharedStatus) -> GatewayFrame {
    let st = status.lock().unwrap();
    GatewayFrame::KwsReport {
        device: device.to_string(),
        state: st.state.clone(),
        detail: st.detail.clone(),
        restarts: st.restarts,
        detections: st.detections,
        // `guarded` was missing from this frame (it only lived in as_json),
        // so the core could never see the echo guard's work (P7).
        guarded: st.guarded,
        epoch: st.epoch_ms,
        loaded_hash: st.loaded_hash.clone(),
        loaded_ms: st.loaded_ms,
        local_exit: st.local_exit,
        threshold: st.threshold,
        score: st.score,
    }
}

/// Drain queued blocks into the child's stdin. Ends on write error (child
/// gone) or when the supervisor drops the sender.
fn pump_stdin(stdin: Option<std::process::ChildStdin>, rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let Some(mut stdin) = stdin else { return };
    for block in rx {
        if stdin.write_all(&block).is_err() {
            return;
        }
    }
}

/// Read driver events until EOF, reporting keyword hits north. A hit the
/// echo guard flags as self-heard (D/T5, RUI Local defense ①) is counted
/// and logged — never sent north: the core cannot tell a self-heard hit
/// from a real one, which is exactly why the physical guard lives here.
#[allow(clippy::too_many_arguments)]
fn read_events(
    stdout: std::process::ChildStdout,
    device: &str,
    north: &NorthSink,
    guard: &Option<Guard>,
    level: &Option<LevelSource>,
    retreat: &Option<RetreatSink>,
    admit: &Option<HitAdmission>,
    lifecycle: &Option<KwsLifecycleSink>,
    exit_words: &std::collections::HashSet<String>,
    status: &SharedStatus,
    spawn_hash: Option<String>,
    vad_state: &AtomicU8,
    spawn_gen: u64,
    saw_ready: &std::sync::atomic::AtomicBool,
) {
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let Ok(line) = line else { return };
        match parse_event_line(&line) {
            DriverEvent::Ready {
                load_ms,
                threshold,
                score,
            } => {
                saw_ready.store(true, Ordering::Release);
                let mut st = status.lock().unwrap();
                st.state = "running".into();
                // #4: the model's OWN numbers are the applied fact.
                if let Some(v) = threshold {
                    st.threshold = v;
                }
                if let Some(v) = score {
                    st.score = v;
                }
                st.detail = match (threshold, score) {
                    (Some(t), Some(s)) => {
                        format!("ready in {load_ms}ms (threshold={t:.2} score={s:.2})")
                    }
                    _ => format!("ready in {load_ms}ms"),
                };
                // M6b: the child opened the file that was on disk at spawn;
                // only NOW is "it loaded this vocabulary" a fact.
                if let Some(h) = spawn_hash.clone() {
                    st.loaded_hash = h;
                    st.loaded_ms = now_ms();
                }
                drop(st);
                fwlog!("[kws] sidecar ready in {load_ms}ms");
                north(status_frame(device, status));
                if let Some(lc) = lifecycle {
                    lc(KwsLifecycleEvent::Ready {
                        reload_gen: spawn_gen,
                        load_ms,
                        threshold,
                        score,
                        loaded_hash: spawn_hash.clone(),
                        local_exit: exit_words.len() as u32,
                    });
                }
            }
            DriverEvent::Keyword {
                keyword,
                fed_ms,
                lag_ms,
                meta,
            } => {
                // R1g: the guard's judgement rides WITH the fact instead of
                // eating it. Suppressing here made the wake word unusable
                // during our own playback (the only interruption path on a
                // half-duplex link), and the guard cannot know whether the
                // phrase came from our answer's text or from a person.
                let suspected = guard.as_ref().map(|g| g()).unwrap_or(false);
                // v0.2.9 / #47: the level is read HERE, after the model fired,
                // over the wake phrase span aligned to `fed_ms` / `lag_ms`
                // rather than the full unaligned 1.6 s history buffer.
                let window_ms = wake_phrase_window_ms(fed_ms, lag_ms);
                let rms = level.as_ref().and_then(|l| l(window_ms));
                let hit_ms = now_ms();
                if admit.as_ref().is_some_and(|gate| {
                    !gate(&keyword, suspected, exit_words.contains(keyword.trim()))
                }) {
                    continue;
                }
                if suspected {
                    let mut st = status.lock().unwrap();
                    st.detail = format!("guarded hit '{keyword}' reported with self_heard=true");
                    drop(st);
                    fwlog!("[kws] guard suspects a self-heard hit '{keyword}' (fed {fed_ms}ms) — reported as a hint");
                }
                let kw_frame = north_keyword(
                    device, &keyword, fed_ms, hit_ms, status, suspected, rms, lag_ms, &meta,
                );
                fwlog!(
                    "[kws] HIT {} tier={} rms={} lag={}",
                    keyword,
                    meta.tier.as_deref().unwrap_or("legacy"),
                    rms.map(|d| format!("{d:.1}dBFS"))
                        .unwrap_or_else(|| "n/a".into()),
                    lag_ms
                        .map(|l| format!("{l}ms"))
                        .unwrap_or_else(|| "n/a".into())
                );
                // Contract v2 §2.5 (#5, 2026-11-16): a RETREAT word the guard
                // did NOT call self-heard cuts this device's audio now and
                // defers sending `Keyword` until `FlushDone` emits
                // `LocalExitFlushed` so Core observes the local flush strictly
                // BEFORE `Keyword` closes the session.
                let send_now = if local_exit_for(&keyword, exit_words, suspected) {
                    match retreat {
                        Some(r) => match r(&keyword, fed_ms, kw_frame) {
                            None => {
                                let mut st = status.lock().unwrap();
                                st.detail = format!("local exit: '{keyword}' flushed the audio");
                                drop(st);
                                fwlog!("[kws] HIT {keyword} is a retreat word — local exit running (keyword deferred until flush)");
                                None
                            }
                            Some(frame) => {
                                fwlog!(
                                    "[kws] HIT {keyword} is a retreat word — nothing to flush locally, reporting north"
                                );
                                Some(frame)
                            }
                        },
                        None => Some(kw_frame),
                    }
                } else {
                    Some(kw_frame)
                };
                if let Some(frame) = send_now {
                    north(frame);
                }
            }
            DriverEvent::Vad { speech, .. } => {
                vad_state.store(
                    if speech { VAD_SPEECH } else { VAD_SILENCE },
                    Ordering::Release,
                );
            }
            DriverEvent::Exit { fed_ms, detections } => {
                vad_state.store(VAD_UNKNOWN, Ordering::Release);
                fwlog!(
                    "[kws] sidecar closed: fed={}ms detections={detections}",
                    fed_ms
                );
            }
            DriverEvent::Error { stage } => {
                vad_state.store(VAD_UNKNOWN, Ordering::Release);
                let detail = format!("sidecar error at {stage}");
                set_status(status, "down", detail.clone(), false);
                north(status_frame(device, status));
                if !saw_ready.load(Ordering::Acquire) {
                    if let Some(lc) = lifecycle {
                        lc(KwsLifecycleEvent::Failed {
                            reload_gen: spawn_gen,
                            error_code: "kws_sidecar_error",
                            detail,
                        });
                    }
                }
            }
            DriverEvent::Unknown => {}
        }
    }
    vad_state.store(VAD_UNKNOWN, Ordering::Release);
}

/// Build the northbound keyword fact and bump the counters. Split out so the
/// frame shape is testable.
#[allow(clippy::too_many_arguments)]
pub fn north_keyword(
    device: &str,
    keyword: &str,
    fed_ms: u64,
    wall_ms: u64,
    status: &SharedStatus,
    self_heard: bool,
    rms_dbfs: Option<f32>,
    lag_ms: Option<u64>,
    meta: &KeywordMeta,
) -> GatewayFrame {
    let (bundle_id, bundle_generation) = {
        let mut st = status.lock().unwrap();
        st.detections += 1;
        if self_heard {
            // Counted as a detection AND as a guarded one: `guarded` is the
            // subset the echo guard was suspicious about, never a separate
            // silent bucket (P7: the two numbers must add up).
            st.guarded += 1;
        }
        st.last_keyword_ms = wall_ms;
        st.detail = format!(
            "hit '{keyword}' at {fed_ms}ms of stream{}",
            if self_heard {
                " (guard: possibly self-heard)"
            } else {
                ""
            }
        );
        (st.bundle_id.clone(), st.bundle_generation)
    };
    GatewayFrame::Keyword {
        device: device.to_string(),
        ts: wall_ms,
        seq: 0,
        keyword: keyword.to_string(),
        fed_ms: Some(fed_ms),
        self_heard,
        rms_dbfs,
        lag_ms,
        bundle_id: (!bundle_id.is_empty()).then_some(bundle_id),
        kws_generation: (bundle_generation > 0).then_some(bundle_generation),
        kws_tier: meta.tier.clone(),
        wake_dur_ms: meta.wake_dur_ms,
        max_gap_ms: meta.max_gap_ms,
        l2_passed: meta.l2_passed,
    }
}

/// FNV-1a of a file's bytes (same function as the core/deploy path, so the
/// two hashes are comparable). `None` when the file cannot be read — an
/// unreadable vocabulary is reported as "no load proof", never as a match.
fn read_file_hash(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(super::fnv1a_hex(&bytes))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Monotonic milliseconds without the armv7 time64 syscall path. See the
/// firmware's time note: musl's `clock_gettime` tries `clock_gettime64`
/// (syscall 403) first, which this 3.18 kernel does not have — every call
/// costs a kernel warning on the serial console (measured 2/s, a printk
/// flood that also delays the watchdog kicker). Used by call sites that
/// only need a monotonic tick.
pub fn mono_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // arm EABI: __NR_clock_gettime = 263 (armv7); on other targets fall back
    // to the libc wrapper (aarch64 has no time64 split).
    #[cfg(target_arch = "arm")]
    let rc = unsafe { libc::syscall(263, libc::CLOCK_MONOTONIC, &mut ts as *mut libc::timespec) };
    #[cfg(not(target_arch = "arm"))]
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1000 + (ts.tv_nsec as u64) / 1_000_000
}

/// Count dropped blocks without a status lock per block (atomic-free path:
/// only the error path takes the lock).
pub fn count_drop(status: &SharedStatus) {
    if let Ok(mut st) = status.lock() {
        st.dropped_blocks += 1;
    }
}

/// Sequence helper for north frames (the firmware's shared counter).
pub fn next_seq(seq: &AtomicU64) -> u64 {
    seq.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(test)]
#[path = "kws_tests.rs"]
mod tests;
