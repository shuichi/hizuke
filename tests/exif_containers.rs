//! Upstream test containers, plus explicitly generated date-bearing derivatives.
//! These exercise metadata/container interoperability, not image pixel decoding.
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use hizuke::metadata::{Timezone, scan};
use hizuke::planner::build_plan;
use tempfile::tempdir;

const FIXTURES: &[(&str, &[u8])] = &[
    ("jpg", include_bytes!("fixtures/exif.jpg")),
    ("tif", include_bytes!("fixtures/exif.tif")),
    ("png", include_bytes!("fixtures/exif.png")),
    ("webp", include_bytes!("fixtures/exif.webp")),
    ("heic", include_bytes!("fixtures/exif.heic")),
];

fn write_with_conflicting_mtime(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(946_684_800, 0)).unwrap();
}

#[test]
fn untouched_upstream_containers_use_the_metadata_they_actually_have() {
    let dir = tempdir().unwrap();
    for (extension, bytes) in FIXTURES {
        // Independently confirm the original upstream container parses as EXIF.
        let exif = exif::Reader::new()
            .read_from_container(&mut Cursor::new(bytes))
            .unwrap();
        assert!(exif.fields().len() > 0);
        write_with_conflicting_mtime(&dir.path().join(format!("original.{extension}")), bytes);
    }
    let result = scan(dir.path(), false, Timezone::Utc).unwrap();
    assert_eq!(result.images.len(), 5);
    for image in result.images {
        if image.path.extension().unwrap() == "jpg" {
            assert_eq!(image.timestamp.to_string(), "2016-05-04 03:02:01");
            assert!(image.time_source.starts_with("EXIF DateTime"));
        } else {
            // The other four upstream examples deliberately contain no date tags.
            assert_eq!(image.timestamp.to_string(), "2000-01-01 00:00:00");
            assert_eq!(image.time_source, "mtime");
        }
    }
}

#[test]
fn date_bearing_derivatives_prioritize_exif_and_produce_canonical_targets() {
    let dir = tempdir().unwrap();
    for (extension, original) in FIXTURES {
        let bytes = date_bearing_container(extension, original);
        write_with_conflicting_mtime(&dir.path().join(format!("camera.{extension}")), &bytes);
    }
    let result = scan(dir.path(), false, Timezone::Local).unwrap();
    assert_eq!(result.images.len(), 5);
    for image in &result.images {
        assert_eq!(
            image.timestamp.to_string(),
            "2024-06-07 08:09:10",
            "{:?}",
            image.path
        );
        assert_eq!(image.time_source, "EXIF DateTimeOriginal");
        assert_eq!(image.fingerprint.modified_secs, 946_684_800);
        assert!(
            image.warnings.is_empty(),
            "{:?}: {:?}",
            image.path,
            image.warnings
        );
    }
    let plan = build_plan(&result, &[], &BTreeMap::new()).unwrap();
    assert_eq!(plan.operations.len(), 5);
    for row in plan.rows {
        let extension = row.source.extension().unwrap().to_str().unwrap();
        assert_eq!(
            row.target.unwrap(),
            Path::new(&format!("2024-06-07 08.09.10.{extension}"))
        );
    }
}

#[test]
fn truncated_and_damaged_container_corpus_never_panics_or_mutates_inputs() {
    let dir = tempdir().unwrap();
    let mut expected = BTreeMap::new();
    for (extension, original) in FIXTURES {
        let mut lengths = vec![
            0,
            1,
            2,
            4,
            8,
            16,
            32,
            original.len() / 2,
            original.len() - 1,
        ];
        lengths.sort_unstable();
        lengths.dedup();
        for length in lengths {
            let name = format!("prefix-{length:04}.{extension}");
            let bytes = original[..length].to_vec();
            write_with_conflicting_mtime(&dir.path().join(&name), &bytes);
            expected.insert(name, bytes);
        }
        let mut corrupt = original.to_vec();
        for byte in corrupt.iter_mut().take(16) {
            *byte = 0xff;
        }
        let name = format!("damaged-header.{extension}");
        write_with_conflicting_mtime(&dir.path().join(&name), &corrupt);
        expected.insert(name, corrupt);
    }
    // Malformed metadata is expected to use any still-valid EXIF, otherwise mtime.
    // Unwinding on any parser panic fails this test, including worker-thread panics.
    let result = scan(dir.path(), false, Timezone::Utc).unwrap();
    assert_eq!(result.images.len(), expected.len());
    assert!(
        result
            .images
            .iter()
            .any(|image| image.time_source == "mtime")
    );
    for image in result.images {
        if image.time_source == "mtime" {
            assert_eq!(image.timestamp.to_string(), "2000-01-01 00:00:00");
        }
        let bytes = &expected[image.path.to_str().unwrap()];
        assert_eq!(fs::read(dir.path().join(&image.path)).unwrap(), *bytes);
        assert_eq!(image.fingerprint.size, bytes.len() as u64);
    }
}

fn be_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(bytes[at..at + 2].try_into().unwrap())
}
fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn put_be_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_be_bytes());
}

fn entries(bytes: &[u8], offset: usize) -> Vec<[u8; 12]> {
    let count = usize::from(be_u16(bytes, offset));
    (0..count)
        .map(|i| {
            bytes[offset + 2 + i * 12..offset + 14 + i * 12]
                .try_into()
                .unwrap()
        })
        .collect()
}

fn field(tag: u16, kind: u16, count: u32, value: u32) -> [u8; 12] {
    let mut bytes = [0; 12];
    bytes[..2].copy_from_slice(&tag.to_be_bytes());
    bytes[2..4].copy_from_slice(&kind.to_be_bytes());
    put_be_u32(&mut bytes, 4, count);
    put_be_u32(&mut bytes, 8, value);
    bytes
}

fn append_ifd(bytes: &mut Vec<u8>, mut fields: Vec<[u8; 12]>, next: u32) -> u32 {
    if bytes.len() % 2 == 1 {
        bytes.push(0);
    }
    let offset = bytes.len() as u32;
    fields.sort_by_key(|entry| be_u16(entry, 0));
    bytes.extend_from_slice(&(fields.len() as u16).to_be_bytes());
    for entry in fields {
        bytes.extend_from_slice(&entry);
    }
    bytes.extend_from_slice(&next.to_be_bytes());
    offset
}

// Keep upstream TIFF pixels, offsets, and existing fields intact; append new IFDs
// and update the header pointer. All five upstream EXIF payloads are big-endian.
fn add_dates(raw: &[u8]) -> Vec<u8> {
    assert_eq!(&raw[..4], b"MM\0*");
    let primary_offset = be_u32(raw, 4) as usize;
    let mut primary = entries(raw, primary_offset);
    let next = be_u32(raw, primary_offset + 2 + primary.len() * 12);
    let mut sub = primary
        .iter()
        .find(|entry| be_u16(*entry, 0) == 0x8769)
        .map(|entry| entries(raw, be_u32(entry, 8) as usize))
        .unwrap_or_default();
    primary.retain(|entry| !matches!(be_u16(entry, 0), 0x0132 | 0x8769));
    sub.retain(|entry| !matches!(be_u16(entry, 0), 0x9003 | 0x9004));
    let mut bytes = raw.to_vec();
    let mut offsets = Vec::new();
    for date in [
        b"2024:06:07 08:09:10\0",
        b"2023:05:06 07:08:09\0",
        b"2022:04:05 06:07:08\0",
    ] {
        offsets.push(bytes.len() as u32);
        bytes.extend_from_slice(date);
    }
    sub.push(field(0x9003, 2, 20, offsets[0]));
    sub.push(field(0x9004, 2, 20, offsets[1]));
    let sub_offset = append_ifd(&mut bytes, sub, 0);
    primary.push(field(0x0132, 2, 20, offsets[2]));
    primary.push(field(0x8769, 4, 1, sub_offset));
    let primary_offset = append_ifd(&mut bytes, primary, next);
    put_be_u32(&mut bytes, 4, primary_offset);
    bytes
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn date_bearing_container(extension: &str, original: &[u8]) -> Vec<u8> {
    let exif = exif::Reader::new()
        .read_from_container(&mut Cursor::new(original))
        .unwrap();
    let raw = exif.buf();
    let replacement = add_dates(raw);
    if extension == "tif" {
        return replacement;
    }
    let offset = original
        .windows(raw.len())
        .position(|window| window == raw)
        .unwrap();
    let mut bytes = original.to_vec();
    match extension {
        "jpg" => {
            assert_eq!(&original[offset - 10..offset - 8], b"\xff\xe1");
            bytes.splice(offset..offset + raw.len(), replacement.iter().copied());
            bytes[offset - 8..offset - 6]
                .copy_from_slice(&((replacement.len() + 8) as u16).to_be_bytes());
        }
        "png" => {
            assert_eq!(&original[offset - 4..offset], b"eXIf");
            bytes.splice(offset..offset + raw.len(), replacement.iter().copied());
            put_be_u32(&mut bytes, offset - 8, replacement.len() as u32);
            let crc = crc32(&bytes[offset - 4..offset + replacement.len()]);
            put_be_u32(&mut bytes, offset + replacement.len(), crc);
        }
        "webp" => {
            assert_eq!(&original[offset - 8..offset - 4], b"EXIF");
            assert_eq!(le_u32(original, offset - 4) as usize, raw.len());
            let mut payload = replacement.clone();
            if payload.len() % 2 == 1 {
                payload.push(0);
            }
            bytes.splice(offset..offset + raw.len() + raw.len() % 2, payload);
            bytes[offset - 4..offset].copy_from_slice(&(replacement.len() as u32).to_le_bytes());
            let riff_size = (bytes.len() - 8) as u32;
            bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        }
        "heic" => {
            // Upstream fixture stores its EXIF item last in mdat with a 4-byte
            // TIFF offset prefix. Update that item's extent and the mdat size.
            assert_eq!(offset + raw.len(), original.len());
            assert_eq!(&original[offset - 4..offset], &[0; 4]);
            let iloc = original
                .windows(4)
                .position(|window| window == b"iloc")
                .unwrap()
                - 4;
            assert_eq!(be_u32(original, iloc), 52);
            assert_eq!(be_u32(original, iloc + 48) as usize, raw.len() + 4);
            let mdat = original
                .windows(4)
                .position(|window| window == b"mdat")
                .unwrap()
                - 4;
            assert_eq!(mdat + be_u32(original, mdat) as usize, original.len());
            bytes.splice(offset.., replacement.iter().copied());
            put_be_u32(&mut bytes, iloc + 48, (replacement.len() + 4) as u32);
            let mdat_length = (bytes.len() - mdat) as u32;
            put_be_u32(&mut bytes, mdat, mdat_length);
        }
        _ => unreachable!(),
    }
    bytes
}
