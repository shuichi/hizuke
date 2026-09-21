//! Synthetic MP4 metadata containers, not playable camera footage.
//! Test the public scanner/planner and the CLI without external video tools.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use chrono::{DateTime, Local, Utc};
use filetime::{FileTime, set_file_mtime};
use hizuke::metadata::{Timezone, scan};
use hizuke::planner::build_plan;
use tempfile::tempdir;

const MTIME: i64 = 946_684_800;
const CREATION: u64 = 3_800_592_550; // 2024-06-07 08:09:10 UTC, epoch 1904.
const STAMP: &str = "2024-06-07 08.09.10";

fn atom(kind: &[u8; 4], payload: &[u8], extended: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    if extended {
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&(payload.len() as u64 + 16).to_be_bytes());
    } else {
        bytes.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
        bytes.extend_from_slice(kind);
    }
    bytes.extend_from_slice(payload);
    bytes
}

fn mvhd(version: u8, seconds: u64, extended: bool) -> Vec<u8> {
    let mut payload = vec![version, 0, 0, 0];
    if version == 1 {
        payload.extend_from_slice(&seconds.to_be_bytes());
        payload.extend_from_slice(&seconds.to_be_bytes());
        payload.extend_from_slice(&1000u32.to_be_bytes());
        payload.extend_from_slice(&1000u64.to_be_bytes());
    } else {
        payload.extend_from_slice(&(seconds as u32).to_be_bytes());
        payload.extend_from_slice(&(seconds as u32).to_be_bytes());
        payload.extend_from_slice(&1000u32.to_be_bytes());
        payload.extend_from_slice(&1000u32.to_be_bytes());
    }
    payload.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // Normal rate.
    payload.extend_from_slice(&0x0100u16.to_be_bytes()); // Normal volume.
    payload.extend_from_slice(&[0; 10]);
    for element in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        payload.extend_from_slice(&element.to_be_bytes());
    }
    payload.extend_from_slice(&[0; 24]);
    payload.extend_from_slice(&1u32.to_be_bytes());
    atom(b"mvhd", &payload, extended)
}

fn movie(version: u8, seconds: u64, extended: bool, tail_moov: bool) -> Vec<u8> {
    let mut bytes = atom(b"ftyp", b"isom\0\0\0\0isommp42", false);
    let moov = atom(b"moov", &mvhd(version, seconds, extended), extended);
    let mdat = atom(b"mdat", b"opaque video payload", extended);
    if tail_moov {
        bytes.extend(mdat);
        bytes.extend(moov);
    } else {
        bytes.extend(moov);
        bytes.extend(mdat);
    }
    bytes
}

fn put(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::write(&path, bytes).unwrap();
    set_file_mtime(path, FileTime::from_unix_time(MTIME, 123_456_700)).unwrap();
}

#[test]
fn versions_box_sizes_and_moov_positions_use_creation_time() {
    for version in [0, 1] {
        for extended in [false, true] {
            for tail in [false, true] {
                let dir = tempdir().unwrap();
                put(
                    dir.path(),
                    "camera.MP4",
                    &movie(version, CREATION, extended, tail),
                );
                let scanned = scan(dir.path(), false, Timezone::Utc).unwrap();
                assert_eq!(scanned.images.len(), 1);
                let image = &scanned.images[0];
                assert_eq!(image.timestamp.to_string(), "2024-06-07 08:09:10");
                assert_eq!(image.time_source, "MP4 creation_time");
                assert!(image.warnings.is_empty(), "{:?}", image.warnings);
                let plan = build_plan(&scanned, &[], &BTreeMap::new()).unwrap();
                assert_eq!(plan.rows[0].target, Some(format!("{STAMP}.MP4").into()));
                assert_eq!(plan.summary.mp4_dates, 1);
                assert_eq!(plan.summary.exif_dates, 0);
                assert_eq!(plan.summary.mtime_dates, 0);
            }
        }
    }
}

#[test]
fn version_one_supports_dates_beyond_32_bits_and_local_timezone() {
    let dir = tempdir().unwrap();
    let utc = DateTime::parse_from_rfc3339("2042-02-03T22:05:06Z")
        .unwrap()
        .with_timezone(&Utc);
    let seconds = (utc.timestamp() + 2_082_844_800) as u64;
    assert!(seconds > u64::from(u32::MAX));
    put(dir.path(), "future.mp4", &movie(1, seconds, false, true));
    let local = scan(dir.path(), false, Timezone::Local).unwrap();
    assert_eq!(
        local.images[0].timestamp,
        utc.with_timezone(&Local).naive_local()
    );
    let universal = scan(dir.path(), false, Timezone::Utc).unwrap();
    assert_eq!(universal.images[0].timestamp, utc.naive_utc());
}

#[test]
fn missing_zero_or_invalid_dates_fall_back_to_mtime_without_mutation() {
    let dir = tempdir().unwrap();
    let mut cases = vec![
        movie(0, 0, false, false),
        atom(b"ftyp", b"isom\0\0\0\0isom", false),
        movie(1, u64::MAX, false, false),
        movie(1, 255_485_145_600, false, false), // Year 10000.
        movie(2, CREATION, false, false),
        atom(b"moov", &atom(b"mvhd", &[0; 8], false), false),
        b"not an MP4".to_vec(),
        vec![0, 0, 0, 4, b'f', b'r', b'e', b'e'],
        vec![0, 0, 0, 1, b'm', b'o', b'o', b'v'],
        [b"\0\0\0\x01moov".as_slice(), &u64::MAX.to_be_bytes()].concat(),
    ];
    let complete = atom(b"moov", &mvhd(0, CREATION, false), false);
    cases.extend((0..complete.len()).map(|length| complete[..length].to_vec()));
    for (index, bytes) in cases.iter().enumerate() {
        put(dir.path(), &format!("bad-{index:03}.mp4"), bytes);
    }
    let scanned = scan(dir.path(), false, Timezone::Utc).unwrap();
    assert_eq!(scanned.images.len(), cases.len());
    for (image, bytes) in scanned.images.iter().zip(&cases) {
        assert_eq!(image.time_source, "mtime", "{:?}", image.path);
        assert_eq!(image.timestamp.and_utc().timestamp(), MTIME);
        assert_eq!(fs::read(dir.path().join(&image.path)).unwrap(), *bytes);
        assert_eq!(image.fingerprint.modified_secs, MTIME);
    }
    assert!(!scanned.images[2].warnings.is_empty());
}

#[test]
fn box_boundaries_prevent_false_dates_in_media_and_nested_containers() {
    let dir = tempdir().unwrap();
    let header = mvhd(0, CREATION, false);
    for (index, bytes) in [
        atom(b"mdat", &atom(b"moov", &header, false), false),
        atom(b"moov", &atom(b"trak", &header, false), false),
        // The mvhd claims bytes outside its containing moov, even though those
        // bytes exist in the file. Never borrow them from a sibling box.
        [atom(b"moov", &header[..16], false), header[16..].to_vec()].concat(),
    ]
    .iter()
    .enumerate()
    {
        put(dir.path(), &format!("unrelated-{index}.mp4"), bytes);
    }
    let scanned = scan(dir.path(), false, Timezone::Utc).unwrap();
    assert!(
        scanned
            .images
            .iter()
            .all(|image| image.time_source == "mtime")
    );
}

#[test]
fn zero_sized_final_boxes_and_recursive_discovery_work() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    let mut header = mvhd(0, CREATION, false);
    header[..4].fill(0);
    let mut bytes = atom(b"moov", &header, false);
    bytes[..4].fill(0);
    put(dir.path(), "nested/camera.mP4", &bytes);
    assert!(
        scan(dir.path(), false, Timezone::Utc)
            .unwrap()
            .images
            .is_empty()
    );
    let scanned = scan(dir.path(), true, Timezone::Utc).unwrap();
    assert_eq!(scanned.images[0].time_source, "MP4 creation_time");
    let plan = build_plan(&scanned, &[], &BTreeMap::new()).unwrap();
    assert_eq!(
        plan.rows[0].target,
        Some(Path::new("nested").join(format!("{STAMP}.mP4")))
    );
}

fn cli(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_hizuke"))
        .arg(args[0])
        .arg(root)
        .args(&args[1..])
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_preview_apply_duplicates_collisions_and_undo_preserve_mp4_files() {
    let dir = tempdir().unwrap();
    let first = movie(0, CREATION, false, true);
    let second = movie(1, CREATION, true, false);
    put(dir.path(), "first.MP4", &first);
    put(dir.path(), "copy.mp4", &first);
    put(dir.path(), "second.mp4", &second);
    put(dir.path(), "without-date.mp4", &movie(0, 0, false, false));
    put(dir.path(), "photo.jpg", include_bytes!("fixtures/exif.jpg"));
    let before: BTreeMap<_, _> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name(),
                (
                    fs::read(entry.path()).unwrap(),
                    fs::metadata(entry.path()).unwrap().modified().unwrap(),
                ),
            )
        })
        .collect();
    let plan = cli(dir.path(), &["preview", "--timezone", "utc"]);
    assert_eq!(plan["scanned"], 5);
    assert_eq!(plan["summary"]["mp4_dates"], 3);
    assert_eq!(plan["summary"]["exif_dates"], 1);
    assert_eq!(plan["summary"]["mtime_dates"], 1);
    assert_eq!(plan["duplicates"].as_array().unwrap().len(), 1);
    assert!(!dir.path().join(".hizuke").exists());
    for (name, (bytes, modified)) in &before {
        assert_eq!(fs::read(dir.path().join(name)).unwrap(), *bytes);
        assert_eq!(
            fs::metadata(dir.path().join(name))
                .unwrap()
                .modified()
                .unwrap(),
            *modified
        );
    }
    cli(
        dir.path(),
        &[
            "apply",
            "--yes",
            "--duplicates",
            "keep-all",
            "--timezone",
            "utc",
        ],
    );
    for (name, bytes) in [
        (format!("{STAMP}.mp4"), &first),
        (format!("{STAMP} (1).MP4"), &first),
        (format!("{STAMP} (2).mp4"), &second),
    ] {
        let path = dir.path().join(name);
        assert_eq!(fs::read(&path).unwrap(), *bytes);
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(path).unwrap()).unix_seconds(),
            MTIME
        );
    }
    assert!(dir.path().join("2000-01-01 00.00.00.mp4").is_file());
    let repeated = cli(
        dir.path(),
        &["preview", "--duplicates", "keep-all", "--timezone", "utc"],
    );
    assert_eq!(repeated["summary"]["rename"], 0);
    cli(dir.path(), &["undo", "--yes"]);
    for (name, (bytes, modified)) in before {
        let path = dir.path().join(name);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
}
