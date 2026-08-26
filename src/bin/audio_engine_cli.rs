//! Headless Reference Command-Line Player for the Independent Audio Engine.
//!
//! Demonstrates how an external host application embeds and controls the audio
//! engine via [`AudioEngine`], [`EngineHandle`], [`AudioSource`], and [`EngineEvent`].

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use engine::events::EngineEvent;
use engine::{AudioEngine, EngineConfig, EngineHandle};

fn print_help() {
    println!("\n=== Headless Audio Engine CLI Commands ===");
    println!("  open <file-or-uri>   - Open and play an audio source (file path or URI)");
    println!("  queue <file>          - Add a file to the playback queue");
    println!("  clear                 - Clear the playback queue");
    println!("  next                  - Skip to the next queue entry");
    println!("  prev                  - Skip to the previous queue entry");
    println!("  shuffle [on|off]      - Toggle shuffle mode");
    println!("  repeat [off|all|one]  - Set repeat mode");
    println!("  play                  - Resume or start playback");
    println!("  pause                 - Pause playback");
    println!("  stop                  - Stop playback and reset playhead");
    println!("  seek <seconds>        - Seek to position in seconds (e.g. seek 30.5)");
    println!("  volume <val>          - Set linear volume [0.0 - 1.0] or dB (e.g. 0.8 or -6db)");
    println!("  speed <multiplier>    - Set speed multiplier (e.g. speed 1.25)");
    println!("  eq off|on|<preset>    - Enable, disable, or load an EQ preset");
    println!("  eq-band <n> <freq> <gain> <q> [on|off] - Set a specific EQ band");
    println!("  levels                - Show real-time peak/RMS levels");
    println!("  scan <file>           - Scan a file for EBU R128 loudness");
    println!("  capture start [file]  - Start recording system audio (WASAPI loopback)");
    println!("  capture stop          - Stop recording and finalize the WAV file");
    println!("  devices               - List available audio output endpoints");
    println!("  device <name>         - Switch active output endpoint (or 'default')");
    println!("  info                  - Display atomic playback telemetry snapshot");
    println!("  events                - Drain and display discrete engine events");
    println!("  help                  - Show this help summary");
    println!("  quit | exit           - Gracefully shutdown engine and exit\n");
}

fn print_info(handle: &EngineHandle) {
    let info = handle.playback_info();
    let vol_db = if info.volume > 1e-6 {
        20.0 * info.volume.log10()
    } else {
        -100.0
    };
    println!("\x1b[1m--- Playback State Snapshot ---\x1b[0m");
    println!(
        "  Source:       {}",
        match &info.current_source {
            Some(s) => s.to_string(),
            None => "<none>".to_string(),
        }
    );
    println!("  State:        {:?}", info.state);
    println!(
        "  Playhead:     {:.2}s / {:.2}s",
        info.position_secs, info.duration_secs
    );
    println!("  Compensated:  {:.2}s", info.position_secs_compensated);
    println!("  Sample Rate:  {} Hz", info.sample_rate);
    println!("  Volume:       {:.2} ({:.1} dB)", info.volume, vol_db);
    println!("  Speed:        {:.2}x", info.speed);
    println!("  Latency:      {:.2} ms", info.latency_ms);
    println!("  Bit-Perfect:  {}", info.bit_perfect);
    if let Some(ref stats) = info.engine_stats {
        println!("  DSP Load:     {:.1}%", info.cpu_usage_pct);
        println!("  Codec:        {}", stats.decoder_format);
        println!("  Backend:      {}", stats.output_backend);
    }
    if let (Some(idx), len) = (info.playlist_index, info.playlist_length) {
        if len > 0 {
            println!("  Playlist:     [{}/{len}]", idx + 1);
        }
    }
    println!("\x1b[2m-------------------------------\x1b[0m");
}

fn drain_events(handle: &EngineHandle) {
    let mut count = 0;
    while let Ok(event) = handle.events().try_recv() {
        count += 1;
        println!("  \x1b[36m[Event #{}]\x1b[0m {:?}", count, event);
    }
    if count == 0 {
        println!("  (No new events in queue)");
    }
}

fn print_levels(handle: &EngineHandle) {
    let snap = handle.analyzer().snapshot();
    if snap.peak_db_l > -100.0 || snap.peak_db_r > -100.0 {
        println!(
            "  Peak:  {:>6.1} dBFS (L)  {:>6.1} dBFS (R)  dom. freq: {}",
            snap.peak_db_l,
            snap.peak_db_r,
            snap.dominant_frequency_hz()
                .map(|f| format!("{} Hz", f as u32))
                .unwrap_or_else(|| "(none)".to_string())
        );
        println!(
            "  RMS:   {:>6.1} dBFS (L)  {:>6.1} dBFS (R)",
            snap.rms_db_l, snap.rms_db_r
        );
    } else {
        println!("  (no audio signal detected)");
    }
}

fn do_scan(path: &str) {
    use std::path::Path;
    let p = Path::new(path);
    if !p.exists() {
        println!("  Error: file '{}' does not exist", path);
        return;
    }
    println!("  Scanning '{}' for EBU R128 loudness...", path);
    match engine::decode::scan_track_loudness(p) {
        Some(r) => {
            println!(
                "  Integrated: {:.1} LUFS",
                r.ebu_r128_loudness.unwrap_or(0.0)
            );
            println!(
                "  True Peak:  {:.1} dBTP",
                r.ebu_r128_peak_dbtp.unwrap_or(0.0)
            );
            println!("  LRA:        {:.1} LU", r.lra_lu.unwrap_or(0.0));
            println!("  Scanned {} frames", r.frames_scanned);
        }
        None => {
            println!(
                "  No measurable audio in '{}' or file cannot be decoded",
                path
            );
        }
    }
}

fn parse_backend(s: &str) -> Option<config::AudioBackend> {
    match s.to_lowercase().as_str() {
        "auto" => Some(config::AudioBackend::Auto),
        "wasapi" | "wasapi-exclusive" => Some(config::AudioBackend::ExclusiveWasapi),
        "alsa" | "alsa-exclusive" => Some(config::AudioBackend::ExclusiveAlsa),
        "coreaudio" | "coreaudio-hog" => Some(config::AudioBackend::ExclusiveCoreAudioHog),
        "asio" | "asio-exclusive" => Some(config::AudioBackend::ExclusiveAsio),
        _ => None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().collect();
    let mut config = EngineConfig::default();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--backend" | "-b" => {
                i += 1;
                if i < args.len() {
                    if let Some(b) = parse_backend(&args[i]) {
                        config.output_backend = b;
                    } else {
                        eprintln!(
                            "Unknown backend '{}'. Try: auto, wasapi, alsa, coreaudio, asio",
                            args[i]
                        );
                        return Ok(());
                    }
                }
            }
            "--device" | "-d" => {
                i += 1;
                if i < args.len() {
                    config.output_device = Some(args[i].clone());
                }
            }
            "--log-level" => {
                i += 1;
                if i < args.len() {
                    std::env::set_var("RUST_LOG", &args[i]);
                    // Re-init with new level.
                    let _ = env_logger::Builder::from_env(
                        env_logger::Env::default().default_filter_or(&args[i]),
                    )
                    .try_init();
                }
            }
            "--help" | "-h" => {
                println!("Usage: audio-engine-cli [OPTIONS] [file_path_or_uri]");
                println!();
                println!("Options:");
                println!("  --backend, -b <backend>   Output backend (auto, wasapi, alsa, coreaudio, asio)");
                println!("  --device, -d <name>       Output device name (or 'default')");
                println!("  --log-level <level>       Log level (error, warn, info, debug, trace)");
                println!("  --help, -h                Show this help");
                println!();
                println!("Interactive commands can be entered once launched.");
                return Ok(());
            }
            other => {
                // Could be a file path to play immediately.
                let _ = other;
            }
        }
        i += 1;
    }

    println!("Initializing Independent Headless Audio Engine...");

    let mut engine = AudioEngine::new(config)?;
    let handle = engine.handle();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();

    // Spawn the engine worker tick/render thread — uses tick_blocking so
    // the thread sleeps when idle and wakes instantly on a new command.
    let engine_thread = std::thread::Builder::new()
        .name("audio-engine-worker".into())
        .spawn(move || {
            while engine_running.load(Ordering::Relaxed) {
                engine.tick_blocking(Duration::from_millis(5));
            }
            engine.stop();
        })?;

    // Spawn background event listener thread.
    let event_rx = handle.clone_event_receiver();
    let events_running = running.clone();
    let _event_thread = std::thread::Builder::new()
        .name("audio-engine-events".into())
        .spawn(move || {
            while events_running.load(Ordering::Relaxed) {
                match event_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => print_event(event),
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;

    // Spawn background output-device event listener thread.
    #[cfg(feature = "audio-output")]
    {
        let output_event_rx = handle.clone_output_event_receiver();
        let output_running = running.clone();
        std::thread::Builder::new()
            .name("audio-engine-output-events".into())
            .spawn(move || {
                use engine::events::OutputEvent;
                while output_running.load(Ordering::Relaxed) {
                    match output_event_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(event) => match event {
                            OutputEvent::OutputDeviceChanged { device } => {
                                println!(
                                    "\n  \x1b[33m[Output] Device changed: {:?}\x1b[0m",
                                    device
                                );
                            }
                            OutputEvent::DeviceConnected { device } => {
                                println!("\n  \x1b[32m[Output] Connected: '{}'\x1b[0m", device);
                            }
                            OutputEvent::DeviceDisconnected { device } => {
                                println!("\n  \x1b[33m[Output] Disconnected: '{}'\x1b[0m", device);
                            }
                            OutputEvent::DeviceListChanged { devices } => {
                                println!(
                                    "\n  \x1b[2m[Output] List changed ({} devices)\x1b[0m",
                                    devices.len()
                                );
                            }
                        },
                        Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
    }

    // If a file was given on the command-line, play it immediately.
    let play_arg = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--") && !a.starts_with('-'));
    if let Some(target) = play_arg {
        let target = target.clone();
        println!("Opening source from CLI argument: {}", target);
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("file://")
        {
            handle.open_uri(&target);
        } else {
            handle.open_file(PathBuf::from(&target));
        }
        handle.play();
    }

    println!("\nHeadless Audio Engine ready. Type 'help' for commands, 'quit' to exit.");
    print_help();

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut line_buf = String::new();
    loop {
        print!("audio-engine> ");
        let _ = stdout.flush();
        line_buf.clear();
        if stdin.lock().read_line(&mut line_buf).is_err() || line_buf.is_empty() {
            break;
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next();

        match cmd.as_str() {
            "help" => print_help(),

            "open" => {
                if let Some(target) = arg {
                    println!("Opening source: {}", target);
                    if target.starts_with("http://")
                        || target.starts_with("https://")
                        || target.starts_with("file://")
                    {
                        handle.open_uri(target);
                    } else {
                        handle.open_file(PathBuf::from(target));
                    }
                    handle.play();
                } else {
                    println!("Error: 'open' requires a file path or URI argument.");
                }
            }

            "queue" => {
                if let Some(path) = arg {
                    handle.enqueue_file(PathBuf::from(path));
                    println!("Added to queue: {}", path);
                } else {
                    println!("Error: 'queue' requires a file path.");
                }
            }

            "clear" => {
                handle.clear_playlist();
                println!("Playlist cleared.");
            }

            "next" => {
                handle.next();
                println!("Skipped to next track.");
            }

            "prev" => {
                handle.previous();
                println!("Skipped to previous track.");
            }

            "shuffle" => {
                let enabled = !matches!(arg, Some("off") | Some("false") | Some("0"));
                handle.set_shuffle(enabled);
                println!("Shuffle: {}", if enabled { "on" } else { "off" });
            }

            "repeat" => {
                let mode = match arg.unwrap_or("off") {
                    "all" => engine::RepeatMode::All,
                    "one" | "1" => engine::RepeatMode::One,
                    _ => engine::RepeatMode::Off,
                };
                handle.set_repeat_mode(mode);
                println!("Repeat: {:?}", mode);
            }

            "play" => {
                handle.play();
                println!("Sent Play command.");
            }

            "pause" => {
                handle.pause();
                println!("Sent Pause command.");
            }

            "stop" => {
                handle.stop();
                println!("Sent Stop command.");
            }

            "seek" => {
                if let Some(pos_str) = arg {
                    if let Ok(pos) = pos_str.parse::<f32>() {
                        handle.seek(pos);
                        println!("Sent Seek({:.2}s) command.", pos);
                    } else {
                        println!("Error: Invalid seek position '{}'", pos_str);
                    }
                } else {
                    println!("Error: 'seek' requires a position in seconds.");
                }
            }

            "volume" => {
                if let Some(v_str) = arg {
                    if v_str.ends_with("db") || v_str.ends_with("dB") {
                        let clean = v_str.trim_end_matches("db").trim_end_matches("dB");
                        if let Ok(db) = clean.parse::<f32>() {
                            handle.set_volume_db(db);
                            println!("Set volume to {:.1} dB", db);
                        } else {
                            println!("Error: Invalid dB volume '{}'", v_str);
                        }
                    } else if let Ok(linear) = v_str.parse::<f32>() {
                        handle.set_volume(linear);
                        println!("Set linear volume to {:.2}", linear);
                    } else {
                        println!("Error: Invalid volume '{}'", v_str);
                    }
                } else {
                    let v = handle.volume();
                    let v_db = if v > 1e-6 { 20.0 * v.log10() } else { -100.0 };
                    println!("Current volume: {:.2} ({:.1} dB)", v, v_db);
                }
            }

            "speed" => {
                if let Some(s_str) = arg {
                    if let Ok(speed) = s_str.parse::<f32>() {
                        handle.set_speed(speed);
                        println!("Set speed to {:.2}x", speed);
                    } else {
                        println!("Error: Invalid speed multiplier '{}'", s_str);
                    }
                } else {
                    println!("Current speed: {:.2}x", handle.speed());
                }
            }

            "eq" => {
                if let Some(sub) = arg {
                    match sub {
                        "off" | "disable" => {
                            handle.set_eq_enabled(false);
                            println!("EQ disabled.");
                        }
                        "on" | "enable" => {
                            handle.set_eq_enabled(true);
                            println!("EQ enabled.");
                        }
                        preset_name => {
                            // Load any named preset — starts as enabled.
                            let preset = config::EqPreset {
                                name: preset_name.to_string(),
                                output_device_pattern: None,
                                preamp_db: 0.0,
                                bands: Vec::new(),
                            };
                            handle.set_eq_preset(preset);
                            println!("EQ preset '{}' loaded.", preset_name);
                        }
                    }
                } else {
                    let info = handle.playback_info();
                    if let Some(ref stats) = info.engine_stats {
                        println!(
                            "EQ: {}",
                            if stats.eq_enabled {
                                "enabled"
                            } else {
                                "disabled"
                            }
                        );
                    } else {
                        println!("EQ status: unknown");
                    }
                }
            }

            "eq-band" => {
                let band: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(0);
                let freq: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1000.0);
                let gain: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let q: f32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1.0);
                let enabled = parts.next() != Some("off");
                handle.set_eq_band(band, freq, gain, q, enabled);
                println!(
                    "EQ band {} set: {} Hz, {:.1} dB, Q {:.2}, {}",
                    band,
                    freq,
                    gain,
                    q,
                    if enabled { "on" } else { "off" }
                );
            }

            "levels" => {
                print_levels(&handle);
            }

            "scan" => {
                if let Some(path) = arg {
                    do_scan(path);
                } else {
                    println!("Error: 'scan' requires a file path.");
                }
            }

            "capture" => match arg.unwrap_or("toggle").to_lowercase().as_str() {
                "start" => {
                    let path = parts.next().map(PathBuf::from);
                    handle.start_capture(path, None);
                    println!("  Capture start requested (watch for the CaptureStarted event).");
                }
                "stop" => {
                    handle.stop_capture();
                    println!("  Capture stop requested (watch for the CaptureStopped event).");
                }
                other => {
                    println!("Error: unknown capture subcommand '{}' (use 'start [file.wav]' or 'stop').", other);
                }
            },

            "fingerprint" => {
                if let Some(path) = arg {
                    #[cfg(feature = "fingerprint")]
                    {
                        match engine::decode::extract_fingerprint(std::path::Path::new(path)) {
                            Ok(fp) => {
                                println!(
                                    "  Fingerprint: {} sub-fingerprints, {:.1}s ({:?})",
                                    fp.data.len(),
                                    fp.duration_secs,
                                    engine::decode::fingerprint_to_hex(
                                        &fp.data[..fp.data.len().min(4)]
                                    )
                                );
                            }
                            Err(e) => println!("  Error: {}", e),
                        }
                    }
                    #[cfg(not(feature = "fingerprint"))]
                    {
                        let _ = path;
                        println!("  Error: built without the 'fingerprint' feature.");
                    }
                } else {
                    println!("Error: 'fingerprint' requires a file path.");
                }
            }

            "devices" => {
                let devices = handle.available_devices();
                println!("\x1b[1m--- Available Output Endpoints ---\x1b[0m");
                if devices.is_empty() {
                    println!("  <no devices detected or default system output only>");
                } else {
                    for (i, dev) in devices.iter().enumerate() {
                        println!("  [{}] {}", i + 1, dev);
                    }
                }
                println!("\x1b[2m----------------------------------\x1b[0m");
            }

            "device" => {
                if let Some(dev_name) = arg {
                    if dev_name.eq_ignore_ascii_case("default")
                        || dev_name.eq_ignore_ascii_case("none")
                    {
                        handle.set_output_device(None);
                        println!("Switched to system default output device.");
                    } else {
                        handle.set_output_device(Some(dev_name.to_string()));
                        println!("Selected output device '{}'.", dev_name);
                    }
                } else {
                    println!("Error: 'device' requires a device name.");
                }
            }

            "info" => {
                print_info(&handle);
            }

            "events" => {
                drain_events(&handle);
            }

            "quit" | "exit" | "q" => {
                println!("Shutting down engine...");
                break;
            }

            unknown => {
                println!(
                    "Unknown command '{}'. Type 'help' for available commands.",
                    unknown
                );
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    handle.shutdown();
    let _ = engine_thread.join();
    println!("Engine stopped. Goodbye!");

    Ok(())
}

fn print_event(event: EngineEvent) {
    match event {
        EngineEvent::PlaybackStarted => println!("\n  \x1b[32m[Engine] Playback started\x1b[0m"),
        EngineEvent::PlaybackPaused => println!("\n  \x1b[33m[Engine] Playback paused\x1b[0m"),
        EngineEvent::PlaybackStopped => println!("\n  \x1b[2m[Engine] Playback stopped\x1b[0m"),
        EngineEvent::SourceOpened { ref source, sample_rate, channels, duration_secs } => {
            println!(
                "\n  \x1b[32m[Engine] Opened '{}' ({} Hz, {} ch, {:.2}s)\x1b[0m",
                source, sample_rate, channels, duration_secs
            );
        }
        EngineEvent::SourceFinished { ref source } => {
            println!("\n  \x1b[2m[Engine] Finished '{}'\x1b[0m", source);
        }
        EngineEvent::SeekCompleted { position_secs } => {
            println!("\n  \x1b[2m[Engine] Seek to {:.2}s\x1b[0m", position_secs);
        }
        EngineEvent::FormatChanged { sample_rate, channels } => {
            println!("\n  \x1b[2m[Engine] Format: {} Hz, {} ch\x1b[0m", sample_rate, channels);
        }
        EngineEvent::Error(ref msg) => {
            println!("\n  \x1b[31m[Engine] Error: {}\x1b[0m", msg);
        }
        EngineEvent::LoudnessScanComplete { ref path, ref result } => {
            match result {
                Some(r) => println!(
                    "\n  \x1b[2m[Engine] Scan complete '{}': {:.1} LUFS, {:.1} dBTP, LRA {:.1} LU\x1b[0m",
                    path.display(),
                    r.ebu_r128_loudness.unwrap_or(0.0),
                    r.ebu_r128_peak_dbtp.unwrap_or(0.0),
                    r.lra_lu.unwrap_or(0.0),
                ),
                None => println!(
                    "\n  \x1b[2m[Engine] Scan complete '{}': no measurable audio\x1b[0m",
                    path.display(),
                ),
            }
        }
        EngineEvent::PlaylistChanged { current_index, length } => {
            if let Some(idx) = current_index {
                println!("\n  \x1b[2m[Engine] Playlist: [{}/{}]\x1b[0m", idx + 1, length);
            } else {
                println!("\n  \x1b[2m[Engine] Playlist cleared ({} entries removed)\x1b[0m", length);
            }
        }
        EngineEvent::CaptureStarted { ref path } => {
            println!(
                "\n  \x1b[35m[Engine] Capture started -> '{}' (system audio)\x1b[0m",
                path.display()
            );
        }
        EngineEvent::CaptureStopped { ref path, frames, duration_secs } => {
            println!(
                "\n  \x1b[35m[Engine] Capture stopped: '{}' ({} frames, {:.1}s)\x1b[0m",
                path.display(),
                frames,
                duration_secs
            );
        }
        EngineEvent::CaptureError(ref msg) => {
            println!("\n  \x1b[31m[Engine] Capture error: {}\x1b[0m", msg);
        }
    }
}
