use super::*;

#[test]
fn a_missing_sidecar_is_not_spawned() {
    let dir = std::env::temp_dir().join(format!("hm-kws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("run.sh");
    assert!(
        !script_ready(&script),
        "absent script must not look runnable"
    );
    std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
    assert!(
        !script_ready(&script),
        "non-executable script must not look runnable"
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(script_ready(&script), "executable script is runnable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parses_ready_and_keyword_lines() {
    assert_eq!(
        parse_event_line(r#"{"event":"ready","name":"dot2-d","load_ms":6797}"#),
        DriverEvent::Ready {
            load_ms: 6797,
            threshold: None,
            score: None
        }
    );
    assert_eq!(
        parse_event_line(
            r#"{"event":"keyword","keyword":"你好高达","fed_samples":43520,"fed_ms":2720,"last_token_s":0.84,"detect_ms":1880,"wall_ms":649750,"n":1}"#
        ),
        DriverEvent::Keyword {
            keyword: "你好高达".into(),
            fed_ms: 2720,
            lag_ms: None,
            meta: KeywordMeta::default(),
        }
    );
    assert_eq!(
        parse_event_line(
            r#"{"event":"keyword","keyword":"你好小智","fed_ms":3200,"lag_ms":40,"wake_dur_ms":720,"max_gap_ms":160,"tier":"real","l2_passed":true,"l2_ms":18}"#
        ),
        DriverEvent::Keyword {
            keyword: "你好小智".into(),
            fed_ms: 3200,
            lag_ms: Some(40),
            meta: KeywordMeta {
                tier: Some("real".into()),
                wake_dur_ms: Some(720),
                max_gap_ms: Some(160),
                l2_passed: Some(true),
            },
        }
    );
}

#[test]
fn ignores_log_noise_and_malformed_lines() {
    assert_eq!(parse_event_line("[kws] HIT 你好高达"), DriverEvent::Unknown);
    assert_eq!(parse_event_line("{not json"), DriverEvent::Unknown);
    assert_eq!(
        parse_event_line(r#"{"event":"something_new"}"#),
        DriverEvent::Unknown
    );
}

#[test]
fn error_line_names_the_stage() {
    assert_eq!(
        parse_event_line(r#"{"event":"error","stage":"load","load_ms":12}"#),
        DriverEvent::Error {
            stage: "load".into()
        }
    );
}

/// 2026-11-14 field defect: on a MUTED device no PCM reaches the
/// sidecar, so a driver parked in a (SA_RESTART) `read()` ignored
/// SIGTERM; `reflex.deploy` was accepted, the old vocabulary kept
/// running, and only a firmware restart applied the file. The manager
/// was even told "spotter restarting" because `kill()` had succeeded.
///
/// The driver now polls with a timeout, but the deploy contract must not
/// depend on the sidecar cooperating: this test uses a sidecar that
/// deliberately ignores SIGTERM and asserts the supervisor still replaces
/// it (SIGKILL escalation) without spending the failure budget.
#[test]
fn a_deploy_replaces_a_sidecar_that_ignores_sigterm() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("hm-kws-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let starts = dir.join("starts");
    let script = dir.join("run.sh");
    // Records each spawn, refuses to die on SIGTERM, and blocks in a
    // shell builtin (no child process to leak). `read` also never sees
    // data: the supervisor feeds nothing unless audio arrives.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho start >> \"{}\"\ntrap '' TERM\nread line\n",
            starts.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("kw.txt"), "n ǐ h ǎo g āo d á @你好高达\n").unwrap();

    let north: NorthSink = Arc::new(|_f: GatewayFrame| {});
    let feed = spawn(dir.clone(), 3, "test".into(), north, None, None, None);
    assert!(
        wait_for_spawns(&starts, 1, 5_000),
        "first sidecar never started"
    );

    // The supervisor stores the pid just after spawn. On a fast host the
    // script's "start" line can land before that store, so restart()
    // briefly sees pid 0. Poll until a signal is actually delivered.
    let signalled = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2_000);
        loop {
            if feed.control().restart() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    assert!(signalled, "a live sidecar must be signalled");
    // SIGKILL escalation is 1500 ms; allow generous slack for CI hosts.
    assert!(
        wait_for_spawns(&starts, 2, 6_000),
        "the supervisor did not replace a sidecar that ignored SIGTERM"
    );

    let status = feed.status();
    let st = status.lock().unwrap();
    assert_eq!(
        st.restarts, 0,
        "a deliberate deploy restart is not a failure"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// How many times has the fake sidecar started? Polls until `at_least`
/// lines are present or the deadline passes.
fn wait_for_spawns(path: &std::path::Path, at_least: usize, timeout_ms: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        let n = std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        if n >= at_least {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn keyword_frame_carries_the_fact_and_counts() {
    let status: SharedStatus = Arc::new(Mutex::new(KwsStatus::default()));
    let meta = KeywordMeta {
        tier: Some("suspect".into()),
        wake_dur_ms: Some(180),
        max_gap_ms: Some(420),
        l2_passed: Some(false),
    };
    let frame = north_keyword(
        "dot2-d",
        "你好高达",
        2720,
        1_700_000_000_000,
        &status,
        false,
        Some(-31.5),
        Some(90),
        &meta,
    );
    match frame {
        GatewayFrame::Keyword {
            device,
            keyword,
            fed_ms,
            self_heard,
            kws_tier,
            wake_dur_ms,
            max_gap_ms,
            l2_passed,
            ..
        } => {
            assert_eq!(device, "dot2-d");
            assert_eq!(keyword, "你好高达");
            assert_eq!(fed_ms, Some(2720));
            assert!(!self_heard);
            assert_eq!(kws_tier.as_deref(), Some("suspect"));
            assert_eq!(wake_dur_ms, Some(180));
            assert_eq!(max_gap_ms, Some(420));
            assert_eq!(l2_passed, Some(false));
        }
        other => panic!("wrong frame: {other:?}"),
    }
    let st = status.lock().unwrap();
    assert_eq!(st.detections, 1);
    assert_eq!(st.guarded, 0);
    assert_eq!(st.last_keyword_ms, 1_700_000_000_000);
}

/// R1g: a guarded hit is REPORTED with the hint, counted in both numbers
/// (guarded ⊆ detections) — never silently eaten (that made the wake word
/// unusable during our own playback).
#[test]
fn a_guarded_hit_is_reported_as_a_hint_not_dropped() {
    let status: SharedStatus = Arc::new(Mutex::new(KwsStatus::default()));
    let frame = north_keyword(
        "dot2-d",
        "你好高达",
        100,
        1_700_000_000_000,
        &status,
        true,
        None,
        None,
        &KeywordMeta::default(),
    );
    match frame {
        GatewayFrame::Keyword { self_heard, .. } => assert!(self_heard),
        other => panic!("wrong frame: {other:?}"),
    }
    let st = status.lock().unwrap();
    assert_eq!(st.detections, 1);
    assert_eq!(st.guarded, 1);
}

#[test]
fn feed_without_sidecar_drops_instead_of_blocking() {
    let status: SharedStatus = Arc::new(Mutex::new(KwsStatus::default()));
    let feed = KwsFeed {
        tx: Arc::new(Mutex::new(None)),
        status: status.clone(),
        control: KwsControl {
            dir: PathBuf::from("/nonexistent"),
            pid: Arc::new(AtomicU64::new(0)),
            intentional: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            signalled_at: Arc::new(AtomicU64::new(0)),
            reload_gen: Arc::new(AtomicU64::new(0)),
            wake: Arc::new((Mutex::new(0), std::sync::Condvar::new())),
        },
        vad_state: Arc::new(AtomicU8::new(VAD_UNKNOWN)),
    };
    let block = vec![0i16; 1280];
    feed.feed(&block);
    feed.feed(&block);
    assert_eq!(status.lock().unwrap().dropped_blocks, 2);
}

#[test]
fn mono_ms_moves_forward() {
    let a = mono_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b = mono_ms();
    assert!(b >= a, "monotonic clock went backwards: {a} -> {b}");
}

/// Contract v2 §2.5 (#5): the local exit is armed only for a retreat word
/// the guard did not call self-heard. Both vetoes matter: a wake word must
/// never cut the audio, and our own answer saying 「停止播放」 is not a
/// person asking.
#[test]
fn only_an_unshadowed_retreat_word_is_acted_on_locally() {
    let mut words = std::collections::HashSet::new();
    words.insert("停止播放".to_string());
    assert!(local_exit_for("停止播放", &words, false));
    assert!(
        local_exit_for(" 停止播放 ", &words, false),
        "the driver's text is trimmed"
    );
    assert!(
        !local_exit_for("停止播放", &words, true),
        "self-heard: the guard keeps its veto"
    );
    assert!(
        !local_exit_for("你好高达", &words, false),
        "a wake word is not an exit"
    );
    assert!(
        !local_exit_for("停止播放", &std::collections::HashSet::new(), false),
        "no set, no exit"
    );
}

/// Contract v2 #4: the sensitivity file is what the launcher reads as
/// `KWS_THRESHOLD`/`KWS_SCORE`; a removed file returns the bundle default.
#[test]
fn the_tuning_file_is_atomic_and_removable() {
    let dir = std::env::temp_dir().join(format!("hm-kws-tune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(load_tuning(&dir).is_none(), "no file = the bundle default");
    write_tuning(&dir, Some((0.18, 3.5))).unwrap();
    assert_eq!(load_tuning(&dir), Some((0.18, 3.5)));
    // Anything but a finite pair arms nothing.
    std::fs::write(
        dir.join("kw.tuning.json"),
        r#"{"threshold":null,"score":3}"#,
    )
    .unwrap();
    assert!(load_tuning(&dir).is_none());
    write_tuning(&dir, Some((0.25, 3.0))).unwrap();
    write_tuning(&dir, None).unwrap();
    assert!(load_tuning(&dir).is_none(), "removed = back to the default");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The exit set is read from the deploy's own file; a missing or broken
/// file is "no local exit" (the core still closes the session), never a
/// half-armed vocabulary.
#[test]
fn the_exit_set_comes_from_the_deployed_file() {
    let dir = std::env::temp_dir().join(format!("hm-kws-exit-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(load_exit_words(&dir).is_empty(), "no file = no local exit");

    std::fs::write(
        dir.join("kw.exit.json"),
        r#"{"hash":"h1","words":["停止播放","退下"]}"#,
    )
    .unwrap();
    let set = load_exit_words(&dir);
    assert_eq!(set.len(), 2);
    assert!(set.contains("退下"));

    std::fs::write(dir.join("kw.exit.json"), "{not json").unwrap();
    assert!(
        load_exit_words(&dir).is_empty(),
        "a broken file arms nothing"
    );
    std::fs::write(dir.join("kw.exit.json"), r#"{"hash":"h2","words":[]}"#).unwrap();
    assert!(load_exit_words(&dir).is_empty(), "an empty set is no exit");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn vad_event_parses_and_tristate_decodes() {
    assert_eq!(
        parse_event_line(r#"{"event":"vad","speech":true}"#),
        DriverEvent::Vad {
            speech: true,
            prob: None,
        }
    );
    assert_eq!(
        parse_event_line(r#"{"event":"vad","speech":false,"prob":0.12}"#),
        DriverEvent::Vad {
            speech: false,
            prob: Some(0.12),
        }
    );
    assert_eq!(
        parse_event_line(r#"{"event":"vad"}"#),
        DriverEvent::Unknown,
        "missing boolean speech field must be ignored"
    );
    assert_eq!(decode_vad_state(VAD_UNKNOWN), None);
    assert_eq!(decode_vad_state(VAD_SILENCE), Some(false));
    assert_eq!(decode_vad_state(VAD_SPEECH), Some(true));
}

#[test]
fn reconcile_bundle_meta_respects_flash_floor_when_tmpfs_missing_or_stale() {
    let floor = (Some("flash-b7".to_string()), 7u64);
    // 1. Clean tmpfs (no kw.bundle.json yet) inherits the persisted flash floor.
    assert_eq!(
        reconcile_bundle_meta("", 0, None, Some(&floor)),
        ("flash-b7".to_string(), 7)
    );
    // 2. Stale backup on tmpfs (gen 4 < flash floor 7) must never regress below 7.
    assert_eq!(
        reconcile_bundle_meta("", 0, Some(("stale-b4".to_string(), 4)), Some(&floor)),
        ("flash-b7".to_string(), 7)
    );
    // 3. Newer tmpfs bundle (gen 9 >= flash floor 7) advances cleanly.
    assert_eq!(
        reconcile_bundle_meta(
            "flash-b7",
            7,
            Some(("fresh-b9".to_string(), 9)),
            Some(&floor)
        ),
        ("fresh-b9".to_string(), 9)
    );
    // 4. Subsequent spawn without file keeps live status generation >= floor.
    assert_eq!(
        reconcile_bundle_meta("fresh-b9", 9, None, Some(&floor)),
        ("fresh-b9".to_string(), 9)
    );
}
