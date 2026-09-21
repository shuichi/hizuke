//! Read only the standard MP4 movie creation time (moov/mvhd).
//!
//! Box payloads such as mdat are skipped with seeks, not buffered or decoded.
//! Dates are unsigned seconds since 1904-01-01 UTC (version 0: 32 bits,
//! version 1: 64 bits). Zero denotes an unavailable date.
//! https://developer.apple.com/documentation/quicktime-file-format/movie_header_atom/creation_time
use std::io::{self, Read, Seek, SeekFrom};

use chrono::{DateTime, Datelike, Utc};

const MAX_BOXES: usize = 100_000;
const MP4_UNIX_EPOCH_DELTA: i64 = 2_082_844_800;

struct BoxHeader {
    kind: [u8; 4],
    payload: u64,
    end: u64,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn header<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    limit: u64,
    remaining: &mut usize,
) -> io::Result<BoxHeader> {
    crate::control::check_cancelled().map_err(io::Error::other)?;
    *remaining = remaining
        .checked_sub(1)
        .ok_or_else(|| invalid("MP4 metadata exceeded the 100000-box limit"))?;
    if limit.saturating_sub(start) < 8 {
        return Err(invalid("truncated MP4 box header"));
    }
    reader.seek(SeekFrom::Start(start))?;
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    let size = u32::from_be_bytes(bytes[..4].try_into().unwrap());
    let kind = bytes[4..].try_into().unwrap();
    let (size, header_size) = match size {
        0 => (limit - start, 8),
        1 => {
            if limit - start < 16 {
                return Err(invalid("truncated MP4 extended box header"));
            }
            reader.read_exact(&mut bytes)?;
            (u64::from_be_bytes(bytes), 16)
        }
        size => (u64::from(size), 8),
    };
    if size < header_size || size > limit - start {
        return Err(invalid(
            "MP4 box size exceeds its container or is too small",
        ));
    }
    Ok(BoxHeader {
        kind,
        payload: start + header_size,
        end: start + size,
    })
}

fn movie_date<R: Read>(reader: &mut R, payload_size: u64) -> io::Result<Option<DateTime<Utc>>> {
    if payload_size < 4 {
        return Err(invalid("truncated MP4 movie header"));
    }
    let mut version_flags = [0; 4];
    reader.read_exact(&mut version_flags)?;
    let seconds = match version_flags[0] {
        0 if payload_size >= 100 => {
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes)?;
            u64::from(u32::from_be_bytes(bytes))
        }
        1 if payload_size >= 112 => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            u64::from_be_bytes(bytes)
        }
        0 | 1 => return Err(invalid("truncated MP4 movie header")),
        _ => return Err(invalid("unsupported MP4 movie header version")),
    };
    if seconds == 0 {
        return Ok(None);
    }
    let date = i64::try_from(seconds)
        .ok()
        .and_then(|seconds| seconds.checked_sub(MP4_UNIX_EPOCH_DELTA))
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .filter(|date| (1..=9999).contains(&date.year()))
        .ok_or_else(|| invalid("MP4 creation time is out of range"))?;
    Ok(Some(date))
}

/// Only inspect top-level boxes and direct children of moov. No recursion,
/// allocations based on file data, or scans through compressed media payloads.
pub(crate) fn creation_time<R: Read + Seek>(
    reader: &mut R,
    file_size: u64,
) -> io::Result<Option<DateTime<Utc>>> {
    let mut remaining = MAX_BOXES;
    let mut position = 0;
    while position < file_size {
        let outer = header(reader, position, file_size, &mut remaining)?;
        if outer.kind == *b"moov" {
            let mut child_position = outer.payload;
            while child_position < outer.end {
                let child = header(reader, child_position, outer.end, &mut remaining)?;
                if child.kind == *b"mvhd" {
                    return movie_date(reader, child.end - child.payload);
                }
                child_position = child.end;
            }
        }
        position = outer.end;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn large_media_payload_is_skipped_without_reading_or_allocating_it() {
        // A virtual 5 GiB mdat followed by a movie header. Any read within the
        // media payload fails, so this also verifies 64-bit seeks and offsets.
        const TAIL: u64 = 5 * 1024 * 1024 * 1024;
        struct SparseMovie {
            head: Vec<u8>,
            tail: Vec<u8>,
            position: u64,
            read: usize,
        }
        impl Read for SparseMovie {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let source = if self.position < self.head.len() as u64 {
                    &self.head[self.position as usize..]
                } else if self.position >= TAIL {
                    &self.tail[(self.position - TAIL) as usize..]
                } else {
                    panic!("MP4 parser attempted to read the media payload");
                };
                let count = source.len().min(buffer.len());
                buffer[..count].copy_from_slice(&source[..count]);
                self.position += count as u64;
                self.read += count;
                Ok(count)
            }
        }
        impl Seek for SparseMovie {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                let SeekFrom::Start(position) = position else {
                    panic!("expected an absolute box seek");
                };
                self.position = position;
                Ok(position)
            }
        }
        let head = [b"\0\0\0\x01mdat".as_slice(), &TAIL.to_be_bytes()].concat();
        let mut tail = b"\0\0\0\x74moov\0\0\0\x6cmvhd\0\0\0\0".to_vec();
        tail.extend_from_slice(&3_800_592_550u32.to_be_bytes());
        tail.resize(116, 0);
        let length = TAIL + tail.len() as u64;
        let mut reader = SparseMovie {
            head,
            tail,
            position: 0,
            read: 0,
        };
        let date = creation_time(&mut reader, length).unwrap().unwrap();
        assert_eq!(date.to_rfc3339(), "2024-06-07T08:09:10+00:00");
        assert_eq!(reader.read, 40);
    }

    #[test]
    fn pathological_box_counts_are_bounded() {
        let bytes = b"\0\0\0\x08free".repeat(MAX_BOXES + 1);
        let length = bytes.len() as u64;
        let error = creation_time(&mut Cursor::new(bytes), length).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("box limit"));
    }

    #[test]
    fn underlying_io_errors_are_not_treated_as_absent_metadata() {
        struct Unreadable;
        impl Read for Unreadable {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
        }
        impl Seek for Unreadable {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }
        let error = creation_time(&mut Unreadable, 100).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }
}
