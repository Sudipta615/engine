//! Endpoint selection and exclusive-mode client construction.

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Foundation::HANDLE,
    Media::{
        Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback},
        Audio::{
            eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceCollection,
            IMMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            DEVICE_STATE_ACTIVE, WAVEFORMATEX,
        },
    },
    System::{
        Com::{CoTaskMemFree, StructuredStorage::PropVariantClear, CLSCTX_ALL, STGM_READ},
        Variant::VT_LPWSTR,
    },
    UI::Shell::PropertiesSystem::IPropertyStore,
};

use crate::buffer::FixedFrameBuffer;
use crate::output::cpal_output::OutputError;
use crate::output::format_converter::AudioFormatConverter;
use crate::output::output::StreamErrorState;

use super::com::{SendCom, SendHandle};
use super::format::{build_format, exclusive_format_supported, WasapiContainer};
use super::volume::{EndpointVolumeCallback, VolumeCallbackState};

/// Endpoint ID string (allocated by COM; freed via CoTaskMemFree).
/// `pub(crate)` so `WasapiOutput` can capture the stable ID for
/// profile matching (§10) at construction.
pub(crate) fn endpoint_id_of(device: &IMMDevice) -> Option<String> {
    endpoint_id(device)
}

fn endpoint_id(device: &IMMDevice) -> Option<String> {
    let id = unsafe { device.GetId() }.ok()?;
    // PWSTR::to_string is unsafe and returns a Result in windows 0.59.
    let s = unsafe { id.to_string() }.ok()?;
    unsafe {
        CoTaskMemFree(Some(id.0 as *const _));
    }
    Some(s)
}

/// Friendly name of an endpoint (`PKEY_Device_FriendlyName`). This is the
/// name cpal reports as `Device::name()` on WASAPI, so it matches the
/// engine's `output_device` config values. The string is owned by the
/// PROPVARIANT; `PropVariantClear` releases it.
fn device_friendly_name(device: &IMMDevice) -> Option<String> {
    let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    // GetValue takes `*const PROPERTYKEY` in windows 0.59.
    let value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.ok()?;
    let name = unsafe {
        if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            value.Anonymous.Anonymous.Anonymous.pwszVal.to_string().ok()
        } else {
            None
        }
    };
    unsafe {
        let _ = PropVariantClear(&value as *const _ as *mut _);
    }
    name
}

/// Pick the render endpoint to open, honoring the engine's `output_device`
/// config (`target_device`).
///
/// - No target (or the "Default / Automatic" sentinel): the default render
///   endpoint, as before.
/// - Otherwise: enumerate active render endpoints via
///   `IMMDeviceEnumerator::EnumAudioEndpoints` and match `target_device` in
///   confidence order:
///   1. **Endpoint ID** (`IMMDevice::GetId`) — the OS's stable device
///      identifier, preferred because friendly names are ambiguous and can
///      change with the OS language.
///   2. **Friendly name** (`PKEY_Device_FriendlyName`) — exact match first,
///      then case-insensitive.
///   3. **Substring** on the friendly name as a last resort (logs a warning).
///
///   An unmatched target logs a warning and falls back to the default
///   endpoint, so a stale config value never hard-fails playback.
///
/// Returns the chosen device and its display name.
pub(crate) fn select_device(
    enumerator: &IMMDeviceEnumerator,
    target_device: Option<&str>,
) -> Result<(IMMDevice, String), OutputError> {
    let default = |enumerator: &IMMDeviceEnumerator| {
        let device =
            unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(|e| {
                OutputError::StreamOpen(format!("GetDefaultAudioEndpoint(eRender): {e}"))
            })?;
        let name = device_friendly_name(&device)
            .or_else(|| endpoint_id(&device))
            .unwrap_or_else(|| "WASAPI default render endpoint".to_string());
        Ok::<(IMMDevice, String), OutputError>((device, name))
    };

    let target = match target_device {
        Some(t) if !t.is_empty() && t != "Default / Automatic" => t,
        _ => return default(enumerator),
    };

    let collection: IMMDeviceCollection =
        unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .map_err(|e| OutputError::StreamOpen(format!("EnumAudioEndpoints(eRender): {e}")))?;
    let count = unsafe { collection.GetCount() }
        .map_err(|e| OutputError::StreamOpen(format!("IMMDeviceCollection::GetCount: {e}")))?;

    // Collect (device, friendly name, endpoint ID) triples. The endpoint ID
    // (`IMMDevice::GetId`, e.g. `{0.0.0.00000000}.{guid}`) is the OS's stable
    // identifier, so an exact ID match is the most precise selection; friendly
    // names can collide across endpoints and change with the OS language.
    // Skip endpoints whose property store is unreadable (no name to match).
    let mut devices: Vec<(IMMDevice, String, Option<String>)> = Vec::new();
    for i in 0..count {
        if let Ok(device) = unsafe { collection.Item(i) } {
            let id = endpoint_id(&device);
            if let Some(name) = device_friendly_name(&device) {
                devices.push((device, name, id));
            }
        }
    }

    // 1. Exact endpoint-ID match (stable identifier; highest confidence).
    for (device, name, id) in &devices {
        if id.as_deref() == Some(target) {
            log::info!("WASAPI: selected target device by endpoint ID: {name}");
            return Ok((device.clone(), name.clone()));
        }
    }
    for (device, name, id) in &devices {
        if id
            .as_deref()
            .is_some_and(|id| id.eq_ignore_ascii_case(target))
        {
            log::info!("WASAPI: selected target device by endpoint ID (case-insensitive): {name}");
            return Ok((device.clone(), name.clone()));
        }
    }

    // 2. Friendly-name match: exact, then case-insensitive.
    for (device, name, _) in &devices {
        if name == target {
            log::info!("WASAPI: selected target device: {name}");
            return Ok((device.clone(), name.clone()));
        }
    }
    for (device, name, _) in &devices {
        if name.eq_ignore_ascii_case(target) {
            log::info!("WASAPI: selected target device: {name} (case-insensitive)");
            return Ok((device.clone(), name.clone()));
        }
    }

    // 3. Substring fallback (ambiguous; kept only for backward compatibility).
    for (device, name, _) in &devices {
        if name.contains(target) {
            log::warn!(
                "WASAPI: target '{}' matched device '{}' by substring only; \
                 prefer an exact device name or endpoint ID",
                target,
                name
            );
            return Ok((device.clone(), name.clone()));
        }
    }

    log::warn!(
        "WASAPI: target audio device '{}' not found among {} active render endpoint(s); \
         falling back to the default render endpoint",
        target,
        devices.len()
    );
    default(enumerator)
}

/// A negotiated exclusive-mode client: the initialized `IAudioClient`, its
/// render service, the event, and the negotiated format parameters.
pub(crate) struct ExclusiveClient {
    pub(crate) audio_client: IAudioClient,
    pub(crate) render_client: IAudioRenderClient,
    pub(crate) endpoint_volume: Option<IAudioEndpointVolume>,
    /// Event signaled by the audio engine each time the render buffer is
    /// available to be filled (buffer-end event).
    pub(crate) event: HANDLE,
    /// Buffer size in frames (from `IAudioClient::GetBufferSize`).
    pub(crate) buffer_size_frames: u32,
    /// Sample rate this client was initialized at.
    pub(crate) sample_rate: u32,
    /// Channels this client was initialized with.
    pub(crate) channels: u16,
    /// The sample container this client was initialized with (f32, i32,
    /// 24-bit-in-32 or i16 — the first container the endpoint accepted in
    /// exclusive mode).
    pub(crate) sample_format: WasapiContainer,
    /// Shared state written by the endpoint-volume change-notification
    /// callback (OS slider / hardware knob / programmatic sets). `None`
    /// when the endpoint lacks a volume service or registration failed.
    pub(crate) volume_callback_state: Option<VolumeCallbackState>,
    /// Keeps the registered COM callback alive for the client's lifetime;
    /// unregistered in [`Drop for ExclusiveClient`].
    pub(crate) volume_callback: Option<SendCom<IAudioEndpointVolumeCallback>>,
}

impl Drop for ExclusiveClient {
    fn drop(&mut self) {
        // Unregister the change-notification callback before the endpoint
        // volume interface is released. Fields are dropped after this body,
        // so `endpoint_volume` is still alive here.
        if let (Some(volume), Some(callback)) =
            (self.endpoint_volume.as_ref(), self.volume_callback.as_ref())
        {
            unsafe {
                let _ = volume.UnregisterControlChangeNotify(&callback.0);
            }
        }
    }
}

/// Render-thread context: everything the thread needs, moved in once at
/// spawn. `audio_client` is cloned from the owner so the thread can query
/// `GetCurrentPadding`; the owner keeps its own copy for `Stop`/volume.
pub(crate) struct RenderContext {
    pub(crate) audio_client: SendCom<IAudioClient>,
    pub(crate) render_client: SendCom<IAudioRenderClient>,
    pub(crate) event: SendHandle,
    pub(crate) buffer: Arc<FixedFrameBuffer>,
    pub(crate) paused: Arc<AtomicBool>,
    pub(crate) in_callback: Arc<AtomicBool>,
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) underruns: Arc<AtomicU32>,
    pub(crate) clip_counter: Arc<AtomicU32>,
    pub(crate) nan_counter: Arc<AtomicU32>,
    pub(crate) stream_errors: StreamErrorState,
    pub(crate) buffer_size_frames: u32,
    pub(crate) channels: usize,
    /// The negotiated container; the write step converts the f32 scratch
    /// block into this format (i16/i24-in-32/i32 with TPDF dither, f32
    /// passthrough).
    pub(crate) sample_format: WasapiContainer,
    pub(crate) dither_enabled: Arc<AtomicBool>,
    /// f32 → i16/i32 converter with TPDF dither, owned by the render thread
    /// so the dither state carries across periods (cpal-callback parity).
    pub(crate) converter: AudioFormatConverter,
}

/// Activate a fresh client and negotiate + initialize it in exclusive mode.
///
/// Tries the containers in preference order — f32, then i32, then
/// 24-bit-in-32, then i16 — and opens the first one the endpoint accepts in
/// exclusive mode at `rate` (each on a fresh client, since a client can
/// only be initialized once). This is the integer-container fallback: an
/// endpoint that refuses f32 exclusive mode still gets a real exclusive
/// stream (with TPDF dither at the 16/24-bit quantization boundary in the
/// render loop) instead of silently running through the shared mixer via
/// cpal.
///
/// Returns `Err` (which the factory turns into a cpal shared-mode fallback)
/// only when **every** container is refused at the requested rate — the
/// honest outcome; a cpal "exclusive" stream would silently run through the
/// shared mixer instead.
/// Container negotiation order for [`open_exclusive_client_preferring`]:
/// `preferred` first (when given), then the standard preference order —
/// f32, then i32, then 24-bit-in-32, then i16 — minus the preferred entry.
pub(crate) fn candidate_order(preferred: Option<WasapiContainer>) -> Vec<WasapiContainer> {
    const BASE_ORDER: [WasapiContainer; 4] = [
        WasapiContainer::F32,
        WasapiContainer::I32,
        WasapiContainer::I24Le,
        WasapiContainer::I16,
    ];
    let mut candidates: Vec<WasapiContainer> = Vec::with_capacity(5);
    if let Some(p) = preferred {
        candidates.push(p);
    }
    candidates.extend(BASE_ORDER.iter().copied().filter(|f| Some(*f) != preferred));
    candidates
}

pub(crate) fn open_exclusive_client(
    device: &IMMDevice,
    rate: u32,
) -> Result<ExclusiveClient, OutputError> {
    open_exclusive_client_preferring(device, rate, None)
}

/// Activate a fresh client and negotiate + initialize it in exclusive mode,
/// trying `preferred` first (e.g. I32 for DSD-over-PCM) before the standard
/// preference order — f32, then i32, then 24-bit-in-32, then i16 — and
/// opening the first one the endpoint accepts in exclusive mode at `rate`
/// (each on a fresh client, since a client can only be initialized once).
///
/// The preferred container is a *request*, not a guarantee: an endpoint that
/// refuses it still opens the next acceptable container, and callers that
/// need an exact container (the engine's DoP path verifies
/// `sample_format() == I32`) fall back explicitly when the negotiated
/// container differs — never silently.
pub(crate) fn open_exclusive_client_preferring(
    device: &IMMDevice,
    rate: u32,
    preferred: Option<WasapiContainer>,
) -> Result<ExclusiveClient, OutputError> {
    // Exclusive-mode buffer sizing: the buffer duration must be an integer
    // multiple of the periodicity, and the periodicity must be the default
    // device period or a multiple of the minimum. 10× the default period is
    // a conservative, widely-compatible choice.
    // (windows 0.59 exposes these as out-params, not a tuple.)
    let mut default_period = 0i64;
    let mut _min_period = 0i64;

    let candidates = candidate_order(preferred);
    let mut failures: Vec<String> = Vec::new();

    for format in candidates {
        let client: IAudioClient = match unsafe { device.Activate(CLSCTX_ALL, None) } {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("IAudioClient::Activate: {e}"));
                continue;
            }
        };

        let wfx = build_format(rate, 2, format);
        if !exclusive_format_supported(&client, &wfx) {
            failures.push(format!("{format:?} refused (AUDCLNT_E_UNSUPPORTED_FORMAT)"));
            continue;
        }

        unsafe { client.GetDevicePeriod(Some(&mut default_period), Some(&mut _min_period)) }
            .map_err(|e| OutputError::StreamOpen(format!("IAudioClient::GetDevicePeriod: {e}")))?;
        let periodicity = default_period;
        let buffer_duration = default_period * 10;

        // The exclusive-mode open. Success here (plus Start() in
        // start_client) is what makes output_info() report verified
        // exclusivity.
        let init = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_duration,
                periodicity,
                &wfx.Format as *const WAVEFORMATEX,
                None,
            )
        };
        if let Err(e) = init {
            failures.push(format!("Initialize({format:?}): {e}"));
            continue;
        }

        let buffer_size_frames = unsafe { client.GetBufferSize() }
            .map_err(|e| OutputError::StreamOpen(format!("IAudioClient::GetBufferSize: {e}")))?;
        let render_client: IAudioRenderClient = unsafe { client.GetService() }
            .map_err(|e| OutputError::StreamOpen(format!("GetService(IAudioRenderClient): {e}")))?;
        // Optional: some endpoints lack an endpoint-volume service; hardware
        // volume then reports unsupported instead of failing every set.
        let endpoint_volume = unsafe { client.GetService::<IAudioEndpointVolume>() }.ok();

        // Register an IAudioEndpointVolume change-notification callback so
        // external volume changes (OS slider, hardware knob, other apps)
        // can be surfaced in PlaybackInfo. Registration failure is
        // non-fatal: the stream still works, it just cannot observe
        // external changes.
        let (volume_callback_state, volume_callback) = match endpoint_volume.clone() {
            Some(volume) => {
                let state = VolumeCallbackState::default();
                let callback: IAudioEndpointVolumeCallback = EndpointVolumeCallback {
                    state: state.clone(),
                }
                .into();
                match unsafe { volume.RegisterControlChangeNotify(&callback) } {
                    Ok(()) => (Some(state), Some(SendCom(callback))),
                    Err(e) => {
                        log::warn!(
                            "WASAPI: IAudioEndpointVolume::RegisterControlChangeNotify failed \
                             ({e}); external volume changes will not be tracked"
                        );
                        (None, None)
                    }
                }
            }
            None => (None, None),
        };

        // The render-thread event is created in start_client() (a fresh
        // event is needed for every Start cycle).
        let event = HANDLE(std::ptr::null_mut());

        if format != WasapiContainer::F32 {
            log::info!(
                "WASAPI exclusive: endpoint refused f32; negotiated {format:?} container \
                 at {rate} Hz (TPDF dither at the quantization boundary)"
            );
        }
        return Ok(ExclusiveClient {
            audio_client: client,
            render_client,
            endpoint_volume,
            event,
            buffer_size_frames,
            sample_rate: rate,
            channels: 2,
            sample_format: format,
            volume_callback_state,
            volume_callback,
        });
    }

    Err(OutputError::StreamOpen(format!(
        "Endpoint refuses exclusive mode at {rate} Hz for f32/i32/i24/i16: {}",
        failures.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_order_prefers_requested_container() {
        // DoP path: I32 must be tried first, then the standard order minus I32.
        let order = candidate_order(Some(WasapiContainer::I32));
        assert_eq!(order[0], WasapiContainer::I32);
        assert_eq!(
            order,
            vec![
                WasapiContainer::I32,
                WasapiContainer::F32,
                WasapiContainer::I24Le,
                WasapiContainer::I16,
            ]
        );
    }

    #[test]
    fn candidate_order_default_matches_standard_preference() {
        let order = candidate_order(None);
        assert_eq!(
            order,
            vec![
                WasapiContainer::F32,
                WasapiContainer::I32,
                WasapiContainer::I24Le,
                WasapiContainer::I16,
            ]
        );
    }

    #[test]
    fn candidate_order_preferred_never_duplicated() {
        for p in [
            None,
            Some(WasapiContainer::F32),
            Some(WasapiContainer::I32),
            Some(WasapiContainer::I24Le),
            Some(WasapiContainer::I16),
        ] {
            let order = candidate_order(p);
            for f in order.iter() {
                assert_eq!(
                    order.iter().filter(|x| *x == f).count(),
                    1,
                    "duplicate {f:?}"
                );
            }
        }
    }
}
