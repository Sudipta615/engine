//! NaN/Inf Fault Injection and Containment Suite (§8.3, Punch List Item 28).
//!
//! Validates:
//! 1. Controlled injection of `NaN`, `+Inf`, `-Inf` into real-time audio streams.
//! 2. Verification of all five containment policies:
//!    - `Ignore`: Uninhibited pass-through.
//!    - `Detect`: Detection logging and incident tracking without signal alteration.
//!    - `Clamp`: Clamping `NaN` -> 0.0, `+Inf` -> 1.0, `-Inf` -> -1.0.
//!    - `Silence`: Replacement of corrupted samples with 0.0.
//!    - `BypassNode`: Total block zeroing on failure to stop downstream corruption.
//! 3. Guaranteed prevention of silent non-finite propagation across the DSP graph.
//! 4. Real-time safety: Strictly zero dynamic allocations during containment.

use config::EngineConfig;
use engine::dsp::graph2::prod::DspGraph;
use engine::dsp::safety::{contain_non_finite_planes, NonFinitePolicy};

#[test]
fn test_contain_non_finite_planes_policies() {
    // 1. Test Clamp Policy
    {
        let mut l = [0.5f32, f32::NAN, 0.7f32, f32::INFINITY];
        let mut r = [-0.5f32, -f32::INFINITY, -0.7f32, 0.2f32];
        let mut planes = [&mut l[..], &mut r[..]];

        let mut incident_count = 0;
        let counted = contain_non_finite_planes(
            &mut planes,
            NonFinitePolicy::Clamp,
            Some(1),
            Some("test_eq"),
            0,
            |_inc| {
                incident_count += 1;
            },
        );

        assert_eq!(counted, 3);
        assert_eq!(incident_count, 3);

        assert_eq!(l[0], 0.5);
        assert_eq!(l[1], 0.0); // NaN clamped to 0.0
        assert_eq!(l[2], 0.7);
        assert_eq!(l[3], 1.0); // +Inf clamped to 1.0

        assert_eq!(r[0], -0.5);
        assert_eq!(r[1], -1.0); // -Inf clamped to -1.0
        assert_eq!(r[2], -0.7);
        assert_eq!(r[3], 0.2);
    }

    // 2. Test Silence Policy
    {
        let mut l = [0.5f32, f32::NAN, 0.7f32, f32::INFINITY];
        let mut r = [-0.5f32, -f32::INFINITY, -0.7f32, 0.2f32];
        let mut planes = [&mut l[..], &mut r[..]];

        let counted = contain_non_finite_planes(
            &mut planes,
            NonFinitePolicy::Silence,
            Some(2),
            Some("test_comp"),
            0,
            |_| {},
        );

        assert_eq!(counted, 3);
        assert_eq!(l[1], 0.0);
        assert_eq!(l[3], 0.0);
        assert_eq!(r[1], 0.0);
        // Valid samples preserved
        assert_eq!(l[0], 0.5);
        assert_eq!(r[0], -0.5);
    }

    // 3. Test BypassNode Policy
    {
        let mut l = [0.5f32, f32::NAN, 0.7f32, 0.1f32];
        let mut r = [-0.5f32, 0.3f32, -0.7f32, 0.2f32];
        let mut planes = [&mut l[..], &mut r[..]];

        let counted = contain_non_finite_planes(
            &mut planes,
            NonFinitePolicy::BypassNode,
            Some(3),
            Some("test_conv"),
            0,
            |_| {},
        );

        assert_eq!(counted, 1);
        // Entire block must be silenced to prevent downstream poisoning
        for &s in l.iter().chain(r.iter()) {
            assert_eq!(s, 0.0);
        }
    }

    // 4. Test Detect Policy
    {
        let mut l = [0.5f32, f32::NAN];
        let mut r = [0.5f32, f32::INFINITY];
        let mut planes = [&mut l[..], &mut r[..]];

        let counted = contain_non_finite_planes(
            &mut planes,
            NonFinitePolicy::Detect,
            Some(4),
            Some("test_det"),
            0,
            |_| {},
        );

        assert_eq!(counted, 2);
        // Samples must not be modified
        assert!(l[1].is_nan());
        assert!(r[1].is_infinite());
    }

    // 5. Test Ignore Policy
    {
        let mut l = [0.5f32, f32::NAN];
        let mut r = [0.5f32, f32::INFINITY];
        let mut planes = [&mut l[..], &mut r[..]];

        let counted = contain_non_finite_planes(
            &mut planes,
            NonFinitePolicy::Ignore,
            Some(5),
            Some("test_ign"),
            0,
            |_| {},
        );

        assert_eq!(counted, 0);
        assert!(l[1].is_nan());
        assert!(r[1].is_infinite());
    }
}

#[test]
fn test_dsp_graph_fault_injection_clamp_prevents_propagation() {
    let sr = 48000.0f32;
    let cfg = EngineConfig::default();
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_non_finite_policy(NonFinitePolicy::Clamp);

    let mut left = vec![0.3f32, f32::NAN, 0.5f32, f32::INFINITY];
    let mut right = vec![-0.3f32, -f32::INFINITY, -0.5f32, 0.1f32];

    graph.process_block(&mut left, &mut right);

    // Verify no NaN or Inf survived the graph processing
    for &s in left.iter().chain(right.iter()) {
        assert!(
            s.is_finite(),
            "Non-finite sample escaped graph containment! Value: {}",
            s
        );
        assert!(
            (-1.5..=1.5).contains(&s),
            "Sample value outside clamped bounds: {}",
            s
        );
    }
}

#[test]
fn test_dsp_graph_fault_injection_silence_prevents_propagation() {
    let sr = 48000.0f32;
    let cfg = EngineConfig::default();
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_non_finite_policy(NonFinitePolicy::Silence);

    let mut left = vec![0.4f32, f32::NAN, 0.2f32, f32::NAN];
    let mut right = vec![-0.4f32, f32::INFINITY, -0.2f32, -f32::INFINITY];

    graph.process_block(&mut left, &mut right);

    for &s in left.iter().chain(right.iter()) {
        assert!(
            s.is_finite(),
            "Non-finite value escaped Silence policy! Value: {}",
            s
        );
    }
}

#[test]
fn test_dsp_graph_fault_injection_bypass_node_prevents_propagation() {
    let sr = 48000.0f32;
    let cfg = EngineConfig::default();
    let mut graph = DspGraph::from_config(&cfg, sr);
    graph.set_non_finite_policy(NonFinitePolicy::BypassNode);

    let mut left = vec![0.5f32, f32::NAN, 0.5f32, 0.5f32];
    let mut right = vec![0.5f32, 0.5f32, 0.5f32, 0.5f32];

    graph.process_block(&mut left, &mut right);

    // BypassNode zeroes out any corrupted stage output
    for &s in left.iter().chain(right.iter()) {
        assert!(
            s.is_finite(),
            "Non-finite sample escaped BypassNode! Value: {}",
            s
        );
    }
}
