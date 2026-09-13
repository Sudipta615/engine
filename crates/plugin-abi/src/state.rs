//! Plugin state save/restore over caller-owned buffers.
//!
//! The ABI's `save_state`/`load_state` cross plain byte buffers; this
//! module provides the host-side [`StateBuffer`] wrapper that owns its
//! bytes, grows on the control path, and hands the ABI raw pointers +
//! lengths. Serialized bytes are opaque to the host — the plugin owns
//! the format (recommended: its own serde types via the `serde-types`
//! feature).

use crate::{AbiStatus, PluginAbiError, MAX_STATE_BYTES};

/// A caller-owned state buffer with bounded growth (≤
/// [`MAX_STATE_BYTES`]).
#[derive(Debug, Default, Clone)]
pub struct StateBuffer {
    bytes: Vec<u8>,
}

impl StateBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The serialized bytes (empty when no state was saved).
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of serialized bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Adopt bytes returned by a save call (control path). Rejects
    /// oversize payloads with [`AbiStatus::BadState`]. `saved` is the
    /// host-owned buffer the plugin wrote `status` bytes into.
    pub fn from_save(&mut self, status: isize, saved: &[u8]) -> Result<(), PluginAbiError> {
        if status < 0 {
            return Err(PluginAbiError::Status(AbiStatus::BadState));
        }
        let len = status as usize;
        if len > MAX_STATE_BYTES {
            return Err(PluginAbiError::State(format!(
                "state is {len} bytes, over the {}-byte cap",
                MAX_STATE_BYTES
            )));
        }
        if len == 0 {
            self.bytes.clear();
            return Ok(());
        }
        if len > saved.len() {
            return Err(PluginAbiError::Status(AbiStatus::InvalidArgument));
        }
        self.bytes.clear();
        self.bytes.extend_from_slice(&saved[..len]);
        Ok(())
    }

    /// Prepare a load view for `load_state`: `(ptr, len)` or `(null, 0)`
    /// for an empty state.
    pub fn as_load_args(&self) -> (*const u8, usize) {
        if self.bytes.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (self.bytes.as_ptr(), self.bytes.len())
        }
    }

    /// Replace the contents from a byte slice (control path; used by
    /// config load / state restore).
    pub fn set_bytes(&mut self, bytes: &[u8]) -> Result<(), PluginAbiError> {
        if bytes.len() > MAX_STATE_BYTES {
            return Err(PluginAbiError::State(format!(
                "state is {} bytes, over the {}-byte cap",
                bytes.len(),
                MAX_STATE_BYTES
            )));
        }
        self.bytes.clear();
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
}

/// Errors from host-side state handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginStateError {
    /// The plugin refused the load (corrupt / wrong-version payload).
    Refused,
    /// The payload exceeded the size cap.
    TooLarge { size: usize },
    /// No state was saved yet.
    Empty,
}

impl std::fmt::Display for PluginStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginStateError::Refused => write!(f, "plugin refused the state payload"),
            PluginStateError::TooLarge { size } => {
                write!(f, "plugin state is {size} bytes, over the cap")
            }
            PluginStateError::Empty => write!(f, "no plugin state saved"),
        }
    }
}

impl std::error::Error for PluginStateError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_roundtrip() {
        let mut b = StateBuffer::new();
        assert!(b.is_empty());
        let (p, l) = b.as_load_args();
        assert!(p.is_null());
        assert_eq!(l, 0);
        assert!(b.from_save(0, &[]).is_ok());
    }

    #[test]
    fn adopt_and_load() {
        let mut b = StateBuffer::new();
        let backing = [7u8; 4];
        assert!(b.from_save(4, &backing).is_ok());
        assert_eq!(b.as_slice(), &[7, 7, 7, 7]);
        let (p, l) = b.as_load_args();
        assert!(!p.is_null());
        assert_eq!(l, 4);
    }

    #[test]
    fn negative_save_status_refuses() {
        let mut b = StateBuffer::new();
        assert!(matches!(
            b.from_save(-1, &[]),
            Err(PluginAbiError::Status(AbiStatus::BadState))
        ));
    }
}
