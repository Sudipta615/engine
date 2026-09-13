//! # Reference plugin: `echo`
//!
//! A deliberately simple but *complete* Phase-49 plugin: a feedback-free
//! delay line ("echo") with input gain, per-channel delay in
//! milliseconds, and wet/dry mix. It demonstrates every ABI obligation:
//!
//! - the `#[repr(C)]` vtable with all ten entry points,
//! - the version handshake ([`PLUGIN_ABI_SYMBOL`]),
//! - plain-data state save/restore over caller-owned buffers,
//! - a **realtime-safe `process`** (no allocation, no locks, no
//!   panics-on-audio-path — plane lengths are trusted per host
//!   contract),
//! - a `worst-case` feature that *deliberately allocates in `process`*
//!   so the host's zero-allocation fidelity suite can prove it catches
//!   offenders.
//!
//! Compiled as both `rlib` (for the engine's in-process tests) and
//! `cdylib` (a real loadable `.so`/`.dylib`/`.dll` for the host loader
//! path). The `plugin_abi_v1` symbol is the single export.

// no_std-able except for state serialization; keep the audio path
// allocation-free regardless.

use plugin_abi::{
    AbiStatus, AudioBlockMut, PluginAbiV1, PluginDescriptor, PluginUid, PluginVTable,
    PLUGIN_ABI_VERSION,
};

/// Delay line capacity at 192 kHz with the maximum delay (2 s) plus one
/// block: 384,000 + 4096 ≈ 388,096 → 393,216 (a power of two).
const MAX_DELAY_SAMPLES: usize = 393_216;

/// Parameters (indices are the ABI contract).
mod param {
    pub const INPUT_GAIN: u32 = 0; // linear gain, 0..=4
    pub const DELAY_MS: u32 = 1; // 0..=2000 ms
    pub const FEEDBACK: u32 = 2; // 0..=0.95
    pub const WET_MIX: u32 = 3; // 0..=1
}

#[repr(C)]
struct EchoState {
    /// One ring per channel, `MAX_DELAY_SAMPLES` long (see `prepare`).
    rings: Option<Vec<Vec<f32>>>,
    write_pos: usize,
    channels: usize,
    sample_rate: f32,
    max_frames: usize,
    // Current parameter values (last-applied wins).
    input_gain: f32,
    delay_ms: f32,
    feedback: f32,
    wet_mix: f32,
}

impl EchoState {
    fn new(sample_rate: f32) -> Self {
        Self {
            rings: None,
            write_pos: 0,
            channels: 0,
            sample_rate,
            max_frames: 0,
            input_gain: 1.0,
            delay_ms: 250.0,
            feedback: 0.0,
            wet_mix: 0.25,
        }
    }

    fn delay_samples(&self) -> usize {
        let ms = self.delay_ms.clamp(0.0, 2000.0);
        ((ms / 1000.0) * self.sample_rate).round() as usize
    }
}

unsafe extern "C" fn descriptor(d: *mut PluginDescriptor, abi: u32) -> i32 {
    if abi != PLUGIN_ABI_VERSION {
        return AbiStatus::VersionMismatch as i32;
    }
    if d.is_null() {
        return AbiStatus::InvalidArgument as i32;
    }
    let name = str_to_fixed(b"echo");
    let version = str_to_fixed(env!("CARGO_PKG_VERSION").as_bytes());
    let vendor = str_to_fixed(b"Shadow Desktop reference");
    // SAFETY: `d` is host-owned storage for exactly one descriptor.
    unsafe {
        *d = PluginDescriptor {
            abi_version: PLUGIN_ABI_VERSION,
            uid: ECHO_UID,
            name,
            version,
            vendor,
            param_count: 4,
            latency_samples: 0,
            tail_samples: 0,
            supports_multichannel: 1,
        };
    }
    AbiStatus::Ok as i32
}

fn str_to_fixed(s: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = s.len().min(31);
    out[..n].copy_from_slice(&s[..n]);
    out
}

unsafe extern "C" fn instantiate(abi: u32, sample_rate: f32) -> *mut std::ffi::c_void {
    if abi != PLUGIN_ABI_VERSION || !sample_rate.is_finite() || sample_rate <= 0.0 {
        return std::ptr::null_mut();
    }
    let state = Box::new(EchoState::new(sample_rate));
    // SAFETY: the box is handed to the host; `drop_instance` frees it.
    Box::into_raw(state) as *mut std::ffi::c_void
}

unsafe extern "C" fn prepare(
    instance: *mut std::ffi::c_void,
    channels: u32,
    max_frames: u32,
) -> i32 {
    let channels = channels as usize;
    if instance.is_null()
        || channels == 0
        || channels > plugin_abi::MAX_PLUGIN_CHANNELS
        || max_frames == 0
    {
        return AbiStatus::InvalidArgument as i32;
    }
    // SAFETY: the pointer was created by `instantiate`.
    let state = unsafe { &mut *(instance as *mut EchoState) };
    if state.rings.as_ref().map(|r| r.len()) != Some(channels) {
        // Control path: allocation is fine here.
        state.rings = Some(
            (0..channels)
                .map(|_| vec![0.0; MAX_DELAY_SAMPLES])
                .collect(),
        );
    }
    state.channels = channels;
    state.max_frames = max_frames as usize;
    AbiStatus::Ok as i32
}

unsafe extern "C" fn set_param(instance: *mut std::ffi::c_void, index: u32, value: f32) -> i32 {
    if instance.is_null() || !value.is_finite() {
        return AbiStatus::InvalidArgument as i32;
    }
    // SAFETY: see `prepare`.
    let state = unsafe { &mut *(instance as *mut EchoState) };
    match index {
        param::INPUT_GAIN => state.input_gain = value.clamp(0.0, 4.0),
        param::DELAY_MS => state.delay_ms = value.clamp(0.0, 2000.0),
        param::FEEDBACK => state.feedback = value.clamp(0.0, 0.95),
        param::WET_MIX => state.wet_mix = value.clamp(0.0, 1.0),
        _ => return AbiStatus::BadParam as i32,
    }
    AbiStatus::Ok as i32
}

unsafe extern "C" fn process(instance: *mut std::ffi::c_void, block: *const AudioBlockMut) -> i32 {
    if instance.is_null() || block.is_null() {
        return AbiStatus::InvalidArgument as i32;
    }
    // SAFETY: the host guarantees the block layout matches `prepare`.
    let block = unsafe { &*block };
    let frames = block.frames as usize;
    let channels = block.channels as usize;
    if frames == 0 || channels == 0 {
        return AbiStatus::Ok as i32;
    }
    // SAFETY: host-contract pointer; see `prepare`.
    let state = unsafe { &mut *(instance as *mut EchoState) };

    // The worst-case build allocates in process so the fidelity suite
    // can prove the host catches offenders. Never enabled in release.
    #[cfg(feature = "worst-case")]
    let _leak = {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static WORST_CASE_ALLOC: AtomicUsize = AtomicUsize::new(0);
        let mut v = Vec::with_capacity(64);
        v.push(frames as f32);
        WORST_CASE_ALLOC.fetch_add(v.len(), Ordering::Relaxed);
        v
    };

    // Copy the scalar parameters out before borrowing the rings — the
    // ring processing below only mutates the rings and write_pos.
    let delay = state.delay_samples();
    let dry_gain = 1.0 - state.wet_mix;
    let feedback = state.feedback;
    let input_gain = state.input_gain;
    let wet_mix = state.wet_mix;
    let mut write_pos = state.write_pos;

    let Some(rings) = state.rings.as_mut() else {
        return AbiStatus::UnsupportedFormat as i32;
    };
    if rings.len() < channels || rings.is_empty() {
        return AbiStatus::UnsupportedFormat as i32;
    }
    // SAFETY: the host guarantees `channels` plane pointers, each with
    // at least `frames` samples.
    let planes = unsafe { std::slice::from_raw_parts(block.planes, channels) };
    let ring_len = rings[0].len();
    debug_assert!(ring_len > delay.max(1));
    for f in 0..frames {
        for c in 0..channels {
            let ring = &mut rings[c];
            let read = (write_pos + ring_len - delay) % ring_len;
            let wet = ring[read];
            let input = unsafe { *planes[c].add(f) };
            let delayed = wet * feedback + input * input_gain;
            ring[write_pos] = delayed;
            unsafe {
                *planes[c].add(f) = input * dry_gain + wet * wet_mix;
            }
        }
        write_pos = (write_pos + 1) % ring_len;
    }
    state.write_pos = write_pos;
    AbiStatus::Ok as i32
}

unsafe extern "C" fn save_state(
    instance: *mut std::ffi::c_void,
    buffer: *mut u8,
    capacity: usize,
) -> isize {
    if instance.is_null() {
        return AbiStatus::InvalidArgument as isize;
    }
    // SAFETY: see `prepare`.
    let state = unsafe { &*(instance as *mut EchoState) };
    // Plain-data serialization: parameters + write position (the ring
    // contents are intentionally NOT saved — a restored echo restarts
    // from silence, matching the reference pipeline's state semantics).
    let mut payload = [0u8; 24];
    payload[0..4].copy_from_slice(&state.input_gain.to_le_bytes());
    payload[4..8].copy_from_slice(&state.delay_ms.to_le_bytes());
    payload[8..12].copy_from_slice(&state.feedback.to_le_bytes());
    payload[12..16].copy_from_slice(&state.wet_mix.to_le_bytes());
    payload[16..20].copy_from_slice(&(state.write_pos as u32).to_le_bytes());
    payload[20..24].copy_from_slice(&(state.sample_rate.to_bits()).to_le_bytes());
    let need = payload.len();
    if capacity < need {
        return need as isize; // size query
    }
    if buffer.is_null() {
        return AbiStatus::InvalidArgument as isize;
    }
    // SAFETY: the host owns `capacity` bytes at `buffer`.
    unsafe {
        std::ptr::copy_nonoverlapping(payload.as_ptr(), buffer, need);
    }
    need as isize
}

unsafe extern "C" fn load_state(
    instance: *mut std::ffi::c_void,
    bytes: *const u8,
    len: usize,
) -> i32 {
    if instance.is_null() {
        return AbiStatus::InvalidArgument as i32;
    }
    if len != 24 {
        return AbiStatus::BadState as i32;
    }
    if bytes.is_null() {
        return AbiStatus::InvalidArgument as i32;
    }
    // SAFETY: the host guarantees `len` readable bytes.
    let raw = unsafe { std::slice::from_raw_parts(bytes, len) };
    // SAFETY: see `prepare`.
    let state = unsafe { &mut *(instance as *mut EchoState) };
    let rd = |i: usize| -> f32 { f32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) };
    state.input_gain = rd(0).clamp(0.0, 4.0);
    state.delay_ms = rd(4).clamp(0.0, 2000.0);
    state.feedback = rd(8).clamp(0.0, 0.95);
    state.wet_mix = rd(12).clamp(0.0, 1.0);
    state.write_pos = u32::from_le_bytes(raw[16..20].try_into().unwrap()) as usize;
    AbiStatus::Ok as i32
}

unsafe extern "C" fn reset(instance: *mut std::ffi::c_void) {
    if instance.is_null() {
        return;
    }
    // SAFETY: see `prepare`.
    let state = unsafe { &mut *(instance as *mut EchoState) };
    if let Some(rings) = state.rings.as_mut() {
        for ring in rings.iter_mut() {
            ring.fill(0.0);
        }
    }
    state.write_pos = 0;
}

unsafe extern "C" fn drop_instance(instance: *mut std::ffi::c_void) {
    if instance.is_null() {
        return;
    }
    // SAFETY: created by `instantiate`, freed exactly once here.
    unsafe { drop(Box::from_raw(instance as *mut EchoState)) };
}

/// The stable `echo` plugin UID (first 16 bytes of the UUIDv4-like
/// constant chosen for the reference plugin).
pub const ECHO_UID: PluginUid = [
    0xe1, 0xc0, 0x0e, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0e,
];

/// The vtable the `cdylib` export and the static registry share.
pub const ECHO_VTABLE: PluginVTable = PluginVTable {
    descriptor: Some(descriptor),
    instantiate: Some(instantiate),
    prepare: Some(prepare),
    set_param: Some(set_param),
    process: Some(process),
    save_state: Some(save_state),
    load_state: Some(load_state),
    reset: Some(reset),
    drop_instance: Some(drop_instance),
};

/// The single exported ABI handshake for the `cdylib` build.
#[no_mangle]
pub extern "C" fn plugin_abi_v1() -> *const PluginAbiV1 {
    static ABI: PluginAbiV1 = PluginAbiV1 {
        abi_version: PLUGIN_ABI_VERSION,
        size: std::mem::size_of::<PluginAbiV1>() as u32,
        vtable: PluginVTable {
            descriptor: Some(descriptor),
            instantiate: Some(instantiate),
            prepare: Some(prepare),
            set_param: Some(set_param),
            process: Some(process),
            save_state: Some(save_state),
            load_state: Some(load_state),
            reset: Some(reset),
            drop_instance: Some(drop_instance),
        },
    };
    &ABI
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_abi::{PluginHost, MAX_PLUGIN_CHANNELS};

    fn host() -> PluginHost {
        unsafe { PluginHost::from_vtable(ECHO_VTABLE) }.expect("reference vtable validates")
    }

    #[test]
    fn handshake_and_descriptor() {
        let host = host();
        assert_eq!(host.name(), "echo");
        assert_eq!(host.descriptor().param_count, 4);
        assert!(host.supports_multichannel());
    }

    #[test]
    fn process_delay_and_mix() {
        let host = host();
        let mut inst = host.instantiate(48_000.0).expect("instantiates");
        inst.prepare(2, 256).expect("prepares");
        inst.set_param(param::DELAY_MS, 1000.0).unwrap();
        inst.set_param(param::WET_MIX, 0.5).unwrap();
        // Two frames: impulse then silence.
        let mut l = [1.0f32, 0.0];
        let mut r = [0.0f32, 0.0];
        let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];
        // SAFETY: planes match the prepared shape.
        unsafe { inst.process(&mut planes) }.expect("processes");
        // 1 s at 48 kHz = 48,000 samples — beyond the impulse, so frame 0
        // is dry only, frame 1 too.
        assert!((l[0] - 0.5).abs() < 1e-6);
        assert!((l[1] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn state_roundtrip() {
        let host = host();
        let mut inst = host.instantiate(48_000.0).expect("instantiates");
        inst.prepare(2, 128).unwrap();
        inst.set_param(param::WET_MIX, 0.75).unwrap();
        inst.set_param(param::DELAY_MS, 123.0).unwrap();
        let saved = inst.save_state().expect("saves");
        assert_eq!(saved.len(), 24);
        let mut inst2 = host.instantiate(48_000.0).unwrap();
        inst2.prepare(2, 128).unwrap();
        inst2.load_state(&saved).expect("restores");
        // The restored wet mix is observable via a save round-trip.
        let again = inst2.save_state().unwrap();
        assert_eq!(saved, again);
    }

    #[test]
    fn multichannel_prepare_rejects_over_max() {
        let host = host();
        let mut inst = host.instantiate(44_100.0).unwrap();
        assert!(inst.prepare(MAX_PLUGIN_CHANNELS + 1, 64).is_err());
        assert!(inst.prepare(2, 64).is_ok());
    }
}
