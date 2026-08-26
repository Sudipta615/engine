//! Offline ReplayGain / EBU R128 scanner + tag writer.
//!
//! Walks a directory tree for supported audio files, measures each file's
//! integrated loudness with the engine's BS.1770-4 meter (the *same* meter
//! used during playback), and optionally writes the results back into the
//! file's ReplayGain / R128 tags.
//!
//! ```text
//! replaygain-scanner /music/collection --write
//! ```
//!
//! Requires the `tag-write` feature (declared in `Cargo.toml`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine::decode::{scan_track_loudness, write_loudness_tags, LoudnessScanResult};

/// Extensions the scanner will measure.
const AUDIO_EXTS: &[&str] = &[
    "flac", "mp3", "ogg", "oga", "opus", "wav", "wave", "aiff", "aif", "aifc", "m4a", "mp4", "m4b",
    "alac", "ape", "mac", "wv", "tta",
];

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Recursively collect audio files under `root`.
fn collect_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else if is_audio(&path) {
            out.push(path);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write = false;
    let mut jobs: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let mut roots: Vec<PathBuf> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--write" | "-w" => write = true,
            "--jobs" | "-j" => {
                i += 1;
                if i < args.len() {
                    jobs = args[i].parse().unwrap_or(jobs).clamp(1, 64);
                }
            }
            "--help" | "-h" => {
                println!("Usage: replaygain-scanner [--write] [--jobs N] <dir-or-file>...");
                println!("Measures EBU R128 integrated loudness and (with --write)");
                println!("writes ReplayGain 2.0 / R128 tags back into each file.");
                return Ok(());
            }
            other => roots.push(PathBuf::from(other)),
        }
        i += 1;
    }

    if roots.is_empty() {
        eprintln!("error: no input directory or file given (see --help)");
        std::process::exit(2);
    }

    let mut files = Vec::new();
    for root in &roots {
        if root.is_dir() {
            collect_files(root, &mut files);
        } else if is_audio(root) {
            files.push(root.clone());
        } else {
            eprintln!("skipping non-audio input: {}", root.display());
        }
    }

    if files.is_empty() {
        eprintln!("no audio files found under the given paths");
        std::process::exit(1);
    }

    println!(
        "Scanning {} file(s) across {} thread(s)...",
        files.len(),
        jobs
    );

    let counter = Arc::new(AtomicUsize::new(0));
    let total = files.len();
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let queue = Arc::new(Mutex::new(files.into_iter()));
    let mut workers = Vec::new();
    for _ in 0..jobs {
        let queue = Arc::clone(&queue);
        let counter = Arc::clone(&counter);
        let failures = Arc::clone(&failures);
        workers.push(std::thread::spawn(move || loop {
            let next = { queue.lock().unwrap().next() };
            let Some(path) = next else { break };
            match process_file(&path, write) {
                Ok(()) => {}
                Err(e) => failures
                    .lock()
                    .unwrap()
                    .push(format!("{}: {e}", path.display())),
            }
            let done = counter.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!("\r  [{}/{}] {}", done, total, path.display());
        }));
    }
    for w in workers {
        let _ = w.join();
    }

    let failures = failures.lock().unwrap();
    if !failures.is_empty() {
        eprintln!("\n{} file(s) failed:", failures.len());
        for f in failures.iter() {
            eprintln!("  {f}");
        }
    }

    println!(
        "\nDone. {}/{} file(s) scanned successfully{}.",
        total - failures.len(),
        total,
        if write { " and tagged" } else { "" }
    );
    Ok(())
}

fn process_file(path: &Path, write: bool) -> Result<(), String> {
    let result = scan_track_loudness(path).ok_or_else(|| "no measurable audio".to_string())?;

    println!(
        "{}  {:>7.1} LUFS  {:>6.1} dBTP  LRA {:>5.1} LU",
        path.display(),
        result.ebu_r128_loudness.unwrap_or(0.0),
        result.ebu_r128_peak_dbtp.unwrap_or(0.0),
        result.lra_lu.unwrap_or(0.0),
    );

    if write {
        let meta = engine::dsp::LoudnessMetadata {
            ebu_r128_loudness: result.ebu_r128_loudness,
            ebu_r128_peak: result.ebu_r128_peak_dbtp,
            replaygain_track_db: result.replaygain_track_db,
            replaygain_track_peak: result.replaygain_track_peak,
            ..Default::default()
        };
        write_loudness_tags(path, &meta).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// Silence unused import when the binary is compiled without the feature
// (it can't be — `required-features` — but keeps the type-checker quiet if
// someone builds it manually).
#[allow(dead_code)]
fn _assert_scan_result_cloneable(r: &LoudnessScanResult) -> &LoudnessScanResult {
    r
}
