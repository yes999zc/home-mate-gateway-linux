//! Gateway binaries: virtual dot2-d terminal (dev tool) and the
//! direct-occupation firmware.
//!
//!   cargo run -- virtual [--core 127.0.0.1:8932] [--out ./audio-out] [--afplay]
//!   cargo run -- firmware --core 127.0.0.1:8932 \
//!              [--hw-ref] [--mic-channels 0,1,2] [--ref-channel 8]
//!
//! Virtual terminal commands:
//!   w | wake          simulate a wake event
//!   <any text>        utterance (recognized-text simulation, source "text")
//!   s | stop          action button press (= stop request semantics)
//!   mute | unmute     hardware mute state toggle
//!   q | quit          exit

#[cfg(feature = "board-dot2")]
use home_mate_device_gateway::client::{self, GatewayConfig};
use home_mate_device_gateway::firmware::loudness;
#[cfg(feature = "board-dot2")]
use home_mate_device_gateway::protocol::GatewayFrame;
#[cfg(feature = "board-dot2")]
use home_mate_device_gateway::terminal::VirtualDot2d;
use std::path::PathBuf;
#[cfg(feature = "board-dot2")]
use std::sync::Arc;
#[cfg(feature = "board-dot2")]
use tokio::sync::{mpsc, Mutex};

#[cfg(feature = "board-dot2")]
#[tokio::main]
async fn main() {
    // armv7 only: route clock_gettime through the syscall this kernel has
    // (see clock32) — musl's time64 entry point traps and the vendor traps
    // handler's serial register dump kills the board's audio SPI path.
    home_mate_device_gateway::clock32::keep();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "home_mate_device_gateway=info".into()),
        )
        .init();
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "virtual".into());
    if cmd == "serve-file" {
        // G6 device-side deploy channel: the device (emOS, no adb) pulls
        // firmware builds over WiFi with busybox wget. Minimal std-only
        // HTTP/1.1 GET, one directory, no traversal.
        run_serve_file(args).await;
        return;
    }
    if cmd == "firmware" {
        run_firmware(args);
        return;
    }
    if cmd == "led-cal" {
        run_led_cal(args);
        return;
    }
    #[cfg(feature = "board-lx06")]
    if cmd == "lx06-wake-contract" {
        println!("lx06-idle-wake-v1");
        return;
    }
    #[cfg(feature = "board-lx06")]
    if cmd == "lx06-switch-watch" {
        run_lx06_switch_watch(args);
        return;
    }
    if cmd == "aec-probe" {
        run_aec_probe();
        return;
    }
    #[cfg(target_os = "linux")]
    if cmd == "pcmprobe" {
        run_pcmprobe(args);
        return;
    }
    if cmd != "virtual" {
        eprintln!("unknown subcommand '{cmd}' (virtual | firmware)");
        std::process::exit(2);
    }

    let mut cfg = GatewayConfig::default();
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--core" => cfg.core_addr = args.next().expect("--core needs a host:port"),
            "--auth-token" => {
                let tok = args.next().expect("--auth-token needs a token");
                cfg.auth_token = if tok.trim().is_empty() {
                    None
                } else {
                    Some(tok)
                };
            }
            "--out" => cfg.out_dir = Some(PathBuf::from(args.next().expect("--out needs a dir"))),
            "--afplay" => cfg.afplay = true,
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
        }
    }
    if cfg.out_dir.is_none() {
        cfg.out_dir = Some(PathBuf::from("./audio-out"));
    }

    println!(
        "Home Mate Device Gateway {} — virtual dot2-d → {}",
        env!("CARGO_PKG_VERSION"),
        cfg.core_addr
    );
    println!(
        "commands: w(ake) | <text> | s(top) | mute/unmute | q(uit)   audio → {}",
        cfg.out_dir.as_ref().unwrap().display()
    );

    let device = Arc::new(Mutex::new(VirtualDot2d::new("dot2-d")));
    let (event_tx, event_rx) = mpsc::channel::<GatewayFrame>(64);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    // Connection actor (reconnects until shutdown).
    let conn = tokio::spawn(client::run(cfg, device.clone(), event_rx, shutdown_rx));

    // Stdin → events (async line reader).
    let stdin_dev = device.clone();
    let stdin = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut d = stdin_dev.lock().await;
            let trimmed = line.trim();
            let frame = match trimmed {
                "" => continue,
                "q" | "quit" | "exit" => break,
                "w" | "wake" => d.wake(),
                "s" | "stop" => d.button("action_press"),
                "mute" => d.mute(true),
                "unmute" => d.mute(false),
                text => d.utterance(text),
            };
            drop(d);
            if event_tx.send(frame).await.is_err() {
                break;
            }
        }
        let _ = shutdown_tx.send(()).await;
    });

    let _ = tokio::join!(conn, stdin);
    println!("bye");
}

#[cfg(not(feature = "board-dot2"))]
fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "firmware".into());
    match cmd.as_str() {
        "firmware" => run_firmware(args),
        "led-cal" => run_led_cal(args),
        #[cfg(feature = "board-lx06")]
        "lx06-wake-contract" => println!("lx06-idle-wake-v1"),
        #[cfg(feature = "board-lx06")]
        "lx06-switch-watch" => run_lx06_switch_watch(args),
        other => {
            eprintln!("unknown subcommand '{other}' (firmware | led-cal | lx06-wake-contract | lx06-switch-watch)");
            std::process::exit(2);
        }
    }
}

/// Passive non-grabbing `/dev/input/event0` monitor for LX06 when stock XiaoAi
/// is active: holding Play/Pause (`code = 114`) for >= 1.5s writes `"homemate\n"`
/// to `/data/hm/mode`. The native touchpad continues to receive its events.
#[cfg(all(feature = "board-lx06", target_os = "linux"))]
fn run_lx06_switch_watch(mut args: impl Iterator<Item = String>) {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::time::Instant;
    let dev = args
        .next()
        .unwrap_or_else(|| "/dev/input/event0".to_string());
    let mode_path = args.next().unwrap_or_else(|| "/data/hm/mode".to_string());
    let mut file = match std::fs::OpenOptions::new().read(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[lx06-switch-watch] open {dev} failed: {e}");
            std::process::exit(1);
        }
    };
    let fd = file.as_raw_fd();
    let mut pressed_at: Option<Instant> = None;
    let mut buf = [0u8; 24];
    loop {
        if let Some(start) = pressed_at {
            if start.elapsed().as_millis() >= 1500 {
                break;
            }
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = if pressed_at.is_some() { 100 } else { 1000 };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return;
        }
        if rc == 0 || (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }
        if file.read_exact(&mut buf).is_err() {
            return;
        }
        let kind = u16::from_le_bytes([buf[16], buf[17]]);
        let code = u16::from_le_bytes([buf[18], buf[19]]);
        let val = i32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        if kind == 0 && code == 3 {
            pressed_at = None;
        }
        if kind != 1 || code != 114 {
            continue;
        }
        match val {
            1 => {
                pressed_at = Some(Instant::now());
            }
            0 => {
                if let Some(start) = pressed_at.take() {
                    if start.elapsed().as_millis() >= 1500 {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if let Err(err) = home_mate_device_gateway::firmware::lx06::select_mode(
        std::path::Path::new(&mode_path),
        "homemate",
    ) {
        eprintln!("[lx06-switch-watch] failed to select dual wake: {err}");
        std::process::exit(1);
    }
}

#[cfg(all(feature = "board-lx06", not(target_os = "linux")))]
fn run_lx06_switch_watch(_args: impl Iterator<Item = String>) {
    eprintln!("[lx06-switch-watch] only supported on Linux");
    std::process::exit(2);
}

// --- LED calibration (D/T5 debt #6) -----------------------------------------------
//
// Light exactly ONE ring LED and hold it. The sweep/timing lives in the
// host-side tool (tools/led_calibrate.py over the serial console), NOT in
// this binary: a device-side sleep loop could reach a time64 syscall this
// 3.18 kernel lacks (the P0 watchdog root cause). One process = one lamp.
//
//   gateway led-cal 0..11 [--frame /sys/.../frame] [--map /data/.../led-map.json]
//   gateway led-cal off                     (dark ring)
fn run_led_cal(mut args: impl Iterator<Item = String>) {
    use home_mate_device_gateway::firmware::leds::{self, LedRing};
    let which = args.next().unwrap_or_else(|| {
        eprintln!("led-cal needs a ring index 0..11 or 'off'");
        std::process::exit(2);
    });
    let mut frame_path = "/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame".to_string();
    let mut map_path: Option<PathBuf> = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--frame" => frame_path = args.next().expect("--frame needs a path"),
            "--map" => map_path = Some(PathBuf::from(args.next().expect("--map needs a path"))),
            other => {
                eprintln!("unknown led-cal flag {other}");
                std::process::exit(2);
            }
        }
    }
    let mut ring = match map_path {
        Some(p) => LedRing::with_calibrated_map(&frame_path, &p),
        None => LedRing::new(&frame_path),
    };
    let rgb = if which == "off" {
        [(0u8, 0u8, 0u8); leds::RING_LEDS]
    } else {
        let idx: usize = which.parse().unwrap_or_else(|_| {
            eprintln!("led-cal index must be 0..11 or 'off' (got '{which}')");
            std::process::exit(2);
        });
        if idx >= leds::RING_LEDS {
            eprintln!(
                "led-cal index must be 0..{} (got {idx})",
                leds::RING_LEDS - 1
            );
            std::process::exit(2);
        }
        leds::calibration_frame(idx)
    };
    #[cfg(feature = "board-lx06")]
    let write_res = if frame_path == home_mate_device_gateway::firmware::lx06::LX06_LED_PATH
        || (unsafe { libc::access(c"/sys/devices/i2c-0/0-003a/led_rgb".as_ptr(), libc::W_OK) == 0 }
            && frame_path == "/sys/devices/soc/11007000.i2c/i2c-0/0-003f/frame")
    {
        frame_path = home_mate_device_gateway::firmware::lx06::LX06_LED_PATH.to_string();
        home_mate_device_gateway::firmware::lx06::Aw20054Ring::new(&frame_path).write(&rgb)
    } else {
        ring.write(&rgb)
    };
    #[cfg(not(feature = "board-lx06"))]
    let write_res = ring.write(&rgb);
    if let Err(e) = write_res {
        eprintln!("led-cal write to {frame_path} failed: {e}");
        std::process::exit(1);
    }
    println!("[led-cal] {which} written to {frame_path}");
}

// --- firmware (G5-2) ---------------------------------------------------------------
//
// The direct-occupation audio component: the EchoMuse-server successor with
// the controller role ABSORBED — owns the dot2-d hardware directly (ALSA PCM
// pair, is31fl3236 LED sysfs, evdev buttons, software volume gain,
// wake_lock) and speaks the northbound v0 protocol straight to the core.
// See docs/g5/direct-firmware-design-20261108.md.
//
//   gateway firmware --core 127.0.0.1:8932 [--hw-ref]

fn run_firmware(mut args: impl Iterator<Item = String>) {
    use home_mate_device_gateway::firmware::{open_hardware, Firmware, FirmwareConfig};
    // 2026-11-13: every firmware line goes through the bounded writer thread
    // from the first one — a full /run must cost log lines, never audio (see
    // `fwlog`). CLI errors below still use the direct fallback.
    home_mate_device_gateway::fwlog::init();
    let mut cfg = FirmwareConfig::default();
    let mut explicit_device_id: Option<String> = None;
    let mut explicit_model = false;
    let mut explicit_volume_cap = false;
    let mut explicit_barge_in_db = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--core" => cfg.core_addr = args.next().expect("--core needs host:port"),
            "--allow-lan-core" => cfg.allow_lan_core = true,
            "--auth-token" => {
                let tok = args.next().expect("--auth-token needs a token");
                let trimmed = tok.trim();
                cfg.auth_token = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            "--device-id" => {
                explicit_device_id = Some(args.next().expect("--device-id needs an id"));
            }
            "--model" => {
                cfg.model = args.next().expect("--model needs a name");
                explicit_model = true;
            }
            "--state-dir" => {
                cfg.state_dir =
                    std::path::PathBuf::from(args.next().expect("--state-dir needs a dir"))
            }
            "--volume-cap" => {
                cfg.volume_cap = args
                    .next()
                    .expect("--volume-cap needs 0..1")
                    .parse()
                    .expect("volume cap must be a number");
                explicit_volume_cap = true;
            }
            "--hw-ref" => cfg.hw_ref = true,
            "--aec" => cfg.aec = true,
            "--aec-tail" => {
                let t: usize = args
                    .next()
                    .expect("--aec-tail needs a sample count")
                    .parse()
                    .expect("--aec-tail must be a number");
                if t == 0 || !t.is_multiple_of(home_mate_device_gateway::firmware::aec::FRAME) {
                    eprintln!("--aec-tail must be a positive multiple of FRAME (320)");
                    std::process::exit(2);
                }
                cfg.aec = true;
                cfg.aec_tail = t;
            }
            "--aec-measure" => {
                cfg.aec = true;
                cfg.aec_measure = true;
            }
            "--kws" => {
                // D/T4: local keyword spotting sidecar (post-AEC reflex).
                cfg.kws = true;
            }
            "--kws-dir" => {
                cfg.kws = true;
                cfg.kws_dir =
                    std::path::PathBuf::from(args.next().expect("--kws-dir needs a directory"));
            }
            "--kws-cpu" => {
                cfg.kws_cpu = args
                    .next()
                    .expect("--kws-cpu needs a CPU index")
                    .parse()
                    .expect("--kws-cpu must be a number");
            }
            "--barge-in" => {
                // "--barge-in off" disables the semantic barge-in detector;
                // the detector is on by default (D/T5) and needs no flag.
                match args.next().as_deref() {
                    Some("off") => cfg.barge_in = false,
                    Some(other) => {
                        eprintln!("--barge-in takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--barge-in needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--barge-in-db" => {
                cfg.barge_in_db = args
                    .next()
                    .expect("--barge-in-db needs a dBFS value")
                    .parse()
                    .expect("--barge-in-db must be a number");
                explicit_barge_in_db = true;
            }
            "--barge-in-margin" => {
                // Near-end criterion: within this many dB OF THE REFERENCE
                // (R1 Step 3, default 15). The old absolute onset is gone.
                let v: f32 = args
                    .next()
                    .expect("--barge-in-margin needs dB")
                    .parse()
                    .expect("--barge-in-margin must be a number");
                assert!(
                    (0.0..=40.0).contains(&v),
                    "--barge-in-margin takes 0..40 dB"
                );
                cfg.barge_in_margin = v;
            }
            "--barge-in-converged" => {
                // Echo-only blocks of demonstrated suppression before a
                // near-end fact is allowed (0 disables the convergence gate
                // — guard calibration only).
                cfg.barge_in_converged = args
                    .next()
                    .expect("--barge-in-converged needs blocks")
                    .parse()
                    .expect("--barge-in-converged must be a number");
            }
            "--barge-in-corr" => {
                let v: f32 = args
                    .next()
                    .expect("--barge-in-corr needs 0..1")
                    .parse()
                    .expect("--barge-in-corr must be a number");
                assert!(
                    (0.0..=1.0).contains(&v),
                    "--barge-in-corr must be within 0..1"
                );
                cfg.barge_in_corr = v;
            }
            "--barge-in-settle" => {
                // Playback settle window (D/T5 R1): onsets this soon after
                // the voice channel starts are the answer's own TTS onset.
                cfg.barge_in_settle = args
                    .next()
                    .expect("--barge-in-settle needs ms")
                    .parse()
                    .expect("--barge-in-settle must be a number");
            }
            "--barge-in-stats" => {
                // Calibration telemetry only (corr distribution); 0 = off.
                cfg.barge_in_stats = args
                    .next()
                    .expect("--barge-in-stats needs ms")
                    .parse()
                    .expect("--barge-in-stats must be a number");
            }
            "--barge-in-protect" => {
                // R1d local stop: the device refuses to cut a voice play
                // younger than this (mirrors the core's 800 ms protection).
                cfg.barge_in_protect = args
                    .next()
                    .expect("--barge-in-protect needs ms")
                    .parse()
                    .expect("--barge-in-protect must be a number");
            }
            "--aec-measure-secs" => {
                cfg.aec = true;
                cfg.aec_measure = true;
                cfg.aec_measure_secs = args
                    .next()
                    .expect("--aec-measure-secs needs seconds")
                    .parse()
                    .expect("--aec-measure-secs must be a number");
            }
            "--loopback-probe" => {
                // Task B: DAC-side write track + ch8 track to /run/hm-loop,
                // compared by the core's playback_loopback diagnostic. Off
                // by default (no /run writes in normal operation).
                cfg.loopback_probe = true;
            }
            "--loudness-bass" => {
                // D 1.1 A/B: scale the loudness bass shelf (0 = bypassed).
                let v: f32 = args
                    .next()
                    .expect("--loudness-bass needs 0..1")
                    .parse()
                    .expect("--loudness-bass must be a number");
                assert!((0.0..=1.0).contains(&v), "--loudness-bass takes 0..1");
                cfg.loudness_bass = v;
            }
            "--loudness" => {
                // "--loudness off" A/Bs the flat chain; the stage is on by
                // default (D stage 2026-11-10).
                match args.next().as_deref() {
                    Some("off") => cfg.loudness = false,
                    // M6c: "on" is explicit so an A/B line can name the
                    // state it wants instead of relying on the default.
                    Some("on") => cfg.loudness = true,
                    Some(other) => {
                        eprintln!("--loudness takes 'off' or 'on' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--loudness needs 'off' or 'on'");
                        std::process::exit(2);
                    }
                }
            }
            "--loudness-curve" => {
                // M6c A/B: the stock (official) curve fitted from the
                // device's own EQ_50 FIR, the RUI ladder (default), or a
                // user-defined peaking chain (--eq-band).
                match args.next().as_deref() {
                    Some("official") => cfg.loudness_curve = Some(loudness::Curve::Official),
                    Some("rui") => cfg.loudness_curve = Some(loudness::Curve::Rui),
                    Some("custom") => cfg.loudness_curve = Some(loudness::Curve::Custom),
                    Some("echomuse") => cfg.loudness_curve = Some(loudness::Curve::EchoMuse),
                    Some(other) => {
                        eprintln!("--loudness-curve takes 'rui', 'official', 'custom' or 'echomuse' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!(
                            "--loudness-curve needs 'rui', 'official', 'custom' or 'echomuse'"
                        );
                        std::process::exit(2);
                    }
                }
            }
            "--eq-band" => {
                // A user curve band: freq_hz:gain_db:q (repeatable, max 10).
                // `--loudness-curve custom` is required for these to apply.
                let spec = args.next().unwrap_or_default();
                let parts: Vec<&str> = spec.split(':').collect();
                let parsed = if parts.len() == 3 {
                    match (
                        parts[0].parse::<f32>(),
                        parts[1].parse::<f32>(),
                        parts[2].parse::<f32>(),
                    ) {
                        (Ok(f), Ok(g), Ok(q)) => Some((f, g, q)),
                        _ => None,
                    }
                } else {
                    None
                };
                match parsed {
                    Some((f, g, q))
                        if (20.0..=16000.0).contains(&f)
                            && (-24.0..=24.0).contains(&g)
                            && (0.3..=8.0).contains(&q) =>
                    {
                        cfg.eq_bands.push(loudness::CustomBand {
                            freq: f,
                            gain_db: g,
                            q,
                        });
                        assert!(cfg.eq_bands.len() <= 10, "--eq-band takes at most 10 bands");
                    }
                    _ => {
                        eprintln!("--eq-band takes freq:gain:q with freq 20..16000 Hz, gain -24..24 dB, Q 0.3..8 (got '{spec}')");
                        std::process::exit(2);
                    }
                }
            }
            "--loudness-mbcl" => {
                // M6c A/B: the stock 4-band MBCL replaces the broadband
                // limiter (only with the official curve).
                match args.next().as_deref() {
                    Some("on") => cfg.loudness_mbcl = Some(true),
                    Some("off") => cfg.loudness_mbcl = Some(false),
                    Some(other) => {
                        eprintln!("--loudness-mbcl takes 'on' or 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--loudness-mbcl needs 'on' or 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--play-buffer" => {
                // D/T5 debt #4: A/B the 170ms buffer on ONE binary. Frames
                // must be a positive multiple of the period (2048) and at
                // least two periods — otherwise ALSA refuses hw_params.
                let v: u32 = args
                    .next()
                    .expect("--play-buffer needs a frame count")
                    .parse()
                    .expect("--play-buffer must be a number");
                let period = home_mate_device_gateway::firmware::PLAY_PERIOD;
                assert!(
                    v >= period * 2 && v.is_multiple_of(period),
                    "--play-buffer must be a multiple of {period} and >= {} (got {v})",
                    period * 2
                );
                cfg.play_buffer = v;
            }
            "--play-watchdog" => {
                // 2026-11-13: a play whose DAC write stopped making progress
                // is failed by name, then (M4) the process exits for a
                // supervisor restart. 0 disables the watchdog; default 10 s.
                let v: u64 = args
                    .next()
                    .expect("--play-watchdog needs seconds")
                    .parse()
                    .expect("--play-watchdog must be a number");
                cfg.play_watchdog_ms = v.saturating_mul(1000);
            }
            "--capture-watchdog" => {
                // M4: no successful capture block for this long = audio fault.
                // 0 disables it; default 10 s.
                let v: u64 = args
                    .next()
                    .expect("--capture-watchdog needs seconds")
                    .parse()
                    .expect("--capture-watchdog must be a number");
                cfg.capture_watchdog_ms = v.saturating_mul(1000);
            }
            "--audio-fault-exit" => {
                // M4: contract tests run the fault path in-process and must
                // not lose the test runner; production keeps the default (on).
                match args.next().as_deref() {
                    Some("off") => cfg.exit_on_audio_fault = false,
                    Some(other) => {
                        eprintln!("--audio-fault-exit takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--audio-fault-exit needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--fault-inject-ms" => {
                // M4 test hook: request an audio fault N ms after boot through
                // the real receipt → exit → supervisor-restart path. 0 = off.
                cfg.fault_inject_ms = args
                    .next()
                    .expect("--fault-inject-ms needs milliseconds")
                    .parse()
                    .expect("--fault-inject-ms must be a number");
            }
            "--boot-health" => {
                // D/T5 debt #1: off disables the boot capture sentinel.
                match args.next().as_deref() {
                    Some("off") => cfg.boot_health = false,
                    Some(other) => {
                        eprintln!("--boot-health takes 'off' (got '{other}')");
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--boot-health needs 'off'");
                        std::process::exit(2);
                    }
                }
            }
            "--mic-channels" => {
                let spec = args.next().expect("--mic-channels needs 0,1,2");
                cfg.mic_channels = spec
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if cfg.mic_channels.is_empty() {
                    eprintln!("--mic-channels produced no valid indices");
                    std::process::exit(2);
                }
            }
            "--ref-channel" => {
                cfg.ref_channel = args
                    .next()
                    .expect("--ref-channel needs an index")
                    .parse()
                    .expect("ref channel must be a number")
            }
            other => {
                eprintln!("unknown firmware flag {other}");
                std::process::exit(2);
            }
        }
    }
    // #22: Production firmware startup must have an explicit stable_id
    // (--device-id or <state_dir>/device-id); never silently fall back to
    // "dot2-d".
    cfg.device_id = match home_mate_device_gateway::firmware::resolve_production_device_id(
        explicit_device_id.as_deref(),
        &cfg.state_dir,
    ) {
        Ok(id) => id,
        Err(msg) => {
            home_mate_device_gateway::fwlog!("[firmware] {msg}");
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    // #25: Load per-unit profile (preset or <state_dir>/profile.json).
    let loaded_profile = home_mate_device_gateway::firmware::profile::DeviceProfile::load_from_dir(
        &cfg.state_dir,
        &cfg.device_id,
    );
    cfg.apply_profile(
        loaded_profile,
        explicit_model,
        explicit_volume_cap,
        explicit_barge_in_db,
    );
    // #35: Load persisted LED night-mode and orientation preferences if present.
    if let Some(saved_led) =
        home_mate_device_gateway::firmware::leds::load_led_config(&cfg.state_dir)
    {
        cfg.night_mode = saved_led.night_mode;
        cfg.led_orientation = saved_led.led_orientation;
    }
    if !(0.0..=1.0).contains(&cfg.volume_cap) {
        eprintln!("--volume-cap must be within 0..1 (got {})", cfg.volume_cap);
        std::process::exit(2);
    }
    if !home_mate_device_gateway::firmware::core_addr_allowed(&cfg.core_addr, cfg.allow_lan_core) {
        if cfg.allow_lan_core {
            eprintln!(
                "refusing --core {} — even with --allow-lan-core, only loopback or private LAN/Tailscale/ULA IP:port addresses are permitted",
                cfg.core_addr
            );
        } else {
            eprintln!(
                "refusing --core {} — the firmware only dials a core on this speaker (127.0.0.1, ::1, or localhost) by default; pass --allow-lan-core for a private LAN/Tailscale/ULA core",
                cfg.core_addr
            );
        }
        std::process::exit(2);
    }
    if !std::path::Path::new(&cfg.led_frame_path).exists() {
        eprintln!(
            "LED frame node {} not found — is this the dot2-d board?",
            cfg.led_frame_path
        );
        std::process::exit(2);
    }
    // 2026-11-13 (M4): a TERM from init is an INTENTIONAL stop, not a crash.
    // Without a handler the default disposition kills the process with 143,
    // so the supervisor's "exit 0 = intentional" rule could never apply; on
    // shutdown it relied solely on the (previously wrong) /run marker.
    // Installed BEFORE the codec bring-up (which opens PCMs and takes
    // seconds): a TERM that lands inside that window must also be an
    // intentional stop, not a 143 the supervisor counts as a fast exit.
    // The handler is async-signal-safe: it only _exit(0). The supervisor's own
    // TERM trap does amp_off, so no hardware teardown is needed here.
    #[cfg(target_os = "linux")]
    unsafe {
        // NOTE: a nested `extern "C" fn` does NOT inherit the enclosing
        // unsafe block — the `_exit` call needs its own (this only showed up
        // in the armv7 build; the host build never compiles this cfg).
        extern "C" fn on_term(_sig: libc::c_int) {
            unsafe { libc::_exit(0) };
        }
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
    let hw = match open_hardware(&cfg) {
        Ok(h) => h,
        Err(e) => {
            // EBUSY here usually means the EchoMuse server still holds the
            // PCM devices — the supervisor counts this as a fast-exit.
            eprintln!("cannot open hardware: {e}");
            std::process::exit(1);
        }
    };
    #[cfg(feature = "board-dot2")]
    {
        // The firmware builds its own single-thread runtime; main is #[tokio::main],
        // so run it on a dedicated thread (its exit code drives the supervisor).
        let handle = std::thread::spawn(move || {
            let fw = Firmware::new(cfg, hw);
            fw.run()
        });
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("firmware failed: {e}");
                std::process::exit(1);
            }
            Err(_) => {
                eprintln!("firmware thread panicked");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "board-dot2"))]
    {
        // The firmware builds its own single-thread runtime inside Firmware::run().
        let fw = Firmware::new(cfg, hw);
        if let Err(e) = fw.run() {
            eprintln!("firmware failed: {e}");
            std::process::exit(1);
        }
    }
}

// --- pcmprobe (G5-2 diagnostics) ----------------------------------------------------
//
// Empirical ioctl probe for the dot2-d kernel (arm64 kernel + arm32
// userspace): verifies the control-device encoding against tinymix's known-
// good behavior, then brute-forces PCM hw_params across ioctl encodings and
// open flags to find what the kernel's compat layer actually accepts.

#[cfg(all(feature = "board-dot2", target_os = "linux"))]
fn run_pcmprobe(_args: impl Iterator<Item = String>) {
    use std::os::unix::io::AsRawFd;
    fn ioc_generic(dir: u32, ty: u8, nr: u8, size: u32) -> libc::c_ulong {
        ((dir << 30) | (size << 16) | ((ty as u32) << 8) | (nr as u32)) as libc::c_ulong
    }
    fn ioc_arm(dir: u32, ty: u8, nr: u8, size: u32) -> libc::c_ulong {
        ((dir << 29) | (size << 16) | ((ty as u32) << 8) | (nr as u32)) as libc::c_ulong
    }
    // Diagnostic ioctl()s below are compiled into the Linux host binary too.
    // glibc wants the request as c_ulong; musl wants c_int. See alsa.rs.
    #[cfg(target_env = "musl")]
    unsafe fn probe_ioctl(
        fd: libc::c_int,
        req: libc::c_ulong,
        arg: *mut libc::c_void,
    ) -> libc::c_int {
        libc::ioctl(fd, req as libc::c_int, arg)
    }
    #[cfg(not(target_env = "musl"))]
    unsafe fn probe_ioctl(
        fd: libc::c_int,
        req: libc::c_ulong,
        arg: *mut libc::c_void,
    ) -> libc::c_int {
        libc::ioctl(fd, req, arg)
    }
    unsafe {
        // 0. Sanity: ioctl on stdin (a real tty from the adb session).
        // If this fails too, the whole syscall path is broken for us.
        let mut ws = [0u8; 8];
        let r = libc::ioctl(0, libc::TIOCGWINSZ, ws.as_mut_ptr());
        println!(
            "tty TIOCGWINSZ rc={r} errno={}",
            std::io::Error::last_os_error()
        );
        // 0b. fstat on the control device node to prove what we opened.
        let ctl2 = libc::open(
            c"/dev/snd/controlC0".as_ptr(),
            libc::O_RDWR | libc::O_CLOEXEC,
        );
        let mut st: libc::stat = std::mem::zeroed();
        let fr = libc::fstat(ctl2, &mut st);
        println!(
            "controlC0 open fd={ctl2} fstat rc={fr} dev={:#x} rdev={:#x} mode={:#o}",
            st.st_dev, st.st_rdev, st.st_mode
        );
        if ctl2 >= 0 {
            libc::close(ctl2);
        }

        // 1. Control device: SNDRV_CTL_IOCTL_ELEM_LIST = _IOWR('U', 0x10, 32B-ish);
        //    tinymix (arm32) works on this kernel, so this validates encoding.
        let ctl = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/snd/controlC0");
        match ctl {
            Ok(f) => {
                for (name, req) in [
                    ("ctl pversion generic", ioc_generic(2, b'U', 0x00, 4)),
                    ("ctl elem_list generic", ioc_generic(3, b'U', 0x10, 80)),
                    ("ctl elem_list arm", ioc_arm(3, b'U', 0x10, 80)),
                ] {
                    let mut buf = [0u8; 64];
                    let r = probe_ioctl(f.as_raw_fd(), req, buf.as_mut_ptr().cast());
                    println!(
                        "{name} req={req:#x} rc={r} errno={}",
                        std::io::Error::last_os_error()
                    );
                }
            }
            Err(e) => println!("controlC0 open: {e}"),
        }

        // 2. PCM capture across encodings × open flags. hw_params = _IOWR('A',0x11,392).
        for (flagname, flags) in [("rdwr", libc::O_RDWR), ("rdonly", libc::O_RDONLY)] {
            let fd = libc::open(c"/dev/snd/pcmC0D24c".as_ptr(), flags | libc::O_CLOEXEC);
            if fd < 0 {
                println!("open {flagname}: {}", std::io::Error::last_os_error());
                continue;
            }
            // PCM fd identity: pversion must reply on a real PCM fd.
            {
                let mut ver = 0u32;
                let r = probe_ioctl(
                    fd,
                    ioc_generic(2, b'A', 0x00, 4),
                    (&mut ver as *mut u32).cast(),
                );
                let e = std::io::Error::last_os_error();
                println!("pcm pversion {flagname} rc={r} ver={ver:#x} errno={e}");
                // info ioctl: _IOR('A',0x01, struct snd_pcm_info ~ 0x60 bytes mainline
                let mut info = [0u8; 0x70];
                for sz in [0x60u32, 0x68, 0x70, 0x58] {
                    let r =
                        probe_ioctl(fd, ioc_generic(2, b'A', 0x01, sz), info.as_mut_ptr().cast());
                    let e = std::io::Error::last_os_error();
                    let raw = e.raw_os_error().unwrap_or(0);
                    if r != -1 || raw != 25 {
                        println!("pcm info {flagname} size={sz:#x} rc={r} errno={e} <<<");
                    }
                }
            }
            // Literals lifted from the EchoMuse Go binary's constant pool
            // (scan: words with low16==0x4111). Try them verbatim.
            for (name, lit) in [
                ("emuse 0x00e04111", 0x00e04111u32),
                ("emuse 0x01e04111", 0x01e04111u32),
                ("emuse 0x02e04111", 0x02e04111u32),
                ("emuse 0x00e24111", 0x00e24111u32),
            ] {
                let mut buf = [0u8; 1024];
                let r = probe_ioctl(fd, lit as libc::c_ulong, buf.as_mut_ptr().cast());
                let e = std::io::Error::last_os_error();
                let raw = e.raw_os_error().unwrap_or(0);
                println!(
                    "pcm literal {flagname}/{name} rc={r} errno={e}{}",
                    if r != -1 || raw != 25 {
                        " <<< DISPATCH"
                    } else {
                        ""
                    }
                );
            }
            // Refine at the confirmed size 608, fully open, dump the
            // kernel's constraint reply in full — the layout decoder.
            {
                let req = ioc_generic(3, b'A', 0x10, 608u32);
                let mut buf = [0u8; 608];
                // open ALL masks (offsets guess: 4..100) and ALL intervals
                // (120..264): min=0,max=0xFFFFFFFF,integer bit? flags=0.
                for i in 4..100u32 {
                    buf[i as usize] = 0xFF;
                }
                for w in 0..12u32 {
                    let o = (120 + w * 12) as usize;
                    buf[o..o + 4].copy_from_slice(&0u32.to_le_bytes());
                    buf[o + 4..o + 8].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
                }
                // request refine of all mainline params
                let rmask: u32 = 0x7FFF;
                buf[300..304].copy_from_slice(&rmask.to_le_bytes());
                let r = probe_ioctl(fd, req, buf.as_mut_ptr().cast());
                println!("REFINE608 rc={r} errno={}", std::io::Error::last_os_error());
                if r == 0 {
                    for row in 0..19 {
                        let o = row * 32;
                        let mut line = format!("{:03x}:", o);
                        for w in 0..8 {
                            let a = o + w * 4;
                            if a + 4 <= 608 {
                                let v = u32::from_le_bytes(buf[a..a + 4].try_into().unwrap());
                                line.push_str(&format!(" {:08x}", v));
                            }
                        }
                        println!("{}", line);
                    }
                }
            }
            // DECISIVE v8 — the real tinyalsa flow from 32-bit compat:
            // param_init → HW_REFINE (kernel legitimizes masks/info) → pin
            // our values → HW_PARAMS → prepare → start → readi.
            {
                let mut buf = [0u8; 604];
                let b = &mut buf;
                b[0] = 0x04;
                for off in [4usize, 8, 0x24, 0x28, 0x44, 0x48] {
                    b[off..off + 4].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
                }
                for i in 0..12usize {
                    let o = 0x104 + i * 12;
                    b[o + 4..o + 8].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
                }
                b[0x200..0x204].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
                let refine_req = ioc_generic(3, b'A', 0x10, 604u32);
                let rr = probe_ioctl(fd, refine_req, b.as_mut_ptr().cast());
                println!(
                    "v8 refine rc={rr} errno={}",
                    std::io::Error::last_os_error()
                );
                if rr == 0 {
                    println!(
                        "  masks: acc={:08x} fmt0={:08x} fmt1={:08x} sub={:08x} info={:08x}",
                        u32::from_le_bytes(b[4..8].try_into().unwrap()),
                        u32::from_le_bytes(b[0x24..0x28].try_into().unwrap()),
                        u32::from_le_bytes(b[0x28..0x2C].try_into().unwrap()),
                        u32::from_le_bytes(b[0x44..0x48].try_into().unwrap()),
                        u32::from_le_bytes(b[0x208..0x20C].try_into().unwrap())
                    );
                    for (name, param) in [
                        ("ch", 5usize),
                        ("rate", 6),
                        ("period", 8),
                        ("periods", 10),
                        ("buffer", 12),
                    ] {
                        let o = 0x104 + (param - 3) * 12;
                        let mn = u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                        let mx = u32::from_le_bytes(b[o + 4..o + 8].try_into().unwrap());
                        println!("  {name}: {mn}..{mx}");
                    }
                    // Pin our targets on the REFINED buffer.
                    for (param, v) in [(5usize, 9u32), (6, 16000), (8, 512), (10, 5), (12, 2560)] {
                        let o = 0x104 + (param - 3) * 12;
                        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
                        b[o + 4..o + 8].copy_from_slice(&v.to_le_bytes());
                    }
                    let req = ioc_generic(3, b'A', 0x11, 604u32);
                    let r = probe_ioctl(fd, req, b.as_mut_ptr().cast());
                    println!(
                        "v8 hw_params rc={r} errno={}",
                        std::io::Error::last_os_error()
                    );
                    if r == 0 {
                        println!("  *** CONFIGURED ***");
                        probe_ioctl(fd, 0x4140, std::ptr::null_mut());
                        probe_ioctl(fd, 0x4142, std::ptr::null_mut());
                        let mut pcm = vec![0u8; 1280 * 9 * 3];
                        #[repr(C)]
                        struct Xferi32 {
                            result: i32,
                            buf: *mut u8,
                            frames: u32,
                        }
                        let mut x = Xferi32 {
                            result: 0,
                            buf: pcm.as_mut_ptr(),
                            frames: 1280,
                        };
                        let readi_req = ioc_generic(2, b'A', 0x51, 12);
                        let rc2 = probe_ioctl(fd, readi_req, (&mut x as *mut Xferi32).cast());
                        println!("  readi rc={rc2} frames={}", x.frames);
                        if rc2 == 0 {
                            let mut nz = 0;
                            for bb in pcm.iter() {
                                if *bb != 0 {
                                    nz += 1;
                                }
                            }
                            println!("  *** CAPTURED: nonzero {}/{} ***", nz, 1280 * 9 * 3);
                        }
                    }
                }
            }
            // Size sweep: this FireOS kernel's snd_pcm_hw_params may differ
            // from mainline (392). The cmd encodes the size; the kernel
            // compares the full cmd, so any size != its own → ENOTTY. Any
            // other reply (EINVAL etc.) means DISPATCH found.
            for size in (4..2048u32).step_by(4) {
                let req = ioc_generic(3, b'A', 0x11, size);
                let mut buf = [0u8; 536];
                let r = probe_ioctl(fd, req, buf.as_mut_ptr().cast());
                let e = std::io::Error::last_os_error();
                let raw = e.raw_os_error().unwrap_or(0);
                if r != -1 || raw != 25 {
                    println!("pcm hw_params {flagname} size={size} req={req:#x} rc={r} errno={e} <<< DISPATCH");
                }
            }
            println!("pcm hw_params {flagname} size sweep done (only non-ENOTTY shown)");
            libc::close(fd);
        }
    }
}

// --- serve-file: minimal HTTP GET server for device-side pulls (G6) ----------------
#[cfg(feature = "board-dot2")]
async fn run_serve_file(mut args: impl Iterator<Item = String>) {
    let usage = "usage: gateway serve-file <bind_addr> <dir>";
    let bind = args.next().expect(usage);
    let dir = std::path::PathBuf::from(args.next().expect(usage));
    let canonical = std::fs::canonicalize(&dir).expect("dir must exist");
    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!(
        "[serve-file] {bind} -> {} (device: wget http://<host>:<port>/<name>)",
        canonical.display()
    );
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let root = canonical.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut buf = [0u8; 2048];
            let Ok(n) = sock.read(&mut buf).await else {
                return;
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let Some(path) = req.lines().next().and_then(|l| l.split_whitespace().nth(1)) else {
                return;
            };
            let name = path.trim_matches('/');
            if name.is_empty() || name.contains("..") {
                let _ = sock
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
            let full = root.join(name);
            match std::fs::canonicalize(&full) {
                Ok(c) if c.starts_with(&root) => match std::fs::read(&c) {
                    Ok(data) => {
                        let hdr = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = sock.write_all(hdr.as_bytes()).await;
                        let _ = sock.write_all(&data).await;
                    }
                    Err(e) => {
                        let body = format!("{e}");
                        let hdr = format!(
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(hdr.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                    }
                },
                _ => {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });
    }
}

// --- aec-probe: does ch8 == every-3rd-written-sample on THIS unit? (G6) -------------
//
// Plays a deterministic LFSR tone out the speaker while capturing the 9ch
// array, then answers three questions about the ch8 loopback:
//   1. EXACT: longest run of samples agreeing with every-3rd-written at
//      tolerance +/-2 (the dealias premise, "to 1 LSB" on the living room
//      unit).
//   2. ALIGNMENT: where that best run sits (output-sample lag).
//   3. CORRELATION: normalized peak at the same decimation — high
//      correlation without exact match means the loopback carries a
//      transformation and the sample-exact match can never lock here.
// Standalone: no core — stop the supervisor first (it owns the PCMs).
#[cfg(feature = "board-dot2")]
fn run_aec_probe() {
    use home_mate_device_gateway::firmware::{DeviceHardware, FirmwareConfig, Hardware};
    let hw = match DeviceHardware::open(&FirmwareConfig::default()) {
        Ok(h) => std::sync::Arc::new(h),
        Err(e) => {
            eprintln!("open: {e}");
            std::process::exit(1);
        }
    };
    const RATE_OUT: usize = 48000;
    const RATE_IN: usize = 16000;
    const SECS: usize = 5;
    // LFSR pseudo-noise: broadband, deterministic, never silent.
    let mut lfsr: u32 = 0xACE1_2345;
    let mut tone = vec![0i16; RATE_OUT * SECS];
    for s in tone.iter_mut() {
        let bit = ((lfsr ^ (lfsr >> 2) ^ (lfsr >> 3) ^ (lfsr >> 5)) & 1) as i16;
        lfsr >>= 1;
        *s = bit * 12000 - 6000;
    }
    let written: std::sync::Arc<std::sync::Mutex<Vec<i16>>> = Default::default();
    // Writer: 2048-frame stereo periods of the tone.
    {
        let hw = hw.clone();
        let wlog = written.clone();
        std::thread::spawn(move || {
            let mut off = 0usize;
            while off < tone.len() {
                let end = (off + 2048).min(tone.len());
                let mut stereo = Vec::with_capacity((end - off) * 2);
                for s in &tone[off..end] {
                    stereo.push(*s);
                    stereo.push(*s);
                }
                let _ = hw.playback_write(&stereo, 2048);
                wlog.lock().unwrap().extend_from_slice(&stereo);
                off = end;
            }
        });
    }
    // Reader: 9ch blocks until the tone window is covered.
    let frames = RATE_IN * SECS;
    let mut raw = vec![0u8; frames * 27];
    let mut got = 0usize;
    while got < frames {
        let want = 1280.min(frames - got);
        match hw.capture_read(&mut raw[got * 27..(got + want) * 27], want as u32) {
            Ok(n) => got += n as usize,
            Err(e) => {
                eprintln!("capture: {e}");
                break;
            }
        }
    }
    let out = written.lock().unwrap().clone();
    let ch8: Vec<i16> = raw
        .as_chunks::<27>()
        .0
        .iter()
        .map(|f| {
            let s = &f[24..27];
            let u = (s[0] as i32) | ((s[1] as i32) << 8) | ((s[2] as i32) << 16);
            (((u << 8) >> 8) >> 8) as i16
        })
        .collect();
    let left: Vec<i16> = out.as_chunks::<2>().0.iter().map(|p| p[0]).collect();
    println!(
        "captured {} frames, written {} output samples",
        ch8.len(),
        left.len()
    );
    let Some(start) = (0..ch8.len()).find(|&i| ch8[i].abs() > 64) else {
        println!("ch8 NEVER WENT LOUD — loopback silent? (amp off / wrong channel)");
        return;
    };
    let n = 512.min(ch8.len() - start);
    let r = &ch8[start..start + n];
    let mut best_run = 0usize;
    let mut best_pos: i64 = -1;
    let mut best_corr = 0f64;
    let mut best_corr_pos: i64 = -1;
    let span = left.len().saturating_sub(3 * n);
    for p in 0..span {
        let mut run = 0usize;
        let mut max_run = 0usize;
        let mut dot = 0f64;
        let mut energy = 0f64;
        for i in 0..n {
            let w = left[p + 3 * i] as i32;
            if (r[i] as i32 - w).abs() <= 2 {
                run += 1;
                max_run = max_run.max(run);
            } else {
                run = 0;
            }
            dot += r[i] as f64 * w as f64;
            energy += (w as f64) * (w as f64);
        }
        if max_run > best_run {
            best_run = max_run;
            best_pos = p as i64 - 3 * start as i64;
        }
        let corr = dot / energy.sqrt().max(1.0);
        if corr > best_corr {
            best_corr = corr;
            best_corr_pos = p as i64 - 3 * start as i64;
        }
    }
    println!(
        "EXACT best run {best_run}/{} at out-lag {best_pos}; CORR peak {:.3} at lag {best_corr_pos}",
        n, best_corr
    );
    if best_run >= 64 {
        println!("VERDICT: premise HOLDS on this unit (dealias can lock)");
    } else if best_corr > 0.8 {
        println!(
            "VERDICT: loopback carries a TRANSFORMATION (corr {best_corr:.3}, not sample-exact)"
        );
    } else {
        println!("VERDICT: no relationship found — ch8 is not this output's loopback as assumed");
    }
}
