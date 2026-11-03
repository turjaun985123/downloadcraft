//! List a remote ZIP's contents before downloading it.
//!
//! A ZIP keeps its table of contents (the central directory) at the end of
//! the file. Fetching only the last few kilobytes with a Range request is
//! enough to show what is inside a multi-gigabyte archive, and one more
//! Range request fetches a single file out of it (IDM's "download part of a
//! ZIP"): only that file's compressed bytes cross the network.

use crate::{Error, RequestFn};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Limits for one remote ZIP operation. Every byte written is measured, not
/// the archive's own (untrusted) declaration.
#[derive(Debug, Clone, Copy)]
pub struct ZipLimits {
    /// Largest single entry, in uncompressed bytes actually written.
    pub max_entry_size: u64,
    /// Largest compressed body streamed for one entry.
    pub max_compressed_size: u64,
    /// Largest central directory fetched into memory.
    pub max_directory_size: u64,
    /// Largest number of entries accepted in a directory.
    pub max_entries: usize,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self {
            max_entry_size: 64 * 1024 * 1024 * 1024,
            max_compressed_size: 64 * 1024 * 1024 * 1024,
            max_directory_size: 8 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}

/// One file inside a ZIP archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    /// Uncompressed size.
    pub size: u64,
    pub compressed_size: u64,
    pub is_dir: bool,
    /// 0 = stored, 8 = deflate (the only two this crate unpacks).
    pub method: u16,
    /// Where the entry's local header starts in the archive.
    pub local_offset: u64,
}

impl ZipEntry {
    /// Whether [`extract_remote`] can unpack it.
    pub fn can_extract(&self) -> bool {
        !self.is_dir && (self.method == 0 || self.method == 8)
    }
}

/// Fetch one entry of the remote ZIP at `url` and write it, unpacked, to
/// `dest`. Two Range requests: the local header (its name and extra field
/// can differ from the directory's) and the compressed bytes, which are
/// inflated as they stream in. Returns the bytes written.
pub async fn extract_remote(
    request: &RequestFn,
    url: &str,
    entry: &ZipEntry,
    dest: &std::path::Path,
) -> Result<u64, Error> {
    extract_remote_with_limits(request, url, entry, dest, ZipLimits::default()).await
}

/// [`extract_remote`] with explicit limits (used by tests and embedders).
///
/// The decompressed stream is capped at the entry's declared size *before*
/// it reaches the disk, so a lying central directory cannot fill the disk:
/// the writer fails as soon as one more byte would cross the declared size.
/// The temporary file is unique, is removed on every failure path, and is
/// only renamed over the destination after its size has been verified.
pub async fn extract_remote_with_limits(
    request: &RequestFn,
    url: &str,
    entry: &ZipEntry,
    dest: &std::path::Path,
    limits: ZipLimits,
) -> Result<u64, Error> {
    use futures_util::StreamExt;
    if !entry.can_extract() {
        return Err(Error::Network(format!("{}: compression method {} is not supported", entry.name, entry.method)));
    }
    if entry.size > limits.max_entry_size {
        return Err(limit_error(&format!(
            "{} is {} bytes, larger than the {}-byte entry limit",
            entry.name, entry.size, limits.max_entry_size
        )));
    }
    if entry.compressed_size > limits.max_compressed_size {
        return Err(limit_error(&format!(
            "{} needs {} compressed bytes, larger than the {}-byte limit",
            entry.name, entry.compressed_size, limits.max_compressed_size
        )));
    }
    let (head, _, total) = fetch_range_capped(request, url, (entry.local_offset, 30), 30).await?;
    if !head.starts_with(b"PK\x03\x04") {
        return Err(bad("local header not found"));
    }
    let name_len = u16_at(&head, 26).ok_or_else(|| bad("short local header"))?;
    let extra_len = u16_at(&head, 28).ok_or_else(|| bad("short local header"))?;
    let data_at = entry
        .local_offset
        .checked_add(30)
        .and_then(|v| v.checked_add(name_len))
        .and_then(|v| v.checked_add(extra_len))
        .ok_or_else(|| bad("local header points past the end of the file"))?;
    let data_end = data_at
        .checked_add(entry.compressed_size)
        .ok_or_else(|| bad("entry data points past the end of the file"))?;
    if data_end > total {
        return Err(bad("entry data points past the end of the file"));
    }
    if let Some(dir) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let (tmp, file) = open_temp(dest)?;
    let mut guard = TempGuard { path: tmp.clone(), keep: false };
    // The limit is the declared size itself: a stream that expands further
    // is stopped before the extra bytes are written.
    let bounded = LimitedWriter::new(file, entry.size);
    let mut sink: Box<dyn Write + Send> = if entry.method == 8 {
        Box::new(flate2::write::DeflateDecoder::new(std::io::BufWriter::new(bounded)))
    } else {
        Box::new(std::io::BufWriter::new(bounded))
    };
    if entry.compressed_size > 0 {
        let end = data_end - 1;
        let resp = request(url)
            .header(reqwest::header::RANGE, format!("bytes={data_at}-{end}"))
            .send()
            .await
            .map_err(|e| Error::Network(e.to_string()))?;
        check_range_reply(&resp, data_at, end, total, url)?;
        let mut body = resp.bytes_stream();
        let mut compressed = 0u64;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| Error::Network(e.to_string()))?;
            compressed = compressed
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| limit_error("the compressed body is too large"))?;
            if compressed > entry.compressed_size || compressed > limits.max_compressed_size {
                return Err(limit_error("the server sent more compressed bytes than the entry declares"));
            }
            sink.write_all(&chunk).map_err(|e| bad(&format!("could not unpack: {e}")))?;
        }
        if compressed != entry.compressed_size {
            return Err(bad(&format!(
                "{} was cut short ({} of {} compressed bytes)",
                entry.name, compressed, entry.compressed_size
            )));
        }
    }
    sink.flush().map_err(|e| bad(&format!("could not unpack: {e}")))?;
    drop(sink);
    let written = std::fs::metadata(&tmp)?.len();
    if written != entry.size {
        return Err(bad(&format!("{} unpacked to {written} bytes, expected {}", entry.name, entry.size)));
    }
    std::fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    replace_file(&tmp, dest)?;
    guard.keep = true;
    Ok(written)
}

/// A writer that refuses to accept one byte past `limit`.
struct LimitedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self { inner, written: 0, limit }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "archive extraction limit exceeded",
            ));
        }
        let n = self.inner.write(buf)?;
        self.written = self.written.saturating_add(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Removes an unfinished temporary file unless it was committed.
struct TempGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create a unique temporary file next to `dest` (same filesystem, so the
/// final rename cannot copy across devices).
fn open_temp(dest: &Path) -> Result<(PathBuf, std::fs::File), Error> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().replace(['/', '\\'], "_"))
        .unwrap_or_else(|| "download".to_string());
    for _ in 0..64 {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = parent.join(format!(".{name}.dczip-{}-{seq}-{nanos}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(bad("could not create a temporary file next to the destination"))
}

/// Rename `tmp` over `dest` without deleting the old file first. On Unix this
/// is one atomic rename. On Windows a replace may fail because the target
/// exists; the old file is moved aside, the new one renamed in, and the old
/// one restored if that second rename fails.
fn replace_file(tmp: &Path, dest: &Path) -> std::io::Result<()> {
    if std::fs::rename(tmp, dest).is_ok() {
        return Ok(());
    }
    let parent = dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = dest.file_name().map(|n| n.to_string_lossy().replace(['/', '\\'], "_")).unwrap_or_else(|| "download".to_string());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let backup = parent.join(format!(".{name}.dcbak-{}-{nanos}", std::process::id()));
    std::fs::rename(dest, &backup)?;
    match std::fs::rename(tmp, dest) {
        Ok(()) => {
            let _ = std::fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&backup, dest);
            Err(e)
        }
    }
}

fn limit_error(msg: &str) -> Error {
    Error::Network(format!("remote ZIP limit: {msg}"))
}

/// How much of the tail to fetch: the end record plus a comment of up to
/// 64 KiB, which also covers the whole directory of most archives.
const TAIL: u64 = 66 * 1024;

/// Read the central directory of the ZIP at `url` with a few small Range
/// requests. `request` builds the GET (the same hook the downloader uses).
pub async fn list_remote(request: &RequestFn, url: &str) -> Result<Vec<ZipEntry>, Error> {
    list_remote_with_limits(request, url, ZipLimits::default()).await
}

/// [`list_remote`] with explicit limits (used by tests and embedders).
pub async fn list_remote_with_limits(
    request: &RequestFn,
    url: &str,
    limits: ZipLimits,
) -> Result<Vec<ZipEntry>, Error> {
    // Size first: "the last N bytes" (`bytes=-N`) is not supported by every
    // CDN (GitHub's answers 501), an explicit range always is.
    let (_, _, total) = fetch_range_capped(request, url, (0, 1), 1).await?;
    if total == 0 {
        return Err(bad("the file is empty"));
    }
    let from = total.saturating_sub(TAIL);
    let (tail, tail_start, _) = fetch_range_capped(request, url, (from, total - from), TAIL + 1).await?;
    match parse(&tail, tail_start, total, &limits)? {
        Parsed::Entries(e) => Ok(e),
        Parsed::NeedDirectory { offset, len } => {
            if len == 0 {
                return Ok(Vec::new());
            }
            let (dir, _, _) = fetch_range_capped(request, url, (offset, len), len).await?;
            parse_directory(&dir, limits.max_entries)
        }
    }
}

/// A Range reply whose body is streamed into memory under a hard cap.
async fn fetch_range_capped(
    request: &RequestFn,
    url: &str,
    (off, len): (u64, u64),
    cap: u64,
) -> Result<(Vec<u8>, u64, u64), Error> {
    use futures_util::StreamExt;
    let (resp, start, end, total) = send_range(request, url, off, len).await?;
    let expected = end - start + 1;
    if expected > cap {
        return Err(limit_error(&format!("the server wants to send {expected} bytes for a {cap}-byte request")));
    }
    let mut out = Vec::new();
    let mut got = 0u64;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::Network(e.to_string()))?;
        got = got
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| limit_error("the response is too large"))?;
        if got > cap {
            return Err(limit_error(&format!("the response is larger than the {cap}-byte limit")));
        }
        out.extend_from_slice(&chunk);
    }
    if got != expected {
        return Err(bad(&format!("the server sent {got} of {expected} requested bytes")));
    }
    Ok((out, start, total))
}

/// Send one explicit Range request and validate the reply's Content-Range
/// against what was asked for.
async fn send_range(
    request: &RequestFn,
    url: &str,
    off: u64,
    len: u64,
) -> Result<(reqwest::Response, u64, u64, u64), Error> {
    let want_end = off
        .checked_add(len.max(1) - 1)
        .ok_or_else(|| bad("range request overflows the file size"))?;
    let header = format!("bytes={off}-{want_end}");
    let resp = request(url)
        .header(reqwest::header::RANGE, header)
        .send()
        .await
        .map_err(|e| Error::Network(e.to_string()))?;
    let status = resp.status().as_u16();
    if status != 206 {
        return Err(if status == 200 {
            Error::Network("the server does not support Range requests, so the ZIP cannot be previewed".into())
        } else {
            Error::Http { status, url: url.to_string() }
        });
    }
    let (start, end, total) = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::download::parse_content_range)
        .ok_or_else(|| Error::Network("no Content-Range in the reply".into()))?;
    let total = total.ok_or_else(|| Error::Network("the server did not say the file size".into()))?;
    if start != off || end < start || end > want_end {
        return Err(Error::Network(format!(
            "the server returned range {start}-{end} for the requested {off}-{want_end}"
        )));
    }
    Ok((resp, start, end, total))
}

/// Check a streaming response before its body is consumed.
fn check_range_reply(resp: &reqwest::Response, off: u64, want_end: u64, total: u64, url: &str) -> Result<(), Error> {
    let status = resp.status().as_u16();
    if status != 206 {
        return Err(if status == 200 {
            Error::Network("the server does not support Range requests, so the ZIP cannot be previewed".into())
        } else {
            Error::Http { status, url: url.to_string() }
        });
    }
    let (start, end, got_total) = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(crate::download::parse_content_range)
        .ok_or_else(|| Error::Network("no Content-Range in the reply".into()))?;
    if got_total != Some(total) {
        return Err(Error::Network("the file changed while the ZIP was being read".into()));
    }
    if start != off || end < start || end > want_end {
        return Err(Error::Network(format!(
            "the server returned range {start}-{end} for the requested {off}-{want_end}"
        )));
    }
    Ok(())
}

enum Parsed {
    Entries(Vec<ZipEntry>),
    NeedDirectory { offset: u64, len: u64 },
}

fn u16_at(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u64)
}
fn u32_at(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as u64)
}
fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    b.get(i..i + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn bad(msg: &str) -> Error {
    Error::Network(format!("not a readable ZIP: {msg}"))
}

/// Find the end record in the tail and either read the directory from the
/// same bytes or say which range holds it. All sizes are untrusted and are
/// checked against `limits` before they are allocated.
fn parse(tail: &[u8], tail_start: u64, total: u64, limits: &ZipLimits) -> Result<Parsed, Error> {
    let eocd = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| tail[i..].starts_with(b"PK\x05\x06"))
        .ok_or_else(|| bad("no end-of-directory record"))?;
    let mut count = u16_at(tail, eocd + 10).ok_or_else(|| bad("short end record"))?;
    let mut size = u32_at(tail, eocd + 12).ok_or_else(|| bad("short end record"))?;
    let mut offset = u32_at(tail, eocd + 16).ok_or_else(|| bad("short end record"))?;
    // ZIP64: the real numbers live in a second record the locator points at.
    if count == 0xFFFF || size == 0xFFFF_FFFF || offset == 0xFFFF_FFFF {
        let loc = eocd.checked_sub(20).filter(|&l| tail[l..].starts_with(b"PK\x06\x07"));
        let rec_abs = loc.and_then(|l| u64_at(tail, l + 8)).ok_or_else(|| bad("missing ZIP64 locator"))?;
        let rec = rec_abs
            .checked_sub(tail_start)
            .map(|r| r as usize)
            .filter(|&r| tail.get(r..).is_some_and(|s| s.starts_with(b"PK\x06\x06")))
            .ok_or_else(|| bad("ZIP64 record outside the fetched tail"))?;
        count = u64_at(tail, rec + 32).ok_or_else(|| bad("short ZIP64 record"))?;
        size = u64_at(tail, rec + 40).ok_or_else(|| bad("short ZIP64 record"))?;
        offset = u64_at(tail, rec + 48).ok_or_else(|| bad("short ZIP64 record"))?;
    }
    if count > limits.max_entries as u64 {
        return Err(limit_error(&format!("it holds {count} entries, more than the {}-entry limit", limits.max_entries)));
    }
    if size > limits.max_directory_size {
        return Err(limit_error(&format!(
            "its directory is {size} bytes, larger than the {}-byte limit",
            limits.max_directory_size
        )));
    }
    if offset.checked_add(size).map_or(true, |end| end > total) {
        return Err(bad("directory points past the end of the file"));
    }
    if offset >= tail_start {
        let at = usize::try_from(offset - tail_start).map_err(|_| bad("directory offset is too large"))?;
        let len = usize::try_from(size).map_err(|_| bad("directory size is too large"))?;
        let end = at.checked_add(len).ok_or_else(|| bad("directory size is too large"))?;
        let dir = tail.get(at..end).ok_or_else(|| bad("directory cut short"))?;
        return Ok(Parsed::Entries(parse_directory(dir, count as usize)?));
    }
    Ok(Parsed::NeedDirectory { offset, len: size })
}

fn parse_directory(dir: &[u8], max: usize) -> Result<Vec<ZipEntry>, Error> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < max {
        if i >= dir.len() {
            break;
        }
        if !dir[i..].starts_with(b"PK\x01\x02") {
            // A directory may end with zero padding, but a partial or corrupt
            // entry must not be reported as a shorter, valid archive.
            if dir[i..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(bad("directory entry is truncated or corrupt"));
        }
        let short = || bad("directory entry cut short");
        let method = u16_at(dir, i + 10).ok_or_else(short)? as u16;
        let mut local = u32_at(dir, i + 42).ok_or_else(short)?;
        let mut comp = u32_at(dir, i + 20).ok_or_else(short)?;
        let mut size = u32_at(dir, i + 24).ok_or_else(short)?;
        let name_len = u16_at(dir, i + 28).ok_or_else(short)? as usize;
        let extra_len = u16_at(dir, i + 30).ok_or_else(short)? as usize;
        let comment_len = u16_at(dir, i + 32).ok_or_else(short)? as usize;
        let name_at = i + 46;
        let name = dir.get(name_at..name_at + name_len).ok_or_else(short)?;
        let name = String::from_utf8_lossy(name).into_owned();
        // ZIP64 sizes sit in the extra field (id 0x0001), in this order.
        let extra = dir.get(name_at + name_len..name_at + name_len + extra_len).ok_or_else(short)?;
        let mut e = 0;
        while e + 4 <= extra.len() {
            let id = u16_at(extra, e).unwrap_or(0);
            let len = u16_at(extra, e + 2).unwrap_or(0) as usize;
            if id == 1 {
                let mut f = e + 4;
                if size == 0xFFFF_FFFF {
                    size = u64_at(extra, f).unwrap_or(size);
                    f += 8;
                }
                if comp == 0xFFFF_FFFF {
                    comp = u64_at(extra, f).unwrap_or(comp);
                    f += 8;
                }
                if local == 0xFFFF_FFFF {
                    local = u64_at(extra, f).unwrap_or(local);
                }
            }
            e += 4 + len;
        }
        out.push(ZipEntry { is_dir: name.ends_with('/'), name, size, compressed_size: comp, method, local_offset: local });
        i = name_at
            .checked_add(name_len)
            .and_then(|v| v.checked_add(extra_len))
            .and_then(|v| v.checked_add(comment_len))
            .ok_or_else(short)?;
        if i > dir.len() {
            return Err(short());
        }
    }
    if i < dir.len() && !dir[i..].iter().all(|&b| b == 0) {
        return Err(limit_error(&format!("it holds more than {max} entries")));
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stored ZIP with the given files (enough of the format for tests).
    pub(crate) fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let (mut out, mut dir) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let local = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            dir.extend_from_slice(b"PK\x01\x02");
            dir.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0]);
            dir.extend_from_slice(&[0; 8]);
            dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
            dir.extend_from_slice(&[0; 12]);
            dir.extend_from_slice(&local.to_le_bytes());
            dir.extend_from_slice(name.as_bytes());
        }
        let at = out.len() as u32;
        let n = files.len() as u16;
        out.extend_from_slice(&dir);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn lists_from_the_tail() {
        let z = make_zip(&[("a.txt", b"hello"), ("dir/", b""), ("dir/b.bin", &[7u8; 300])]);
        let total = z.len() as u64;
        let limits = ZipLimits::default();
        match parse(&z, 0, total, &limits).unwrap() {
            Parsed::Entries(e) => {
                assert_eq!(e.len(), 3);
                assert_eq!(e[0].name, "a.txt");
                assert_eq!(e[0].size, 5);
                assert!(e[1].is_dir);
                assert_eq!(e[2].size, 300);
                assert_eq!(e[0].local_offset, 0);
                assert_eq!(e[1].local_offset, 30 + 5 + 5);
                assert!(e[0].can_extract() && !e[1].can_extract());
            }
            Parsed::NeedDirectory { .. } => panic!("directory was in the tail"),
        }
        // Only the end record fetched: the directory range is requested.
        let start = total - 22;
        match parse(&z[start as usize..], start, total, &limits).unwrap() {
            Parsed::NeedDirectory { offset, len } => {
                let dir = &z[offset as usize..(offset + len) as usize];
                assert_eq!(parse_directory(dir, usize::MAX).unwrap().len(), 3);
            }
            Parsed::Entries(_) => panic!("directory was not in the tail"),
        }
        assert!(parse(b"not a zip at all, definitely not", 0, 32, &limits).is_err());
    }

    #[test]
    fn a_lying_directory_size_is_refused_before_it_is_fetched() {
        let z = make_zip(&[("a.txt", b"hello")]);
        let mut tail = z[z.len() - 22..].to_vec();
        // size field of the EOCD -> 4 GiB
        tail[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = parse(&tail, (z.len() - 22) as u64, u32::MAX as u64 + 1, &ZipLimits::default()).unwrap_err();
        assert!(err.to_string().contains("directory is"), "{err}");
    }

    #[test]
    fn the_writer_stops_before_writing_past_its_limit() {
        let mut w = LimitedWriter::new(Vec::new(), 4);
        assert!(w.write_all(b"abcd").is_ok());
        assert!(w.write_all(b"e").is_err());
        assert_eq!(w.written, 4);
    }

    #[test]
    fn a_truncated_directory_is_not_reported_as_a_shorter_archive() {
        let z = make_zip(&[("a.txt", b"hello")]);
        let dir_at = z.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
        let mut dir = z[dir_at..z.len() - 22].to_vec();
        // A torn record after the complete one must not look like a smaller
        // (valid) archive.
        dir.push(b'P');
        let err = parse_directory(&dir, 100).unwrap_err();
        assert!(err.to_string().contains("truncated or corrupt"), "{err}");
    }
}
