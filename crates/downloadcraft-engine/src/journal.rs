//! Crash-safe resume state.
//!
//! Next to the download's data file (`<file>.dcdata`) the engine keeps a small
//! journal (`<file>.dcjournal`) listing the byte ranges that are still
//! missing. It is rewritten atomically at most once per second and on pause or
//! failure, after the data written so far has been flushed to disk, so a
//! crash or power cut loses at most the last few seconds.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Journal {
    pub v: u32,
    /// What the bytes belong to (the job's resume key, normally its URL).
    pub key: String,
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unfinished `[from, to)` ranges.
    pub remaining: Vec<(u64, u64)>,
}

impl Journal {
    pub fn new(
        key: &str,
        total: u64,
        etag: Option<String>,
        last_modified: Option<String>,
        remaining: Vec<(u64, u64)>,
    ) -> Self {
        Self { v: VERSION, key: key.to_string(), total, etag, last_modified, remaining }
    }

    pub fn load(path: &Path) -> Option<Self> {
        let raw = std::fs::read(path).ok()?;
        let j: Self = serde_json::from_slice(&raw).ok()?;
        (j.v == VERSION).then_some(j)
    }

    /// Write to a temporary file and rename over the old journal, so a crash
    /// mid-write never leaves a half journal behind.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = with_suffix(path, ".tmp");
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        use std::io::Write;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&tmp, path)?;
        sync_parent(path)
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(not(windows))]
fn sync_parent(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn sync_parent(_: &Path) -> std::io::Result<()> {
    // MoveFileExW above requests MOVEFILE_WRITE_THROUGH on Windows.
    Ok(())
}

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("dce-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.bin.dcjournal");
        let j = Journal::new("https://x/a.bin", 100, Some("\"e\"".into()), None, vec![(10, 20), (50, 100)]);
        j.save(&p).unwrap();
        assert_eq!(Journal::load(&p), Some(j));
        let updated = Journal::new("https://x/a.bin", 100, Some("\"e\"".into()), None, vec![(50, 100)]);
        updated.save(&p).unwrap();
        assert_eq!(Journal::load(&p), Some(updated));
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(Journal::load(&p), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
