//! One-click update of the portable Windows build.
//!
//! 1. Download the release's portable .zip to a temp folder and check it
//!    against the SHA-256 GitHub publishes for the asset (and its size).
//! 2. Unpack it there; the folder holding `download_craft.exe` is the new
//!    portable folder.
//! 3. Install: every file the new folder has replaces the one in the app's
//!    folder. The old file is first renamed to `<name>.dcold` (Windows lets a
//!    running program be renamed, not overwritten), then the new one is
//!    copied in. Any failure puts every old file back: the app keeps working.
//! 4. Restart: the new program starts with `--wait-pid <old pid>` and waits
//!    for this one to close before it claims the single-instance slot.
//!
//! Leftover `.dcold` files (the running program cannot delete its own) are
//! removed at the next start. Settings, the list and history live in the
//! config folder, which an update never touches.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A release asset: where it is and what it must hash to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Asset {
    pub url: String,
    pub name: String,
    /// Lower-case hex SHA-256 from the release (`digest: "sha256:…"`), when
    /// GitHub published one.
    pub sha256: Option<String>,
    pub size: u64,
}

const OLD_SUFFIX: &str = ".dcold";

/// The updater refuses anything larger than this before writing a byte.
const MAX_ARCHIVE_SIZE: u64 = 512 * 1024 * 1024;
/// Unpacked output cap for one update (a release is a few hundred MB).
const MAX_UNPACKED_TOTAL: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRY_SIZE: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
const MAX_INSTALL_FILES: usize = 20_000;

fn valid_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The app's folder when it can update itself there: a portable copy in a
/// folder this user can write to. A build run from a source checkout
/// (`…\target\…`) is never overwritten by a release.
pub fn portable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    if dir.components().any(|c| c.as_os_str().eq_ignore_ascii_case("target")) {
        return None;
    }
    writable(&dir).then_some(dir)
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".dc-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"x").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A fresh, private work folder. The name carries the PID and a monotonic
/// timestamp so a local user cannot pre-create it in the shared temp folder,
/// and Unix gets mode 0700 before anything is written.
fn work_dir() -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    for _ in 0..64 {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = base.join(format!("download-craft-update-{}-{seq}-{nanos}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow!("could not create a private update folder"))
}

/// Download `asset` and check it. `progress` gets 0.0..=1.0.
///
/// Fails closed: a release without a SHA-256 and a size is never installed,
/// and neither the declared size nor the 512 MiB archive cap can be crossed
/// while streaming.
pub async fn download(client: &reqwest::Client, asset: &Asset, mut progress: impl FnMut(f32)) -> Result<PathBuf> {
    let want = asset
        .sha256
        .as_deref()
        .filter(|s| valid_sha256(s))
        .ok_or_else(|| anyhow!("the release does not publish a valid SHA-256 for this update; refusing to install it"))?
        .to_ascii_lowercase();
    if asset.size == 0 {
        return Err(anyhow!("the release does not publish the update's size; refusing to install it"));
    }
    if asset.size > MAX_ARCHIVE_SIZE {
        return Err(anyhow!("the update is {} bytes, larger than the {} MiB limit", asset.size, MAX_ARCHIVE_SIZE / (1024 * 1024)));
    }
    let dir = work_dir()?;
    match download_into(&dir, client, asset, &want, &mut progress).await {
        Ok(path) => Ok(path),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

async fn download_into(
    dir: &Path,
    client: &reqwest::Client,
    asset: &Asset,
    want: &str,
    progress: &mut impl FnMut(f32),
) -> Result<PathBuf> {
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    let name = if asset.name.is_empty() { "update.zip".to_string() } else { crate::engine::sanitize_filename(&asset.name) };
    let path = dir.join(name);
    let resp = client.get(&asset.url).send().await.context("downloading the update")?;
    if !resp.status().is_success() {
        return Err(anyhow!("the update download failed (HTTP {})", resp.status()));
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_ARCHIVE_SIZE {
            return Err(anyhow!("the update is {len} bytes, larger than the {} MiB limit", MAX_ARCHIVE_SIZE / (1024 * 1024)));
        }
        if len != asset.size {
            return Err(anyhow!("the update is {len} bytes, the release says {}", asset.size));
        }
    }
    let total = asset.size.max(1);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("downloading the update")?;
        got = got
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!("the update is larger than the release says"))?;
        if got > asset.size || got > MAX_ARCHIVE_SIZE {
            return Err(anyhow!("the update is larger than the release says"));
        }
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        progress((got as f32 / total as f32).min(1.0));
    }
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if got != asset.size {
        return Err(anyhow!("the update is {got} bytes, the release says {}", asset.size));
    }
    let have = format!("{:x}", hasher.finalize());
    if !have.eq_ignore_ascii_case(want) {
        return Err(anyhow!("the update's checksum does not match the release (got {have}, expected {want})"));
    }
    Ok(path)
}

/// Remove a path without ever following a symbolic link.
fn remove_any(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Refuse to touch a file that is not a plain regular file. Symlinks and
/// reparse points are how an archive escapes its destination.
fn is_plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.is_file() && !m.file_type().is_symlink() && !is_reparse_point(&m))
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_reparse_point(m: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
}

#[cfg(not(windows))]
fn is_reparse_point(_m: &std::fs::Metadata) -> bool {
    false
}

/// Create `dir` one component at a time under `root`, refusing to pass
/// through a symlink or a reparse point.
fn create_dirs_under(root: &Path, dir: &Path) -> Result<()> {
    let rel = dir.strip_prefix(root).map_err(|_| anyhow!("an update path leaves its folder"))?;
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else { return Err(anyhow!("an update path is not a plain relative path")) };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() || is_reparse_point(&m) => {
                return Err(anyhow!("the update would write through the link {}", cur.display()));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(anyhow!("{} is not a folder", cur.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn inside_root(root: &Path, path: &Path) -> bool {
    let Some(parent) = path.parent() else { return false };
    match std::fs::canonicalize(parent) {
        Ok(real) => real.starts_with(root),
        Err(_) => false,
    }
}

/// Copy at most `cap` bytes, refusing to write one byte past it.
fn copy_capped(reader: &mut impl Read, writer: &mut impl Write, cap: u64) -> Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(written);
        }
        written = written
            .checked_add(n as u64)
            .ok_or_else(|| anyhow!("the update unpacks to more than the allowed size"))?;
        if written > cap {
            bail!("the update unpacks to more than the allowed size");
        }
        writer.write_all(&buf[..n])?;
    }
}

/// Unpack the zip next to it and return the folder that holds the program.
pub fn unpack(zip_path: &Path) -> Result<PathBuf> {
    let out = zip_path.with_extension("unpacked");
    remove_any(&out);
    std::fs::create_dir_all(&out)?;
    let out = std::fs::canonicalize(&out)?;
    if let Err(e) = unpack_into(zip_path, &out) {
        remove_any(&out);
        return Err(e);
    }
    find_program_dir(&out).ok_or_else(|| anyhow!("the update has no {}", exe_name()))
}

fn unpack_into(zip_path: &Path, out: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut zip = zip::ZipArchive::new(file).context("the update is not a readable zip")?;
    if zip.len() > MAX_ENTRIES {
        bail!("the update holds more than {MAX_ENTRIES} entries");
    }
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        // `enclosed_name` refuses absolute paths and `..`: nothing may land
        // outside the unpack folder.
        let Some(rel) = entry.enclosed_name() else { return Err(anyhow!("the update holds an unsafe path: {}", entry.name())) };
        let dest = out.join(rel);
        if entry.is_dir() {
            create_dirs_under(out, &dest)?;
            continue;
        }
        // Symlink entries are not part of a portable build, and following
        // one is exactly the escape this path must prevent.
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            return Err(anyhow!("the update holds a symbolic link: {}", entry.name()));
        }
        if entry.size() > MAX_ENTRY_SIZE {
            return Err(anyhow!("{} is larger than the {} MiB per-file limit", entry.name(), MAX_ENTRY_SIZE / (1024 * 1024)));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("the update unpacks to more than the allowed size"))?;
        if total > MAX_UNPACKED_TOTAL {
            return Err(anyhow!("the update unpacks to more than {} GiB", MAX_UNPACKED_TOTAL / (1024 * 1024 * 1024)));
        }
        if let Some(p) = dest.parent() {
            create_dirs_under(out, p)?;
        }
        if !inside_root(out, &dest) {
            return Err(anyhow!("the update would write outside its folder: {}", dest.display()));
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&dest)?;
        // Reading to the end checks each file's CRC; the cap also protects
        // against a header that understates the real stream.
        let n = copy_capped(&mut entry, &mut f, MAX_ENTRY_SIZE).with_context(|| format!("unpacking {}", entry.name()))?;
        f.sync_all()?;
        if n != entry.size() {
            return Err(anyhow!("{} unpacked to {n} bytes, expected {}", entry.name(), entry.size()));
        }
    }
    Ok(())
}

fn exe_name() -> &'static str {
    if cfg!(windows) { "download_craft.exe" } else { "download_craft" }
}

/// The folder (the unpack root or one level down) holding the program.
fn find_program_dir(root: &Path) -> Option<PathBuf> {
    if is_plain_file(&root.join(exe_name())) {
        return Some(root.to_path_buf());
    }
    std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).find(|p| is_plain_file(&p.join(exe_name())))
}

/// Every regular file under `dir`, as paths relative to it. Links are
/// refused: an update tree must not reach outside itself.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let md = std::fs::symlink_metadata(&p)?;
            if md.file_type().is_symlink() || is_reparse_point(&md) {
                bail!("refusing to follow the link {}", p.display());
            }
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                if let Ok(rel) = p.strip_prefix(dir) {
                    out.push(rel.to_path_buf());
                }
                if out.len() > MAX_INSTALL_FILES {
                    bail!("the update holds more than {MAX_INSTALL_FILES} files");
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

fn old_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push(OLD_SUFFIX);
    PathBuf::from(s)
}

/// A unique sibling path (the caller creates it with `create_new`).
fn unique_sibling(path: &Path, tag: &str) -> Result<PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or_else(|| anyhow!("{} has no parent folder", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().replace(['/', '\\'], "_")).unwrap_or_else(|| "file".to_string());
    for _ in 0..64 {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let p = parent.join(format!(".{name}.{tag}-{}-{seq}-{nanos}", std::process::id()));
        if std::fs::symlink_metadata(&p).is_err() {
            return Ok(p);
        }
    }
    bail!("could not create a temporary file next to {}", path.display())
}

/// Write a small file atomically (used for the installed-files manifest).
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = unique_sibling(path, "dctmp")?;
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Copy the new folder over `target`, all or nothing.
pub fn install(new_dir: &Path, target: &Path) -> Result<usize> {
    let new_root = std::fs::canonicalize(new_dir).context("reading the unpacked update")?;
    let target = std::fs::canonicalize(target).context("reading the app folder")?;
    let files = files_under(&new_root)?;
    if !files.iter().any(|f| f.as_os_str().eq_ignore_ascii_case(exe_name())) {
        return Err(anyhow!("the update has no {}", exe_name()));
    }
    let mut total = 0u64;
    for rel in &files {
        let src = new_root.join(rel);
        let md = std::fs::symlink_metadata(&src)?;
        if !md.is_file() || md.file_type().is_symlink() || is_reparse_point(&md) {
            return Err(anyhow!("the update holds an unsafe file: {}", rel.display()));
        }
        total = total
            .checked_add(md.len())
            .ok_or_else(|| anyhow!("the update is larger than the allowed size"))?;
        if total > MAX_UNPACKED_TOTAL {
            return Err(anyhow!("the update is larger than {} GiB", MAX_UNPACKED_TOTAL / (1024 * 1024 * 1024)));
        }
    }
    // (moved aside: original → backup), installed files, staged files
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut created: Vec<PathBuf> = Vec::new();
    let mut staged: Vec<PathBuf> = Vec::new();
    let result: Result<()> = (|| {
        for rel in &files {
            let dest = target.join(rel);
            if !dest.starts_with(&target) {
                return Err(anyhow!("an update path leaves the app folder: {}", rel.display()));
            }
            if let Some(p) = dest.parent() {
                create_dirs_under(&target, p)?;
            }
            if !inside_root(&target, &dest) {
                return Err(anyhow!("the update would write through a link: {}", dest.display()));
            }
            // Stage the complete file next to its destination first.
            let tmp = unique_sibling(&dest, "dcnew")?;
            {
                let mut src = std::fs::File::open(new_root.join(rel))?;
                let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                staged.push(tmp.clone());
                copy_capped(&mut src, &mut out, MAX_ENTRY_SIZE).with_context(|| format!("staging {}", dest.display()))?;
                out.sync_all()?;
            }
            if std::fs::symlink_metadata(&dest).is_ok() {
                let bak = old_path(&dest);
                // A backup from an earlier update that is still in use (an
                // old program a browser started) cannot be removed: then
                // this file is replaced without one only if that fails too.
                if std::fs::symlink_metadata(&bak).is_ok() {
                    std::fs::remove_file(&bak).with_context(|| format!("removing {}", bak.display()))?;
                }
                std::fs::rename(&dest, &bak).with_context(|| format!("moving {} aside", dest.display()))?;
                moved.push((dest.clone(), bak));
            }
            std::fs::rename(&tmp, &dest).with_context(|| format!("writing {}", dest.display()))?;
            created.push(dest);
        }
        Ok(())
    })();
    if let Err(e) = result {
        // Undo: the new files go, the old ones come back.
        for c in created.iter().rev() {
            let _ = std::fs::remove_file(c);
        }
        for s in staged.iter().rev() {
            let _ = std::fs::remove_file(s);
        }
        for (orig, bak) in moved.iter().rev() {
            let _ = std::fs::rename(bak, orig);
        }
        return Err(e);
    }
    // Backups that are not in use go now; the running program's waits for
    // the next start (see `cleanup`).
    for (_, bak) in &moved {
        let _ = std::fs::remove_file(bak);
    }
    remove_dropped(&target, &files);
    Ok(files.len())
}

/// The list of files the last update installed, kept in the app's folder.
const MANIFEST: &str = ".dc-files";

fn rel_key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Files the previous version installed that this one no longer has (an old
/// `DownloadCraft-Firefox-0.9.10.xpi`, a script dropped from the extension)
/// go, and the new list is written. Without a list (a copy older than the
/// list) only what is certainly the app's own goes: release packages named
/// `DownloadCraft-…` and files inside its two extension folders. Anything
/// else in the folder is the user's and is never touched.
fn remove_dropped(target: &Path, files: &[PathBuf]) {
    let new: std::collections::HashSet<String> = files.iter().map(|f| rel_key(f)).collect();
    let old: Vec<String> = match std::fs::read_to_string(target.join(MANIFEST)) {
        Ok(text) => text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect(),
        Err(_) => files_under(target)
            .unwrap_or_default()
            .iter()
            .map(|f| rel_key(f))
            .filter(|k| {
                let top = k.split('/').next().unwrap_or("");
                let package = !k.contains('/')
                    && top.starts_with("DownloadCraft-")
                    && (k.ends_with(".xpi") || k.ends_with(".zip"));
                package || top == "browser-extension" || top == "browser-extension-firefox"
            })
            .collect(),
    };
    for k in old {
        // Never a path that leaves the folder, the program, or the list.
        let safe = !k.is_empty()
            && !k.contains("..")
            && !k.contains([':', '\0', '\\'])
            && !k.starts_with('/')
            && k.split('/').all(|p| !p.is_empty() && p != ".");
        if new.contains(&k) || !safe || k == exe_name() || k == MANIFEST {
            continue;
        }
        let p = target.join(&k);
        if is_plain_file(&p) {
            let _ = std::fs::remove_file(&p);
            // An emptied folder of the app's goes too.
            if let Some(dir) = p.parent().filter(|d| *d != target) {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
    let mut list: Vec<&String> = new.iter().collect();
    list.sort();
    let text: String = list.into_iter().map(|k| format!("{k}\n")).collect();
    let _ = write_atomic(&target.join(MANIFEST), text.as_bytes());
}

/// Remove `.dcold` leftovers in the app's folder and stale update folders
/// (best effort, at start). Only this program's own prefix is touched, and
/// only when it is at least a day old, so a concurrent update survives.
pub fn cleanup() {
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        if let Ok(files) = files_under(&dir) {
            for rel in files {
                if rel.to_string_lossy().ends_with(OLD_SUFFIX) {
                    let _ = std::fs::remove_file(dir.join(rel));
                }
            }
        }
    }
    let base = std::env::temp_dir();
    // The pre-0.9.13 updater used this fixed name; it is never current.
    let _ = std::fs::remove_dir_all(base.join("download-craft-update"));
    let Ok(entries) = std::fs::read_dir(&base) else { return };
    let day = Duration::from_secs(24 * 60 * 60);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("download-craft-update-") {
            continue;
        }
        let stale = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > day);
        if stale {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Start the (new) program; it waits for this process to end first.
pub fn relaunch() -> Result<()> {
    let exe = std::env::current_exe()?;
    crate::platform::silent_command(&exe)
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .spawn()
        .context("starting the new version")?;
    Ok(())
}

/// `--wait-pid <pid>`: wait (up to 20 s) for the old program to close
/// before this one starts, so it does not just wake the old window.
pub fn wait_for_previous(args: &[String]) {
    let Some(pid) = args
        .iter()
        .position(|a| a == "--wait-pid")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if !h.is_null() {
            WaitForSingleObject(h, 20_000);
            CloseHandle(h);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
}

/// Only release assets on GitHub's own HTTPS hosts are installable.
fn trusted_update_url(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    if u.scheme() != "https" {
        return false;
    }
    let Some(host) = u.host_str().map(|h| h.to_ascii_lowercase()) else { return false };
    host == "github.com"
        || host.ends_with(".github.com")
        || host == "githubusercontent.com"
        || host.ends_with(".githubusercontent.com")
}

/// The portable Windows asset of a GitHub release JSON, with its checksum.
///
/// Returns `None` unless the release publishes both a valid SHA-256 and a
/// positive size: an update without them is never offered, let alone
/// installed. `DOWNLOAD_CRAFT_UPDATE_API` is the existing end-to-end test
/// hook and is the only way to point this at a non-GitHub URL.
pub fn pick_asset(v: &serde_json::Value) -> Option<Asset> {
    let url = crate::model::pick_windows_asset(v)?;
    let override_api = std::env::var("DOWNLOAD_CRAFT_UPDATE_API").map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !override_api && !trusted_update_url(&url) {
        return None;
    }
    let a = v.get("assets")?.as_array()?.iter().find(|a| a.get("browser_download_url").and_then(|u| u.as_str()) == Some(url.as_str()))?;
    let sha256 = a
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| valid_sha256(h))
        .map(|h| h.to_ascii_lowercase())?;
    let size = a.get("size").and_then(|s| s.as_u64()).filter(|&s| s > 0)?;
    Some(Asset {
        name: a.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
        size,
        url,
        sha256: Some(sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc-selfupdate-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        // A stored zip written by hand (the crate is built read-only).
        let mut out = Vec::new();
        let mut dir = Vec::new();
        for (name, data) in files {
            let crc = crc32(data);
            let at = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            dir.extend_from_slice(b"PK\x01\x02");
            dir.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            dir.extend_from_slice(&crc.to_le_bytes());
            dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
            dir.extend_from_slice(&[0; 12]);
            dir.extend_from_slice(&at.to_le_bytes());
            dir.extend_from_slice(name.as_bytes());
        }
        let at = out.len() as u32;
        out.extend_from_slice(&dir);
        out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
        out.extend_from_slice(&at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        std::fs::File::create(path).unwrap().write_all(&out).unwrap();
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    #[test]
    fn release_json_gives_the_portable_zip_and_its_checksum() {
        let h = "a".repeat(64);
        let v = serde_json::json!({ "assets": [
            { "name": "DownloadCraft-1.0-x86_64.AppImage", "browser_download_url": "https://x/a", "size": 1 },
            { "name": "DownloadCraft-1.0-windows-x64-portable.zip", "browser_download_url": "https://github.com/download-craft/releases/download/v1.0/p.zip", "size": 42, "digest": format!("sha256:{h}") },
        ]});
        let a = pick_asset(&v).unwrap();
        assert_eq!((a.url.as_str(), a.size, a.sha256.as_deref()), ("https://github.com/download-craft/releases/download/v1.0/p.zip", 42, Some(h.as_str())));
    }

    #[test]
    fn unpack_finds_the_program_and_refuses_escaping_paths() {
        let d = tmp("unpack");
        let z = d.join("u.zip");
        let exe = format!("DC-1.0/{}", exe_name());
        make_zip(&z, &[(exe.as_str(), b"new exe"), ("DC-1.0/browser-extension/content.js", b"js")]);
        let root = unpack(&z).unwrap();
        assert!(root.ends_with("DC-1.0"));
        assert_eq!(std::fs::read(root.join("browser-extension/content.js")).unwrap(), b"js");
        let bad = d.join("bad.zip");
        make_zip(&bad, &[("../evil.txt", b"x"), (exe_name(), b"e")]);
        assert!(unpack(&bad).is_err());
        assert!(!d.join("evil.txt").exists());
    }

    #[test]
    fn install_replaces_files_and_keeps_others() {
        let d = tmp("install");
        let (new, app) = (d.join("new"), d.join("app"));
        std::fs::create_dir_all(new.join("browser-extension")).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(new.join(exe_name()), b"v2").unwrap();
        std::fs::write(new.join("browser-extension/content.js"), b"js2").unwrap();
        std::fs::write(app.join(exe_name()), b"v1").unwrap();
        std::fs::write(app.join("my-notes.txt"), b"mine").unwrap();
        assert_eq!(install(&new, &app).unwrap(), 2);
        assert_eq!(std::fs::read(app.join(exe_name())).unwrap(), b"v2");
        assert_eq!(std::fs::read(app.join("browser-extension/content.js")).unwrap(), b"js2");
        assert_eq!(std::fs::read(app.join("my-notes.txt")).unwrap(), b"mine");
        assert!(!old_path(&app.join(exe_name())).exists(), "unused backups are removed");
    }

    #[test]
    fn an_update_removes_what_the_new_version_dropped() {
        let d = tmp("dropped");
        let (new, app) = (d.join("new"), d.join("app"));
        std::fs::create_dir_all(new.join("browser-extension")).unwrap();
        std::fs::create_dir_all(app.join("browser-extension")).unwrap();
        std::fs::write(new.join(exe_name()), b"v2").unwrap();
        std::fs::write(new.join("DownloadCraft-Firefox-0.9.11.xpi"), b"x2").unwrap();
        std::fs::write(new.join("browser-extension/content.js"), b"js2").unwrap();
        std::fs::write(app.join(exe_name()), b"v1").unwrap();
        std::fs::write(app.join("DownloadCraft-Firefox-0.9.10.xpi"), b"x1").unwrap();
        std::fs::write(app.join("browser-extension/old-helper.js"), b"gone").unwrap();
        std::fs::write(app.join("my-notes.txt"), b"mine").unwrap();
        // First update: no list yet, so only the app's own leftovers go.
        install(&new, &app).unwrap();
        assert!(!app.join("DownloadCraft-Firefox-0.9.10.xpi").exists());
        assert!(!app.join("browser-extension/old-helper.js").exists());
        assert!(app.join("DownloadCraft-Firefox-0.9.11.xpi").exists());
        assert_eq!(std::fs::read(app.join("my-notes.txt")).unwrap(), b"mine");
        // Next update: the list says what the previous one installed.
        let newer = d.join("newer");
        std::fs::create_dir_all(newer.join("browser-extension")).unwrap();
        std::fs::write(newer.join(exe_name()), b"v3").unwrap();
        std::fs::write(newer.join("DownloadCraft-Firefox-0.9.12.xpi"), b"x3").unwrap();
        std::fs::write(newer.join("browser-extension/content.js"), b"js3").unwrap();
        install(&newer, &app).unwrap();
        assert!(!app.join("DownloadCraft-Firefox-0.9.11.xpi").exists());
        assert!(app.join("DownloadCraft-Firefox-0.9.12.xpi").exists());
        assert_eq!(std::fs::read(app.join("my-notes.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(app.join(exe_name())).unwrap(), b"v3");
    }

    #[test]
    fn a_failed_install_puts_everything_back() {
        let d = tmp("rollback");
        let (new, app) = (d.join("new"), d.join("app"));
        std::fs::create_dir_all(&new).unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(new.join("a.txt"), b"a2").unwrap();
        std::fs::write(new.join(exe_name()), b"v2").unwrap();
        std::fs::create_dir_all(new.join("sub")).unwrap();
        std::fs::write(new.join("sub/x.txt"), b"x2").unwrap();
        std::fs::write(app.join("a.txt"), b"a1").unwrap();
        std::fs::write(app.join(exe_name()), b"v1").unwrap();
        // A file where the update needs a folder: the last step fails after
        // the first two files were already replaced.
        std::fs::write(app.join("sub"), b"in the way").unwrap();
        assert!(install(&new, &app).is_err());
        assert_eq!(std::fs::read(app.join("a.txt")).unwrap(), b"a1");
        assert_eq!(std::fs::read(app.join(exe_name())).unwrap(), b"v1");
        assert!(!old_path(&app.join("a.txt")).exists());
    }

    #[tokio::test]
    async fn download_checks_the_checksum() {
        use sha2::{Digest, Sha256};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = b"pretend zip bytes".to_vec();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/u.zip", l.local_addr().unwrap().port());
        let b2 = body.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { break };
                let b = b2.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len()).as_bytes()).await;
                    let _ = s.write_all(&b).await;
                });
            }
        });
        let client = reqwest::Client::new();
        let good = Asset { url: url.clone(), name: "u.zip".into(), sha256: Some(format!("{:x}", Sha256::digest(&body))), size: body.len() as u64 };
        let p = download(&client, &good, |_| {}).await.unwrap();
        assert_eq!(std::fs::read(p).unwrap(), body);
        let bad = Asset { sha256: Some("0".repeat(64)), ..good };
        assert!(download(&client, &bad, |_| {}).await.unwrap_err().to_string().contains("checksum"));
    }
}
