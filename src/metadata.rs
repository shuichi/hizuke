//! Read-only image discovery, strict EXIF dates, and stable streaming comparisons.
//!
//! The extension allowlist controls discovery, not a promise to decode every RAW
//! container. Unsupported/malformed EXIF containers use the file modification time.
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, Utc};
use exif::{In, Tag, Value};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Timezone {
    /// Preserve EXIF camera wall time; use the system local timezone for mtime.
    Local,
    /// Convert EXIF with its matching offset to UTC; use UTC for mtime.
    Utc,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub size: u64,
    pub sha256: String,
    pub modified_secs: i64,
    pub modified_nanos: u32,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ImageFile {
    pub path: PathBuf,
    pub fingerprint: Fingerprint,
    pub timestamp: NaiveDateTime,
    pub time_source: String,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf,
    pub images: Vec<ImageFile>,
    pub skipped: usize,
    pub skipped_directories: usize,
    pub warnings: Vec<String>,
}

// Case-insensitive. GIF/BMP and unsupported RAW variants use mtime when needed.
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "tif", "tiff", "heic", "heif", "png", "webp", "avif", "gif", "bmp",
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef",
    "srw", "rwl", "3fr", "fff", "iiq", "kdc", "dcr", "mos", "raw",
];

/// Scan a directory without modifying files or following child symlinks.
/// Traversal, open, and read failures abort the entire scan.
pub fn scan(root: &Path, recursive: bool, timezone: Timezone) -> Result<Scan> {
    scan_with_progress(root, recursive, timezone, &|_, _| {})
}

/// Progress reports completed files and total discovered images. Callbacks are
/// serialized, monotonic, and may run on a worker thread. Hashing remains complete.
pub fn scan_with_progress(
    root: &Path,
    recursive: bool,
    timezone: Timezone,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<Scan> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    scan_with_workers(root, recursive, timezone, progress, workers)
}

fn scan_with_workers(
    root: &Path,
    recursive: bool,
    timezone: Timezone,
    progress: &(dyn Fn(usize, usize) + Sync),
    workers: usize,
) -> Result<Scan> {
    crate::control::check_cancelled()?;
    let input_meta =
        fs::symlink_metadata(root).with_context(|| format!("cannot inspect directory {root:?}"))?;
    ensure!(
        input_meta.is_dir() && !input_meta.file_type().is_symlink(),
        "input must be a directory, not a symlink: {root:?}"
    );
    let root =
        fs::canonicalize(root).with_context(|| format!("cannot resolve directory {root:?}"))?;
    ensure!(
        root.to_str().is_some(),
        "non-UTF-8 root paths are not supported: {root:?}"
    );
    ensure!(
        !root.components().any(|c| c
            .as_os_str()
            .to_str()
            .is_some_and(crate::engine::is_state_dir_name)),
        "refusing to scan a reserved hizuke state directory"
    );
    let mut result = Scan {
        root: root.clone(),
        images: Vec::new(),
        skipped: 0,
        skipped_directories: 0,
        warnings: Vec::new(),
    };
    let mut walker = WalkDir::new(&root).follow_links(false).sort_by_file_name();
    if !recursive {
        walker = walker.max_depth(1);
    }
    let mut walker = walker.into_iter();
    let mut candidates = Vec::new();
    while let Some(entry) = walker.next() {
        crate::control::check_cancelled()?;
        let entry = entry.with_context(|| format!("cannot traverse {root:?}"))?;
        if entry.depth() == 0 {
            continue;
        }
        let reserved = entry.file_name().to_str().is_some_and(|name| {
            crate::engine::is_state_dir_name(name) || name.eq_ignore_ascii_case(".git")
        });
        if reserved {
            if entry.file_type().is_dir() {
                walker.skip_current_dir();
            }
            continue;
        }
        ensure!(
            entry.path().to_str().is_some(),
            "non-UTF-8 paths are not supported; no files were changed: {:?}",
            entry.path()
        );
        if entry.file_type().is_dir() {
            if recursive && directory_has_state(entry.path())? {
                walker.skip_current_dir();
                result.skipped_directories += 1;
                result.warnings.push(format!(
                    "skipped independently managed directory {:?}; process that directory separately", entry.path()));
            }
            continue;
        }
        if !entry.file_type().is_file() {
            result.skipped += 1;
            if entry.file_type().is_symlink() {
                result
                    .warnings
                    .push(format!("skipped symlink: {:?}", entry.path()));
            }
            continue;
        }
        let extension = entry
            .path()
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("");
        if !IMAGE_EXTENSIONS
            .iter()
            .any(|supported| extension.eq_ignore_ascii_case(supported))
        {
            result.skipped += 1;
            continue;
        }
        candidates.push(entry.path().strip_prefix(&root)?.to_path_buf());
    }
    candidates.sort();
    let count = candidates.len();
    progress(0, count);
    let next = AtomicUsize::new(0);
    // Earlier in-flight entries finish so the reported error is deterministic by
    // sorted path. Later entries stop promptly once an earlier failure is known.
    let first_error = AtomicUsize::new(count);
    let completed = Mutex::new(0usize);
    let slots: Vec<Mutex<Option<Result<ImageFile>>>> =
        (0..count).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers.clamp(1, 4).min(count) {
            scope.spawn(|| {
                loop {
                    if crate::control::is_cancelled() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= first_error.load(Ordering::Acquire) {
                        break;
                    }
                    let relative = &candidates[index];
                    let check = || -> Result<()> {
                        crate::control::check_cancelled()?;
                        ensure!(
                            index <= first_error.load(Ordering::Acquire),
                            "scan stopped after an earlier image failed"
                        );
                        Ok(())
                    };
                    let image = inspect_image_with_check(
                        &root.join(relative),
                        relative.clone(),
                        timezone,
                        &check,
                    )
                    .with_context(|| format!("cannot inspect image {relative:?}"));
                    if image.is_err() {
                        first_error.fetch_min(index, Ordering::AcqRel);
                    }
                    let succeeded = image.is_ok();
                    *slots[index].lock().expect("result mutex poisoned") = Some(image);
                    if succeeded {
                        let mut done = completed.lock().expect("progress mutex poisoned");
                        *done += 1;
                        progress(*done, count);
                    }
                }
            });
        }
    });
    crate::control::check_cancelled()?;
    for slot in slots {
        match slot.into_inner().expect("result mutex poisoned") {
            Some(image) => result.images.push(image?),
            None => bail!("scan stopped before all images could be inspected"),
        }
    }
    Ok(result)
}

fn directory_has_state(path: &Path) -> Result<bool> {
    // Listing names also recognizes case variants on case-sensitive filesystems.
    // Any marker type, including a symlink, forms a boundary and is never followed.
    let entries =
        fs::read_dir(path).with_context(|| format!("cannot inspect directory {path:?}"))?;
    for entry in entries {
        crate::control::check_cancelled()?;
        let entry = entry.with_context(|| format!("cannot inspect directory {path:?}"))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(crate::engine::is_state_dir_name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Identity and metadata sampled around each read; atime is deliberately excluded.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64, i64, i64),
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl Snapshot {
    fn from_metadata(meta: &Metadata) -> Result<Self> {
        ensure!(meta.is_file(), "expected a regular file");
        Ok(Self {
            size: meta.len(),
            modified: meta
                .modified()
                .context("cannot read file modification time")?,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    meta.dev(),
                    meta.ino(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.ctime(),
                    meta.ctime_nsec(),
                )
            },
            #[cfg(not(unix))]
            created: meta.created().ok(),
        })
    }
}

fn open_regular(path: &Path) -> Result<(File, Snapshot)> {
    let path_meta =
        fs::symlink_metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    ensure!(
        path_meta.is_file() && !path_meta.file_type().is_symlink(),
        "refusing non-regular file or symlink: {}",
        path.display()
    );
    let before = Snapshot::from_metadata(&path_meta)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    ensure!(
        before == Snapshot::from_metadata(&file.metadata()?)?,
        "file changed while opening: {}",
        path.display()
    );
    Ok((file, before))
}

fn verify_stable(path: &Path, file: &File, before: &Snapshot) -> Result<()> {
    let after = Snapshot::from_metadata(&file.metadata()?)?;
    let path_meta = fs::symlink_metadata(path)
        .with_context(|| format!("file disappeared while reading: {}", path.display()))?;
    ensure!(
        !path_meta.file_type().is_symlink(),
        "file became a symlink: {}",
        path.display()
    );
    let at_path = Snapshot::from_metadata(&path_meta)?;
    ensure!(
        *before == after && *before == at_path,
        "file changed while reading; retry when files are stable: {}",
        path.display()
    );
    Ok(())
}

fn hash_reader(
    file: &mut File,
    before: &Snapshot,
    check: &dyn Fn() -> Result<()>,
) -> Result<Fingerprint> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    let mut size = 0u64;
    loop {
        check()?;
        let count = match file.read(&mut buffer) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            value => value?,
        };
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .context("file size overflow")?;
        digest.update(&buffer[..count]);
    }
    let (modified_secs, modified_nanos) = epoch_parts(before.modified)?;
    let (device, inode) = {
        #[cfg(unix)]
        {
            (Some(before.identity.0), Some(before.identity.1))
        }
        #[cfg(not(unix))]
        {
            (None, None)
        }
    };
    Ok(Fingerprint {
        size,
        sha256: format!("{:x}", digest.finalize()),
        modified_secs,
        modified_nanos,
        device,
        inode,
    })
}

pub fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let (mut file, before) = open_regular(path)?;
    let fingerprint = hash_reader(&mut file, &before, &crate::control::check_cancelled)
        .with_context(|| format!("cannot hash {}", path.display()))?;
    verify_stable(path, &file, &before)?;
    ensure!(
        fingerprint.size == before.size,
        "file length changed while hashing: {}",
        path.display()
    );
    Ok(fingerprint)
}

/// SHA-256 is only a candidate filter: this function proves equality byte by byte.
pub fn byte_equal(a: &Path, b: &Path) -> Result<bool> {
    crate::control::check_cancelled()?;
    let (mut left, left_before) = open_regular(a)?;
    let (mut right, right_before) = open_regular(b)?;
    let equal = if left_before.size != right_before.size {
        false
    } else {
        let mut left_buffer = [0u8; 128 * 1024];
        let mut right_buffer = [0u8; 128 * 1024];
        let mut remaining = left_before.size;
        let mut equal = true;
        while remaining > 0 {
            crate::control::check_cancelled()?;
            let length = remaining.min(left_buffer.len() as u64) as usize;
            left.read_exact(&mut left_buffer[..length])
                .with_context(|| format!("cannot compare {}", a.display()))?;
            right
                .read_exact(&mut right_buffer[..length])
                .with_context(|| format!("cannot compare {}", b.display()))?;
            if left_buffer[..length] != right_buffer[..length] {
                equal = false;
                break;
            }
            remaining -= length as u64;
        }
        if equal {
            ensure!(
                left.read(&mut left_buffer[..1])? == 0 && right.read(&mut right_buffer[..1])? == 0,
                "file grew during duplicate comparison"
            );
        }
        equal
    };
    verify_stable(a, &left, &left_before)?;
    verify_stable(b, &right, &right_before)?;
    Ok(equal)
}

// Bound actual bytes consumed by container parsing, including real oversized
// metadata chunks. Seeking to HEIF extents does not replenish this budget.
struct ReadBudget<R> {
    inner: R,
    remaining: u64,
    hit: bool,
}

impl<R: Read> Read for ReadBudget<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        crate::control::check_cancelled().map_err(std::io::Error::other)?;
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.hit = true;
            return Ok(0);
        }
        let length = self.remaining.min(buffer.len() as u64).min(128 * 1024) as usize;
        let count = self.inner.read(&mut buffer[..length])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

impl<R: Seek> Seek for ReadBudget<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        crate::control::check_cancelled().map_err(std::io::Error::other)?;
        self.inner.seek(position)
    }
}

fn inspect_image_with_check(
    path: &Path,
    relative: PathBuf,
    timezone: Timezone,
    check: &dyn Fn() -> Result<()>,
) -> Result<ImageFile> {
    let (mut file, before) = open_regular(path)?;
    let fingerprint = hash_reader(&mut file, &before, check)?;
    ensure!(
        fingerprint.size == before.size,
        "file length changed while hashing"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut warnings = Vec::new();
    let mut partial_io_error = None;
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    // The library otherwise buffers the entire TIFF, including all pixel data.
    // Restrict very large TIFFs to a bounded prefix; their IFDs are usually near
    // the front. Fields outside that prefix are unavailable, with a visible warning.
    const METADATA_READ_LIMIT: u64 = 64 * 1024 * 1024;
    let mut signature = [0u8; 4];
    let is_tiff = if before.size >= 4 {
        file.read_exact(&mut signature)?;
        signature == *b"II\x2a\x00" || signature == *b"MM\x00\x2a"
    } else {
        false
    };
    file.seek(SeekFrom::Start(0))?;
    let parsed = if is_tiff && before.size > METADATA_READ_LIMIT {
        let mut prefix = Vec::new();
        let mut limited = ReadBudget { inner: &mut file, remaining: METADATA_READ_LIMIT, hit: false };
        limited.read_to_end(&mut prefix)?;
        warnings.push("TIFF EXIF parsing limited to the first 64 MiB; metadata beyond that limit is unavailable".to_owned());
        reader.read_raw(prefix)
    } else {
        let mut limited = ReadBudget { inner: &mut file, remaining: METADATA_READ_LIMIT, hit: false };
        let parsed = reader.read_from_container(&mut BufReader::new(&mut limited));
        if limited.hit {
            warnings.push("EXIF parsing reached its 64 MiB read budget; metadata beyond that limit is unavailable".to_owned());
        }
        parsed
    }
        .or_else(|error| {
            error.distill_partial_result(|errors| {
                for error in errors {
                    if let exif::Error::Io(ref io) = error
                        && io.kind() != std::io::ErrorKind::UnexpectedEof
                    {
                        partial_io_error = Some(error.to_string());
                    }
                    warnings.push(format!("EXIF parsing warning: {error}"));
                }
            })
        });
    check()?;
    if let Some(error) = partial_io_error {
        bail!("EXIF read failed: {error}");
    }
    let exif_time = match parsed {
        Ok(exif) => select_exif_time(&exif, timezone, &mut warnings),
        Err(exif::Error::Io(error)) if error.kind() != std::io::ErrorKind::UnexpectedEof => {
            return Err(error).context("cannot read EXIF data");
        }
        Err(exif::Error::NotFound(_)) => None,
        Err(error) => {
            warnings.push(format!(
                "EXIF unavailable ({error}); falling back to modification time"
            ));
            None
        }
    };
    let (timestamp, time_source) = match exif_time {
        Some(value) => value,
        None => (mtime_date(before.modified, timezone)?, "mtime".to_owned()),
    };
    verify_stable(path, &file, &before)?;
    Ok(ImageFile {
        path: relative,
        fingerprint,
        timestamp,
        time_source,
        warnings,
    })
}

fn select_exif_time(
    exif: &exif::Exif,
    timezone: Timezone,
    warnings: &mut Vec<String>,
) -> Option<(NaiveDateTime, String)> {
    for (date_tag, offset_tag) in [
        (Tag::DateTimeOriginal, Tag::OffsetTimeOriginal),
        (Tag::DateTimeDigitized, Tag::OffsetTimeDigitized),
        (Tag::DateTime, Tag::OffsetTime),
    ] {
        let Some(field) = exif.get_field(date_tag, In::PRIMARY) else {
            continue;
        };
        let timestamp = first_ascii(&field.value).and_then(parse_exif_date);
        let Some(mut timestamp) = timestamp else {
            warnings.push(format!(
                "invalid EXIF {date_tag}; trying the next timestamp"
            ));
            continue;
        };
        let mut source = format!("EXIF {date_tag}");
        if matches!(timezone, Timezone::Utc) {
            let offset = exif
                .get_field(offset_tag, In::PRIMARY)
                .and_then(|field| first_ascii(&field.value))
                .and_then(parse_offset);
            if let Some(seconds) = offset {
                match timestamp.checked_sub_signed(Duration::seconds(i64::from(seconds))) {
                    Some(utc) if valid_year(utc) => {
                        timestamp = utc;
                        source.push_str(" (UTC)");
                    }
                    _ => {
                        warnings.push(format!("EXIF {date_tag} is out of range after UTC conversion; trying the next timestamp"));
                        continue;
                    }
                }
            } else {
                warnings.push(format!("EXIF {date_tag} has no valid {offset_tag}; preserving camera wall time even in UTC mode"));
                source.push_str(" (wall time; offset unknown)");
            }
        }
        return Some((timestamp, source));
    }
    None
}

fn first_ascii(value: &Value) -> Option<&[u8]> {
    match value {
        Value::Ascii(values) if values.len() == 1 => Some(values[0].as_slice()),
        _ => None,
    }
}

fn parse_exif_date(bytes: &[u8]) -> Option<NaiveDateTime> {
    // Do not use chrono's permissive textual parser: EXIF is exactly 19 ASCII bytes.
    if bytes.len() != 19
        || bytes[4] != b':'
        || bytes[7] != b':'
        || bytes[10] != b' '
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u32> {
        bytes[range].iter().try_fold(0, |n, digit| {
            digit
                .is_ascii_digit()
                .then_some(n * 10 + u32::from(digit.wrapping_sub(b'0')))
        })
    };
    let year = number(0..4)? as i32;
    if year == 0 {
        return None;
    }
    NaiveDate::from_ymd_opt(year, number(5..7)?, number(8..10)?)?.and_hms_opt(
        number(11..13)?,
        number(14..16)?,
        number(17..19)?,
    )
}

fn parse_offset(bytes: &[u8]) -> Option<i32> {
    if bytes.len() != 6
        || !matches!(bytes[0], b'+' | b'-')
        || bytes[3] != b':'
        || ![bytes[1], bytes[2], bytes[4], bytes[5]]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return None;
    }
    let hours = i32::from(bytes[1] - b'0') * 10 + i32::from(bytes[2] - b'0');
    let minutes = i32::from(bytes[4] - b'0') * 10 + i32::from(bytes[5] - b'0');
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((hours * 3600 + minutes * 60) * if bytes[0] == b'-' { -1 } else { 1 })
}

fn valid_year(date: NaiveDateTime) -> bool {
    (1..=9999).contains(&date.year())
}

fn epoch_parts(time: SystemTime) -> Result<(i64, u32)> {
    Ok(match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            i64::try_from(duration.as_secs()).context("mtime is out of range")?,
            duration.subsec_nanos(),
        ),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).context("mtime is out of range")?;
            if duration.subsec_nanos() == 0 {
                (-seconds, 0)
            } else {
                (
                    seconds
                        .checked_neg()
                        .and_then(|s| s.checked_sub(1))
                        .context("mtime is out of range")?,
                    1_000_000_000 - duration.subsec_nanos(),
                )
            }
        }
    })
}

fn mtime_date(time: SystemTime, timezone: Timezone) -> Result<NaiveDateTime> {
    let (seconds, nanos) = epoch_parts(time)?;
    let date =
        chrono::DateTime::<Utc>::from_timestamp(seconds, nanos).context("mtime is out of range")?;
    let date = match timezone {
        Timezone::Utc => date.naive_utc(),
        Timezone::Local => {
            let offset = date.with_timezone(&Local).offset().local_minus_utc();
            date.naive_utc()
                .checked_add_signed(Duration::seconds(i64::from(offset)))
                .context("local mtime is out of range")?
        }
    };
    ensure!(valid_year(date), "mtime year must be between 0001 and 9999");
    Ok(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Minimal little-endian TIFF: primary IFD contains DateTime and an EXIF pointer;
    // the EXIF IFD carries Original/Digitized dates and matching UTC offsets.
    fn tiff(fields: &[(u16, &str)]) -> Vec<u8> {
        let primary: Vec<_> = fields
            .iter()
            .copied()
            .filter(|(tag, _)| *tag == 0x0132)
            .collect();
        let exif_fields: Vec<_> = fields
            .iter()
            .copied()
            .filter(|(tag, _)| *tag != 0x0132)
            .collect();
        let primary_count = primary.len() + usize::from(!exif_fields.is_empty());
        let exif_start = 8 + 2 + primary_count * 12 + 4;
        let data_start = exif_start
            + if exif_fields.is_empty() {
                0
            } else {
                2 + exif_fields.len() * 12 + 4
            };
        let mut out = vec![0u8; data_start];
        out[..8].copy_from_slice(b"II\x2a\x00\x08\x00\x00\x00");
        out[8..10].copy_from_slice(&(primary_count as u16).to_le_bytes());
        fn entry(out: &mut Vec<u8>, position: usize, tag: u16, text: &str) {
            out[position..position + 2].copy_from_slice(&tag.to_le_bytes());
            out[position + 2..position + 4].copy_from_slice(&2u16.to_le_bytes());
            out[position + 4..position + 8]
                .copy_from_slice(&((text.len() + 1) as u32).to_le_bytes());
            if text.len() + 1 > 4 {
                let offset = out.len() as u32;
                out[position + 8..position + 12].copy_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(text.as_bytes());
                out.push(0);
            } else {
                out[position + 8..position + 8 + text.len()].copy_from_slice(text.as_bytes());
            }
        }
        for (i, (tag, value)) in primary.iter().enumerate() {
            entry(&mut out, 10 + i * 12, *tag, value);
        }
        if !exif_fields.is_empty() {
            let position = 10 + primary.len() * 12;
            out[position..position + 2].copy_from_slice(&0x8769u16.to_le_bytes());
            out[position + 2..position + 4].copy_from_slice(&4u16.to_le_bytes());
            out[position + 4..position + 8].copy_from_slice(&1u32.to_le_bytes());
            out[position + 8..position + 12].copy_from_slice(&(exif_start as u32).to_le_bytes());
            out[exif_start..exif_start + 2]
                .copy_from_slice(&(exif_fields.len() as u16).to_le_bytes());
            for (i, (tag, value)) in exif_fields.iter().enumerate() {
                entry(&mut out, exif_start + 2 + i * 12, *tag, value);
            }
        }
        out
    }

    fn parsed(fields: &[(u16, &str)]) -> exif::Exif {
        exif::Reader::new().read_raw(tiff(fields)).unwrap()
    }

    #[test]
    fn strict_exif_dates_reject_malformed_values() {
        for value in [
            "0000:01:01 00:00:00",
            "2025:02:29 00:00:00",
            "2024:13:01 00:00:00",
            "2024:01:01 24:00:00",
            "2024:01:01 12:00:60",
            "2024:01:01 12:00:00 ",
            "2024-01-01 12:00:00",
            "2024: 1:01 12:00:00",
            "    :  :     :  :  ",
        ] {
            assert!(parse_exif_date(value.as_bytes()).is_none(), "{value}");
        }
        assert_eq!(
            parse_exif_date(b"2024:02:29 12:34:56").unwrap().to_string(),
            "2024-02-29 12:34:56"
        );
    }

    #[test]
    fn exif_priority_and_invalid_primary_fallback() {
        let fields = [
            (0x9003, "2024:02:29 12:34:56"),
            (0x9004, "2023:01:02 03:04:05"),
            (0x0132, "2022:01:02 03:04:05"),
        ];
        let mut warnings = Vec::new();
        let (date, source) =
            select_exif_time(&parsed(&fields), Timezone::Local, &mut warnings).unwrap();
        assert_eq!(date.year(), 2024);
        assert!(source.contains("DateTimeOriginal"));
        let fields = [(0x9003, "2024:02:30 12:34:56"), fields[1], fields[2]];
        let (date, source) =
            select_exif_time(&parsed(&fields), Timezone::Local, &mut warnings).unwrap();
        assert_eq!(date.year(), 2023);
        assert!(source.contains("DateTimeDigitized"));
        assert!(!warnings.is_empty());
        let fields = [(0x9003, "bad"), (0x9004, "bad"), fields[2]];
        assert_eq!(
            select_exif_time(&parsed(&fields), Timezone::Local, &mut warnings)
                .unwrap()
                .0
                .year(),
            2022
        );
    }

    #[test]
    fn offsets_match_selected_tag_and_wall_time_is_default() {
        let exif = parsed(&[
            (0x9003, "2024:01:01 01:00:00"),
            (0x9011, "+09:00"),
            (0x9010, "-05:00"),
        ]);
        let mut warnings = Vec::new();
        assert_eq!(
            select_exif_time(&exif, Timezone::Local, &mut warnings)
                .unwrap()
                .0
                .to_string(),
            "2024-01-01 01:00:00"
        );
        assert_eq!(
            select_exif_time(&exif, Timezone::Utc, &mut warnings)
                .unwrap()
                .0
                .to_string(),
            "2023-12-31 16:00:00"
        );
        let wrong_offset = parsed(&[(0x9003, "2024:01:01 01:00:00"), (0x9010, "+09:00")]);
        assert_eq!(
            select_exif_time(&wrong_offset, Timezone::Utc, &mut warnings)
                .unwrap()
                .0
                .to_string(),
            "2024-01-01 01:00:00"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("offset") || w.contains("Offset"))
        );
        assert_eq!(parse_offset(b"-03:30"), Some(-12600));
        for value in ["+24:00", "+09:60", "09:00", "+9:00", "+ab:cd"] {
            assert!(parse_offset(value.as_bytes()).is_none());
        }
    }

    #[test]
    fn discovery_reads_tiff_and_mtime_fallback() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("camera.TIFF"),
            tiff(&[(0x9003, "2024:06:07 08:09:10")]),
        )
        .unwrap();
        let fallback = dir.path().join("plain.gif");
        fs::write(&fallback, b"GIF89a").unwrap();
        filetime::set_file_mtime(&fallback, filetime::FileTime::from_unix_time(0, 0)).unwrap();
        fs::write(dir.path().join("notes.txt"), b"notes").unwrap();
        fs::write(dir.path().join(".hidden.png"), b"not actually png").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/nested.jpg"), b"nested").unwrap();
        for ignored in [".hizuke", ".git"] {
            fs::create_dir(dir.path().join(ignored)).unwrap();
            fs::write(dir.path().join(ignored).join("ignored.jpg"), b"ignored").unwrap();
        }
        let shallow = scan(dir.path(), false, Timezone::Utc).unwrap();
        assert_eq!(shallow.images.len(), 3);
        assert_eq!(shallow.skipped, 1);
        let all = scan(dir.path(), true, Timezone::Utc).unwrap();
        assert_eq!(all.images.len(), 4);
        let camera = all
            .images
            .iter()
            .find(|v| v.path == Path::new("camera.TIFF"))
            .unwrap();
        assert_eq!(camera.timestamp.to_string(), "2024-06-07 08:09:10");
        let plain = all
            .images
            .iter()
            .find(|v| v.path == Path::new("plain.gif"))
            .unwrap();
        assert_eq!(plain.timestamp.to_string(), "1970-01-01 00:00:00");
        assert_eq!(plain.time_source, "mtime");
    }

    #[test]
    fn streaming_fingerprint_and_comparison() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"abc").unwrap();
        fs::write(&b, b"abc").unwrap();
        let digest = fingerprint(&a).unwrap();
        assert_eq!(digest.size, 3);
        assert_eq!(
            digest.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(byte_equal(&a, &b).unwrap());
        fs::write(&b, b"abd").unwrap();
        assert!(!byte_equal(&a, &b).unwrap());
        let bytes = vec![42; 300_000];
        fs::write(&a, &bytes).unwrap();
        fs::write(&b, &bytes).unwrap();
        assert!(byte_equal(&a, &b).unwrap());
        fs::write(&b, b"short").unwrap();
        assert!(!byte_equal(&a, &b).unwrap());
    }

    #[test]
    fn mutation_and_replacement_are_detected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file.jpg");
        fs::write(&path, b"before").unwrap();
        let (file, before) = open_regular(&path).unwrap();
        fs::write(&path, b"after").unwrap();
        assert!(verify_stable(&path, &file, &before).is_err());
        let (file, before) = open_regular(&path).unwrap();
        fs::rename(&path, dir.path().join("old.jpg")).unwrap();
        fs::write(&path, b"after").unwrap();
        assert!(verify_stable(&path, &file, &before).is_err());
    }

    #[test]
    fn persistent_fingerprint_survives_rename_but_detects_mtime_change() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.jpg");
        let second = dir.path().join("second.jpg");
        fs::write(&first, b"photo").unwrap();
        let before = fingerprint(&first).unwrap();
        fs::rename(&first, &second).unwrap();
        assert_eq!(before, fingerprint(&second).unwrap());
        filetime::set_file_mtime(&second, filetime::FileTime::from_unix_time(0, 123)).unwrap();
        let after = fingerprint(&second).unwrap();
        assert_ne!(before, after);
        assert_eq!(before.sha256, after.sha256);
        assert_eq!(after.modified_secs, 0);
        assert_eq!(after.modified_nanos, 123);
    }

    #[test]
    fn large_tiff_reads_bounded_prefix_and_retains_early_exif() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.tiff");
        fs::write(&path, tiff(&[(0x9003, "2024:06:07 08:09:10")])).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        let result = scan(dir.path(), false, Timezone::Local).unwrap();
        assert_eq!(
            result.images[0].timestamp.to_string(),
            "2024-06-07 08:09:10"
        );
        assert!(
            result.images[0]
                .warnings
                .iter()
                .any(|warning| warning.contains("64 MiB"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        fs::write(dir.path().join("real.jpg"), b"photo").unwrap();
        fs::write(elsewhere.path().join("external.jpg"), b"external").unwrap();
        symlink(dir.path().join("real.jpg"), dir.path().join("link.jpg")).unwrap();
        symlink(elsewhere.path(), dir.path().join("linked-dir")).unwrap();
        assert_eq!(
            scan(dir.path(), true, Timezone::Utc).unwrap().images.len(),
            1
        );
        assert!(fingerprint(&dir.path().join("link.jpg")).is_err());
        assert!(scan(&dir.path().join("linked-dir"), true, Timezone::Utc).is_err());
    }

    // APFS rejects invalid UTF-8 names itself (EPERM). Exercise application-level
    // rejection on Linux filesystems that permit those names.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_aborts() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path()
                .join(std::ffi::OsString::from_vec(b"invalid\xff.jpg".to_vec())),
            b"invalid",
        )
        .unwrap();
        assert!(
            scan(dir.path(), true, Timezone::Utc)
                .unwrap_err()
                .to_string()
                .contains("non-UTF-8")
        );
    }

    #[test]
    fn metadata_read_budget_stops_reads_and_is_not_reset_by_seek() {
        let mut limited = ReadBudget {
            inner: std::io::Cursor::new(b"12345"),
            remaining: 3,
            hit: false,
        };
        let mut output = Vec::new();
        limited.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"123");
        assert!(limited.hit);
        limited.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(limited.read(&mut [0u8; 1]).unwrap(), 0);
    }

    #[test]
    fn parallel_scan_matches_sequential_and_progress_is_monotonic() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("album")).unwrap();
        for index in 0..19 {
            let name = format!("album/{index:02}.jpg");
            let contents = tiff(&[(0x9003, "2024:01:02 03:04:05")]);
            fs::write(dir.path().join(name), contents).unwrap();
        }
        let sequential = scan_with_workers(dir.path(), true, Timezone::Utc, &|_, _| {}, 1).unwrap();
        let reports = Mutex::new(Vec::new());
        let parallel = scan_with_workers(
            dir.path(),
            true,
            Timezone::Utc,
            &|done, total| reports.lock().unwrap().push((done, total)),
            4,
        )
        .unwrap();
        assert_eq!(sequential.images.len(), parallel.images.len());
        for (left, right) in sequential.images.iter().zip(&parallel.images) {
            assert_eq!(left.path, right.path);
            assert_eq!(left.fingerprint, right.fingerprint);
            assert_eq!(left.timestamp, right.timestamp);
            assert_eq!(left.time_source, right.time_source);
            assert_eq!(left.warnings, right.warnings);
        }
        assert_eq!(
            *reports.lock().unwrap(),
            (0..=19).map(|done| (done, 19)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn nested_independent_libraries_are_pruned() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".hizuke")).unwrap();
        fs::write(dir.path().join("root.jpg"), b"root").unwrap();
        fs::create_dir(dir.path().join("managed")).unwrap();
        fs::create_dir(dir.path().join("managed/.HIZUKE")).unwrap();
        fs::write(dir.path().join("managed/untouched.jpg"), b"nested").unwrap();
        fs::create_dir(dir.path().join("ordinary")).unwrap();
        fs::write(dir.path().join("ordinary/included.jpg"), b"included").unwrap();
        let result = scan(dir.path(), true, Timezone::Local).unwrap();
        assert_eq!(result.images.len(), 2);
        assert_eq!(result.skipped_directories, 1);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("process that directory separately"))
        );
        assert!(
            result
                .images
                .iter()
                .all(|image| !image.path.starts_with("managed"))
        );
    }

    #[test]
    fn legacy_state_paths_are_rejected_and_excluded() {
        for marker in [".imgrename", ".IMGRENAME"] {
            let dir = tempdir().unwrap();
            let state = dir.path().join(marker);
            fs::create_dir(&state).unwrap();
            fs::create_dir(state.join("backups")).unwrap();
            fs::write(state.join("backups/hidden.jpg"), b"legacy backup").unwrap();
            fs::write(dir.path().join("visible.jpg"), b"visible photo").unwrap();
            let scanned = scan(dir.path(), true, Timezone::Utc).unwrap();
            assert_eq!(scanned.images.len(), 1);
            assert_eq!(scanned.images[0].path, Path::new("visible.jpg"));
            for prohibited in [&state, &state.join("backups")] {
                let error = scan(prohibited, true, Timezone::Utc).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("reserved hizuke state directory")
                );
            }
        }
    }

    #[test]
    fn nested_legacy_libraries_remain_independent_boundaries() {
        for marker in [".imgrename", ".IMGRENAME"] {
            let dir = tempdir().unwrap();
            fs::write(dir.path().join("root.jpg"), b"root photo").unwrap();
            let nested = dir.path().join("legacy-library");
            fs::create_dir(&nested).unwrap();
            fs::create_dir(nested.join(marker)).unwrap();
            fs::write(nested.join("untouched.jpg"), b"legacy photo").unwrap();
            let scanned = scan(dir.path(), true, Timezone::Utc).unwrap();
            assert_eq!(scanned.images.len(), 1);
            assert_eq!(scanned.skipped_directories, 1);
            assert!(scanned.warnings.iter().any(|warning| {
                warning.contains("legacy-library")
                    && warning.contains("process that directory separately")
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_state_markers_form_boundaries_without_following() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/keep.jpg"), b"keep").unwrap();
        symlink(
            "/nonexistent/hizuke-test-target",
            dir.path().join("nested/.hizuke"),
        )
        .unwrap();
        let result = scan(dir.path(), true, Timezone::Utc).unwrap();
        assert!(result.images.is_empty());
        assert_eq!(result.skipped_directories, 1);
    }

    #[cfg(unix)]
    #[test]
    fn parallel_scan_reports_earliest_sorted_failure() {
        use std::os::unix::fs::symlink;
        for workers in [1, 4] {
            let dir = tempdir().unwrap();
            for name in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"] {
                fs::write(dir.path().join(name), b"photo").unwrap();
            }
            let error = scan_with_workers(
                dir.path(),
                false,
                Timezone::Utc,
                &|done, _| {
                    if done == 0 {
                        for name in ["a.jpg", "b.jpg"] {
                            let path = dir.path().join(name);
                            fs::remove_file(&path).unwrap();
                            symlink("/nonexistent/hizuke-test-target", path).unwrap();
                        }
                    }
                },
                workers,
            )
            .unwrap_err();
            assert!(error.to_string().contains("a.jpg"), "{error:#}");
        }
    }

    #[test]
    fn hashing_checks_cancellation_between_chunks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("large.jpg");
        fs::write(&path, vec![0u8; 512 * 1024]).unwrap();
        let (mut file, snapshot) = open_regular(&path).unwrap();
        let checks = std::cell::Cell::new(0usize);
        let check = || -> Result<()> {
            checks.set(checks.get() + 1);
            if checks.get() == 3 {
                return Err(crate::control::Cancelled.into());
            }
            Ok(())
        };
        let error = hash_reader(&mut file, &snapshot, &check).unwrap_err();
        assert!(error.downcast_ref::<crate::control::Cancelled>().is_some());
        assert_eq!(file.stream_position().unwrap(), 256 * 1024);
    }

    #[test]
    fn huge_declared_png_metadata_in_small_file_falls_back() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&u32::MAX.to_be_bytes());
        bytes.extend_from_slice(b"eXIf");
        bytes.extend_from_slice(b"short");
        fs::write(&path, bytes).unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(0, 0)).unwrap();
        let result = scan(dir.path(), false, Timezone::Utc).unwrap();
        assert_eq!(result.images[0].time_source, "mtime");
        assert!(!result.images[0].warnings.is_empty());
    }

    #[test]
    fn mtime_before_epoch_is_handled_without_rounding_up() {
        let time = UNIX_EPOCH - std::time::Duration::from_millis(1);
        assert_eq!(
            mtime_date(time, Timezone::Utc)
                .unwrap()
                .format("%Y-%m-%d %H.%M.%S")
                .to_string(),
            "1969-12-31 23.59.59"
        );
    }
}
