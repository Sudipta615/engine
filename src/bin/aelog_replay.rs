//! `aelog-replay` — the guide's `engine replay recording.aelog`.
//!
//! Loads a recorded render session (a JSON `.aelog`), re-executes it
//! against a fresh timeline, and prints the deterministic outcome: how many
//! events fired, where the clock ended, and any pending events. Used as the
//! reproducibility oracle for bug reports and golden renders.
//!
//! ```text
//! cargo run --bin aelog_replay -- path/to/recording.aelog
//! ```

use engine::prelude::{replay_events, Aelog, TransportState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: aelog_replay <recording.aelog> [--verbose]");
        std::process::exit(2);
    };
    let verbose = args.any(|a| a == "--verbose");

    let log = Aelog::load_json(&path)?;
    log.check_version()?;

    let outcome = replay_events(&log)?;
    let clock = outcome.timeline.clock();
    let label = if log.header.label.is_empty() {
        path.clone()
    } else {
        log.header.label.clone()
    };

    println!(
        "aelog replay: {label} (format v{})",
        log.header.format_version
    );
    println!(
        "  commands: {}, fired events: {}, pending: {}",
        log.commands.len(),
        outcome.fired.len(),
        outcome.timeline.pending()
    );
    println!(
        "  end state: pos {} master {} tempo {:.1} bpm state {:?}",
        clock.position(),
        clock.master_position(),
        clock.tempo_bpm(),
        match clock.state() {
            TransportState::Playing => "playing",
            TransportState::Paused => "paused",
            TransportState::Stopped => "stopped",
        }
    );

    if verbose {
        for e in &outcome.fired {
            println!("  fired @{}: {:?} {:?}", e.at, e.time, e.payload);
        }
        for (i, c) in log.commands.iter().enumerate() {
            println!("  cmd {i}: {c:?}");
        }
    }
    Ok(())
}
