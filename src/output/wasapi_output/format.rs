//! Sample-container definition and exclusive-mode format/rate negotiation.

use windows::Win32::{
    Media::{
        Audio::{
            IAudioClient, IMMDevice, AUDCLNT_SHAREMODE_EXCLUSIVE, WAVEFORMATEX,
            WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
        },
        KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM,
        Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    },
    System::Com::{CoTaskMemFree, CLSCTX_ALL},
};

use crate::output::capabilities::STANDARD_RATES;

/// WAVE_FORMAT_EXTENSIBLE (mmreg.h). Defined locally to avoid depending on
/// which windows-crate module exports it.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// SPDIF/FRONT_LEFT | FRONT_RIGHT speaker mask for a stereo stream.
const SPEAKER_FRONT_LEFT_RIGHT: u32 = 0x3;

/// A sample container this backend can negotiate and render. Broader than
/// cpal's `SampleFormat` because WASAPI endpoints can open 24-bit-in-32
/// (`I24Le` — 24 valid bits in a 32-bit PCM container), which cpal cannot
/// express; `Output::sample_format()` reports I24Le as
/// `cpal::SampleFormat::I32` (the container width).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasapiContainer {
    F32,
    I32,
    I24Le,
    I16,
}

impl WasapiContainer {
    /// The cpal-vocabulary equivalent (container width). I24Le maps to I32:
    /// it IS a 32-bit container holding 24 valid bits.
    pub(crate) fn cpal(self) -> cpal::SampleFormat {
        match self {
            WasapiContainer::F32 => cpal::SampleFormat::F32,
            WasapiContainer::I32 | WasapiContainer::I24Le => cpal::SampleFormat::I32,
            WasapiContainer::I16 => cpal::SampleFormat::I16,
        }
    }
}

/// The endpoint's default (shared-mix) rate — the closest reasonable guess
/// for the initial exclusive open. The real negotiation happens in
/// [`open_exclusive_client`].
pub(crate) fn default_exclusive_rate(device: &IMMDevice) -> Option<u32> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;
    let mix = unsafe { client.GetMixFormat() }.ok()?;
    let rate = unsafe { (*mix).nSamplesPerSec };
    unsafe {
        CoTaskMemFree(Some(mix as *const _));
    }
    Some(rate)
}

/// Build a WAVEFORMATEXTENSIBLE for the given rate/channels and container.
///
/// f32 uses `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`; the integer containers use
/// `KSDATAFORMAT_SUBTYPE_PCM`. I24Le is a 32-bit container (`wBitsPerSample`
/// = 32) holding 24 valid bits (`wValidBitsPerSample` = 24); per the
/// extensible-format convention the sample data is left-justified in the
/// container, which the render path does (`<< 8`).
pub(crate) fn build_format(
    rate: u32,
    channels: u16,
    format: WasapiContainer,
) -> WAVEFORMATEXTENSIBLE {
    let (bits, valid_bits, bytes_per_sample, subtype) = match format {
        WasapiContainer::I16 => (16, 16, 2, KSDATAFORMAT_SUBTYPE_PCM),
        WasapiContainer::I24Le => (32, 24, 4, KSDATAFORMAT_SUBTYPE_PCM),
        WasapiContainer::I32 => (32, 32, 4, KSDATAFORMAT_SUBTYPE_PCM),
        WasapiContainer::F32 => (32, 32, 4, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT),
    };
    let bytes_per_frame = u32::from(channels) * bytes_per_sample;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * bytes_per_frame,
            nBlockAlign: bytes_per_frame as u16,
            wBitsPerSample: bits,
            cbSize: 22, // WAVEFORMATEXTENSIBLE tail (Samples + dwChannelMask + SubFormat)
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: SPEAKER_FRONT_LEFT_RIGHT,
        SubFormat: subtype,
    }
}

/// Whether the endpoint accepts the format in exclusive mode. Real
/// negotiation (not a name/rate heuristic): every true is an
/// `IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, ...)` success.
pub(crate) fn exclusive_format_supported(
    client: &IAudioClient,
    format: &WAVEFORMATEXTENSIBLE,
) -> bool {
    // `IsFormatSupported` returns the HRESULT directly (not a `Result`). The
    // closest-match out-param is only meaningful in shared mode, so None.
    unsafe {
        client.IsFormatSupported(
            AUDCLNT_SHAREMODE_EXCLUSIVE,
            &format.Format as *const WAVEFORMATEX,
            None,
        )
    }
    .is_ok()
}

/// Probe every standard rate in exclusive mode (on a fresh, un-initialized
/// client) and keep the ones that pass. These are the rates the capabilities
/// snapshot reports — every entry is a genuine exclusive-mode probe result.
pub(crate) fn probe_exclusive_rates(device: &IMMDevice) -> Vec<u32> {
    let client = match unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    STANDARD_RATES
        .iter()
        .copied()
        .filter(|&rate| {
            let fmt = build_format(rate, 2, WasapiContainer::F32);
            exclusive_format_supported(&client, &fmt)
        })
        .collect()
}

/// Probe which sample containers the endpoint accepts in exclusive mode at
/// the given rate (f32, then i32, then 24-bit-in-32, then i16 — the
/// backend's render order). Every entry is a real
/// `IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, ...)` success; used for
/// the capabilities snapshot.
pub(crate) fn probe_supported_formats(device: &IMMDevice, rate: u32) -> Vec<WasapiContainer> {
    let client = match unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    [
        WasapiContainer::F32,
        WasapiContainer::I32,
        WasapiContainer::I24Le,
        WasapiContainer::I16,
    ]
    .into_iter()
    .filter(|&fmt| exclusive_format_supported(&client, &build_format(rate, 2, fmt)))
    .collect()
}
