//! Release Qualification CLI binary (spec §13.1).
//!
//! Runs the formal release qualification pipeline and prints the results
//! in human-readable ASCII or machine-readable JSON format.
//!
//! Usage:
//! ```bash
//! cargo run --bin release-qualification
//! cargo run --bin release-qualification -- --json
//! ```

use engine::eval::run_qualification_pipeline;
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let json_mode = args.iter().any(|a| a == "--json");

    let report = run_qualification_pipeline();

    if json_mode {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.render_text());
    }

    if report.qualification_status != "PASS" {
        std::process::exit(1);
    }
}
