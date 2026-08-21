//! Platform "user data directory" resolution via `etcetera`.
//!
//! This replaces the former `dirs::data_local_dir()` calls. `etcetera` is the
//! actively maintained replacement (Rust CLI working group; used by cargo
//! itself). Its `data_dir()` matches `dirs::data_local_dir()` on Linux (XDG
//! `$XDG_DATA_HOME`, defaulting to `~/.local/share`) and macOS
//! (`~/Library/Application Support`); on Windows it resolves to roaming
//! `%APPDATA%` instead of `%LOCALAPPDATA%`, which is immaterial for the
//! cache/profile files written below.

use etcetera::BaseStrategy;
use std::path::PathBuf;

/// The user's local application-data directory (best effort, mirroring the
/// semantics of `dirs::data_local_dir()`). Returns `None` when no strategy
/// can be determined (e.g. no home directory in the environment).
pub fn data_local_dir() -> Option<PathBuf> {
    etcetera::base_strategy::choose_base_strategy()
        .ok()
        .map(|strategy| strategy.data_dir())
}
