//! COM ownership and thread-safety wrappers.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::{Foundation::HANDLE, System::Com::CoUninitialize};

/// Event-handle wrapper: `HANDLE` is not `Send`, but the render thread owns
/// the only waiter, so moving it across threads is safe (the handle is not
/// closed until the thread has been joined).
pub(crate) struct SendHandle(pub(crate) HANDLE);
unsafe impl Send for SendHandle {}

/// COM interface wrapper for the render thread. windows-rs 0.59 interfaces
/// are `NonNull<c_void>` handles and are not `Send`/`Sync` by default, but
/// COM refcounting is thread-safe by contract (AddRef/Release are atomic), so
/// moving a cloned interface into the render thread is sound.
/// The native backend has its own explicit COM ownership model; CPAL streams
/// use a dedicated owner thread and do not need an equivalent wrapper.
pub(crate) struct SendCom<T>(pub(crate) T);
unsafe impl<T> Send for SendCom<T> {}
unsafe impl<T> Sync for SendCom<T> {}

/// Guard that un-initializes COM for this thread on drop (used to keep the
/// constructor's error paths leak-free; disarmed by `mem::forget` on success
/// because the output owns the COM lifetime after construction).
pub(crate) struct ComInitGuard;
impl Drop for ComInitGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Guard marking the render thread's callback section (used by `pause()` to
/// wait for the current pull to finish).
pub(crate) struct CallbackGuard<'a>(&'a AtomicBool);
impl<'a> CallbackGuard<'a> {
    pub(crate) fn new(flag: &'a AtomicBool) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag)
    }
}
impl Drop for CallbackGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
