//! Final full-system audit regression tests:
//! - P1-1: `ModeReport` emitted after `HelloAck` AND after `ConfigApply(Duplex)`
//! - P1-2: `ConfigApply(Kws)` defers `Active` until restarted sidecar emits `ready`,
//!   emits `Failed` on sidecar error/exit or monotonic reload timeout (`kws_reload_timeout`),
//!   and settles overlapping requests with `Superseded` (`superseded_by_newer_kws`)
//! - P1-3: Retreat `Keyword` emits `LocalExitFlushed` strictly BEFORE `Keyword`
//!   when playback is active, and passes through immediately when idle.

use crate::support::*;
use home_mate_device_gateway::firmware::{FirmwareConfig, KWS_RELOAD_TIMEOUT_MS};
use home_mate_device_gateway::protocol::{
    ConfigBlock, ConfigReceiptStatus, CoreFrame, EchoPathHealth, GatewayFrame, PROTOCOL_VERSION,
};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn handshake_v2(core: &FakeCore) {
    let hello = core.recv().await;
    assert!(
        matches!(hello, GatewayFrame::Hello { protocol, .. } if protocol == PROTOCOL_VERSION),
        "expected Hello v{PROTOCOL_VERSION}, got {hello:?}"
    );
    core.send(&CoreFrame::HelloAck {
        protocol: 2,
        min_protocol: 0,
        supported_versions: vec![0, 2],
        capabilities: vec!["config_apply_v2".into()],
        core_version: "test-v2".into(),
        generation: 1,
    })
    .await;
}

#[tokio::test]
async fn mode_report_emitted_after_hello_ack_and_config_apply_duplex() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("audit-mode-report");
    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: sd,
            aec: true,
            boot_health: false,
            ..Default::default()
        },
        Arc::new(hw),
    );
    handshake_v2(&core).await;

    // 1. Immediately after HelloAck (without any CoreFrame::ModeSet), gateway
    //    must emit ModeReport carrying actual duplex and AEC ownership state.
    let boot_mode = core
        .until(|f| matches!(f, GatewayFrame::ModeReport { .. }))
        .await;
    let GatewayFrame::ModeReport {
        device,
        duplex,
        aec,
    } = boot_mode
    else {
        unreachable!()
    };
    assert_eq!(device, "dot2-d");
    assert_eq!(duplex.capability, "half");
    assert_eq!(duplex.requested, "half");
    assert_eq!(duplex.negotiated, "half");
    assert_eq!(duplex.effective, "half");
    assert_eq!(aec.owner, "device");
    assert_eq!(aec.reference, "absent");

    // 2. ConfigApply(Duplex, full) -> Unsupported receipt + updated ModeReport.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-dup-1".into(),
        operation_id: "op-dup-full".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Duplex,
        schema_version: 1,
        block_hash: "hash-dup-full".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({ "duplex_requested": "full" }),
    })
    .await;
    let mut saw_receipt = false;
    let mut saw_mode = false;
    while !(saw_receipt && saw_mode) {
        let f = core.recv().await;
        match f {
            GatewayFrame::ConfigReceipt {
                operation_id,
                status,
                error_code,
                ..
            } if operation_id == "op-dup-full" => {
                assert_eq!(status, ConfigReceiptStatus::Unsupported);
                assert_eq!(error_code.as_deref(), Some("full_duplex_unsupported"));
                saw_receipt = true;
            }
            GatewayFrame::ModeReport { duplex, aec, .. } if duplex.requested == "full" => {
                assert_eq!(duplex.capability, "half");
                assert_eq!(duplex.negotiated, "half");
                assert_eq!(duplex.effective, "half");
                assert_eq!(aec.owner, "device");
                saw_mode = true;
            }
            _ => {}
        }
    }

    // 3. ConfigApply(Duplex, half) -> Active receipt + updated ModeReport.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-dup-2".into(),
        operation_id: "op-dup-half".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Duplex,
        schema_version: 1,
        block_hash: "hash-dup-half".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({ "duplex_requested": "half" }),
    })
    .await;
    let mut saw_half_receipt = false;
    let mut saw_half_mode = false;
    while !(saw_half_receipt && saw_half_mode) {
        let f = core.recv().await;
        match f {
            GatewayFrame::ConfigReceipt {
                operation_id,
                status,
                ..
            } if operation_id == "op-dup-half" => {
                assert_eq!(status, ConfigReceiptStatus::Active);
                saw_half_receipt = true;
            }
            GatewayFrame::ModeReport { duplex, .. } if duplex.requested == "half" => {
                assert_eq!(duplex.effective, "half");
                saw_half_mode = true;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn config_apply_kws_defers_active_until_sidecar_ready_and_reports_failed_on_crash() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("audit-kws-defer");
    let kws_dir = sd.join("kws");
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model/tokens.txt"),
        "n 0\ni 1\nh 2\nao 3\nt 4\ning 5\n",
    )
    .unwrap();
    let run_sh = kws_dir.join("run.sh");
    std::fs::write(
        &run_sh,
        "#!/bin/sh\n\
         d=$(dirname \"$0\")\n\
         if [ ! -f \"$d/kw.txt\" ]; then\n\
           echo '{\"event\":\"ready\",\"load_ms\":1,\"threshold\":0.25,\"score\":3.0}'\n\
           exec sleep 30\n\
         fi\n\
         if [ -f \"$d/fail.flag\" ]; then\n\
           echo '{\"event\":\"error\",\"stage\":\"load\"}'\n\
           exit 2\n\
         fi\n\
         while [ ! -f \"$d/release.flag\" ]; do\n\
           sleep 0.05\n\
         done\n\
         echo '{\"event\":\"ready\",\"load_ms\":12,\"threshold\":0.28,\"score\":2.7}'\n\
         exec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: sd.clone(),
            kws: true,
            kws_dir: kws_dir.clone(),
            ..Default::default()
        },
        Arc::new(hw),
    );
    handshake_v2(&core).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;

    // 1. Apply KWS while release.flag is absent -> receipt MUST stay deferred
    //    until the restarted sidecar emits `ready`.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-defer".into(),
        operation_id: "op-kws-defer".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-defer".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-defer",
            "threshold": 0.28,
            "score": 2.7
        }),
    })
    .await;

    let hold_until = tokio::time::Instant::now() + Duration::from_millis(300);
    loop {
        match tokio::time::timeout_at(hold_until, core.frames.lock().unwrap().recv()).await {
            Ok(Some(GatewayFrame::ConfigReceipt { operation_id, .. }))
                if operation_id == "op-kws-defer" =>
            {
                panic!("ConfigReceipt(op-kws-defer) arrived before sidecar emitted ready");
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("core channel closed"),
            Err(_) => break,
        }
    }

    // Release the sidecar's ready line -> deferred Active receipt now arrives.
    std::fs::write(kws_dir.join("release.flag"), "go\n").unwrap();
    let active = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-kws-defer"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        active_hash,
        evidence,
        ..
    } = active
    else {
        unreachable!()
    };
    assert_eq!(status, ConfigReceiptStatus::Active);
    assert_eq!(active_hash.as_deref(), Some("hash-kws-defer"));
    assert_eq!(
        evidence.as_ref().and_then(|e| e["reloaded"].as_bool()),
        Some(true)
    );
    assert_eq!(
        evidence.as_ref().and_then(|e| e["load_ms"].as_u64()),
        Some(12)
    );

    // 2. Sidecar fails during reload -> ConfigReceipt(Failed), never Active.
    std::fs::write(kws_dir.join("fail.flag"), "boom\n").unwrap();
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-crash".into(),
        operation_id: "op-kws-crash".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-crash".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好\nt ing @停止",
            "retreat_words": ["停止"],
            "intents_hash": "ih-crash",
            "threshold": 0.25,
            "score": 3.0
        }),
    })
    .await;
    let failed = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-kws-crash"))
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        persisted_hash,
        active_hash,
        error_code,
        retryable,
        ..
    } = failed
    else {
        unreachable!()
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(persisted_hash.as_deref(), Some("hash-kws-crash"));
    assert_eq!(active_hash, None);
    assert!(
        matches!(
            error_code.as_deref(),
            Some("kws_sidecar_error" | "kws_sidecar_exited")
        ),
        "unexpected error_code: {error_code:?}"
    );
    assert!(retryable);

    // 3. Clearing fail.flag and re-applying wakes supervisor backoff immediately.
    std::fs::remove_file(kws_dir.join("fail.flag")).unwrap();
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-recover".into(),
        operation_id: "op-kws-recover".into(),
        desired_revision: 3,
        block_generation: 3,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-recover".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-recover",
            "threshold": 0.25,
            "score": 3.0
        }),
    })
    .await;
    let recovered = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-kws-recover"))
        .await;
    assert!(matches!(
        recovered,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Active,
            ..
        }
    ));
}

#[tokio::test]
async fn config_apply_kws_overlapping_request_supersedes_pending_and_activates_newer() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("audit-kws-overlap");
    let kws_dir = sd.join("kws");
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model/tokens.txt"),
        "n 0\ni 1\nh 2\nao 3\nt 4\ning 5\n",
    )
    .unwrap();
    let run_sh = kws_dir.join("run.sh");
    std::fs::write(
        &run_sh,
        "#!/bin/sh\n\
         trap 'exit 0' TERM INT\n\
         d=$(dirname \"$0\")\n\
         if [ ! -f \"$d/kw.txt\" ]; then\n\
           echo '{\"event\":\"ready\",\"load_ms\":1,\"threshold\":0.25,\"score\":3.0}'\n\
           exec sleep 30\n\
         fi\n\
         while [ ! -f \"$d/release.flag\" ]; do\n\
           sleep 0.05\n\
         done\n\
         echo '{\"event\":\"ready\",\"load_ms\":17,\"threshold\":0.31,\"score\":2.6}'\n\
         exec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: sd.clone(),
            kws: true,
            kws_dir: kws_dir.clone(),
            ..Default::default()
        },
        Arc::new(hw),
    );
    handshake_v2(&core).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;

    // 1. Send first ConfigApply(Kws) while release.flag is absent so op-1 stays pending.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-overlap-1".into(),
        operation_id: "op-kws-overlap-1".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-overlap-1".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-overlap-1",
            "threshold": 0.28,
            "score": 2.7
        }),
    })
    .await;

    // 2. Before sidecar emits ready for op-1, send an overlapping ConfigApply(Kws)
    //    with a newer vocabulary and hash.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-overlap-2".into(),
        operation_id: "op-kws-overlap-2".into(),
        desired_revision: 2,
        block_generation: 2,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-overlap-2".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好\nt ing @停止",
            "retreat_words": ["停止"],
            "intents_hash": "ih-overlap-2",
            "threshold": 0.31,
            "score": 2.6
        }),
    })
    .await;

    // op-1 MUST settle immediately with Superseded (`superseded_by_newer_kws`).
    let superseded = core
        .until(|f| {
            matches!(
                f,
                GatewayFrame::ConfigReceipt { operation_id, .. }
                    if operation_id == "op-kws-overlap-1"
            )
        })
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        active_hash,
        error_code,
        retryable,
        evidence,
        ..
    } = superseded
    else {
        unreachable!()
    };
    assert_eq!(status, ConfigReceiptStatus::Superseded);
    assert_eq!(active_hash, None);
    assert_eq!(error_code.as_deref(), Some("superseded_by_newer_kws"));
    assert!(!retryable);
    assert_eq!(
        evidence
            .as_ref()
            .and_then(|e| e["superseded_by_operation_id"].as_str()),
        Some("op-kws-overlap-2")
    );
    assert_eq!(
        evidence
            .as_ref()
            .and_then(|e| e["superseded_by_reload_gen"].as_u64()),
        Some(2)
    );

    // op-2 MUST remain deferred until the restarted sidecar emits ready.
    let hold_until = tokio::time::Instant::now() + Duration::from_millis(200);
    loop {
        match tokio::time::timeout_at(hold_until, core.frames.lock().unwrap().recv()).await {
            Ok(Some(GatewayFrame::ConfigReceipt { operation_id, .. }))
                if operation_id == "op-kws-overlap-2" =>
            {
                panic!("ConfigReceipt(op-kws-overlap-2) arrived before sidecar emitted ready");
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("core channel closed"),
            Err(_) => break,
        }
    }

    // Verify kw.txt on disk already carries op-2's vocabulary, then release ready.
    let kw_on_disk = std::fs::read_to_string(kws_dir.join("kw.txt")).unwrap();
    assert!(
        kw_on_disk.contains("@停止"),
        "expected newer kw.txt to contain @停止, got: {kw_on_disk:?}"
    );
    std::fs::write(kws_dir.join("release.flag"), "go\n").unwrap();

    // op-2 MUST settle with Active matching the newer block_hash and loaded vocabulary hash.
    let active = core
        .until(|f| {
            matches!(
                f,
                GatewayFrame::ConfigReceipt { operation_id, .. }
                    if operation_id == "op-kws-overlap-2"
            )
        })
        .await;
    let GatewayFrame::ConfigReceipt {
        status,
        active_hash,
        active_values,
        evidence,
        ..
    } = active
    else {
        unreachable!()
    };
    assert_eq!(status, ConfigReceiptStatus::Active);
    assert_eq!(active_hash.as_deref(), Some("hash-kws-overlap-2"));
    let expected_kw_hash = active_values
        .as_ref()
        .and_then(|v| v["keywords_hash"].as_str())
        .expect("active_values.keywords_hash missing");
    assert_eq!(
        evidence.as_ref().and_then(|e| e["loaded_hash"].as_str()),
        Some(expected_kw_hash),
        "sidecar ready loaded_hash must match newer op-2 keywords_hash"
    );
    assert_eq!(
        evidence.as_ref().and_then(|e| e["reload_gen"].as_u64()),
        Some(2)
    );
    assert_eq!(
        evidence.as_ref().and_then(|e| e["reloaded"].as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn config_apply_kws_timeout_settles_failed_without_waiting_20s() {
    // Pin the production 20 s LX06 reload margin (above 11.8 s cold load + 1.5 s
    // kill grace, strictly below Core's 25 s KWS apply timeout).
    assert_eq!(KWS_RELOAD_TIMEOUT_MS, 20_000);
    assert_eq!(FirmwareConfig::default().kws_reload_timeout_ms, 20_000);

    let probes = Arc::new(Mutex::new(Probes::default()));
    let (hw, _btn) = FakeHardware::new(probes);
    let sd = state_dir("audit-kws-timeout");
    let kws_dir = sd.join("kws");
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model/tokens.txt"),
        "n 0\ni 1\nh 2\nao 3\nt 4\ning 5\n",
    )
    .unwrap();
    let run_sh = kws_dir.join("run.sh");
    std::fs::write(
        &run_sh,
        "#!/bin/sh\n\
         trap 'exit 0' TERM INT\n\
         d=$(dirname \"$0\")\n\
         if [ ! -f \"$d/kw.txt\" ]; then\n\
           echo '{\"event\":\"ready\",\"load_ms\":1,\"threshold\":0.25,\"score\":3.0}'\n\
           exec sleep 30\n\
         fi\n\
         while [ ! -f \"$d/late_ready.flag\" ]; do\n\
           sleep 0.05\n\
         done\n\
         echo '{\"event\":\"ready\",\"load_ms\":99,\"threshold\":0.25,\"score\":3.0}'\n\
         exec sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: sd.clone(),
            kws: true,
            kws_dir: kws_dir.clone(),
            kws_reload_timeout_ms: 400,
            ..Default::default()
        },
        Arc::new(hw),
    );
    handshake_v2(&core).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;

    let started = tokio::time::Instant::now();
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-kws-timeout".into(),
        operation_id: "op-kws-timeout".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-kws-timeout".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好",
            "retreat_words": [],
            "intents_hash": "ih-timeout",
            "threshold": 0.25,
            "score": 3.0
        }),
    })
    .await;

    let timed_out = core
        .until(|f| {
            matches!(
                f,
                GatewayFrame::ConfigReceipt { operation_id, .. }
                    if operation_id == "op-kws-timeout"
            )
        })
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout test must settle deterministically without sleeping 20s (took {:?})",
        started.elapsed()
    );
    let GatewayFrame::ConfigReceipt {
        status,
        persisted_hash,
        active_hash,
        error_code,
        retryable,
        evidence,
        ..
    } = timed_out
    else {
        unreachable!()
    };
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(persisted_hash.as_deref(), Some("hash-kws-timeout"));
    assert_eq!(active_hash, None);
    assert_eq!(error_code.as_deref(), Some("kws_reload_timeout"));
    assert!(retryable);
    assert_eq!(
        evidence.as_ref().and_then(|e| e["timeout_ms"].as_u64()),
        Some(400)
    );
    assert_eq!(
        evidence.as_ref().and_then(|e| e["reload_gen"].as_u64()),
        Some(1)
    );

    // Late sidecar ready after timeout MUST NOT emit a second receipt for op-kws-timeout.
    std::fs::write(kws_dir.join("late_ready.flag"), "late\n").unwrap();
    let _ = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;
    let hold_until = tokio::time::Instant::now() + Duration::from_millis(200);
    loop {
        match tokio::time::timeout_at(hold_until, core.frames.lock().unwrap().recv()).await {
            Ok(Some(GatewayFrame::ConfigReceipt { operation_id, .. }))
                if operation_id == "op-kws-timeout" =>
            {
                panic!("late sidecar ready must not emit a duplicate ConfigReceipt after timeout");
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("core channel closed"),
            Err(_) => break,
        }
    }
}

#[tokio::test]
async fn retreat_keyword_emits_local_exit_flushed_strictly_before_keyword() {
    let probes = Arc::new(Mutex::new(Probes::default()));
    let (mut hw, _btn) = FakeHardware::new(probes.clone());
    hw.pace_playback = true;
    let mode = hw.capture_reference_mode.clone();
    mode.store(1, Ordering::Relaxed);
    let (flush_tx, flush_rx) = std::sync::mpsc::channel();
    hw.flush_gate.lock().unwrap().replace(flush_rx);

    let sd = state_dir("audit-retreat-order");
    let kws_dir = sd.join("kws");
    std::fs::create_dir_all(kws_dir.join("model")).unwrap();
    std::fs::write(
        kws_dir.join("model/tokens.txt"),
        "n 0\ni 1\nh 2\nao 3\nt 4\ning 5\n",
    )
    .unwrap();
    let run_sh = kws_dir.join("run.sh");
    std::fs::write(
        &run_sh,
        "#!/bin/sh\n\
         d=$(dirname \"$0\")\n\
         echo '{\"event\":\"ready\",\"load_ms\":1,\"threshold\":0.25,\"score\":3.0}'\n\
         while true; do\n\
           if [ -f \"$d/fire_idle.flag\" ]; then\n\
             rm -f \"$d/fire_idle.flag\"\n\
             echo '{\"event\":\"keyword\",\"keyword\":\"停止播放\",\"fed_ms\":160,\"lag_ms\":0}'\n\
           fi\n\
           if [ -f \"$d/fire_play.flag\" ]; then\n\
             rm -f \"$d/fire_play.flag\"\n\
             echo '{\"event\":\"keyword\",\"keyword\":\"停止播放\",\"fed_ms\":1200,\"lag_ms\":0}'\n\
           fi\n\
           sleep 0.05\n\
         done\n",
    )
    .unwrap();
    std::fs::set_permissions(&run_sh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (core, port) = spawn_fake_core().await;
    spawn_firmware(
        FirmwareConfig {
            core_addr: format!("127.0.0.1:{port}"),
            state_dir: sd,
            aec: true,
            barge_in: false,
            loudness: false,
            boot_health: false,
            kws: true,
            kws_dir: kws_dir.clone(),
            ..Default::default()
        },
        Arc::new(hw),
    );
    handshake_v2(&core).await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::KwsReport { state, .. } if state == "running"))
        .await;

    // Arm retreat word `停止播放` via ConfigApply(Kws) and wait for reload Active.
    core.send(&CoreFrame::ConfigApply {
        device: "dot2-d".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: "app-retreat".into(),
        operation_id: "op-retreat".into(),
        desired_revision: 1,
        block_generation: 1,
        block: ConfigBlock::Kws,
        schema_version: 1,
        block_hash: "hash-retreat".into(),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({
            "keywords": "n i h ao @你好\nt ing @停止播放",
            "retreat_words": ["停止播放"],
            "intents_hash": "ih-retreat",
            "threshold": 0.25,
            "score": 3.0
        }),
    })
    .await;
    let armed = core
        .until(|f| matches!(f, GatewayFrame::ConfigReceipt { operation_id, .. } if operation_id == "op-retreat"))
        .await;
    assert!(matches!(
        armed,
        GatewayFrame::ConfigReceipt {
            status: ConfigReceiptStatus::Active,
            ..
        }
    ));

    // 1. Idle retreat (no active playback): Keyword leaves immediately with no flush.
    std::fs::write(kws_dir.join("fire_idle.flag"), "1\n").unwrap();
    let idle_kw = core
        .until(|f| matches!(f, GatewayFrame::Keyword { keyword, .. } if keyword == "停止播放"))
        .await;
    assert!(matches!(
        idle_kw,
        GatewayFrame::Keyword {
            self_heard: false,
            ..
        }
    ));
    assert_eq!(probes.lock().unwrap().flushed, 0);

    // 2. Active playback retreat: start MicSession + verified music playback.
    core.send(&CoreFrame::MicSession {
        device: "dot2-d".into(),
        action: "start".into(),
        mode: Some("continuous".into()),
    })
    .await;
    let _ = core
        .until(|f| matches!(f, GatewayFrame::MicReport { active: true, .. }))
        .await;

    let mut rng = 5u64;
    let mut pcm = Vec::new();
    for _ in 0..48000 * 10 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let sample = ((rng >> 33) as i32 % 6000 - 3000) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let mut wav = wav_48k(pcm.len() / 2, 1);
    wav[44..].copy_from_slice(&pcm);
    core.send(&play_frame("retreat-music", &wav, Some("media")))
        .await;

    let mut saw_sent = false;
    let mut saw_ref_locked = false;
    while !(saw_sent && saw_ref_locked) {
        let f = core.recv().await;
        match f {
            GatewayFrame::PlayReceipt { task_id, stage, .. }
                if task_id == "retreat-music" && stage == "sent" =>
            {
                saw_sent = true;
            }
            GatewayFrame::EchoHealth {
                health: EchoPathHealth::Healthy,
                reason,
                ..
            } if reason == "ref_locked" => {
                saw_ref_locked = true;
            }
            _ => {}
        }
    }

    // Trigger retreat word while flush_gate holds playback_flush open.
    std::fs::write(kws_dir.join("fire_play.flag"), "1\n").unwrap();
    let wait_flush_attempt = tokio::time::Instant::now() + Duration::from_secs(3);
    while probes.lock().unwrap().flush_attempts == 0 {
        assert!(
            tokio::time::Instant::now() < wait_flush_attempt,
            "retreat word never started local playback flush"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // While hardware flush is parked on flush_gate, neither LocalExitFlushed
    // nor retreat Keyword may leave on the north link.
    let hold_until = tokio::time::Instant::now() + Duration::from_millis(200);
    loop {
        match tokio::time::timeout_at(hold_until, core.frames.lock().unwrap().recv()).await {
            Ok(Some(GatewayFrame::LocalExitFlushed { .. })) => {
                panic!("LocalExitFlushed arrived before hardware flush completed");
            }
            Ok(Some(GatewayFrame::Keyword { keyword, .. })) if keyword == "停止播放" => {
                panic!("retreat Keyword arrived before LocalExitFlushed");
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("core channel closed"),
            Err(_) => break,
        }
    }

    // Release hardware flush -> LocalExitFlushed MUST arrive strictly BEFORE Keyword.
    flush_tx.send(()).unwrap();
    let mut events: Vec<&'static str> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while events.len() < 2 {
        let f = tokio::time::timeout_at(deadline, core.frames.lock().unwrap().recv())
            .await
            .expect("timed out waiting for LocalExitFlushed + Keyword")
            .expect("core channel closed");
        match f {
            GatewayFrame::LocalExitFlushed {
                keyword, channels, ..
            } if keyword == "停止播放" => {
                assert_eq!(channels, vec!["music".to_string()]);
                events.push("local_exit_flushed");
            }
            GatewayFrame::Keyword {
                keyword,
                self_heard,
                ..
            } if keyword == "停止播放" => {
                assert!(!self_heard);
                events.push("keyword");
            }
            _ => {}
        }
    }
    assert_eq!(
        events,
        vec!["local_exit_flushed", "keyword"],
        "LocalExitFlushed must arrive strictly BEFORE retreat Keyword"
    );
    assert_eq!(probes.lock().unwrap().flushed, 1);
}
