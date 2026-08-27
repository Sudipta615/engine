//! Pluggable byte source for audio decoding.

use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

/// A seekable byte stream for audio decoding.
pub trait AudioByteSource: Read + Seek + fmt::Debug + Send {
    fn extension(&self) -> &str;
    fn len(&self) -> Option<u64>;
    fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }
    fn display_name(&self) -> String;
}

/// A local file opened as a byte source.
pub struct FileByteSource {
    inner: BufReader<File>,
    path: std::path::PathBuf,
    extension: String,
    len: u64,
}

impl fmt::Debug for FileByteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileByteSource")
            .field("path", &self.path)
            .field("len", &self.len)
            .finish()
    }
}

impl FileByteSource {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Ok(Self {
            inner: BufReader::new(file),
            path,
            extension,
            len,
        })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Read for FileByteSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for FileByteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl AudioByteSource for FileByteSource {
    fn extension(&self) -> &str {
        &self.extension
    }
    fn len(&self) -> Option<u64> {
        Some(self.len)
    }
    fn display_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// An in-memory byte buffer presented as a seekable source.
pub struct MemoryByteSource {
    inner: Cursor<Vec<u8>>,
    extension: String,
}

impl fmt::Debug for MemoryByteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let data = self.inner.get_ref();
        f.debug_struct("MemoryByteSource")
            .field("len", &data.len())
            .field("extension", &self.extension)
            .finish()
    }
}

impl MemoryByteSource {
    pub fn new(data: Vec<u8>, extension: &str) -> Self {
        Self {
            inner: Cursor::new(data),
            extension: extension.to_ascii_lowercase(),
        }
    }
}

impl Read for MemoryByteSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for MemoryByteSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl AudioByteSource for MemoryByteSource {
    fn extension(&self) -> &str {
        &self.extension
    }
    fn len(&self) -> Option<u64> {
        Some(self.inner.get_ref().len() as u64)
    }
    fn display_name(&self) -> String {
        format!(
            "<memory: {} bytes, hint: {}>",
            self.inner.get_ref().len(),
            self.extension
        )
    }
}

// ── Network Byte Source (HTTP Range requests) ───────────────────────────────

#[cfg(feature = "network-streaming")]
mod network {
    use std::fmt;
    use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
    use std::time::Duration;

    use super::AudioByteSource;

    /// Default number of bytes fetched per HTTP Range request.
    const DEFAULT_CHUNK_SIZE: usize = 65536;

    /// How long to wait for the initial HEAD / first GET probe.
    const PROBE_TIMEOUT_SECS: u64 = 15;
    /// How long to wait for each subsequent Range GET.
    const FETCH_TIMEOUT_SECS: u64 = 10;

    /// A seekable byte source backed by an HTTP(S) URL using Range requests.
    ///
    /// The source maintains a grow-only buffer starting at byte 0. When the
    /// decoder reads past the buffered prefix, a `Range: bytes=start-end` GET
    /// fetches the missing window. When the server does not advertise
    /// `Accept-Ranges: bytes`, the source falls back to a single GET that
    /// streams the whole file into the buffer.
    ///
    /// # Memory
    ///
    /// The buffer grows to cover every byte that has ever been read, so
    /// long-lived playback of very large files (multi-GB) will eventually hold
    /// the whole file in RAM. For typical audio files (FLAC albums, WAV
    /// stems) this is a few hundred MB at most — well within the realm of
    /// a desktop audio engine.
    pub struct NetworkByteSource {
        url: String,
        agent: ureq::Agent,
        content_length: Option<u64>,
        accepts_ranges: bool,
        extension: String,
        position: u64,
        buffer: Vec<u8>,
        /// When `true`, every byte has already been downloaded (either the
        /// server returned Content-Length matching the buffer, or a non-Range
        /// GET completed). Further reads beyond the buffer are impossible.
        fully_downloaded: bool,
    }

    impl fmt::Debug for NetworkByteSource {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("NetworkByteSource")
                .field("url", &self.url)
                .field("content_length", &self.content_length)
                .field("buffered", &self.buffer.len())
                .field("position", &self.position)
                .field("accepts_ranges", &self.accepts_ranges)
                .finish()
        }
    }

    impl NetworkByteSource {
        /// Create a new network source by probing `url`.
        ///
        /// Sends a HEAD request to discover the content length and whether
        /// the server supports byte-range requests. Falls back to a
        /// `Range: bytes=0-0` probe when HEAD is rejected (some servers
        /// respond to GET but not HEAD).
        pub fn open(url: &str) -> io::Result<Self> {
            let agent = ureq::AgentBuilder::new()
                .timeout_read(Duration::from_secs(PROBE_TIMEOUT_SECS))
                .timeout_write(Duration::from_secs(10))
                .build();

            let extension = url
                .rsplit('.')
                .next()
                .and_then(|ext| {
                    // Strip query params / fragments
                    let clean = ext.split('?').next().unwrap_or(ext);
                    let clean = clean.split('#').next().unwrap_or(clean);
                    if clean.len() <= 10 && clean.is_ascii() {
                        Some(clean.to_ascii_lowercase())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            // Probe: try HEAD, fall back to GET Range:0-0
            let (content_length, accepts_ranges) = Self::probe(&agent, url)?;

            Ok(Self {
                url: url.to_string(),
                agent,
                content_length,
                accepts_ranges,
                extension,
                position: 0,
                buffer: Vec::new(),
                fully_downloaded: false,
            })
        }

        /// The source URL.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Reported content length, if the server returned it.
        pub fn content_length(&self) -> Option<u64> {
            self.content_length
        }

        /// True when the server advertised `Accept-Ranges: bytes`.
        pub fn accepts_ranges(&self) -> bool {
            self.accepts_ranges
        }

        // ── internals ──────────────────────────────────────────────────

        /// Probe the server for content-length and range support.
        fn probe(agent: &ureq::Agent, url: &str) -> io::Result<(Option<u64>, bool)> {
            // Try HEAD first.
            match agent.head(url).call() {
                Ok(resp) => {
                    let len = resp
                        .header("Content-Length")
                        .and_then(|v| v.parse::<u64>().ok());
                    let ranges = resp
                        .header("Accept-Ranges")
                        .is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
                    return Ok((len, ranges));
                }
                Err(ureq::Error::Status(405, _)) | Err(ureq::Error::Status(501, _)) => {
                    // Method Not Allowed / Not Implemented — fall through to
                    // GET probe.
                }
                Err(e) => {
                    return Err(io::Error::new(
                        ErrorKind::ConnectionRefused,
                        format!("HEAD probe failed for {}: {}", url, e),
                    ));
                }
            }

            // Fall back to GET with Range:0-0.
            match agent
                .get(url)
                .set("Range", "bytes=0-0")
                .timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
                .call()
            {
                Ok(resp) => {
                    let ranges = resp
                        .header("Accept-Ranges")
                        .is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
                    // Content-Range: bytes 0-0/12345
                    let len = resp
                        .header("Content-Range")
                        .and_then(|v| v.rsplit('/').next())
                        .and_then(|v| v.parse::<u64>().ok());
                    Ok((len, ranges))
                }
                Err(e) => Err(io::Error::new(
                    ErrorKind::ConnectionRefused,
                    format!("GET probe failed for {}: {}", url, e),
                )),
            }
        }

        /// Fetch bytes starting at `start` (inclusive) up to the lesser of
        /// `start + DEFAULT_CHUNK_SIZE - 1` and `content_length - 1`.
        /// Appends the response body to `self.buffer`.
        fn fetch_chunk(&mut self, start: u64) -> io::Result<()> {
            let end = match self.content_length {
                Some(len) if len > 0 => (start + DEFAULT_CHUNK_SIZE as u64 - 1).min(len - 1),
                _ => start + DEFAULT_CHUNK_SIZE as u64 - 1,
            };

            if start > end {
                return Ok(());
            }

            let range_value = format!("bytes={}-{}", start, end);
            let resp = self
                .agent
                .get(&self.url)
                .set("Range", &range_value)
                .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
                .call()
                .map_err(|e| {
                    io::Error::new(
                        ErrorKind::UnexpectedEof,
                        format!("Range GET {} failed: {}", range_value, e),
                    )
                })?;

            let status = resp.status();
            let mut body_bytes = Vec::new();
            resp.into_reader()
                .read_to_end(&mut body_bytes)
                .map_err(|e| {
                    io::Error::new(
                        ErrorKind::UnexpectedEof,
                        format!("reading response body: {}", e),
                    )
                })?;

            if status == 206 {
                // Partial Content — expected for Range requests.
                if start as usize > self.buffer.len() {
                    // Gap in the buffer — the server returned bytes we haven't
                    // requested yet. Fill the gap with zeros (shouldn't happen
                    // with sequential reads, but be safe).
                    self.buffer.resize(start as usize, 0);
                }
                self.buffer.extend_from_slice(&body_bytes);

                // If we fetched up to the last byte, mark fully downloaded.
                if let Some(content_len) = self.content_length {
                    if self.buffer.len() as u64 >= content_len {
                        self.fully_downloaded = true;
                    }
                }
            } else if status == 200 {
                // Server ignored the Range header — full file returned.
                self.buffer = body_bytes;
                self.content_length = Some(self.buffer.len() as u64);
                self.fully_downloaded = true;
                self.accepts_ranges = false;
            } else {
                return Err(io::Error::other(format!(
                    "unexpected HTTP status {} for Range request",
                    status
                )));
            }

            Ok(())
        }

        /// Stream the remainder of the file from the current position via a
        /// non-Range GET. Used when the server doesn't support Range requests.
        fn stream_remainder_from(&mut self, start: u64) -> io::Result<usize> {
            let resp = self
                .agent
                .get(&self.url)
                .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS * 3))
                .call()
                .map_err(|e| {
                    io::Error::new(
                        ErrorKind::UnexpectedEof,
                        format!("full GET failed for {}: {}", self.url, e),
                    )
                })?;

            let mut body = Vec::new();
            resp.into_reader().read_to_end(&mut body).map_err(|e| {
                io::Error::new(ErrorKind::UnexpectedEof, format!("reading response: {}", e))
            })?;

            self.buffer = body;
            self.content_length = Some(self.buffer.len() as u64);
            self.fully_downloaded = true;
            self.accepts_ranges = false;

            // Return bytes readable from `start`.
            let available = self.buffer.len().saturating_sub(start as usize);
            Ok(available)
        }
    }

    impl Read for NetworkByteSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }

            let needed_end = self.position + buf.len() as u64;

            // If we know the total length and the position is at or past it,
            // there is nothing more to read.
            if let Some(len) = self.content_length {
                if self.position >= len {
                    return Ok(0);
                }
            }

            // Make sure the buffer covers the requested range.
            while (self.buffer.len() as u64) < needed_end && !self.fully_downloaded {
                if self.accepts_ranges {
                    // Fetch from the next unbuffered byte forward.
                    let fetch_start = (self.buffer.len() as u64).max(self.position);
                    if fetch_start < needed_end {
                        self.fetch_chunk(fetch_start)?;
                    } else {
                        break;
                    }
                } else {
                    // No Range support — download the whole file.
                    let available = self.stream_remainder_from(self.position)?;
                    if available == 0 {
                        self.fully_downloaded = true;
                        return Ok(0);
                    }
                    break;
                }
            }

            // Copy from buffer to output.
            let pos = self.position as usize;
            let available = self.buffer.len().saturating_sub(pos);
            let to_copy = available.min(buf.len());

            if to_copy == 0 {
                return Ok(0);
            }

            buf[..to_copy].copy_from_slice(&self.buffer[pos..pos + to_copy]);
            self.position += to_copy as u64;
            Ok(to_copy)
        }
    }

    impl Seek for NetworkByteSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let new_pos = match pos {
                SeekFrom::Start(offset) => offset as i64,
                SeekFrom::End(offset) => {
                    let len = self.content_length.ok_or_else(|| {
                        io::Error::new(
                            ErrorKind::Unsupported,
                            "cannot SeekFrom::End without known content length",
                        )
                    })? as i64;
                    len + offset
                }
                SeekFrom::Current(offset) => self.position as i64 + offset,
            };

            if new_pos < 0 {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "seek before byte 0",
                ));
            }

            self.position = new_pos as u64;
            Ok(self.position)
        }
    }

    impl AudioByteSource for NetworkByteSource {
        fn extension(&self) -> &str {
            &self.extension
        }

        fn len(&self) -> Option<u64> {
            self.content_length
        }

        fn display_name(&self) -> String {
            // Show just the filename part of the URL for readability.
            let name = self.url.rsplit('/').next().unwrap_or(&self.url);
            // Strip query string for display.
            let clean = name.split('?').next().unwrap_or(name);
            clean.to_string()
        }
    }
}

#[cfg(feature = "network-streaming")]
pub use network::NetworkByteSource;

#[cfg(all(test, feature = "network-streaming"))]
mod tests {
    /// `NetworkByteSource` must satisfy the `Send` bound required by
    /// `AudioByteSource` so it can be passed across thread boundaries
    /// into the engine decode loop.
    #[test]
    fn network_byte_source_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<super::network::NetworkByteSource>();
    }
}

// ── Async I/O preparation ─────────────────────────────────────────────────

/// Optional subtrait for byte sources that support asynchronous data arrival
/// (network streams, pipes, FIFOs). When implemented, a dedicated I/O thread
/// can call `poll_fill()` to pre-buffer data while the decode thread reads
/// from the buffer without blocking on network I/O.
///
/// Sources that do not implement this trait are assumed to be synchronous
/// (local files, in-memory buffers) where `Read` never blocks the caller.
///
/// # Design intent
///
/// The current `NetworkByteSource` uses synchronous `ureq` calls that block
/// the decode thread during HTTP Range requests. When this trait matures,
/// `NetworkByteSource` can implement it by moving the HTTP I/O to a
/// background thread and signaling the fill status through the returned enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillStatus {
    /// More data is available; the decode loop can keep reading.
    Ready,
    /// The I/O thread is still fetching the next chunk; retry on the next tick.
    Waiting,
    /// The stream ended (EOF from server); no more data will arrive.
    Ended,
    /// An I/O error occurred; details logged separately.
    Error,
}

/// A byte source that supports asynchronous, progressive fill via an I/O
/// thread. The decode thread calls `Read` on the source as usual; the I/O
/// thread periodically calls `poll_fill()` to fetch more data from the
/// network (or other async source).
pub trait StreamingByteSource: AudioByteSource {
    /// Called by a dedicated I/O thread to fetch the next chunk of data.
    /// Returns the fill status after the attempt. The decode thread reads
    /// from the same underlying buffer through `Read`.
    fn poll_fill(&mut self) -> std::io::Result<FillStatus>;

    /// Whether the full stream has been received (EOF). When true, the I/O
    /// thread can stop calling `poll_fill`.
    fn is_complete(&self) -> bool;
}
