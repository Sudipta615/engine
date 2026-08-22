//! Audio source representation for the independent audio engine.
//!
//! [`AudioSource`] decouple the engine from host-specific identifiers (such as database IDs,
//! playlist indices, or UI references). The host resolves its own domain concepts into an
//! explicit `AudioSource` before communicating with the engine.

use std::fmt;
use std::path::{Path, PathBuf};

/// An explicit audio source that the engine can open and decode.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AudioSource {
    /// A local filesystem path.
    File(PathBuf),
    /// A resource identifier (e.g. `file:///path/to/audio.flac`).
    Uri(String),
    /// In-memory audio payload with a format/extension hint.
    Memory {
        /// Raw file bytes.
        data: Vec<u8>,
        /// File extension hint (e.g. "flac", "wav", "mp3") to guide format probing.
        extension_hint: Option<String>,
    },
}

impl AudioSource {
    /// Create a file-backed audio source.
    pub fn from_file(path: impl Into<PathBuf>) -> Self {
        Self::File(path.into())
    }

    /// Create a URI audio source.
    pub fn from_uri(uri: impl Into<String>) -> Self {
        Self::Uri(uri.into())
    }

    /// Create an in-memory audio source.
    pub fn from_memory(data: Vec<u8>, extension_hint: Option<String>) -> Self {
        Self::Memory {
            data,
            extension_hint,
        }
    }

    /// Returns the local filesystem path if this source is backed by a file.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path.as_path()),
            _ => None,
        }
    }

    /// Returns true if this source refers to a local file.
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Returns a human-readable display label for diagnostics and telemetry.
    pub fn display_name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned()),
            Self::Uri(uri) => uri.clone(),
            Self::Memory { extension_hint, data } => {
                let ext = extension_hint.as_deref().unwrap_or("unknown");
                format!("<memory: {} bytes, hint: {}>", data.len(), ext)
            }
        }
    }
}

impl fmt::Display for AudioSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

impl From<PathBuf> for AudioSource {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&Path> for AudioSource {
    fn from(path: &Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

impl From<&str> for AudioSource {
    fn from(s: &str) -> Self {
        if s.starts_with("file://") || s.starts_with("http://") || s.starts_with("https://") {
            Self::Uri(s.to_string())
        } else {
            Self::File(PathBuf::from(s))
        }
    }
}

impl From<String> for AudioSource {
    fn from(s: String) -> Self {
        if s.starts_with("file://") || s.starts_with("http://") || s.starts_with("https://") {
            Self::Uri(s)
        } else {
            Self::File(PathBuf::from(s))
        }
    }
}
