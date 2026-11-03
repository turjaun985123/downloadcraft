//! Extract finished archives: ZIP, RAR (including multi-part sets) and 7z.
//!
//! Extraction only ever writes inside the folder it was given:
//!
//! * an entry name that is absolute, has a drive letter or climbs with `..`
//!   is refused (and counted), never written;
//! * a file that is already there is kept, not overwritten;
//! * an archive that would unpack past the size or entry limits stops with a
//!   plain reason, so a "zip bomb" cannot fill the disk;
//! * nothing that comes out is ever run.
//!
//! Password-protected archives are reported as such and left alone.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Refuse to write more than this in total (a real archive this big is rare;
/// a bomb is not).
const MAX_TOTAL: u64 = 64 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Rar,
    SevenZ,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Zip => "ZIP",
            Kind::Rar => "RAR",
            Kind::SevenZ => "7z",
        }
    }
}

/// Which archive a file name is, and for RAR sets, where it sits in the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub kind: Kind,
    /// File name of the volume extraction starts from.
    pub first: String,
    /// Shared name of every volume of a multi-part set (lowercase); `None`
    /// for a single file.
    pub set: Option<String>,
}

fn split_part(stem: &str) -> Option<(&str, u32)> {
    let lower = stem.to_ascii_lowercase();
    let i = lower.rfind(".part")?;
    let digits = &stem[i + 5..];
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((&stem[..i], digits.parse().ok()?))
}

/// Read an archive file name. `None` when it is not one this app extracts.
pub fn volume_of(name: &str) -> Option<Volume> {
    let lower = name.to_ascii_lowercase();
    let (stem, ext) = lower.rsplit_once('.')?;
    let orig_stem = &name[..stem.len()];
    match ext {
        "zip" => Some(Volume { kind: Kind::Zip, first: name.to_string(), set: None }),
        "7z" => Some(Volume { kind: Kind::SevenZ, first: name.to_string(), set: None }),
        "rar" => match split_part(orig_stem) {
            // name.part1.rar, name.part01.rar, ...: the set starts at part 1
            // written with the same number of digits.
            Some((base, _)) => {
                let width = orig_stem.len() - base.len() - 5;
                Some(Volume {
                    kind: Kind::Rar,
                    first: format!("{base}.part{:0width$}.rar", 1),
                    set: Some(format!("{}.part", base.to_ascii_lowercase())),
                })
            }
            // name.rar, possibly followed by name.r00, name.r01, ...
            None => Some(Volume { kind: Kind::Rar, first: name.to_string(), set: Some(format!("{stem}.r")) }),
        },
        // Old-style later volumes: name.r00 .. name.r99
        e if e.len() == 3 && e.starts_with('r') && e[1..].bytes().all(|b| b.is_ascii_digit()) => Some(Volume {
            kind: Kind::Rar,
            first: format!("{orig_stem}.rar"),
            set: Some(format!("{stem}.r")),
        }),
        _ => None,
    }
}

/// Does file `name` belong to the multi-part set `set` (see [`Volume::set`])?
pub fn in_set(name: &str, set: &str) -> bool {
    volume_of(name).and_then(|v| v.set).is_some_and(|s| s == set)
}

/// Where an archive's files go: next to it, or in a new folder named after
/// it (`name`, `name (2)`, ... when that is taken).
pub fn destination(archive: &Path, into_folder: bool) -> PathBuf {
    let parent = archive.parent().map(Path::to_path_buf).unwrap_or_default();
    if !into_folder {
        return parent;
    }
    let file = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut stem = file.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(file.clone());
    if let Some((base, _)) = split_part(&stem) {
        stem = base.to_string();
    }
    let stem = stem.trim().trim_end_matches('.').to_string();
    let stem = if stem.is_empty() { "extracted".to_string() } else { stem };
    let mut dir = parent.join(&stem);
    let mut n = 2;
    while dir.exists() {
        dir = parent.join(format!("{stem} ({n})"));
        n += 1;
    }
    dir
}

/// The path an entry name maps to under `root`, or `None` when the name
/// would land outside it.
pub fn safe_join(root: &Path, name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') || name.contains('\0') {
        return None;
    }
    let mut out = root.to_path_buf();
    let mut parts = 0;
    for piece in name.split('/') {
        match piece {
            "" | "." => continue,
            ".." => return None,
            p => {
                // Each piece must be one plain path component here too.
                let mut c = Path::new(p).components();
                match (c.next(), c.next()) {
                    (Some(Component::Normal(_)), None) => {}
                    _ => return None,
                }
                // Windows drops trailing dots and spaces: "a. " would be "a".
                let p = p.trim_end_matches(['.', ' ']);
                if p.is_empty() {
                    return None;
                }
                out.push(p);
                parts += 1;
            }
        }
    }
    (parts > 0).then_some(out)
}

/// Symbolic links and Windows reparse points: a path that goes through one
/// may leave the extraction folder even when its name looks safe.
fn is_link_like(m: &std::fs::Metadata) -> bool {
    if m.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

/// Create `dir` one component at a time under `root`, refusing to pass
/// through a link and refusing anything that is not a folder.
fn create_dirs_under(root: &Path, dir: &Path) -> Result<(), Error> {
    let rel = dir
        .strip_prefix(root)
        .map_err(|_| Error::Io("an archive path leaves the destination folder".into()))?;
    let mut cur = root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(name) = part else {
            return Err(Error::Io("an archive path is not a plain relative path".into()));
        };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if is_link_like(&m) => {
                return Err(Error::Io(format!("refusing to write through the link {}", cur.display())));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(Error::Io(format!("{} is not a folder", cur.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match std::fs::create_dir(&cur) {
                Ok(()) => {}
                // Lost a race creating the same folder: re-check it.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let m = std::fs::symlink_metadata(&cur)?;
                    if is_link_like(&m) || !m.is_dir() {
                        return Err(Error::Io(format!("refusing to write through {}", cur.display())));
                    }
                }
                Err(e) => return Err(e.into()),
            },
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The parent of `path` must really be inside `root` after resolving links.
fn parent_inside(root: &Path, path: &Path) -> bool {
    path.parent()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .is_some_and(|p| p.starts_with(root))
}

/// A name next to `path` that does not exist yet.
fn unique_sibling(path: &Path) -> Result<PathBuf, Error> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().replace(['/', '\\'], "_"))
        .unwrap_or_else(|| "file".to_string());
    for _ in 0..64 {
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = parent.join(format!(".{name}.dctmp-{}-{seq}-{nanos}", std::process::id()));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(Error::Io("could not create a temporary file next to the destination".into()))
}

#[derive(Debug)]
pub enum Error {
    Password,
    Limit(String),
    Broken(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Password => write!(f, "it is password-protected (extract it yourself with the password)"),
            Error::Limit(s) => write!(f, "{s}"),
            Error::Broken(s) => write!(f, "the archive could not be read ({s})"),
            Error::Io(s) => write!(f, "{s}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub dest: PathBuf,
    pub files: usize,
    pub bytes: u64,
    /// Files already there, kept as they were.
    pub skipped: usize,
    /// Entries whose names tried to leave the folder.
    pub refused: usize,
}

impl Outcome {
    /// Everything in the archive is now on disk as extracted.
    pub fn complete(&self) -> bool {
        self.skipped == 0 && self.refused == 0
    }
}

struct Budget {
    out: Outcome,
    /// Real (canonical) destination folder every write must stay inside.
    root: PathBuf,
    entries: usize,
}

impl Budget {
    fn entry(&mut self) -> Result<(), Error> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES {
            return Err(Error::Limit(format!("it holds more than {MAX_ENTRIES} entries")));
        }
        Ok(())
    }

    fn declared(&self, size: u64) -> Result<(), Error> {
        if self.out.bytes.checked_add(size).is_none_or(|v| v > MAX_TOTAL) {
            return Err(Error::Limit("it would unpack to more than 64 GB".into()));
        }
        Ok(())
    }

    /// Create a directory entry, refusing links and collisions.
    fn dir(&mut self, name: &str) -> Result<(), Error> {
        self.entry()?;
        let Some(path) = safe_join(&self.root, name) else {
            self.out.refused += 1;
            return Ok(());
        };
        match std::fs::symlink_metadata(&path) {
            Ok(m) if is_link_like(&m) => {
                self.out.refused += 1;
                return Ok(());
            }
            Ok(m) if m.is_dir() => return Ok(()),
            // The name is taken by a file: keep it, like any existing file.
            Ok(_) => {
                self.out.skipped += 1;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        create_dirs_under(&self.root, &path)
    }

    /// Where an entry goes, or `None` (counted) when it is refused or the
    /// file already exists.
    fn target(&mut self, name: &str) -> Result<Option<PathBuf>, Error> {
        self.entry()?;
        let Some(path) = safe_join(&self.root, name) else {
            self.out.refused += 1;
            return Ok(None);
        };
        // `Path::exists` follows links: a dangling one would make a later
        // create() write through it, outside the folder.
        match std::fs::symlink_metadata(&path) {
            Ok(m) if is_link_like(&m) => {
                self.out.refused += 1;
                return Ok(None);
            }
            Ok(_) => {
                self.out.skipped += 1;
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let parent = path
            .parent()
            .ok_or_else(|| Error::Io("an archive entry has no parent folder".into()))?;
        create_dirs_under(&self.root, parent)?;
        if !parent_inside(&self.root, &path) {
            self.out.refused += 1;
            return Ok(None);
        }
        Ok(Some(path))
    }

    /// Copy one entry's bytes, never past the declared size or the total
    /// limit. A partly written file is removed on failure, and a file that
    /// appeared between `target` and here is kept (counted as skipped).
    fn write(&mut self, path: &Path, reader: &mut dyn Read, expected: u64) -> Result<(), Error> {
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.out.skipped += 1;
                // Solid archives need the entry's bytes consumed anyway.
                std::io::copy(reader, &mut std::io::sink()).map_err(|e| Error::Broken(e.to_string()))?;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let res = (|| -> Result<u64, Error> {
            let mut buf = vec![0u8; 256 * 1024];
            let mut n = 0u64;
            loop {
                let k = reader.read(&mut buf).map_err(|e| Error::Broken(e.to_string()))?;
                if k == 0 {
                    break;
                }
                n = n
                    .checked_add(k as u64)
                    .ok_or_else(|| Error::Limit("an archive entry is too large".into()))?;
                if n > expected {
                    return Err(Error::Broken(format!(
                        "an entry unpacks to more than the {expected} bytes its header declares"
                    )));
                }
                if self.out.bytes.checked_add(n).is_none_or(|v| v > MAX_TOTAL) {
                    return Err(Error::Limit("it would unpack to more than 64 GB".into()));
                }
                file.write_all(&buf[..k])?;
            }
            if n != expected {
                return Err(Error::Broken(format!(
                    "an entry unpacked to {n} bytes, but its header says {expected}"
                )));
            }
            Ok(n)
        })();
        match res {
            Ok(n) => {
                self.out.bytes += n;
                self.out.files += 1;
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(path);
                Err(e)
            }
        }
    }
}

/// Extract `archive` into `dest` (created when missing).
pub fn extract(archive: &Path, dest: &Path) -> Result<Outcome, Error> {
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let vol = volume_of(&name).ok_or_else(|| Error::Broken("not a ZIP, RAR or 7z file".into()))?;
    let start = archive.with_file_name(&vol.first);
    if !start.is_file() {
        return Err(Error::Io(format!("{} is missing", vol.first)));
    }
    std::fs::create_dir_all(dest)?;
    // Resolve the destination once: every later containment check compares
    // against the real folder, not a path that may itself contain a link.
    let root = std::fs::canonicalize(dest)?;
    let mut b = Budget { out: Outcome { dest: dest.to_path_buf(), ..Outcome::default() }, root, entries: 0 };
    match vol.kind {
        Kind::Zip => zip_extract(&start, &mut b)?,
        Kind::SevenZ => sevenz_extract(&start, &mut b)?,
        Kind::Rar => rar_extract(&start, &mut b)?,
    }
    Ok(b.out)
}

fn zip_extract(path: &Path, b: &mut Budget) -> Result<(), Error> {
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| Error::Broken(e.to_string()))?;
    for i in 0..zip.len() {
        let mut entry = match zip.by_index(i) {
            Ok(e) => e,
            Err(zip::result::ZipError::UnsupportedArchive(m)) if m.contains("ncrypt") || m.contains("assword") => {
                return Err(Error::Password)
            }
            Err(e) => return Err(Error::Broken(e.to_string())),
        };
        if entry.encrypted() {
            return Err(Error::Password);
        }
        let name = entry.name().to_string();
        if entry.is_dir() {
            b.dir(&name)?;
            continue;
        }
        // A symlink entry must never be materialized: a later entry could
        // then be written through it.
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            b.entry()?;
            b.out.refused += 1;
            continue;
        }
        b.declared(entry.size())?;
        let size = entry.size();
        if let Some(target) = b.target(&name)? {
            b.write(&target, &mut entry, size)?;
        }
    }
    Ok(())
}

fn sevenz_extract(path: &Path, b: &mut Budget) -> Result<(), Error> {
    use sevenz_rust2::{ArchiveReader, Error as SzError, Password};
    let map = |e: SzError| match e {
        SzError::PasswordRequired | SzError::MaybeBadPassword(_) => Error::Password,
        e => Error::Broken(e.to_string()),
    };
    let mut reader = ArchiveReader::open(path, Password::empty()).map_err(map)?;
    let total: u64 = reader.archive().files.iter().map(|f| f.size).sum();
    b.declared(total)?;
    let mut failure: Option<Error> = None;
    let res = reader.for_each_entries(|entry, data| {
        let step = (|| -> Result<(), Error> {
            if entry.is_anti_item {
                return Ok(());
            }
            if entry.is_directory {
                b.dir(&entry.name)?;
                return Ok(());
            }
            match b.target(&entry.name)? {
                Some(target) => b.write(&target, data, entry.size),
                // Skipped entries still have to be read through: in a solid
                // archive the next file's bytes come after them.
                None => std::io::copy(data, &mut std::io::sink()).map(|_| ()).map_err(|e| Error::Broken(e.to_string())),
            }
        })();
        match step {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Ok(false)
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    res.map_err(map)
}

fn rar_extract(path: &Path, b: &mut Budget) -> Result<(), Error> {
    let broken = |e: unrar::error::UnrarError| {
        if matches!(e.code, unrar::error::Code::MissingPassword | unrar::error::Code::BadPassword) {
            Error::Password
        } else {
            Error::Broken(e.to_string())
        }
    };
    let mut arc = unrar::Archive::new(path).open_for_processing().map_err(broken)?;
    if arc.has_encrypted_headers() {
        return Err(Error::Password);
    }
    while let Some(header) = arc.read_header().map_err(broken)? {
        let (is_encrypted, name, is_link, is_directory, size, is_split) = {
            let entry = header.entry();
            (
                entry.is_encrypted(),
                entry.filename.to_string_lossy().into_owned(),
                entry.file_attr & 0o170000 == 0o120000,
                entry.is_directory(),
                entry.unpacked_size,
                entry.is_split(),
            )
        };
        if is_encrypted {
            return Err(Error::Password);
        }
        // Unix RAR stores the mode in the attribute field; a symlink entry
        // is refused instead of being materialized.
        arc = if is_directory {
            b.dir(&name)?;
            header.skip().map_err(broken)?
        } else {
            if is_link {
                b.entry()?;
                b.out.refused += 1;
                header.skip().map_err(broken)?
            } else {
                b.declared(size)?;
                match b.target(&name)? {
                    Some(target) => {
                        // `unrar` only writes to a path, so unpack to a
                        // unique sibling first and verify what actually
                        // arrived before it replaces the destination.
                        let tmp = unique_sibling(&target)?;
                        let next = match header.extract_to(&tmp) {
                            Ok(next) => next,
                            Err(e) => {
                                let _ = std::fs::remove_file(&tmp);
                                return Err(broken(e));
                            }
                        };
                        let md = match std::fs::symlink_metadata(&tmp) {
                            Ok(m) => m,
                            Err(e) => {
                                let _ = std::fs::remove_file(&tmp);
                                return Err(e.into());
                            }
                        };
                        if is_link_like(&md) || !md.is_file() {
                            let _ = std::fs::remove_file(&tmp);
                            return Err(Error::Broken(format!("{name} did not unpack to a regular file")));
                        }
                        let actual = md.len();
                        // For a split entry `unpacked_size` describes this
                        // part, not the joined file; the total below still
                        // bounds what can reach the disk.
                        if !is_split && actual != size {
                            let _ = std::fs::remove_file(&tmp);
                            return Err(Error::Broken(format!(
                                "{name} unpacked to {actual} bytes, but its header says {size}"
                            )));
                        }
                        if b.out.bytes.checked_add(actual).is_none_or(|v| v > MAX_TOTAL) {
                            let _ = std::fs::remove_file(&tmp);
                            return Err(Error::Limit("it would unpack to more than 64 GB".into()));
                        }
                        if let Err(e) = std::fs::rename(&tmp, &target) {
                            let _ = std::fs::remove_file(&tmp);
                            return Err(e.into());
                        }
                        b.out.bytes += actual;
                        b.out.files += 1;
                        next
                    }
                    None => header.skip().map_err(broken)?,
                }
            }
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_names() {
        assert_eq!(volume_of("a.zip").unwrap().kind, Kind::Zip);
        assert_eq!(volume_of("A.7Z").unwrap().kind, Kind::SevenZ);
        let one = volume_of("Movie.rar").unwrap();
        assert_eq!((one.first.as_str(), one.set.as_deref()), ("Movie.rar", Some("movie.r")));
        let later = volume_of("Movie.r07").unwrap();
        assert_eq!((later.first.as_str(), later.set.as_deref()), ("Movie.rar", Some("movie.r")));
        let p3 = volume_of("Big.Set.part03.rar").unwrap();
        assert_eq!((p3.first.as_str(), p3.set.as_deref()), ("Big.Set.part01.rar", Some("big.set.part")));
        assert_eq!(volume_of("x.part2.rar").unwrap().first, "x.part1.rar");
        assert!(in_set("big.set.part1.rar", "big.set.part"));
        assert!(!in_set("other.part1.rar", "big.set.part"));
        for n in ["a.tar", "a.zipx", "a", "notes.r", "a.rev"] {
            assert_eq!(volume_of(n), None, "{n}");
        }
    }

    #[test]
    fn entry_names_stay_inside() {
        let root = Path::new("/out");
        assert_eq!(safe_join(root, "a/b.txt"), Some(root.join("a").join("b.txt")));
        assert_eq!(safe_join(root, "a\\b.txt"), Some(root.join("a").join("b.txt")));
        assert_eq!(safe_join(root, "./a.txt"), Some(root.join("a.txt")));
        for bad in ["../x", "a/../../x", "/etc/passwd", "\\\\server\\share\\x", "C:\\x", "C:x", "", "./", "a/ . /b"] {
            assert_eq!(safe_join(root, bad), None, "{bad:?}");
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dc_archive_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for (n, data) in entries {
            w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn zip_into_its_own_folder_and_keeps_existing_files() {
        let d = tmp("zip");
        let z = d.join("Pack.zip");
        make_zip(&z, &[("readme.txt", b"hello"), ("sub/data.bin", &[7u8; 5000]), ("../evil.txt", b"no")]);
        let dest = destination(&z, true);
        assert_eq!(dest, d.join("Pack"));
        let out = extract(&z, &dest).unwrap();
        assert_eq!((out.files, out.refused, out.bytes), (2, 1, 5005));
        assert_eq!(std::fs::read(dest.join("sub").join("data.bin")).unwrap().len(), 5000);
        assert!(!d.join("evil.txt").exists());
        // A second run into the same folder keeps what is there.
        std::fs::write(dest.join("readme.txt"), b"mine").unwrap();
        let again = extract(&z, &dest).unwrap();
        assert_eq!((again.files, again.skipped), (0, 2));
        assert_eq!(std::fs::read(dest.join("readme.txt")).unwrap(), b"mine");
        // The folder name is taken now: the next one gets a number.
        assert_eq!(destination(&z, true), d.join("Pack (2)"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_or_folder_is_never_written_through() {
        use std::os::unix::fs::symlink;
        let d = tmp("symlink");
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"keep me").unwrap();

        // A file entry that lands on a symlink is refused and the target is
        // left untouched.
        let z = d.join("file.zip");
        make_zip(&z, &[("link.txt", b"replaced")]);
        let dest = d.join("file-out");
        std::fs::create_dir_all(&dest).unwrap();
        symlink(outside.join("keep.txt"), dest.join("link.txt")).unwrap();
        let out = extract(&z, &dest).unwrap();
        assert_eq!(out.files, 0);
        assert_eq!(out.refused, 1);
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"keep me");

        // A directory entry that goes through a symlinked folder must not
        // write outside: extraction stops instead.
        let z2 = d.join("dir.zip");
        make_zip(&z2, &[("sub/evil.txt", b"escaped")]);
        let dest2 = d.join("dir-out");
        std::fs::create_dir_all(&dest2).unwrap();
        symlink(&outside, dest2.join("sub")).unwrap();
        assert!(extract(&z2, &dest2).is_err());
        assert!(!outside.join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sevenz_extracts() {
        let d = tmp("7z");
        let src = d.join("src");
        std::fs::create_dir_all(src.join("inner")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("inner").join("b.txt"), vec![b'b'; 70_000]).unwrap();
        let archive = d.join("set.7z");
        let mut w = sevenz_rust2::ArchiveWriter::create(&archive).unwrap();
        w.push_source_path(&src, |_| true).unwrap();
        w.finish().unwrap();
        let out = extract(&archive, &d.join("out")).unwrap();
        assert_eq!(out.files, 2);
        assert_eq!(std::fs::read(d.join("out").join("a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(d.join("out").join("inner").join("b.txt")).unwrap().len(), 70_000);
        let _ = std::fs::remove_dir_all(&d);
    }

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("archive").join(name)
    }

    #[test]
    fn rar_extracts_and_reports_passwords() {
        let d = tmp("rar");
        let r = d.join("version.rar");
        std::fs::copy(fixture("version.rar"), &r).unwrap();
        let out = extract(&r, &d.join("o")).unwrap();
        assert_eq!(out.files, 1, "{out:?}");
        assert!(std::fs::read_dir(d.join("o")).unwrap().count() == 1);
        let c = d.join("crypted.rar");
        std::fs::copy(fixture("crypted.rar"), &c).unwrap();
        assert!(matches!(extract(&c, &d.join("c")), Err(Error::Password)));
        std::fs::write(&r, b"not a rar at all").unwrap();
        assert!(matches!(extract(&r, &d.join("o2")), Err(Error::Broken(_))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
