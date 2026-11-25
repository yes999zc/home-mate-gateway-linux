//! Negative controls against production admission, decoder, writer and event paths.
use super::playback::{decode_one, PlayAction, PlayCmd, PlayEvent};
use super::*;

#[derive(Default)]
struct HardwareProbe {
    shared_sink: bool,
    drain_allowed: AtomicBool,
    drain_calls: AtomicU64,
    silence_writes: AtomicU64,
    nonzero: AtomicU64,
    flushes: AtomicU64,
}
impl Hardware for HardwareProbe {
    fn capture_read(&self, _: &mut [u8], _: u32) -> io::Result<u32> {
        Ok(0)
    }
    fn playback_write(&self, samples: &[i16], frames: u32) -> io::Result<u32> {
        if samples.iter().any(|v| *v != 0) {
            self.nonzero.fetch_add(1, Ordering::Relaxed);
        } else {
            self.silence_writes.fetch_add(1, Ordering::Relaxed);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        Ok(frames)
    }
    fn playback_flush(&self) -> io::Result<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn playback_releases_when_idle(&self) -> bool {
        self.shared_sink
    }
    fn playback_drain(&self) -> io::Result<()> {
        self.drain_calls.fetch_add(1, Ordering::Release);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.shared_sink && !self.drain_allowed.load(Ordering::Acquire) {
            if std::time::Instant::now() >= until {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "test drain gate"));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(())
    }
    fn playback_underruns(&self) -> u64 {
        0
    }
    fn led_write(&self, _: &[(u8, u8, u8); 12]) -> io::Result<()> {
        Ok(())
    }
    fn button_next(&self) -> io::Result<ButtonEvent> {
        Err(io::Error::other("fixture"))
    }
    fn wake_lock(&self) -> io::Result<()> {
        Ok(())
    }
}
fn begin() -> CoreFrame {
    serde_json::from_value(json!({"type":"play_begin","task_id":"same","device":"fixture-dot","content_ref":"fixture","audio":{"format":"pcm_s16le","sample_rate":16000,"channels":1}})).unwrap()
}
fn chunk() -> CoreFrame {
    use base64::Engine;
    serde_json::from_value(json!({"type":"play_chunk","task_id":"same","seq":0,"audio_b64":base64::engine::general_purpose::STANDARD.encode(vec![16;4096])})).unwrap()
}
fn end() -> CoreFrame {
    serde_json::from_value(json!({"type":"play_end","task_id":"same"})).unwrap()
}

#[test]
fn same_task_reconnect_rejects_old_chunk_end_whole_decode_and_terminal_events() {
    let (role, rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let old = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 1;
        sh.north_tx = Some(old.clone());
    }
    role.on_core_frame(begin(), &old).unwrap();
    let stamp = role.shared.lock().unwrap().stamp(Chan::Voice);
    let old_chunk = role.on_core_frame(chunk(), &old).unwrap().unwrap();
    let old_end = role.on_core_frame(end(), &old).unwrap().unwrap();
    let old_whole = DecodeCmd::Play {
        task_id: "same".into(),
        chan: Chan::Voice,
        audio_b64: None,
        simulated_ms: Some(300),
        sample_rate: 48000,
        channels: 1,
        stamp,
        north: old.clone(),
    };
    old.fail("peer_closed");
    role.teardown_link();
    while rx.try_recv().is_ok() {}
    let fresh = NorthTx::new(
        "fixture-dot".into(),
        3,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 3;
        sh.north_tx = Some(fresh.clone());
    }
    role.on_core_frame(begin(), &fresh).unwrap();
    let fresh_stamp = role.shared.lock().unwrap().stamp(Chan::Voice);
    decode_one(&role, old_chunk);
    decode_one(&role, old_end);
    decode_one(&role, old_whole);
    assert!(
        rx.try_recv().is_err(),
        "stale decode published PCM/flush/EOS"
    );
    {
        let streams = role.streams.lock().unwrap();
        let state = streams["same"].state.lock().unwrap();
        assert_eq!((state.next_seq, state.pushed), (0, 0));
    }
    {
        let mut sh = role.shared.lock().unwrap();
        sh.pending_stop[0] = Some("same".into());
    }
    role.on_play_event(PlayEvent::StreamDone {
        stamp,
        chan: Chan::Voice,
        bytes: 9600,
        periods: 2,
        underruns: 0,
        delay_frames: None,
    });
    role.on_play_event(PlayEvent::FlushDone {
        stamp,
        chan: Chan::Voice,
        played_ms: 100,
        delay_frames: None,
        flush_err: None,
        at_ms: now_ms(),
    });
    role.on_play_event(PlayEvent::WriteFailed {
        stamps: [stamp, stamp],
        detail: "old writer".into(),
    });
    {
        let sh = role.shared.lock().unwrap();
        assert_eq!(sh.active[0].as_ref().unwrap().0, "same");
        assert_eq!(sh.pending_stop[0].as_deref(), Some("same"));
    }
    assert!(role.streams.lock().unwrap().contains_key("same"));
    assert!(
        rx.try_recv().is_err(),
        "stale completion flushed the new execution"
    );
    let current = role.on_core_frame(chunk(), &fresh).unwrap().unwrap();
    decode_one(&role, current);
    let pcm = rx.try_recv().unwrap();
    assert_eq!(pcm.stamp, fresh_stamp);
    assert!(matches!(pcm.action,PlayAction::Samples{samples,..} if !samples.is_empty()));
    let current = role.on_core_frame(end(), &fresh).unwrap().unwrap();
    decode_one(&role, current);
    assert!(!role.streams.lock().unwrap().contains_key("same"));
    assert!(rx
        .try_iter()
        .any(|cmd| cmd.stamp == fresh_stamp && matches!(cmd.action, PlayAction::Eos(_))));
    role.on_play_event(PlayEvent::FlushDone {
        stamp: fresh_stamp,
        chan: Chan::Voice,
        played_ms: 80,
        delay_frames: None,
        flush_err: None,
        at_ms: now_ms(),
    });
    assert!(
        role.shared.lock().unwrap().active[0].is_none(),
        "current completion must settle its task"
    );
    assert_eq!(
        old.send_ctrl(GatewayFrame::PlayReceipt {
            task_id: "same".into(),
            stage: "failed".into(),
            detail: None,
            ts: 0
        }),
        northbound::Admission::Closed
    );
}

#[test]
fn playback_rejects_old_pcm_end_and_flush_after_fresh_samples_are_installed() {
    let hw = Arc::new(HardwareProbe::default());
    let (role, rx) = role::isolated_role(hw.clone());
    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 3;
        sh.decode_gen[0] = 7;
    }
    let current = role.shared.lock().unwrap().stamp(Chan::Voice);
    let old = PlayStamp {
        link: 1,
        channel: 7,
    };
    let send = |stamp, action| role.play_tx.send(PlayCmd { stamp, action }).unwrap();
    send(
        current,
        PlayAction::Samples {
            chan: Chan::Voice,
            samples: vec![1000; PLAY_PERIOD as usize * 2],
        },
    );
    send(
        old,
        PlayAction::Samples {
            chan: Chan::Voice,
            samples: vec![30000; PLAY_PERIOD as usize * 2],
        },
    );
    send(old, PlayAction::Eos(Chan::Voice));
    send(old, PlayAction::Flush(Chan::Voice));
    send(current, PlayAction::Eos(Chan::Voice));
    let (events, event_rx) = mpsc::unbounded_channel();
    let observer = hw.clone();
    let shared = role.shared.clone();
    let progress = role.progress.clone();
    let fault = role.fault.clone();
    let alive = role.playback_alive.clone();
    let handle = std::thread::spawn(move || {
        playback_thread(
            hw,
            rx,
            events,
            shared,
            None,
            false,
            0.0,
            None,
            None,
            vec![],
            false,
            progress,
            fault,
            alive,
        )
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut event_rx = event_rx;
    let mut completed = false;
    while std::time::Instant::now() < deadline {
        if let Ok(PlayEvent::StreamDone { stamp, bytes, .. }) = event_rx.try_recv() {
            assert_eq!(stamp, current);
            assert_eq!(bytes, PLAY_PERIOD as u64 * 4);
            completed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(completed, "fresh stream failed to finish");
    assert!(observer.nonzero.load(Ordering::Relaxed) > 0);
    assert_eq!(observer.flushes.load(Ordering::Relaxed), 0);
    // Old flush must not clear the fresh PCM or call the hardware at all.
    assert_eq!(role.shared.lock().unwrap().link_gen, 3);
    drop(role);
    handle.join().unwrap();
}

async fn fatal_observer_case(blocked_decode: bool) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (mut role, _play_rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let (decode_tx, _decode_rx) = mpsc::channel(role::DECODE_QUEUE_CAP);
    if blocked_decode {
        for _ in 0..role::DECODE_QUEUE_CAP {
            decode_tx
                .try_send(DecodeCmd::PlayEnd {
                    task_id: "queued".into(),
                    stamp: PlayStamp {
                        link: 0,
                        channel: 0,
                    },
                })
                .ok()
                .unwrap();
        }
    }
    role.decode_tx = decode_tx;
    role.northbound_stats
        .set_decode_queue(role.decode_tx.downgrade());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    let (rd, wr) = gateway.into_split();
    let (peer_rd, mut peer_wr) = peer.into_split();
    let mut lines = BufReader::new(peer_rd).lines();
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let serve_tx = tx.clone();
    let serve_role = role.clone();
    let serve =
        tokio::spawn(async move { north_link::serve_link(&serve_role, rd, wr, serve_tx).await });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&lines.next_line().await.unwrap().unwrap())
            .unwrap()["type"],
        "hello"
    );
    peer_wr.write_all(b"{\"type\":\"hello_ack\",\"protocol\":0,\"min_protocol\":0,\"core_version\":\"test\",\"generation\":1}\n").await.unwrap();
    loop {
        let frame: GatewayFrame =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        if matches!(frame, GatewayFrame::MuteState { .. }) {
            break;
        }
    }
    assert_eq!(role.northbound_stats.probe()["handshake_completed"], true);
    if blocked_decode {
        for frame in [begin(), end()] {
            let mut line = serde_json::to_vec(&frame).unwrap();
            line.push(b'\n');
            peer_wr.write_all(&line).await.unwrap();
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while role.northbound_stats.probe()["decode_waiters"] != 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "production decoder capacity select was never entered"
            );
            tokio::task::yield_now().await;
        }
    }
    tx.fail("critical_count_full");
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), serve)
        .await
        .expect("fatal latch failed to end production select")
        .unwrap();
    assert!(result.is_err());
    let stats = role.northbound_stats.probe();
    assert_eq!(stats["reader_tasks"], 0);
    assert_eq!(stats["writer_tasks"], 0);
    assert_eq!(stats["decode_waiters"], 0);
    assert_eq!(tx.reason(), "critical_count_full");
}
#[tokio::test]
async fn fatal_latch_ends_real_outer_reader_select() {
    fatal_observer_case(false).await;
}
#[tokio::test]
async fn fatal_latch_ends_real_nested_decode_capacity_select() {
    fatal_observer_case(true).await;
}

#[cfg(feature = "board-lx06")]
#[tokio::test]
async fn native_turn_rejects_new_play_and_a_claim_lost_before_decode() {
    use super::lx06_vendor::VendorMonitor;
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    let (role, rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(8192);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    let busy = VendorMonitor::test_observer(true, false);
    role.shared.lock().unwrap().vendor = Some(busy.clone());
    assert!(role.on_core_frame(begin(), &tx).unwrap().is_none());
    assert!(role.streams.lock().unwrap().is_empty());

    role.shared.lock().unwrap().vendor = Some(VendorMonitor::test_observer(false, true));
    let reserve = role
        .on_core_frame(begin(), &tx)
        .unwrap()
        .expect("off-thread claim");
    role.shared.lock().unwrap().vendor = Some(busy);
    decode_one(&role, reserve);
    assert!(role.streams.lock().unwrap().is_empty());
    assert!(role.shared.lock().unwrap().active[0].is_none());
    assert!(
        rx.try_recv().is_err(),
        "an unowned play reached the hardware queue"
    );

    for _ in 0..2 {
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(frame, GatewayFrame::PlayReceipt {task_id, stage, detail, ..}
            if task_id == "same" && stage == "failed" && detail.as_deref() == Some("native_audio_busy"))
        );
    }
    tx.fail("fixture_done");
    writer.abort();
}

#[cfg(feature = "board-lx06")]
#[test]
fn idle_keyword_only_parks_native_when_core_accepts_the_wake() {
    use super::lx06_vendor::VendorMonitor;
    let (role, _) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let monitor = VendorMonitor::test_observer(false, false);
    {
        let mut shared = role.shared.lock().unwrap();
        shared.vendor = Some(monitor.clone());
        shared.north_tx = Some(tx.clone());
    }
    let keyword = role::lx06_keyword_admission(role.shared.clone());
    let state = |state: &str| CoreFrame::AssistantState {
        device: "fixture-dot".into(),
        state: state.into(),
    };
    assert!(keyword("下一首", false, false));
    assert!(
        monitor.test_wants_pns(),
        "an idle non-wake stopped native listening"
    );
    role.on_core_frame(state("idle"), &tx).unwrap();
    assert!(monitor.test_wants_pns());
    assert!(keyword("你好高达", false, false));
    assert!(monitor.test_wants_pns(), "a candidate alone took ownership");
    role.on_core_frame(state("wake"), &tx).unwrap();
    assert!(
        !monitor.test_wants_pns(),
        "confirmed Home wake must park the native engine"
    );
}

#[tokio::test]
async fn shared_playback_waits_for_last_writer_before_finish_and_releases_idle_sink() {
    use tokio::time::{sleep, timeout, Duration};
    let hw = Arc::new(HardwareProbe {
        shared_sink: true,
        ..Default::default()
    });
    let (role, rx) = role::isolated_role(hw.clone());
    let send = |action: PlayAction| {
        let stamp = role.shared.lock().unwrap().stamp(action.chan());
        role.play_tx.send(PlayCmd { stamp, action }).unwrap();
    };
    for chan in [Chan::Music, Chan::Voice] {
        send(PlayAction::Samples {
            chan,
            samples: vec![500; PLAY_PERIOD as usize],
        });
    }
    send(PlayAction::Eos(Chan::Voice));

    let (events, mut observed) = mpsc::unbounded_channel();
    let shared = role.shared.clone();
    let worker_hw = hw.clone();
    let progress = role.progress.clone();
    let fault = role.fault.clone();
    let alive = role.playback_alive.clone();
    let worker = std::thread::spawn(move || {
        playback_thread(
            worker_hw,
            rx,
            events,
            shared,
            None,
            false,
            0.0,
            None,
            None,
            vec![],
            false,
            progress,
            fault,
            alive,
        );
    });
    let next = timeout(Duration::from_secs(1), observed.recv())
        .await
        .unwrap();
    assert!(matches!(
        next,
        Some(PlayEvent::StreamDone {
            chan: Chan::Voice,
            ..
        })
    ));
    assert_eq!(
        hw.drain_calls.load(Ordering::Acquire),
        0,
        "music still owns this writer"
    );
    send(PlayAction::Eos(Chan::Music));
    timeout(Duration::from_secs(1), async {
        while hw.drain_calls.load(Ordering::Acquire) == 0 {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        observed.try_recv().is_err(),
        "last stream finished before its writer drained"
    );
    hw.drain_allowed.store(true, Ordering::Release);
    let next = timeout(Duration::from_secs(1), observed.recv())
        .await
        .unwrap();
    assert!(matches!(
        next,
        Some(PlayEvent::StreamDone {
            chan: Chan::Music,
            ..
        })
    ));
    let silence = hw.silence_writes.load(Ordering::Relaxed);
    sleep(Duration::from_millis(65)).await;
    assert_eq!(
        hw.silence_writes.load(Ordering::Relaxed),
        silence,
        "idle reopened writer"
    );
    assert_eq!(hw.drain_calls.load(Ordering::Acquire), 1);
    drop(role);
    worker.join().unwrap();
}

#[cfg(feature = "board-lx06")]
#[tokio::test]
async fn lx06_shared_playback_flush_confirms_stop_receipt_without_error() {
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    let hw = super::lx06::test_shared_hardware_from_cat();
    let (role, rx) = role::isolated_role(hw.clone());
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(8192);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 1;
        sh.north_tx = Some(tx.clone());
        sh.active[0] = Some(("t-lx06-stop".into(), 1000));
    }
    let stamp = role.shared.lock().unwrap().stamp(Chan::Voice);
    role.play_tx
        .send(PlayCmd {
            stamp,
            action: PlayAction::Samples {
                chan: Chan::Voice,
                samples: vec![500; PLAY_PERIOD as usize * 2],
            },
        })
        .unwrap();

    let (events, mut observed) = mpsc::unbounded_channel();
    let shared = role.shared.clone();
    let progress = role.progress.clone();
    let fault = role.fault.clone();
    let alive = role.playback_alive.clone();
    let worker = std::thread::spawn(move || {
        playback_thread(
            hw,
            rx,
            events,
            shared,
            None,
            false,
            0.0,
            None,
            None,
            vec![],
            false,
            progress,
            fault,
            alive,
        );
    });

    // Wait briefly for playback_thread to write at least one period to the cat helper.
    tokio::time::sleep(Duration::from_millis(20)).await;
    role.on_core_frame(
        CoreFrame::Stop {
            task_id: "t-lx06-stop".into(),
            flush: true,
        },
        &tx,
    )
    .unwrap();

    let ev = timeout(Duration::from_secs(2), observed.recv())
        .await
        .expect("FlushDone timeout")
        .expect("FlushDone event");
    assert!(
        matches!(
            &ev,
            PlayEvent::FlushDone {
                flush_err: None,
                ..
            }
        ),
        "SharedPlayback::flush() must succeed after reaping helper: {ev:?}"
    );
    role.on_play_event(ev);

    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
    match frame {
        GatewayFrame::StopReceipt {
            task_id,
            stage,
            played_ms,
            progress_basis,
            detail,
            ..
        } => {
            assert_eq!(task_id, "t-lx06-stop");
            assert_eq!(stage, "device_confirmed");
            assert_eq!(detail, None);
            assert!(played_ms.is_some());
            assert_eq!(progress_basis.as_deref(), Some("written"));
        }
        other => panic!("expected StopReceipt, got {other:?}"),
    }

    tx.fail("fixture_done");
    writer.abort();
    drop(role);
    worker.join().unwrap();
}

#[tokio::test]
async fn decode_gen_bump_settles_pending_stop_local_exit_and_local_stop_without_locking() {
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    let (role, _rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(16384);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 1;
        sh.north_tx = Some(tx.clone());
        sh.active[0] = Some(("old-voice".into(), 1000));
        sh.pending_stop[0] = Some("old-stop".into());
        sh.local_stop = Some(("old-voice".into(), now_ms()));
        sh.local_exit = Some(wire::LocalExit {
            keyword: "停止播放".into(),
            fired_ms: now_ms(),
            flushed: 1,
            pending: 1,
            voice_task: Some("old-voice".into()),
            music_task: None,
            flush_err: None,
            deferred_keyword: None,
        });
    }

    // 1. A Music FlushDone must NOT consume Voice's local_stop!
    let music_stamp = role.shared.lock().unwrap().stamp(Chan::Music);
    role.on_play_event(PlayEvent::FlushDone {
        stamp: music_stamp,
        chan: Chan::Music,
        played_ms: 0,
        delay_frames: None,
        flush_err: None,
        at_ms: now_ms(),
    });
    assert!(
        role.shared.lock().unwrap().local_stop.is_some(),
        "Music FlushDone must not steal Voice local_stop"
    );

    // 2. New PlayBegin on Voice bumps decode_gen[0]: must settle pending_stop,
    // settle local_exit, and clear local_stop so nothing is dropped or locked.
    role.on_core_frame(begin(), &tx).unwrap();
    {
        let sh = role.shared.lock().unwrap();
        assert!(sh.pending_stop[0].is_none());
        assert!(sh.local_stop.is_none());
        assert!(sh.local_exit.is_none());
    }
    let mut saw_stop = false;
    let mut saw_exit = false;
    for _ in 0..4 {
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
        match frame {
            GatewayFrame::StopReceipt {
                task_id,
                stage,
                detail,
                ..
            } if task_id == "old-stop" => {
                assert_eq!(stage, "device_confirmed");
                assert_eq!(detail, None);
                saw_stop = true;
            }
            GatewayFrame::LocalExitFlushed { keyword, .. } if keyword == "停止播放" => {
                saw_exit = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_stop,
        "pending_stop must emit StopReceipt on decode_gen bump"
    );
    assert!(saw_exit, "local_exit must settle on decode_gen bump");

    // 3. Bounded 2_000 ms age timeout expires orphaned local_exit & local_stop
    //    and still emits LocalExitFlushed strictly BEFORE any deferred retreat Keyword.
    let stale_ts = now_ms().saturating_sub(LOCAL_FLUSH_LOCK_TIMEOUT_MS + 50);
    {
        let mut sh = role.shared.lock().unwrap();
        sh.local_stop = Some(("stuck-voice".into(), stale_ts));
        sh.local_exit = Some(wire::LocalExit {
            keyword: "退出".into(),
            fired_ms: stale_ts,
            flushed: 3,
            pending: 3,
            voice_task: None,
            music_task: None,
            flush_err: None,
            deferred_keyword: Some(GatewayFrame::Keyword {
                device: "fixture-dot".into(),
                ts: stale_ts,
                seq: 0,
                keyword: "退出".into(),
                fed_ms: Some(1200),
                self_heard: false,
                rms_dbfs: None,
                lag_ms: Some(0),
                bundle_id: None,
                kws_generation: None,
                kws_tier: None,
                wake_dur_ms: None,
                max_gap_ms: None,
                l2_passed: None,
            }),
        });
    }
    role.watch_local_flush_locks();
    {
        let sh = role.shared.lock().unwrap();
        assert!(sh.local_stop.is_none(), "stale local_stop must expire");
        assert!(sh.local_exit.is_none(), "stale local_exit must expire");
    }
    let line1 = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame1: GatewayFrame = serde_json::from_str(&line1).unwrap();
    assert!(matches!(
        frame1,
        GatewayFrame::LocalExitFlushed { keyword, detail, .. }
            if keyword == "退出" && detail.as_deref() == Some("flush_timeout")
    ));
    let line2 = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame2: GatewayFrame = serde_json::from_str(&line2).unwrap();
    assert!(matches!(
        frame2,
        GatewayFrame::Keyword { keyword, .. } if keyword == "退出"
    ));

    tx.fail("fixture_done");
    writer.abort();
}

#[tokio::test]
async fn audio_eq_pending_receipt_handles_supersede_legacy_and_dead_playback() {
    use crate::protocol::{ConfigBlock, ConfigReceiptStatus};
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    let (mut role, _rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let state_dir =
        std::env::temp_dir().join(format!("hm-eq-race-{}-{}", std::process::id(), now_ms()));
    std::fs::create_dir_all(&state_dir).unwrap();
    role.cfg.state_dir = state_dir.clone();

    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(16384);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    {
        let mut sh = role.shared.lock().unwrap();
        sh.link_gen = 1;
        sh.north_tx = Some(tx.clone());
        sh.negotiated_protocol = 2;
        sh.config_apply_v2 = true;
    }

    let eq_apply = |op: &str, gen: u64, curve: &str| CoreFrame::ConfigApply {
        device: "fixture-dot".into(),
        core_node_id: "core-1".into(),
        data_epoch: 1,
        application_id: format!("app-{op}"),
        operation_id: op.into(),
        desired_revision: gen,
        block_generation: gen,
        block: ConfigBlock::AudioEq,
        schema_version: 1,
        block_hash: format!("hash-{op}"),
        expected_gateway_boot_id: None,
        expected_state_revision: None,
        capability_fingerprint: None,
        values: json!({ "curve": curve, "bands": [] }),
    };

    // 1. Two rapid ConfigApply(AudioEq) before the 42ms boundary: op-1 must emit Superseded.
    role.on_core_frame(eq_apply("op-eq-1", 1, "rui"), &tx)
        .unwrap();
    let rev1 = role.shared.lock().unwrap().eq_rev;
    role.on_core_frame(eq_apply("op-eq-2", 2, "official"), &tx)
        .unwrap();
    let rev2 = role.shared.lock().unwrap().eq_rev;
    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let f1: GatewayFrame = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        f1,
        GatewayFrame::ConfigReceipt {
            operation_id,
            status: ConfigReceiptStatus::Superseded,
            ..
        } if operation_id == "op-eq-1"
    ));

    // Stale EqApplied for rev1 must NOT settle op-eq-2 prematurely.
    role.on_play_event(PlayEvent::EqApplied { eq_rev: rev1 });
    assert!(role.shared.lock().unwrap().pending_eq_receipt.is_some());

    // EqApplied at rev >= rev2 settles op-eq-2 as Active.
    role.on_play_event(PlayEvent::EqApplied { eq_rev: rev2 + 1 });
    assert!(role.shared.lock().unwrap().pending_eq_receipt.is_none());
    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let f2: GatewayFrame = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        f2,
        GatewayFrame::ConfigReceipt {
            operation_id,
            status: ConfigReceiptStatus::Active,
            ..
        } if operation_id == "op-eq-2"
    ));

    // 2. Legacy CoreFrame::AudioEq arriving mid-flight supersedes pending op-eq-3.
    role.on_core_frame(eq_apply("op-eq-3", 3, "echomuse"), &tx)
        .unwrap();
    role.on_core_frame(
        CoreFrame::AudioEq {
            device: "fixture-dot".into(),
            curve: "rui".into(),
            bands: vec![],
        },
        &tx,
    )
    .unwrap();
    assert!(role.shared.lock().unwrap().pending_eq_receipt.is_none());
    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let f3: GatewayFrame = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        f3,
        GatewayFrame::ConfigReceipt {
            operation_id,
            status: ConfigReceiptStatus::Superseded,
            error_code,
            ..
        } if operation_id == "op-eq-3" && error_code.as_deref() == Some("superseded_by_legacy_audio_eq")
    ));

    // 3. Dead playback_alive rejects ConfigApply(AudioEq) even if sh.play_tx is Some.
    role.shared.lock().unwrap().play_tx = Some(role.play_tx.clone());
    role.playback_alive.store(false, Ordering::Release);
    role.on_core_frame(eq_apply("op-eq-4", 4, "rui"), &tx)
        .unwrap();
    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let f4: GatewayFrame = serde_json::from_str(&line).unwrap();
    assert!(matches!(
        f4,
        GatewayFrame::ConfigReceipt {
            operation_id,
            status: ConfigReceiptStatus::Failed,
            error_code,
            ..
        } if operation_id == "op-eq-4" && error_code.as_deref() == Some("playback_unavailable")
    ));

    tx.fail("fixture_done");
    writer.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn pending_kws_receipt_uses_monotonic_deadline_and_20s_default_timeout() {
    use crate::protocol::{ConfigBlock, ConfigReceiptStatus};
    use tokio::io::AsyncBufReadExt;
    use tokio::time::{timeout, Duration};

    assert_eq!(north_config::KWS_RELOAD_TIMEOUT_MS, 20_000);

    let (mut role, _rx) = role::isolated_role(Arc::new(HardwareProbe::default()));
    let state_dir = std::env::temp_dir().join(format!(
        "hm-kws-timeout-{}-{}",
        std::process::id(),
        now_ms()
    ));
    std::fs::create_dir_all(&state_dir).unwrap();
    role.cfg.state_dir = state_dir.clone();
    assert_eq!(role.cfg.kws_reload_timeout_ms, 20_000);

    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let (sink, reader) = tokio::io::duplex(16384);
    let writer_tx = tx.clone();
    let writer = tokio::spawn(async move { writer_tx.write_to(sink).await });
    let mut lines = BufReader::new(reader).lines();

    let staged_mono = kws::mono_ms();
    let make_pending = |op: &str, staged_ms: u64| north_config::PendingKwsConfig {
        target_gen: 1,
        staged_ms,
        receipt: config_journal::ConfigReceiptData {
            device: "fixture-dot".into(),
            gateway_boot_id: "boot-test".into(),
            operation_id: op.into(),
            application_id: format!("app-{op}"),
            data_epoch: 1,
            desired_revision: 1,
            block_generation: 1,
            block: ConfigBlock::Kws,
            block_hash: format!("hash-{op}"),
            state_revision: 0,
            status: ConfigReceiptStatus::Persisted,
            persisted_hash: Some(format!("hash-{op}")),
            active_hash: None,
            active_values: None,
            evidence: None,
            error_code: None,
            retryable: false,
        },
        persisted_values: serde_json::json!({"keywords_hash": "abcd"}),
        detail: "staged".into(),
        signalled: true,
    };

    {
        let mut sh = role.shared.lock().unwrap();
        sh.north_tx = Some(tx.clone());
        sh.pending_kws_receipt = Some(make_pending("op-mono-live", staged_mono));
    }

    // Live monotonic check must NOT expire a freshly staged entry (even though
    // wall-clock now_ms() is decades ahead of monotonic uptime).
    role.watch_pending_kws_receipt();
    assert!(
        role.shared.lock().unwrap().pending_kws_receipt.is_some(),
        "freshly staged monotonic pending KWS op must not expire"
    );

    // Deterministic monotonic boundary check at 15 s (old limit) and 19.999 s vs 20 s.
    {
        let mut sh = role.shared.lock().unwrap();
        sh.pending_kws_receipt = Some(make_pending("op-mono-20s", 1_000));
    }
    role.watch_pending_kws_receipt_at(1_000 + 15_000);
    assert!(
        role.shared.lock().unwrap().pending_kws_receipt.is_some(),
        "must not time out at old 15_000 ms bound"
    );
    role.watch_pending_kws_receipt_at(1_000 + 19_999);
    assert!(
        role.shared.lock().unwrap().pending_kws_receipt.is_some(),
        "must not time out before 20_000 ms bound"
    );
    role.watch_pending_kws_receipt_at(1_000 + 20_000);
    assert!(
        role.shared.lock().unwrap().pending_kws_receipt.is_none(),
        "must expire at 20_000 ms monotonic bound"
    );

    let line = timeout(Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: GatewayFrame = serde_json::from_str(&line).unwrap();
    let GatewayFrame::ConfigReceipt {
        operation_id,
        status,
        persisted_hash,
        active_hash,
        error_code,
        retryable,
        evidence,
        ..
    } = frame
    else {
        panic!("expected ConfigReceipt, got {frame:?}");
    };
    assert_eq!(operation_id, "op-mono-20s");
    assert_eq!(status, ConfigReceiptStatus::Failed);
    assert_eq!(persisted_hash.as_deref(), Some("hash-op-mono-20s"));
    assert_eq!(active_hash, None);
    assert_eq!(error_code.as_deref(), Some("kws_reload_timeout"));
    assert!(retryable);
    assert_eq!(
        evidence.as_ref().and_then(|e| e["timeout_ms"].as_u64()),
        Some(20_000)
    );

    tx.fail("fixture_done");
    writer.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[cfg(feature = "board-lx06")]
#[test]
fn lx06_indicator_handoff_runs_outside_shared_lock() {
    use super::lx06_vendor::VendorMonitor;

    struct LockCheckingHw {
        shared: Mutex<Option<Arc<Mutex<role::Shared>>>>,
        indicator_calls: AtomicU64,
    }
    impl Hardware for LockCheckingHw {
        fn set_home_indicator(&self, _owner: lx06::IndicatorOwner) {
            self.indicator_calls.fetch_add(1, Ordering::Relaxed);
            if let Some(shared) = self.shared.lock().unwrap().as_ref() {
                assert!(
                    shared.try_lock().is_ok(),
                    "set_home_indicator must NEVER be called while holding shared.lock()"
                );
            }
        }
        fn capture_read(&self, _: &mut [u8], _: u32) -> io::Result<u32> {
            Ok(0)
        }
        fn playback_write(&self, _: &[i16], frames: u32) -> io::Result<u32> {
            Ok(frames)
        }
        fn playback_flush(&self) -> io::Result<()> {
            Ok(())
        }
        fn playback_drain(&self) -> io::Result<()> {
            Ok(())
        }
        fn playback_underruns(&self) -> u64 {
            0
        }
        fn led_write(&self, _: &[(u8, u8, u8); 12]) -> io::Result<()> {
            if let Some(shared) = self.shared.lock().unwrap().as_ref() {
                assert!(
                    shared.try_lock().is_ok(),
                    "led_write must NEVER be called while holding shared.lock()"
                );
            }
            Ok(())
        }
        fn button_next(&self) -> io::Result<ButtonEvent> {
            Err(io::Error::other("fixture"))
        }
        fn wake_lock(&self) -> io::Result<()> {
            Ok(())
        }
    }

    let hw = Arc::new(LockCheckingHw {
        shared: Mutex::new(None),
        indicator_calls: AtomicU64::new(0),
    });
    let (role, _rx) = role::isolated_role(hw.clone());
    *hw.shared.lock().unwrap() = Some(role.shared.clone());
    let monitor = VendorMonitor::test_observer(false, true);
    role.shared.lock().unwrap().vendor = Some(monitor);

    // 1. render_ring must release shared.lock() before set_home_indicator & led_write.
    north::render_ring(&role.shared, &role.hw);
    // 2. reserve_decoded_play must release shared.lock() before set_home_indicator.
    let tx = NorthTx::new(
        "fixture-dot".into(),
        1,
        role.northbound_stats.clone(),
        role.dropped_audio.clone(),
    );
    let reserve = role.on_core_frame(begin(), &tx).unwrap().unwrap();
    decode_one(&role, reserve);
    // 3. teardown_link must release shared.lock() before set_home_indicator.
    role.teardown_link();

    assert!(
        hw.indicator_calls.load(Ordering::Relaxed) >= 3,
        "expected render_ring, reserve_decoded_play, and teardown_link to call set_home_indicator"
    );
}
