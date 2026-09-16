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

use engine::eval::{run_qualification_pipeline, QualificationStatus};
use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::sync::atomic::Ordering;

struct QualCountingAllocator;

unsafe impl GlobalAlloc for QualCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if engine::eval::QUAL_MEASUREMENT_ARMED.load(Ordering::Relaxed) {
            engine::eval::QUAL_REALTIME_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if engine::eval::QUAL_MEASUREMENT_ARMED.load(Ordering::Relaxed) {
            engine::eval::QUAL_REALTIME_ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: QualCountingAllocator = QualCountingAllocator;

fn main() {
    let args: Vec<String> = env::args().collect();
    let json_mode = args.iter().any(|a| a == "--json");

    let report = run_qualification_pipeline();

    if json_mode {
        println!("{}", report.to_json());
    } else {
        println!("{}", report.render_text());
    }

    if report.qualification_status != QualificationStatus::Pass {
        std::process::exit(1);
    }
}
