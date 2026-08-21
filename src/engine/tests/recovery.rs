//! Stream recovery and circuit breaker tests.

use crate::engine::AudioEngine;

#[test]
fn test_recovery_policy_caps_retry_bursts() {
    assert!(!crate::engine::recovery::recovery_attempt_limit_reached(0));
    assert!(!crate::engine::recovery::recovery_attempt_limit_reached(4));
    assert!(crate::engine::recovery::recovery_attempt_limit_reached(5));
    assert!(crate::engine::recovery::recovery_attempt_limit_reached(
        u32::MAX
    ));

    // Health checks remain a no-op when no output stream exists, which is the
    // safe state while a device is disconnected.
    let mut engine = AudioEngine::new_default().expect("engine construction");
    engine.check_stream_health();
}
