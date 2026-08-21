//! Typed helpers over the CoreAudio HAL property API.
//!
//! Everything here is macOS-only (`cfg(target_os = "macos")` in the parent
//! module). The `objc2-core-audio` crate ships pre-generated bindings (no
//! build-time bindgen, no macOS SDK required to *compile* — the C headers are
//! only needed to *link*, which happens on a Mac), so this module stays
//! dependency-light and mirrors the established `unsafe` style of
//! `crate::output::cpal_output::volume::coreaudio`.
//!
//! All `unsafe` blocks here follow the same discipline: buffers and sizes are
//! valid, non-null, and outlive the call; qualifier data is `null` because
//! none of the properties used take qualifiers; status codes are checked and
//! surfaced as `Result`.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectIsPropertySettable, AudioObjectPropertyAddress, AudioObjectPropertyScope,
    AudioObjectPropertySelector, AudioObjectSetPropertyData,
};
use objc2_core_foundation::{CFRetained, CFString, CFStringBuiltInEncodings};

/// The CoreAudio success status.
pub const NO_ERROR: i32 = 0;

/// Build a property address (global scope + main element by default).
#[inline]
pub fn addr(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: u32,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
    }
}

/// Read the size of a property's data (bytes).
///
/// # Safety
///
/// `object` and `address` must be valid HAL objects; no qualifier data is
/// used for any property this module reads.
pub unsafe fn property_size(
    object: AudioObjectID,
    address: &mut AudioObjectPropertyAddress,
) -> Result<usize, i32> {
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(address),
            0,
            ptr::null::<c_void>(),
            NonNull::from(&mut size),
        )
    };
    if status == NO_ERROR {
        Ok(size as usize)
    } else {
        Err(status)
    }
}

/// Read a fixed-size (`Copy`) property value. The data type must match the
/// property; the caller is responsible for that contract (checked by the
/// HAL for most properties).
///
/// # Safety
///
/// `object` and `address` must be valid; `T` must be the property's data
/// type (or a `#[repr(C)]` struct with the same layout).
pub unsafe fn get<T: Copy>(
    object: AudioObjectID,
    address: &mut AudioObjectPropertyAddress,
) -> Result<T, i32> {
    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(address),
            0,
            ptr::null::<c_void>(),
            NonNull::from(&mut size),
            NonNull::new(value.as_mut_ptr() as *mut c_void).unwrap(),
        )
    };
    if status == NO_ERROR {
        Ok(unsafe { value.assume_init() })
    } else {
        Err(status)
    }
}

/// Read a fixed-size property, treating errors as `None` (for optional
/// properties such as the current hog-mode owner, where "no such property"
/// is a legitimate answer).
///
/// # Safety
///
/// Same contract as [`get`].
pub unsafe fn get_opt<T: Copy>(
    object: AudioObjectID,
    address: &mut AudioObjectPropertyAddress,
) -> Option<T> {
    unsafe { get(object, address) }.ok()
}

/// Set a fixed-size property value.
///
/// # Safety
///
/// `object` and `address` must be valid; `T` must be the property's data
/// type.
pub unsafe fn set<T: Copy>(
    object: AudioObjectID,
    address: &mut AudioObjectPropertyAddress,
    value: &T,
) -> Result<(), i32> {
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(address),
            0,
            ptr::null::<c_void>(),
            std::mem::size_of::<T>() as u32,
            NonNull::new(value as *const T as *mut c_void).unwrap(),
        )
    };
    if status == NO_ERROR {
        Ok(())
    } else {
        Err(status)
    }
}

/// Whether `object`'s property is writable.
///
/// # Safety
///
/// `object` and `address` must be valid.
pub unsafe fn is_settable(object: AudioObjectID, address: &mut AudioObjectPropertyAddress) -> bool {
    let mut settable: u8 = 0;
    let status = unsafe {
        AudioObjectIsPropertySettable(object, NonNull::from(address), NonNull::from(&mut settable))
    };
    status == NO_ERROR && settable != 0
}

/// Read a CFString-typed property (e.g. `kAudioDevicePropertyDeviceUID`,
/// `kAudioDevicePropertyDeviceNameCFString`) into a Rust `String`.
///
/// The HAL returns a +1 CFString; it is owned via `CFRetained` and released
/// on drop. Conversion prefers the fast C-pointer path and falls back to a
/// UTF-16 decode when the string is not stored in a direct representation.
///
/// # Safety
///
/// `object` and `address` must be valid and the property must be a CFString.
pub unsafe fn get_cfstring(
    object: AudioObjectID,
    address: &mut AudioObjectPropertyAddress,
) -> Option<String> {
    let raw: *mut c_void = unsafe { get_opt(object, address) }?;
    if raw.is_null() {
        return None;
    }
    // SAFETY: the HAL handed us a +1 CFString for this property; owning it
    // via CFRetained releases it exactly once when dropped.
    let string = unsafe { CFRetained::from_raw(NonNull::new(raw as *mut CFString).unwrap()) };
    let cf = &*string;

    // Fast path: direct UTF-8 C pointer.
    let ptr = cf.c_string_ptr(CFStringBuiltInEncodings::EncodingUTF8.0);
    if !ptr.is_null() {
        // SAFETY: `ptr` is NUL-terminated and valid for the lifetime of `cf`.
        let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
        return Some(cstr.to_string_lossy().into_owned());
    }

    // Fallback: UTF-16 decode via `characters`.
    let len = cf.length();
    if len <= 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is `len` UTF-16 units long, matching `characters`.
    unsafe {
        cf.characters(
            objc2_core_foundation::CFRange {
                location: 0,
                length: len,
            },
            buf.as_mut_ptr(),
        );
    }
    String::from_utf16(&buf).ok()
}
