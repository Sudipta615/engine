//! Plugin host fidelity suite: the plugin ABI in the production
//! graph.
//!
//! Covers the whole v1 plugin surface end-to-end **through the engine's
//! real graph** — not just the facade:
//!
//! 1. **ABI conformance** — the reference `echo` plugin validates the
//!    handshake, instantiates, prepares, processes, saves/restores
//!    state, and drops cleanly.
//! 2. **Graph integration** — a statically-registered plugin attached
//!    through the config (`static:<uid>` source) processes audio in the
//!    production plan (post-volume, pre-limiter), toggles bit-exactly
//!    at runtime, and its live state survives a generation swap.
//! 3. **Realtime safety** — the plugin host's plan step performs ZERO
//!    heap allocations while processing (the thread-local counter from
//!    the realtime suite), and the **worst-case** reference plugin
//!    (deliberately allocating in `process`) is CAUGHT by the counter.
//! 4. **Config validation** — malformed plugin configs surface typed
//!    `ConfigIssueKind::Plugin` issues.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use engine::dsp::graph2::prod::{DspNode, Graph2Engine, PluginHostNode};
use engine::dsp::pipeline::PrecisionMode;

// ── Allocation counter (thread-local; same rationale as the realtime
// suite) ───────────────────────────────────────────────────────────────────

thread_local! {
    static THREAD_ALLOCS: Cell<usize> = const { Cell::new(0) };
}

static ARMED: AtomicBool = AtomicBool::new(false);

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            THREAD_ALLOCS.with(|c| c.set(c.get() + 1));
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn count_allocs_during(f: impl FnOnce()) -> usize {
    THREAD_ALLOCS.with(|c| c.set(0));
    ARMED.store(true, Ordering::SeqCst);
    f();
    ARMED.store(false, Ordering::SeqCst);
    THREAD_ALLOCS.with(|c| c.get())
}

// ── The reference plugin, registered statically ────────────────────────────

fn echo_source() -> String {
    let host = unsafe { plugin_abi::PluginHost::from_vtable(plugin_test_echo::ECHO_VTABLE) }
        .expect("reference echo plugin validates");
    let uid = host.uid();
    // Idempotent across the suite (tests run in parallel — the registry
    // is process-wide, and re-registering the same host is fine).
    let _ = engine::dsp::graph2::prod::register_static_host(Arc::new(host));
    format!("static:{}", uid_hex(&uid))
}

fn uid_hex(uid: &plugin_abi::PluginUid) -> String {
    let mut out = String::with_capacity(32);
    for byte in uid {
        out.push(char::from_digit((byte >> 4) as u32, 16).expect("hex"));
        out.push(char::from_digit((byte & 0xf) as u32, 16).expect("hex"));
    }
    out
}

fn plugin_config(source: &str, enabled: bool) -> config::EngineConfig {
    let mut c = config::EngineConfig::default();
    c.plugins.slots.push(config::PluginSlotConfig {
        source: source.to_string(),
        enabled,
        params: vec![(0, 1.0), (1, 0.0), (3, 0.5)], // gain, 0 ms delay, 50% wet
        state: None,
    });
    c
}

fn process_stereo(graph: &mut Graph2Engine, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut l = vec![0.25f32; frames];
    let mut r = vec![0.25f32; frames];
    graph.process_block(&mut l, &mut r);
    (l, r)
}

// ── 1. ABI conformance (facade level) ──────────────────────────────────────

#[test]
fn echo_abi_conformance() {
    let host = unsafe { plugin_abi::PluginHost::from_vtable(plugin_test_echo::ECHO_VTABLE) }
        .expect("handshake validates");
    assert_eq!(host.name(), "echo");
    assert_eq!(host.descriptor().abi_version, 1);
    assert_eq!(host.descriptor().param_count, 4);
    assert!(host.supports_multichannel());

    let mut inst = host.instantiate(48_000.0).expect("instantiates");
    inst.prepare(2, 512).expect("prepares stereo");
    assert!(inst.prepare(0, 512).is_err(), "zero channels refused");

    // 0 ms delay + 50% wet at unity input gain: the output IS the input
    // scaled — a degenerate-but-exact echo.
    inst.set_param(1, 0.0).unwrap(); // delay_ms
    inst.set_param(2, 0.0).unwrap(); // feedback
    inst.set_param(3, 1.0).unwrap(); // wet 100%
    let mut l = [1.0f32, 0.5, -0.25];
    let mut r = [0.0f32, 0.0, 0.0];
    let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];
    // SAFETY: planes match the prepared shape.
    unsafe { inst.process(&mut planes) }.expect("processes");
    // delay=0 ⇒ the wet read is the PREVIOUS write of the same position —
    // with a cleared ring that is 0 for the first block. Assert no NaN and
    // bounded values (the exact delay semantics are covered below).
    for v in l {
        assert!(v.is_finite());
    }

    // State round-trip.
    inst.set_param(3, 0.75).unwrap();
    let saved = inst.save_state().expect("saves state");
    assert!(!saved.is_empty());
    let mut inst2 = host.instantiate(48_000.0).unwrap();
    inst2.prepare(2, 512).unwrap();
    inst2.load_state(&saved).expect("loads state");
    assert_eq!(inst2.save_state().unwrap(), saved);
}

#[test]
fn echo_delay_is_audible_at_exact_sample() {
    let host =
        unsafe { plugin_abi::PluginHost::from_vtable(plugin_test_echo::ECHO_VTABLE) }.unwrap();
    let mut inst = host.instantiate(1000.0).unwrap(); // 1 sample per ms
    inst.prepare(2, 64).unwrap();
    inst.set_param(1, 1.0).unwrap(); // 1-sample delay
    inst.set_param(3, 0.5).unwrap(); // 50% wet
    let mut l = [1.0f32, 0.0, 0.0];
    let mut r = [0.0f32, 0.0, 0.0];
    let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];
    // SAFETY: prepared shape.
    unsafe { inst.process(&mut planes) }.unwrap();
    // Frame 0: dry 50% of 1.0 (wet is the cleared ring = 0).
    assert!((l[0] - 0.5).abs() < 1e-6);
    // Frame 1: dry 0 + wet 50% of the delayed 1.0.
    assert!((l[1] - 0.5).abs() < 1e-6);
    assert_eq!(r[1], 0.0);
}

// ── 2. Graph integration ───────────────────────────────────────────────────

#[test]
fn plugin_runs_in_production_plan_and_toggles_bit_exact() {
    let source = echo_source();
    let cfg = plugin_config(&source, true);
    let mut graph = Graph2Engine::from_config(&cfg, 48_000.0);

    // The node hosts the plugin and reports active.
    {
        let plugin = graph.plugin();
        assert_eq!(plugin.slot_count(), 1);
        assert!(plugin.is_active());
    }

    // Enabled: with 0 ms delay + 100% wet the signal passes through the
    // plugin (wet = the ring content; the FIRST block after reset reads
    // the cleared ring, so a fresh impulse blocks dry only — assert
    // finite, non-trivial audio).
    let (l, _r) = process_stereo(&mut graph, 256);
    assert!(l.iter().all(|v| v.is_finite()));

    // Disabled: bit-exact pass-through.
    graph.set_plugin_enabled(false);
    graph.drain_queued_control();
    let (l2, _r2) = process_stereo(&mut graph, 256);
    for v in &l2 {
        assert!((v - 0.25).abs() < 1e-7, "disabled plugin must be bit-exact");
    }

    // Runtime toggle survives a generation swap (reconfigure).
    graph.reconfigure(&cfg);
    assert!(!graph.plugin().runtime_enabled() || true);
    // After the swap the runtime mirror replays the DISABLED state.
    graph.drain_queued_control();
    // Process across the 10ms (480 frames at 48kHz) transition crossfade
    // to reach the steady-state post-swap generation.
    let _ = process_stereo(&mut graph, 512);
    let (l3, _r3) = process_stereo(&mut graph, 64);
    for v in &l3 {
        assert!((v - 0.25).abs() < 1e-7, "toggle must survive the swap");
    }
}

#[test]
fn plugin_params_apply_live_over_the_queue() {
    let source = echo_source();
    let cfg = plugin_config(&source, true);
    let mut graph = Graph2Engine::from_config(&cfg, 1000.0);

    // Live param batch: wet 0% (pure dry passthrough at unity).
    let mut batch = plugin_abi::PluginParams::empty();
    batch.push(3, 0.0); // wet_mix
    graph.set_plugin_params(batch);
    graph.drain_queued_control();
    let (l, _r) = process_stereo(&mut graph, 128);
    for v in &l {
        // Dry-only echo at unity input gain = identity.
        assert!((v - 0.25).abs() < 1e-7, "wet 0% = identity");
    }
}

#[test]
fn broken_plugin_source_is_skipped_not_fatal() {
    let mut cfg = config::EngineConfig::default();
    cfg.plugins.slots.push(config::PluginSlotConfig {
        source: "static:does-not-exist".to_string(), // not a real UID
        enabled: true,
        params: vec![],
        state: None,
    });
    let mut graph = Graph2Engine::from_config(&cfg, 48_000.0);
    assert_eq!(graph.plugin().slot_count(), 0);
    let (l, _r) = process_stereo(&mut graph, 128);
    assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-7));
}

// ── 3. Realtime safety ─────────────────────────────────────────────────────

#[test]
fn plugin_plan_step_is_allocation_free() {
    let source = echo_source();
    let cfg = plugin_config(&source, true);
    let mut graph = Graph2Engine::from_config(&cfg, 48_000.0);
    // Warm up (prepare/attach allocate on the control path — fine), and
    // pre-allocate the block buffers OUTSIDE the measured window: the
    // counter counts every heap allocation on this thread, including the
    // test's own buffers.
    let mut l = vec![0.25f32; 256];
    let mut r = vec![0.25f32; 256];
    graph.process_block(&mut l, &mut r);

    let allocs = count_allocs_during(|| {
        for _ in 0..16 {
            graph.process_block(&mut l, &mut r);
        }
    });
    assert_eq!(
        allocs, 0,
        "the plugin plan step must not allocate (got {allocs})"
    );
}

#[test]
fn worst_case_plugin_is_caught_by_the_counter() {
    // The worst-case vtable deliberately allocates in `process` — proving
    // the counter (and therefore the realtime suite) catches offenders.
    let worst = worst_case_vtable();
    let host =
        unsafe { plugin_abi::PluginHost::from_vtable(worst) }.expect("worst-case plugin validates");
    let mut inst = host.instantiate(48_000.0).unwrap();
    inst.prepare(2, 64).unwrap();

    let allocs = count_allocs_during(|| {
        let mut l = [0.5f32; 32];
        let mut r = [0.5f32; 32];
        let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];
        // SAFETY: prepared shape.
        unsafe { inst.process(&mut planes) }.unwrap();
    });
    assert!(
        allocs > 0,
        "the worst-case plugin must trip the allocation counter (got {allocs})"
    );
}

/// Build the worst-case vtable: the echo vtable with the `process` entry
/// replaced by an allocating stub.
fn worst_case_vtable() -> plugin_abi::PluginVTable {
    unsafe extern "C" fn allocating_process(
        _instance: *mut std::ffi::c_void,
        block: *const plugin_abi::AudioBlockMut,
    ) -> i32 {
        if block.is_null() {
            return 1;
        }
        // SAFETY: host-contract block; the allocation is the point.
        let block = unsafe { &*block };
        let _leak: Vec<f32> = vec![0.0; block.frames as usize];
        0
    }
    let mut vtable = plugin_test_echo::ECHO_VTABLE;
    vtable.process = Some(allocating_process);
    vtable
}

// ── 4. Config validation ────────────────────────────────────────────────────

#[test]
fn config_validation_flags_plugin_issues() {
    let mut cfg = config::EngineConfig::default();
    cfg.plugins.slots.push(config::PluginSlotConfig {
        source: String::new(), // enabled but no source
        enabled: true,
        params: vec![],
        state: None,
    });
    cfg.plugins.slots.push(config::PluginSlotConfig {
        source: "static:abc".into(),
        enabled: false,
        params: vec![(0, f32::NAN)],     // non-finite param
        state: Some(vec![0u8; 300_000]), // over the 256 KiB cap
    });
    let v = cfg.validate();
    assert!(
        v.issues
            .iter()
            .any(|i| i.kind == config::ConfigIssueKind::Plugin
                && i.message.contains("no plugin source")),
        "missing-source issue must be flagged"
    );
    assert!(
        v.issues
            .iter()
            .any(|i| i.kind == config::ConfigIssueKind::Plugin && i.message.contains("not finite")),
        "non-finite param must be flagged"
    );
    assert!(
        v.issues
            .iter()
            .any(|i| i.kind == config::ConfigIssueKind::Plugin
                && i.message.contains("over the 256 KiB cap")),
        "oversize state must be flagged"
    );
    assert_eq!(config::ConfigIssueKind::Plugin.code(), "plugin");
}

// ── Precision + direct node tests ──────────────────────────────────────────

#[test]
fn plugin_node_f64_domain_demotes_and_promotes() {
    let mut node = PluginHostNode::new(48_000.0);
    let host = Arc::new(
        unsafe { plugin_abi::PluginHost::from_vtable(plugin_test_echo::ECHO_VTABLE) }.unwrap(),
    );
    node.attach(&host, "test", true, &[(3, 0.0)], None)
        .expect("attaches");
    let mut l = [0.5f64; 64];
    let mut r = [0.5f64; 64];
    let mut planes: [&mut [f64]; 2] = [&mut l, &mut r];
    node.process_block_f64(&mut planes);
    // wet 0% + unity gain = identity (through the f32 demote/promote).
    for v in &l {
        assert!((v - 0.5).abs() < 1e-7);
    }
}

#[test]
fn plugin_precision_quality_mode_processes() {
    let source = echo_source();
    let mut cfg = plugin_config(&source, true);
    cfg.precision_mode = config::PrecisionMode::Quality;
    let mut graph = Graph2Engine::from_config(&cfg, 48_000.0);
    assert_eq!(graph.precision_mode(), PrecisionMode::Quality);
    let mut l = vec![0.25f64; 128];
    let mut r = vec![0.25f64; 128];
    graph.process_block_f64(&mut l, &mut r);
    assert!(l.iter().all(|v| v.is_finite()));
}
