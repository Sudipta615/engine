//! `aelog-replay` — the guide's `engine replay recording.aelog`.
//!
//! Loads a recorded render session (a JSON `.aelog`), re-executes it
//! against a fresh timeline, and prints the deterministic outcome: how many
//! events fired, where the clock ended, and any pending events. Used as the
//! reproducibility oracle for bug reports and golden renders.
//!
//! With `--cache` it also renders the audio through a [`Graph2`] topology
//! via the golden-render cache: the first run is a **miss** (renders and
//! stores a content-addressed capture), and every repeated `engine replay`
//! of the same session **hits** and splices the stored audio instead of
//! re-rendering.
//!
//! ```text
//! cargo run --bin aelog_replay -- path/to/recording.aelog
//! cargo run --bin aelog_replay -- path/to/recording.aelog --cache --graph graph.json
//! cargo run --bin aelog_replay -- path/to/recording.aelog --cache --graph graph.json \
//!     --sink 2 --cache-dir ./cache --verbose
//! ```

use engine::prelude::{
    content_address, Aelog, AelogCache, ExecutionOrder, Graph2, NodeId, NodeKind, TransportState,
};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    const USAGE: &str =
        "usage: aelog_replay <recording.aelog> [--cache] [--graph graph.json] \\\n       \
         [--sink <n>] [--cache-dir <dir>] [--verbose]";

    let mut path: Option<String> = None;
    let mut cache = false;
    let mut verbose = false;
    let mut graph_path: Option<String> = None;
    let mut sink_arg: Option<String> = None;
    let mut cache_dir: Option<String> = None;

    // Single-pass flag parser (no external arg parser; the surface is tiny).
    // Flags and their values may appear in any order; the first bare token is
    // the recording path.
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--cache" => cache = true,
            "--verbose" => verbose = true,
            "--graph" => graph_path = it.next(),
            "--sink" => sink_arg = it.next(),
            "--cache-dir" => cache_dir = it.next(),
            other if other.starts_with("--") => {
                eprintln!("unrecognized argument: {other}\n{USAGE}");
                std::process::exit(2);
            }
            other => {
                if path.replace(other.to_string()).is_some() {
                    eprintln!("duplicate recording path\n{USAGE}");
                    std::process::exit(2);
                }
            }
        }
    }
    let Some(path) = path else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let log = Aelog::load_json(&path)?;
    log.check_version()?;

    let outcome = engine::prelude::replay_events(&log)?;
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

    if cache {
        render_cached(&log, &graph_path, &sink_arg, &cache_dir, verbose)?;
    }
    Ok(())
}

/// The `--cache` path: load a Graph2 topology, pick the capture sink, and
/// replay through the content-addressed golden-render cache, reporting the
/// hit/miss so repeated runs demonstrably skip re-rendering.
fn render_cached(
    log: &Aelog,
    graph_path: &Option<String>,
    sink_arg: &Option<String>,
    cache_dir: &Option<String>,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(graph_path) = graph_path else {
        eprintln!("error: --cache requires --graph <graph.json> (a serialized Graph2 topology)");
        std::process::exit(2);
    };

    // Deserialize the topology; `order` is not persisted, so recompile it.
    let json = std::fs::read_to_string(graph_path)?;
    let mut graph: Graph2 = serde_json::from_str(&json)?;
    let order: ExecutionOrder = graph.compile()?.clone();

    // Capture sink: an explicit `--sink` wins; otherwise the first Sink node
    // in deterministic (BTreeMap) iteration order.
    let sink = match sink_arg {
        Some(n) => {
            let n: u32 = n.parse()?;
            NodeId(n)
        }
        None => graph
            .nodes
            .iter()
            .find(|(_, d)| matches!(d.kind, NodeKind::Sink))
            .map(|(id, _)| *id)
            .ok_or("graph has no Sink node to capture; pass --sink <id>")?,
    };

    let cache = match cache_dir {
        Some(dir) => AelogCache::new(dir),
        None => AelogCache::new(
            AelogCache::default_root().unwrap_or_else(|| PathBuf::from("aelog-cache")),
        ),
    };

    // Probe for the golden render before rendering so we can report the hit.
    let hit = cache.lookup(log, &graph, sink).is_some();
    let address = content_address(log, &graph, sink);
    let outcome = cache.render_cached(log, &graph, &order, sink)?;

    let n = outcome.captured.len();
    let bytes = n * std::mem::size_of::<f32>();
    if hit {
        println!("  cache: HIT  reused golden render ({n} samples, {bytes} B) for {address}");
    } else {
        println!("  cache: MISS rendered & stored ({n} samples, {bytes} B) under {address}");
    }
    if verbose && !outcome.captured.is_empty() {
        let peak = outcome.captured.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
        println!(
            "  render peak |x| = {peak:.6}, end master {}",
            outcome.timeline.clock().master_position()
        );
    }
    Ok(())
}
