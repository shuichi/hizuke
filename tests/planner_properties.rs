//! Deterministic generated scenarios exercise planning invariants across many
//! filenames, extensions, occupied targets, directories, and timestamp bursts.
//! Transactions themselves are covered separately by engine and CLI tests.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use filetime::{FileTime, set_file_mtime};
use hizuke::metadata::{Timezone, scan};
use hizuke::planner::build_plan;
use tempfile::TempDir;

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn photo(path: &Path, bytes: &[u8], timestamp: i64) {
    fs::write(path, bytes).unwrap();
    set_file_mtime(path, FileTime::from_unix_time(timestamp, 0)).unwrap();
}

#[test]
fn generated_plans_preserve_bytes_extensions_parents_and_idempotence() {
    const EPOCH: i64 = 1_704_164_645;
    const EXTENSIONS: [&str; 6] = ["jpg", "JPG", "jpeg", "png", "PNG", "webp"];
    for initial_seed in [1, 0xcafe_babe, 0x1234_5678_abcd, 0xfedc_ba98_7654] {
        let directory = TempDir::new().unwrap();
        let root = directory.path();
        let mut state = initial_seed;
        for parent in ["", "family", "旅行", "with spaces"] {
            let parent = root.join(parent);
            fs::create_dir_all(&parent).unwrap();
            // Occupied directories must reserve would-be image targets, too.
            fs::create_dir(parent.join("2024-01-02 03.04.05.jpg")).unwrap();
            photo(
                &parent.join("2024-01-02 03.04.05 (7).JPG"),
                format!("stationary {initial_seed} {parent:?}").as_bytes(),
                EPOCH,
            );
        }
        for index in 0..256 {
            let parent = ["", "family", "旅行", "with spaces"][(next(&mut state) % 4) as usize];
            let extension = EXTENSIONS[(next(&mut state) % EXTENSIONS.len() as u64) as usize];
            let timestamp = EPOCH + (next(&mut state) % 4) as i64;
            photo(
                &root.join(parent).join(format!(
                    "camera-{index:04}-{}.{extension}",
                    next(&mut state) % 10000
                )),
                format!("distinct payload {initial_seed} {index}").as_bytes(),
                timestamp,
            );
        }
        let before = scan(root, true, Timezone::Utc).unwrap();
        let first = build_plan(&before, &[], &BTreeMap::new()).unwrap();
        let second = build_plan(&before, &[], &BTreeMap::new()).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(first.operations.len(), 256);
        assert_eq!(first.summary.unchanged, 4);
        assert_eq!(
            first.summary.rename + first.summary.unchanged,
            before.images.len()
        );
        let expected_bytes: BTreeSet<_> = before
            .images
            .iter()
            .map(|image| (image.fingerprint.size, image.fingerprint.sha256.clone()))
            .collect();
        assert_eq!(
            expected_bytes.len(),
            before.images.len(),
            "fixture images must be distinct"
        );

        let mut destinations = BTreeSet::new();
        for row in &first.rows {
            let target = row.target.as_ref().unwrap();
            assert_eq!(
                row.source.parent(),
                target.parent(),
                "must stay in its directory"
            );
            assert_eq!(
                row.source.extension(),
                target.extension(),
                "extension case must survive"
            );
            assert!(
                destinations.insert(target.to_string_lossy().to_ascii_lowercase()),
                "case-folded target collision"
            );
            if row.action == "rename" {
                assert!(!root.join(target).exists(), "destination already occupied");
            }
        }
        // Materialize the already-checked plan without exercising the transaction
        // implementation again; only planning/idempotence is under test here.
        for operation in &first.operations {
            let target = root.join(operation.target.as_ref().unwrap());
            assert!(fs::symlink_metadata(&target).is_err());
            fs::rename(root.join(&operation.source), target).unwrap();
        }
        let after = scan(root, true, Timezone::Utc).unwrap();
        let actual_bytes: BTreeSet<_> = after
            .images
            .iter()
            .map(|image| (image.fingerprint.size, image.fingerprint.sha256.clone()))
            .collect();
        assert_eq!(actual_bytes, expected_bytes);
        let repeated = build_plan(&after, &[], &BTreeMap::new()).unwrap();
        assert!(
            repeated.operations.is_empty(),
            "second run must be idempotent (seed {initial_seed})"
        );
        assert_eq!(repeated.summary.unchanged, 260);

        // Importing another image may add a suffix but cannot rename existing files.
        photo(&root.join("new-import.JPG"), b"new arrival", EPOCH);
        let expanded = scan(root, true, Timezone::Utc).unwrap();
        let incremental = build_plan(&expanded, &[], &BTreeMap::new()).unwrap();
        assert_eq!(incremental.operations.len(), 1);
        assert_eq!(
            incremental.operations[0].source,
            Path::new("new-import.JPG")
        );
        assert!(
            !root
                .join(incremental.operations[0].target.as_ref().unwrap())
                .exists()
        );
    }
}
