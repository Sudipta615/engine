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
    println!("  play                 - Resume or start playback");
    println!("  pause                - Pause playback");
    println!("  stop                 - Stop playback and reset playhead");
    println!("  seek <seconds>       - Seek to position in seconds (e.g. seek 30.5)");
    println!("  volume <val>         - Set linear volume [0.0 - 1.0] or dB (e.g. 0.8 or -6db)");
    println!("  speed <multiplier>   - Set speed multiplier (e.g. speed 1.25)");
    println!("  info                 - Display atomic playback telemetry snapshot");
    println!("  events               - Drain and display discrete engine events");
    println!("  help                 - Show this help summary");
    println!("  quit | exit          - Gracefully shutdown engine and exit\n");
}

fn print_info(handle: &EngineHandle) {
    let info = handle.playback_info();
    let vol_db = if info.volume > 1e-6 {
        20.0 * info.volume.log10()
    } else {
        -100.0
    };
    println!("--- Playback State Snapshot ---");
    println!("  Source:       {}", match &info.current_source {
        Some(s) => s.to_string(),
        None => "<none>".to_string(),
    });
    println!("  State:        {:?}", info.state);
    println!("  Playhead:     {:.2}s / {:.2}s", info.position_secs, info.duration_secs);
    println!("  Compensated:  {:.2}s", info.position_secs_compensated);
    println!("  Sample Rate:  {} Hz", info.sample_rate);
    println!("  Volume:       {:.2} ({:.1} dB)", info.volume, vol_db);
    println!("  Speed:        {:.2}x", info.speed);
    println!("  Latency:      {:.2} ms", info.latency_ms);
    println!("  Bit-Perfect:  {}", info.bit_perfect);
    println!("-------------------------------");
}

fn drain_events(handle: &EngineHandle) {
    let mut count = 0;
    while let Ok(event) = handle.events().try_recv() {
        count += 1;
        println!("  [Event #{}] {:?}", count, event);
    }
    if count == 0 {
        println!("  (No new events in queue)");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Initializing Independent Headless Audio Engine...");

    let config = EngineConfig::default();
    let mut engine = AudioEngine::new(config)?;
    let handle = engine.handle();

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();

    // Spawn the engine worker tick/render thread
    let engine_thread = std::thread::Builder::new()
        .name("audio-engine-worker".into())
        .spawn(move || {
            while engine_running.load(Ordering::Relaxed) {
                engine.tick();
                std::thread::sleep(Duration::from_millis(5));
            }
            engine.stop();
        })?;

    // Spawn background event listener thread
    let event_rx = handle.clone_event_receiver();
    let events_running = running.clone();
    let _event_thread = std::thread::Builder::new()
        .name("audio-engine-events".into())
        .spawn(move || {
            while events_running.load(Ordering::Relaxed) {
                match event_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(event) => match event {
                        EngineEvent::PlaybackStarted => println!("\n[Engine Event] Playback started"),
                        EngineEvent::PlaybackPaused => println!("\n[Engine Event] Playback paused"),
                        EngineEvent::PlaybackStopped => println!("\n[Engine Event] Playback stopped"),
                        EngineEvent::SourceOpened { ref source, sample_rate, channels, duration_secs } => {
                            println!("\n[Engine Event] Opened source '{}' ({} Hz, {} ch, {:.2}s)", source, sample_rate, channels, duration_secs);
                        }
                        EngineEvent::SourceFinished { ref source } => {
                            println!("\n[Engine Event] Finished source '{}'", source);
                        }
                        EngineEvent::SeekCompleted { position_secs } => {
                            println!("\n[Engine Event] Seek completed to {:.2}s", position_secs);
                        }
                        EngineEvent::OutputDeviceChanged { ref device } => {
                            println!("\n[Engine Event] Output device changed: {:?}", device);
                        }
                        EngineEvent::FormatChanged { sample_rate, channels } => {
                            println!("\n[Engine Event] Format changed: {} Hz, {} ch", sample_rate, channels);
                        }
                        EngineEvent::Error(ref err) => {
                            println!("\n[Engine Event Error] {}", err);
                        }
                    },
                    Err(crossbeam::channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let target = &args[1];
        if target == "--help" || target == "-h" {
            println!("Usage: audio-engine-cli [file_path_or_uri]");
            println!("Interactive commands can be entered once launched.");
            running.store(false, Ordering::Relaxed);
            let _ = engine_thread.join();
            return Ok(());
        }

        println!("Opening source from CLI argument: {}", target);
        if target.starts_with("http://") || target.starts_with("https://") || target.starts_with("file://") {
            handle.open_uri(target);
        } else {
            handle.open_file(PathBuf::from(target));
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
                    if target.starts_with("http://") || target.starts_with("https://") || target.starts_with("file://") {
                        handle.open_uri(target);
                    } else {
                        handle.open_file(PathBuf::from(target));
                    }
                    handle.play();
                } else {
                    println!("Error: 'open' requires a file path or URI argument.");
                }
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
                        println!("Error: Invalid seek position '{}'. Expected float seconds.", pos_str);
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
                            println!("Error: Invalid dB volume '{}'.", v_str);
                        }
                    } else if let Ok(linear) = v_str.parse::<f32>() {
                        handle.set_volume(linear);
                        println!("Set linear volume to {:.2}", linear);
                    } else {
                        println!("Error: Invalid volume '{}'.", v_str);
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
                        println!("Error: Invalid speed multiplier '{}'.", s_str);
                    }
                } else {
                    println!("Current speed: {:.2}x", handle.speed());
                }
            }
            "info" => {
                print_info(&handle);
            }
            "events" => {
                drain_events(&handle);
            }
            "quit" | "exit" => {
                println!("Shutting down engine...");
                break;
            }
            unknown => {
                println!("Unknown command '{}'. Type 'help' for available commands.", unknown);
            }
        }
    }

    running.store(false, Ordering::Relaxed);
    handle.shutdown();
    let _ = engine_thread.join();
    println!("Engine stopped. Goodbye!");

    Ok(())
}
