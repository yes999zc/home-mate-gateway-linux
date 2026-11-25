//! `config_apply_v2` handlers (`ConfigStateGet`, `ConfigValidate`, `ConfigApply`)
//! and atomic KWS bundle metadata staging (#112 / #113).

use super::config_journal::{
    self, capability_fingerprint, kws_model_token_hashes, ConfigReceiptData, OpLookup,
};
use super::role::PendingEqConfig;
use super::*;
use crate::protocol::{ConfigBlock, ConfigReceiptStatus, MAX_CONFIG_FRAME_BYTES};

/// Bounded wait for a restarted `hm-kws` sidecar to emit `ready` after
/// `ConfigApply(Kws)`. Covers slow cold model loads (~11.8 s measured on LX06
/// plus up to 1.5 s SIGTERM→SIGKILL grace) while staying strictly below Core's
/// 25 s KWS apply timeout and guaranteeing Core never hangs indefinitely on a
/// wedged sidecar.
pub const KWS_RELOAD_TIMEOUT_MS: u64 = 20_000;

#[derive(Debug, Clone)]
pub(super) struct PendingKwsConfig {
    pub(super) target_gen: u64,
    /// Monotonic timestamp (`kws::mono_ms()`) when this KWS reload was staged.
    pub(super) staged_ms: u64,
    pub(super) receipt: ConfigReceiptData,
    pub(super) persisted_values: serde_json::Value,
    pub(super) detail: String,
    pub(super) signalled: bool,
}

pub(super) fn handle_kws_lifecycle(
    shared: &Arc<Mutex<Shared>>,
    state_dir: &std::path::Path,
    ev: kws::KwsLifecycleEvent,
) {
    let (receipt_frame, north) = {
        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pending) = sh.pending_kws_receipt.take() else {
            return;
        };
        match ev {
            kws::KwsLifecycleEvent::Ready {
                reload_gen,
                load_ms,
                threshold,
                score,
                loaded_hash,
                local_exit,
            } => {
                if reload_gen < pending.target_gen {
                    sh.pending_kws_receipt = Some(pending);
                    return;
                }
                let mut receipt = pending.receipt;
                sh.config_journal.state_revision =
                    sh.config_journal.state_revision.saturating_add(1);
                receipt.state_revision = sh.config_journal.state_revision;
                receipt.status = ConfigReceiptStatus::Active;
                receipt.active_hash = Some(receipt.block_hash.clone());
                receipt.active_values = Some(pending.persisted_values.clone());
                receipt.evidence = Some(serde_json::json!({
                    "restarted": pending.signalled,
                    "reloaded": true,
                    "reload_gen": reload_gen,
                    "load_ms": load_ms,
                    "loaded_hash": loaded_hash,
                    "loaded_threshold": threshold,
                    "loaded_score": score,
                    "local_exit": local_exit,
                    "detail": pending.detail,
                }));
                sh.config_journal
                    .record_terminal(&receipt, Some(pending.persisted_values));
                let _ = sh.config_journal.save(state_dir);
                fwlog!(
                    "[config_v2] kws active after sidecar reload (reload_gen={reload_gen}, load_ms={load_ms})"
                );
                (receipt.into_frame(), sh.north_tx.clone())
            }
            kws::KwsLifecycleEvent::Failed {
                reload_gen,
                error_code,
                detail,
            } => {
                if reload_gen < pending.target_gen {
                    sh.pending_kws_receipt = Some(pending);
                    return;
                }
                let mut receipt = pending.receipt;
                receipt.status = ConfigReceiptStatus::Failed;
                receipt.active_hash = None;
                receipt.active_values = None;
                receipt.error_code = Some(error_code.into());
                receipt.retryable = true;
                receipt.evidence = Some(serde_json::json!({
                    "restarted": pending.signalled,
                    "reload_gen": reload_gen,
                    "detail": detail,
                }));
                sh.config_journal
                    .record_terminal(&receipt, Some(pending.persisted_values));
                let _ = sh.config_journal.save(state_dir);
                fwlog!(
                    "[config_v2] kws reload failed (reload_gen={reload_gen}, error_code={error_code}): {detail}"
                );
                (receipt.into_frame(), sh.north_tx.clone())
            }
        }
    };
    if let Some(tx) = north {
        tx.send_ctrl(receipt_frame);
    }
}

impl Role {
    pub(super) fn stage_kws_bundle_meta(
        &self,
        dir: &std::path::Path,
        kw_hash: &str,
        intents_hash: &str,
        tuning: Option<(f32, f32)>,
        local_exit: u32,
    ) -> Result<(String, u64), String> {
        let tuning_str = match tuning {
            Some((t, s)) => format!("{t:.4}:{s:.4}"),
            None => "default".to_string(),
        };
        let bundle_id =
            fnv1a_hex(format!("{kw_hash}:{intents_hash}:{tuning_str}:{local_exit}").as_bytes());
        let status = lock_or_fault(&self.shared, Some(&self.fault), "role.shared")
            .kws_status
            .clone();
        let next_gen = if let Some(ref st_arc) = status {
            let st = st_arc
                .lock()
                .map_err(|_| "kws_status poisoned".to_string())?;
            st.bundle_generation.saturating_add(1)
        } else {
            1
        };
        kws::write_bundle_meta(dir, &bundle_id, next_gen)?;
        if let Some(st_arc) = status {
            if let Ok(mut st) = st_arc.lock() {
                st.bundle_id = bundle_id.clone();
                st.bundle_generation = next_gen;
            }
        }
        Ok((bundle_id, next_gen))
    }

    pub(super) fn handle_config_state_get(
        &self,
        request_id: String,
        device: String,
        direct_tx: &NorthTx,
    ) {
        if device != self.cfg.device_id {
            return;
        }
        let capabilities = self.capabilities();
        let kws_dir = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_state.shared");
            if sh.negotiated_protocol < 2 || !sh.config_apply_v2 {
                fwlog!(
                    "[config_v2] ConfigStateGet ignored: protocol={} config_apply_v2={}",
                    sh.negotiated_protocol,
                    sh.config_apply_v2
                );
                return;
            }
            sh.kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone())
        };
        let (model_hash, token_hash) = if self.cfg.kws {
            kws_model_token_hashes(&kws_dir)
        } else {
            (None, None)
        };
        let (boot_id, state_rev, blocks, tz_offset, tz_name) = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_state.shared");
            (
                sh.gateway_boot_id.clone(),
                sh.config_journal.state_revision,
                sh.config_journal.block_states(),
                sh.led_tz_offset_minutes,
                sh.led_timezone.clone(),
            )
        };
        let mut block_schemas = serde_json::json!({
            "audio_eq": 1,
            "led": 1,
            "duplex": 1,
            "acoustics": 1,
        });
        let mut bounds = serde_json::json!({
            "audio_eq": { "max_bands": 10, "freq_hz": [20.0, 16000.0], "gain_db": [-18.0, 18.0], "q": [0.3, 8.0] },
            "led": { "minute": [0, 1439], "brightness_pct": [0, 100], "rotation": [0, 11], "tz_offset_minutes": [-720, 840] },
            "acoustics": { "volume_cap": [0.0, 1.0], "mix_gain": [0.05, 2.0], "onset_threshold_dbfs": [-120.0, 0.0], "mic_gain_offset_db": [-24.0, 24.0] },
        });
        if self.cfg.kws {
            block_schemas["kws"] = serde_json::json!(1);
            bounds["kws"] = serde_json::json!({ "threshold": [0.0, 1.0], "score": [0.0, 20.0] });
        }
        let _ = direct_tx.send_ctrl(GatewayFrame::ConfigState {
            request_id,
            device,
            gateway_boot_id: boot_id,
            state_revision: state_rev,
            capabilities,
            block_schemas,
            bounds,
            model_id: Some(self.cfg.model.clone()),
            model_hash,
            token_hash,
            time_basis: Some(serde_json::json!({
                "timezone": tz_name.unwrap_or_else(|| "UTC".to_string()),
                "tz_offset_minutes": tz_offset,
                "utc_ms": now_ms(),
            })),
            blocks,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_config_validate(
        &self,
        request_id: String,
        device: String,
        expected_gateway_boot_id: Option<String>,
        expected_state_revision: Option<u64>,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        values: serde_json::Value,
        direct_tx: &NorthTx,
    ) {
        let cap_fp = capability_fingerprint(&self.capabilities());
        let (boot_id, state_rev, kws_dir, v2_negotiated) = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_validate.shared");
            let dir = sh
                .kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone());
            (
                sh.gateway_boot_id.clone(),
                sh.config_journal.state_revision,
                dir,
                sh.negotiated_protocol >= 2 && sh.config_apply_v2,
            )
        };
        let (model_hash, token_hash) = if self.cfg.kws {
            kws_model_token_hashes(&kws_dir)
        } else {
            (None, None)
        };
        let mut blockers = Vec::new();
        if !v2_negotiated {
            blockers.push("config_apply_v2_not_negotiated".into());
        }
        if device != self.cfg.device_id {
            blockers.push(format!("device_mismatch: host is {}", self.cfg.device_id));
        }
        if schema_version != 1 {
            blockers.push(format!("unsupported_schema_version:{schema_version}"));
        }
        if let Some(exp) = expected_gateway_boot_id {
            if exp != boot_id {
                blockers.push(format!("boot_id_mismatch: expected {exp}, got {boot_id}"));
            }
        }
        if let Some(exp_rev) = expected_state_revision {
            if exp_rev != state_rev {
                blockers.push(format!(
                    "state_revision_mismatch: expected {exp_rev}, got {state_rev}"
                ));
            }
        }
        if serde_json::to_vec(&values)
            .map(|v| v.len() > MAX_CONFIG_FRAME_BYTES)
            .unwrap_or(true)
        {
            blockers.push("frame_too_large".into());
        }

        let normalized_values = if blockers.is_empty() {
            match block {
                ConfigBlock::Kws => {
                    if !self.cfg.kws {
                        blockers.push("kws_disabled".into());
                        None
                    } else {
                        match config_journal::validate_kws(&values, &kws_dir) {
                            Ok(v) => Some(v.normalized),
                            Err(e) => {
                                blockers.extend(e);
                                None
                            }
                        }
                    }
                }
                ConfigBlock::AudioEq => match config_journal::validate_audio_eq(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Led => match config_journal::validate_led(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Duplex => match config_journal::validate_duplex(&values) {
                    Ok(v) => {
                        if v.duplex_requested == "full" {
                            blockers.push("full_duplex_unsupported".into());
                        }
                        Some(v.normalized)
                    }
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
                ConfigBlock::Acoustics => match config_journal::validate_acoustics(&values) {
                    Ok(v) => Some(v.normalized),
                    Err(e) => {
                        blockers.extend(e);
                        None
                    }
                },
            }
        } else {
            None
        };

        let _ = direct_tx.send_ctrl(GatewayFrame::ConfigValidated {
            request_id,
            device,
            gateway_boot_id: boot_id,
            state_revision: state_rev,
            block,
            block_hash,
            supported: blockers.is_empty(),
            normalized_values,
            capability_fingerprint: Some(cap_fp),
            model_hash,
            token_hash,
            blockers,
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_config_apply(
        &self,
        device: String,
        _core_node_id: String,
        data_epoch: u64,
        application_id: String,
        operation_id: String,
        desired_revision: u64,
        block_generation: u64,
        block: ConfigBlock,
        schema_version: u32,
        block_hash: String,
        expected_gateway_boot_id: Option<String>,
        expected_state_revision: Option<u64>,
        capability_fingerprint_arg: Option<String>,
        values: serde_json::Value,
        direct_tx: &NorthTx,
    ) {
        let cap_fp = capability_fingerprint(&self.capabilities());

        let make_receipt = |boot_id: String,
                            state_rev: u64,
                            status: ConfigReceiptStatus,
                            persisted_hash: Option<String>,
                            active_hash: Option<String>,
                            active_values: Option<serde_json::Value>,
                            evidence: Option<serde_json::Value>,
                            error_code: Option<String>,
                            retryable: bool| {
            ConfigReceiptData {
                device: self.cfg.device_id.clone(),
                gateway_boot_id: boot_id,
                operation_id: operation_id.clone(),
                application_id: application_id.clone(),
                data_epoch,
                desired_revision,
                block_generation,
                block,
                block_hash: block_hash.clone(),
                state_revision: state_rev,
                status,
                persisted_hash,
                active_hash,
                active_values,
                evidence,
                error_code,
                retryable,
            }
        };

        // 1. Protocol/capability + device identity check.
        let v2_negotiated = {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
            sh.negotiated_protocol >= 2 && sh.config_apply_v2
        };
        if !v2_negotiated {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Unsupported,
                None,
                None,
                None,
                None,
                Some("config_apply_v2_not_negotiated".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }
        if device != self.cfg.device_id {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Failed,
                None,
                None,
                None,
                None,
                Some("device_mismatch".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }

        // 2. Frame size cap check.
        if serde_json::to_vec(&values)
            .map(|v| v.len() > MAX_CONFIG_FRAME_BYTES)
            .unwrap_or(true)
        {
            let (boot_id, state_rev) = {
                let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
                (sh.gateway_boot_id.clone(), sh.config_journal.state_revision)
            };
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Failed,
                None,
                None,
                None,
                None,
                Some("frame_too_large".into()),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }

        // 3. Idempotent journal lookup + preconditions.
        let kws_dir;
        let boot_id;
        let state_rev;
        {
            let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.shared");
            boot_id = sh.gateway_boot_id.clone();
            state_rev = sh.config_journal.state_revision;
            kws_dir = sh
                .kws_control
                .as_ref()
                .map(|c| c.dir.clone())
                .unwrap_or_else(|| self.cfg.kws_dir.clone());

            match sh
                .config_journal
                .lookup_operation(&operation_id, block, &block_hash)
            {
                OpLookup::IdempotentReplay(recorded) => {
                    let _ = direct_tx.send_ctrl((*recorded).into_frame());
                    return;
                }
                OpLookup::Conflict => {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        None,
                        Some("operation_conflict".into()),
                        false,
                    );
                    let _ = direct_tx.send_ctrl(r.into_frame());
                    return;
                }
                OpLookup::New => {}
            }

            if let Some(pending) = sh.pending_kws_receipt.as_ref() {
                if pending.receipt.operation_id == operation_id {
                    if pending.receipt.block == block && pending.receipt.block_hash == block_hash {
                        return;
                    }
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        None,
                        Some("operation_conflict".into()),
                        false,
                    );
                    drop(sh);
                    let _ = direct_tx.send_ctrl(r.into_frame());
                    return;
                }
            }
            if let Some(pending) = sh.pending_eq_receipt.as_ref() {
                if pending.receipt.operation_id == operation_id {
                    if pending.receipt.block == block && pending.receipt.block_hash == block_hash {
                        return;
                    }
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        None,
                        Some("operation_conflict".into()),
                        false,
                    );
                    drop(sh);
                    let _ = direct_tx.send_ctrl(r.into_frame());
                    return;
                }
            }

            if let Err(reason) =
                sh.config_journal
                    .check_high_water(block, data_epoch, block_generation)
            {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Superseded,
                    None,
                    None,
                    None,
                    Some(serde_json::json!({ "reason": reason })),
                    Some("stale_generation".into()),
                    false,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }

        if schema_version != 1 {
            let r = make_receipt(
                boot_id,
                state_rev,
                ConfigReceiptStatus::Unsupported,
                None,
                None,
                None,
                None,
                Some(format!("unsupported_schema_version:{schema_version}")),
                false,
            );
            let _ = direct_tx.send_ctrl(r.into_frame());
            return;
        }
        if let Some(exp) = expected_gateway_boot_id {
            if exp != boot_id {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("boot_id_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }
        if let Some(exp_rev) = expected_state_revision {
            if exp_rev != state_rev {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("state_revision_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }
        if let Some(fp) = capability_fingerprint_arg {
            if fp != cap_fp {
                let r = make_receipt(
                    boot_id,
                    state_rev,
                    ConfigReceiptStatus::Failed,
                    None,
                    None,
                    None,
                    None,
                    Some("capability_fingerprint_mismatch".into()),
                    true,
                );
                let _ = direct_tx.send_ctrl(r.into_frame());
                return;
            }
        }

        // 4. Block-specific validation, persistence, and execution.
        match block {
            ConfigBlock::Kws => {
                if !self.cfg.kws {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Unsupported,
                        None,
                        None,
                        None,
                        None,
                        Some("kws_disabled".into()),
                        false,
                    );
                    let _ = direct_tx.send_ctrl(r.into_frame());
                    return;
                }
                let v = match config_journal::validate_kws(&values, &kws_dir) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let (accepted, kw_hash, count, detail, local_exit) =
                    deploy_keywords(&kws_dir, &v.keywords, &v.retreat_words, &v.intents_hash);
                if !accepted {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": detail })),
                        Some("kws_deploy_rejected".into()),
                        false,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let tuning = v.threshold.zip(v.score);
                if let Err(e) = kws::write_tuning(&kws_dir, tuning) {
                    let bak = kws_dir.join("kw.txt.bak");
                    if bak.exists() {
                        let _ = std::fs::rename(&bak, kws_dir.join("kw.txt"));
                    }
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("kws_tuning_write_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let (bundle_id, bundle_gen) = match self.stage_kws_bundle_meta(
                    &kws_dir,
                    &kw_hash,
                    &v.intents_hash,
                    tuning,
                    local_exit,
                ) {
                    Ok(meta) => meta,
                    Err(e) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "detail": e })),
                            Some("kws_bundle_meta_failed".into()),
                            true,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };

                let (control, status_arc) = {
                    let sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.kws");
                    (sh.kws_control.clone(), sh.kws_status.clone())
                };
                if let Some(st_arc) = status_arc {
                    if let Ok(mut st) = st_arc.lock() {
                        if let Some((t, s)) = tuning {
                            st.threshold = t;
                            st.score = s;
                        }
                        st.local_exit = local_exit;
                    }
                }
                let active_vals = serde_json::json!({
                    "keywords_hash": kw_hash,
                    "intents_hash": v.intents_hash,
                    "keywords": count,
                    "local_exit": local_exit,
                    "threshold": v.threshold,
                    "score": v.score,
                    "bundle_id": bundle_id,
                    "kws_generation": bundle_gen,
                });
                let Some(ctrl) = control else {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        Some(block_hash.clone()),
                        None,
                        None,
                        Some(serde_json::json!({
                            "detail": "kws supervisor control unavailable",
                        })),
                        Some("kws_sidecar_unavailable".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, Some(active_vals), direct_tx);
                    return;
                };
                if !kws::script_ready(&kws_dir.join("run.sh")) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        Some(block_hash.clone()),
                        None,
                        None,
                        Some(serde_json::json!({
                            "detail": format!("keyword sidecar not installed: {}", kws_dir.join("run.sh").display()),
                        })),
                        Some("kws_sidecar_unavailable".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, Some(active_vals), direct_tx);
                    return;
                }
                // Defer `Active` until the restarted `hm-kws` sidecar actually
                // emits `ready` (or emit `Failed` on spawn/error/exit/timeout).
                // Hold `sh` across `request_reload()` so `pending_kws_receipt`
                // is guaranteed visible before `handle_kws_lifecycle` can run.
                let (target_gen, superseded_frame) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.kws");
                    let (target_gen, signalled) = ctrl.request_reload();
                    let pending_receipt = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Persisted,
                        Some(block_hash.clone()),
                        None,
                        None,
                        None,
                        None,
                        false,
                    );
                    let replaced = sh.pending_kws_receipt.replace(PendingKwsConfig {
                        target_gen,
                        staged_ms: kws::mono_ms(),
                        receipt: pending_receipt,
                        persisted_values: active_vals.clone(),
                        detail: detail.clone(),
                        signalled,
                    });
                    let superseded_frame = replaced.map(|old| {
                        let mut r = old.receipt;
                        r.status = ConfigReceiptStatus::Superseded;
                        r.active_hash = None;
                        r.active_values = None;
                        r.error_code = Some("superseded_by_newer_kws".into());
                        r.evidence = Some(serde_json::json!({
                            "superseded_by_reload_gen": target_gen,
                            "superseded_by_operation_id": operation_id,
                        }));
                        r.retryable = false;
                        sh.config_journal.record_terminal(&r, None);
                        let _ = sh.config_journal.save(&self.cfg.state_dir);
                        r.into_frame()
                    });
                    (target_gen, superseded_frame)
                };
                if let Some(frame) = superseded_frame {
                    let _ = direct_tx.send_ctrl(frame);
                }
                fwlog!(
                    "[config_v2] kws staged; awaiting sidecar ready (reload_gen={target_gen}, detail={detail})"
                );
            }
            ConfigBlock::AudioEq => {
                let v = match config_journal::validate_audio_eq(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let saved = config_journal::SavedAudioEq {
                    curve: v.curve_name.clone(),
                    bands: v.raw_bands.clone(),
                };
                if let Err(e) = config_journal::save_audio_eq(&self.cfg.state_dir, &saved) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let playback_alive =
                    self.playback_alive.load(Ordering::Acquire) && !self.fault.is_requested();
                if !playback_alive {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        Some(block_hash.clone()),
                        None,
                        None,
                        None,
                        Some("playback_unavailable".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
                    return;
                }

                // Stage EQ in Shared and defer the `Active` receipt until
                // `playback.rs` rebuilds `Loudness` at the 42 ms period boundary.
                // If an earlier `ConfigApply(AudioEq)` is still awaiting its
                // period boundary, settle it as `Superseded` rather than
                // dropping its receipt.
                let (new_eq_rev, superseded_frame) = {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.eq");
                    sh.eq_curve = Some(v.curve);
                    sh.eq_bands = v.bands.clone();
                    sh.eq_rev = sh.eq_rev.wrapping_add(1);
                    let rev = sh.eq_rev;
                    let pending_receipt = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Persisted,
                        Some(block_hash.clone()),
                        None,
                        None,
                        None,
                        None,
                        false,
                    );
                    let replaced = sh.pending_eq_receipt.replace(PendingEqConfig {
                        eq_rev: rev,
                        receipt: pending_receipt,
                        persisted_values: v.normalized.clone(),
                    });
                    let superseded_frame = replaced.map(|old| {
                        let mut r = old.receipt;
                        r.status = ConfigReceiptStatus::Superseded;
                        r.active_hash = None;
                        r.active_values = None;
                        r.error_code = Some("superseded_by_newer_audio_eq".into());
                        r.evidence = Some(serde_json::json!({
                            "superseded_by_eq_rev": rev,
                            "superseded_by_operation_id": operation_id,
                        }));
                        r.retryable = false;
                        sh.config_journal.record_terminal(&r, None);
                        let _ = sh.config_journal.save(&self.cfg.state_dir);
                        r.into_frame()
                    });
                    (rev, superseded_frame)
                };
                if let Some(frame) = superseded_frame {
                    let _ = direct_tx.send_ctrl(frame);
                }
                fwlog!(
                    "[loudness] audio_eq staged for 42ms period rebuild (eq_rev={new_eq_rev}, curve={:?}, bands={})",
                    v.curve,
                    v.bands.len()
                );
            }
            ConfigBlock::Led => {
                let v = match config_journal::validate_led(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                if let Err(e) = leds::save_led_config(
                    &self.cfg.state_dir,
                    v.night_mode,
                    v.orientation,
                    v.tz_offset_minutes,
                    Some(v.timezone.clone()),
                ) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                {
                    let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_apply.led");
                    sh.night_mode = v.night_mode;
                    sh.led_orientation = v.orientation;
                    sh.led_tz_offset_minutes = v.tz_offset_minutes;
                    sh.led_timezone = Some(v.timezone.clone());
                }
                render_ring(&self.shared, &self.hw);
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    ConfigReceiptStatus::Active,
                    Some(block_hash.clone()),
                    Some(block_hash.clone()),
                    Some(v.normalized.clone()),
                    Some(serde_json::json!({
                        "minute_of_day": leds::minute_of_day_now_with_offset(v.tz_offset_minutes),
                    })),
                    None,
                    false,
                );
                self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
            }
            ConfigBlock::Duplex => {
                let v = match config_journal::validate_duplex(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "config_apply.duplex");
                    sh.duplex_requested = v.duplex_requested.clone();
                    sh.duplex_effective = "half".into();
                }
                let active_vals = serde_json::json!({
                    "capability": "half",
                    "requested": v.duplex_requested,
                    "negotiated": "half",
                    "effective": "half",
                });
                let (status, error_code) = if v.duplex_requested == "full" {
                    (
                        ConfigReceiptStatus::Unsupported,
                        Some("full_duplex_unsupported".into()),
                    )
                } else {
                    (ConfigReceiptStatus::Active, None)
                };
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    status,
                    Some(block_hash.clone()),
                    (status == ConfigReceiptStatus::Active).then(|| block_hash.clone()),
                    Some(active_vals.clone()),
                    None,
                    error_code,
                    false,
                );
                self.record_and_send_receipt(r, Some(active_vals), direct_tx);
                let _ =
                    direct_tx.send_ctrl(self.mode_report_frame(v.duplex_requested, "half".into()));
            }
            ConfigBlock::Acoustics => {
                let v = match config_journal::validate_acoustics(&values) {
                    Ok(v) => v,
                    Err(errs) => {
                        let r = make_receipt(
                            boot_id,
                            state_rev,
                            ConfigReceiptStatus::Failed,
                            None,
                            None,
                            None,
                            Some(serde_json::json!({ "errors": errs })),
                            Some("validation_failed".into()),
                            false,
                        );
                        self.record_and_send_receipt(r, None, direct_tx);
                        return;
                    }
                };
                let saved = config_journal::SavedAcoustics {
                    volume_cap: v.volume_cap,
                    mix_gain: v.mix_gain,
                    onset_threshold_dbfs: v.onset_threshold_dbfs,
                    mic_gain_offset_db: v.mic_gain_offset_db,
                };
                if let Err(e) = config_journal::save_acoustics(&self.cfg.state_dir, &saved) {
                    let r = make_receipt(
                        boot_id,
                        state_rev,
                        ConfigReceiptStatus::Failed,
                        None,
                        None,
                        None,
                        Some(serde_json::json!({ "detail": e })),
                        Some("persistence_failed".into()),
                        true,
                    );
                    self.record_and_send_receipt(r, None, direct_tx);
                    return;
                }
                let barge_arc = {
                    let mut sh =
                        lock_or_fault(&self.shared, Some(&self.fault), "config_apply.acoustics");
                    sh.volume_cap = v.volume_cap;
                    sh.mix_gain = v.mix_gain;
                    sh.vad_threshold = v.onset_threshold_dbfs;
                    // `mic_gain_offset_db` is persisted in `acoustics_config.json`
                    // and echoed in `active_values` for Core wake-arbitration
                    // calibration; gateway wire RMS intentionally stays raw.
                    sh.barge.clone()
                };
                if let Some(barge) = barge_arc {
                    if let Ok(mut det) = barge.lock() {
                        det.set_onset_db(v.onset_threshold_dbfs);
                    }
                }
                let r = make_receipt(
                    boot_id,
                    state_rev.saturating_add(1),
                    ConfigReceiptStatus::Active,
                    Some(block_hash.clone()),
                    Some(block_hash.clone()),
                    Some(v.normalized.clone()),
                    Some(serde_json::json!({
                        "unit_id": self.cfg.profile.unit_id,
                    })),
                    None,
                    false,
                );
                self.record_and_send_receipt(r, Some(v.normalized), direct_tx);
            }
        }
    }

    pub(super) fn watch_pending_kws_receipt(&self) {
        self.watch_pending_kws_receipt_at(kws::mono_ms());
    }

    pub(super) fn watch_pending_kws_receipt_at(&self, now_mono_ms: u64) {
        let timeout_ms = if self.cfg.kws_reload_timeout_ms == 0 {
            KWS_RELOAD_TIMEOUT_MS
        } else {
            self.cfg.kws_reload_timeout_ms
        };
        let (frame, north) = {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_kws.watch");
            let expired = sh
                .pending_kws_receipt
                .as_ref()
                .is_some_and(|p| now_mono_ms.saturating_sub(p.staged_ms) >= timeout_ms);
            if !expired {
                return;
            }
            let Some(pending) = sh.pending_kws_receipt.take() else {
                return;
            };
            let mut receipt = pending.receipt;
            receipt.status = ConfigReceiptStatus::Failed;
            receipt.active_hash = None;
            receipt.active_values = None;
            receipt.error_code = Some("kws_reload_timeout".into());
            receipt.retryable = true;
            receipt.evidence = Some(serde_json::json!({
                "restarted": pending.signalled,
                "reload_gen": pending.target_gen,
                "timeout_ms": timeout_ms,
            }));
            sh.config_journal
                .record_terminal(&receipt, Some(pending.persisted_values));
            let _ = sh.config_journal.save(&self.cfg.state_dir);
            fwlog!(
                "[config_v2] kws reload timed out after {timeout_ms}ms (reload_gen={})",
                pending.target_gen
            );
            (receipt.into_frame(), sh.north_tx.clone())
        };
        if let Some(tx) = north {
            tx.send_ctrl(frame);
        }
    }

    fn record_and_send_receipt(
        &self,
        mut receipt: ConfigReceiptData,
        persisted_values: Option<serde_json::Value>,
        direct_tx: &NorthTx,
    ) {
        {
            let mut sh = lock_or_fault(&self.shared, Some(&self.fault), "config_receipt.shared");
            if matches!(
                receipt.status,
                ConfigReceiptStatus::Persisted
                    | ConfigReceiptStatus::Active
                    | ConfigReceiptStatus::Unsupported
            ) {
                sh.config_journal.state_revision =
                    sh.config_journal.state_revision.saturating_add(1);
                receipt.state_revision = sh.config_journal.state_revision;
            }
            sh.config_journal
                .record_terminal(&receipt, persisted_values);
            let _ = sh.config_journal.save(&self.cfg.state_dir);
        }
        let _ = direct_tx.send_ctrl(receipt.into_frame());
    }
}
