//! Exercise the public CLI against real directories and files. Fixtures deliberately
//! lack EXIF so timestamp fallback is tested without committing binary photos.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use filetime::{FileTime, set_file_mtime};
use tempfile::TempDir;

const STAMP: &str = "2024-01-02 03.04.05";

fn fixture(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    let timestamp = chrono::DateTime::parse_from_rfc3339("2024-01-02T03:04:05Z")
        .unwrap()
        .timestamp();
    set_file_mtime(path, FileTime::from_unix_time(timestamp, 0)).unwrap();
}

/// A minimal JPEG APP1 segment containing an EXIF DateTimeOriginal tag.
fn jpeg_with_capture_time() -> Vec<u8> {
    let mut tiff = b"II\x2a\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&1_u16.to_le_bytes());
    // IFD0 points to the EXIF IFD at byte 26.
    tiff.extend_from_slice(&0x8769_u16.to_le_bytes());
    tiff.extend_from_slice(&4_u16.to_le_bytes());
    tiff.extend_from_slice(&1_u32.to_le_bytes());
    tiff.extend_from_slice(&26_u32.to_le_bytes());
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(&1_u16.to_le_bytes());
    tiff.extend_from_slice(&0x9003_u16.to_le_bytes());
    tiff.extend_from_slice(&2_u16.to_le_bytes());
    tiff.extend_from_slice(&20_u32.to_le_bytes());
    tiff.extend_from_slice(&44_u32.to_le_bytes());
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(b"2012:03:04 05:06:07\0");
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe1];
    jpeg.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
    jpeg.extend_from_slice(b"Exif\0\0");
    jpeg.extend_from_slice(&tiff);
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

fn run(args: &[&str], root: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hizuke"));
    command.arg(args[0]).arg(root).args(&args[1..]);
    command.stdin(Stdio::null()).output().unwrap()
}

fn direct(args: &[&str], working_directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hizuke"))
        .args(args)
        .current_dir(working_directory)
        .stdin(Stdio::null())
        .env_remove("NO_COLOR")
        .output()
        .unwrap()
}

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "CLI failed with {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// Include empty directories and symlinks when detecting preview side effects.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                result.insert(
                    relative,
                    format!("symlink:{}", fs::read_link(path).unwrap().display()).into_bytes(),
                );
            } else if kind.is_dir() {
                result.insert(relative, b"directory".to_vec());
                visit(root, &path, result);
            } else {
                result.insert(relative, fs::read(path).unwrap());
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

fn image_files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(root)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().unwrap().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("jpg"))
        })
        .map(|entry| {
            (
                PathBuf::from(entry.file_name()),
                fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn preview_and_dry_run_do_not_write_or_prompt_even_for_duplicates() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("first.jpg"), b"duplicate image");
    fixture(&directory.path().join("second.jpg"), b"duplicate image");
    let before = snapshot(directory.path());

    let preview = successful(run(&["preview", "--timezone", "utc"], directory.path()));
    assert!(String::from_utf8_lossy(&preview.stdout).contains("ASK"));
    assert_eq!(snapshot(directory.path()), before);

    successful(run(
        &["apply", "--dry-run", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn file_mtime_renaming_preserves_extension_bytes_and_can_be_undone() {
    let directory = TempDir::new().unwrap();
    let original = directory.path().join("camera-original.JPG");
    fixture(&original, b"image payload without EXIF");
    let original_mtime = fs::metadata(&original).unwrap().modified().unwrap();
    let expected = directory.path().join(format!("{STAMP}.JPG"));

    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    assert!(!original.exists());
    assert_eq!(fs::read(&expected).unwrap(), b"image payload without EXIF");
    assert_eq!(
        fs::metadata(&expected).unwrap().modified().unwrap(),
        original_mtime
    );

    successful(run(&["undo", "--yes"], directory.path()));
    assert_eq!(fs::read(&original).unwrap(), b"image payload without EXIF");
    assert_eq!(
        fs::metadata(&original).unwrap().modified().unwrap(),
        original_mtime
    );
    assert!(!expected.exists());
}

#[test]
fn exif_capture_time_takes_precedence_over_file_mtime() {
    let directory = TempDir::new().unwrap();
    let jpeg = jpeg_with_capture_time();
    fixture(&directory.path().join("camera.jpg"), &jpeg);

    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(
        fs::read(directory.path().join("2012-03-04 05.06.07.jpg")).unwrap(),
        jpeg
    );
    assert!(!directory.path().join(format!("{STAMP}.jpg")).exists());
}

#[test]
fn duplicate_ask_refuses_noninteractive_apply_before_any_mutation() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("a.jpg"), b"byte-identical");
    fixture(&directory.path().join("b.jpg"), b"byte-identical");
    let before = snapshot(directory.path());

    let output = run(&["apply", "--yes", "--timezone", "utc"], directory.path());
    assert!(
        !output.status.success(),
        "--yes must not silently choose a duplicate keeper"
    );
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn noninteractive_apply_requires_explicit_yes_even_without_duplicates() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"image");
    let before = snapshot(directory.path());
    let output = run(&["apply", "--timezone", "utc"], directory.path());
    assert!(!output.status.success());
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn skip_leaves_duplicate_groups_untouched_and_renames_unique_images() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("a.jpg"), b"duplicate");
    fixture(&directory.path().join("b.jpg"), b"duplicate");
    fixture(&directory.path().join("unique.jpg"), b"unique");

    successful(run(
        &[
            "apply",
            "--yes",
            "--timezone",
            "utc",
            "--duplicates",
            "skip",
        ],
        directory.path(),
    ));
    assert_eq!(
        fs::read(directory.path().join("a.jpg")).unwrap(),
        b"duplicate"
    );
    assert_eq!(
        fs::read(directory.path().join("b.jpg")).unwrap(),
        b"duplicate"
    );
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP}.jpg"))).unwrap(),
        b"unique"
    );
    assert!(!directory.path().join("unique.jpg").exists());
}

#[test]
fn undo_refuses_to_overwrite_a_new_file_at_the_original_path() {
    let directory = TempDir::new().unwrap();
    let original = directory.path().join("camera.jpg");
    fixture(&original, b"original photo");
    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    fs::write(&original, b"new unrelated photo").unwrap();

    let output = run(&["undo", "--yes"], directory.path());
    assert!(!output.status.success());
    assert_eq!(fs::read(original).unwrap(), b"new unrelated photo");
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP}.jpg"))).unwrap(),
        b"original photo"
    );
}

#[test]
fn undo_refuses_a_photo_that_was_modified_after_apply() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"original photo");
    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    let renamed = directory.path().join(format!("{STAMP}.jpg"));
    fs::write(&renamed, b"edited photo").unwrap();

    let output = run(&["undo", "--yes"], directory.path());
    assert!(!output.status.success());
    assert_eq!(fs::read(renamed).unwrap(), b"edited photo");
    assert!(!directory.path().join("camera.jpg").exists());
}

#[test]
fn keep_all_retains_identical_files_and_second_apply_is_idempotent() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("a.jpg"), b"same content");
    fixture(&directory.path().join("b.jpg"), b"same content");
    let arguments = [
        "apply",
        "--yes",
        "--timezone",
        "utc",
        "--duplicates",
        "keep-all",
    ];

    successful(run(&arguments, directory.path()));
    let after_first_apply = image_files(directory.path());
    assert_eq!(after_first_apply.len(), 2);
    assert_eq!(
        after_first_apply[&PathBuf::from(format!("{STAMP}.jpg"))],
        b"same content"
    );
    assert_eq!(
        after_first_apply[&PathBuf::from(format!("{STAMP} (1).jpg"))],
        b"same content"
    );

    successful(run(&arguments, directory.path()));
    assert_eq!(image_files(directory.path()), after_first_apply);
}

#[test]
fn same_second_different_images_get_distinct_names_without_data_loss() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("a.jpg"), b"first image");
    fixture(&directory.path().join("b.jpg"), b"second image");
    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));

    let files = image_files(directory.path());
    assert_eq!(files.len(), 2);
    assert!(files.contains_key(&PathBuf::from(format!("{STAMP}.jpg"))));
    assert!(files.contains_key(&PathBuf::from(format!("{STAMP} (1).jpg"))));
    let mut contents: Vec<_> = files.into_values().collect();
    contents.sort();
    assert_eq!(
        contents,
        [b"first image".to_vec(), b"second image".to_vec()]
    );
}

#[test]
fn extension_case_collisions_are_safe_on_case_insensitive_filesystems() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("a.jpg"), b"lowercase extension");
    fixture(&directory.path().join("b.JPG"), b"uppercase extension");
    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));

    let files = image_files(directory.path());
    assert_eq!(files.len(), 2);
    assert!(files.keys().any(|path| path.extension().unwrap() == "jpg"));
    assert!(files.keys().any(|path| path.extension().unwrap() == "JPG"));
    let mut contents: Vec<_> = files.into_values().collect();
    contents.sort();
    assert_eq!(
        contents,
        [
            b"lowercase extension".to_vec(),
            b"uppercase extension".to_vec()
        ]
    );
    assert!(!directory.path().join("a.jpg").exists());
    assert!(!directory.path().join("b.JPG").exists());
}

#[test]
fn already_correct_target_stays_in_place_when_another_image_collides() {
    let directory = TempDir::new().unwrap();
    let target = directory.path().join(format!("{STAMP}.jpg"));
    fixture(&target, b"already named correctly");
    fixture(&directory.path().join("camera.jpg"), b"another image");

    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(fs::read(target).unwrap(), b"already named correctly");
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP} (1).jpg"))).unwrap(),
        b"another image"
    );
}

#[test]
fn existing_directory_is_never_replaced_by_a_rename() {
    let directory = TempDir::new().unwrap();
    let target = directory.path().join(format!("{STAMP}.jpg"));
    fs::create_dir(&target).unwrap();
    fs::write(target.join("precious.txt"), b"preserve me").unwrap();
    fixture(&directory.path().join("camera.jpg"), b"image");

    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(
        fs::read(target.join("precious.txt")).unwrap(),
        b"preserve me"
    );
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP} (1).jpg"))).unwrap(),
        b"image"
    );
}

#[test]
fn recursion_is_opt_in_and_keeps_images_in_their_own_directories() {
    let directory = TempDir::new().unwrap();
    let child = directory.path().join("nested");
    fs::create_dir(&child).unwrap();
    fixture(&directory.path().join("root.jpg"), b"root image");
    fixture(&child.join("child.jpg"), b"child image");

    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(fs::read(child.join("child.jpg")).unwrap(), b"child image");
    assert!(directory.path().join(format!("{STAMP}.jpg")).exists());

    successful(run(
        &["apply", "--yes", "--recursive", "--timezone", "utc"],
        directory.path(),
    ));
    assert!(!child.join("child.jpg").exists());
    assert_eq!(
        fs::read(child.join(format!("{STAMP}.jpg"))).unwrap(),
        b"child image"
    );
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP}.jpg"))).unwrap(),
        b"root image"
    );
}

#[cfg(unix)]
#[test]
fn symlink_images_and_directories_are_not_followed() {
    use std::os::unix::fs::symlink;

    let directory = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let external_image = external.path().join("outside.jpg");
    fixture(&external_image, b"outside image");
    symlink(&external_image, directory.path().join("linked.jpg")).unwrap();
    symlink(external.path(), directory.path().join("linked-dir")).unwrap();
    fixture(&directory.path().join("local.jpg"), b"inside image");
    let outside_before = snapshot(external.path());

    successful(run(
        &["apply", "--yes", "--recursive", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(snapshot(external.path()), outside_before);
    assert_eq!(
        fs::read_link(directory.path().join("linked.jpg")).unwrap(),
        external_image
    );
    assert_eq!(
        fs::read_link(directory.path().join("linked-dir")).unwrap(),
        external.path()
    );
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP}.jpg"))).unwrap(),
        b"inside image"
    );
}

#[test]
fn machine_readable_preview_is_valid_json_and_does_not_write() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"image");
    let before = snapshot(directory.path());
    let output = successful(run(
        &["preview", "--json", "--timezone", "utc"],
        directory.path(),
    ));
    let _: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("--json must keep human-readable output off stdout");
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn completions_can_be_generated_without_a_directory() {
    let output = successful(
        Command::new(env!("CARGO_BIN_EXE_hizuke"))
            .args(["completions", "zsh"])
            .stdin(Stdio::null())
            .output()
            .unwrap(),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("hizuke"));
}

#[test]
fn bare_command_previews_current_directory_when_not_attached_to_a_terminal() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());

    let output = successful(direct(&["--timezone", "utc"], directory.path()));
    assert!(String::from_utf8_lossy(&output.stdout).contains(STAMP));
    assert_eq!(snapshot(directory.path()), before);
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}

#[test]
fn bare_directory_argument_previews_duplicates_without_questions() {
    let directory = TempDir::new().unwrap();
    let photos = directory.path().join("写真 with spaces");
    fs::create_dir(&photos).unwrap();
    fixture(&photos.join("a.jpg"), b"identical");
    fixture(&photos.join("b.jpg"), b"identical");
    let before = snapshot(directory.path());

    let output = successful(direct(
        &["写真 with spaces", "--timezone", "utc"],
        directory.path(),
    ));
    assert!(String::from_utf8_lossy(&output.stdout).contains("ASK"));
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn bare_yes_applies_but_never_silently_resolves_duplicates() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    successful(direct(&["-y", "--timezone", "utc"], directory.path()));
    assert_eq!(
        fs::read(directory.path().join(format!("{STAMP}.jpg"))).unwrap(),
        b"photo"
    );

    let duplicates = TempDir::new().unwrap();
    fixture(&duplicates.path().join("a.jpg"), b"duplicate");
    fixture(&duplicates.path().join("b.jpg"), b"duplicate");
    let before = snapshot(duplicates.path());
    assert!(!direct(&["-y"], duplicates.path()).status.success());
    assert_eq!(snapshot(duplicates.path()), before);
}

#[test]
fn dry_run_takes_precedence_over_yes_for_bare_and_explicit_apply() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());
    for arguments in [
        vec!["-n", "-y", "--timezone", "utc"],
        vec!["-y", "--dry-run", "--timezone", "utc"],
        vec!["apply", "--yes", "-n", "--timezone", "utc"],
    ] {
        successful(direct(&arguments, directory.path()));
        assert_eq!(snapshot(directory.path()), before);
    }
}

#[test]
fn globals_before_and_after_subcommand_emit_the_same_clean_json() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());
    let first = successful(direct(
        &[
            "--json",
            "--color",
            "always",
            "preview",
            "--timezone",
            "utc",
        ],
        directory.path(),
    ));
    let second = successful(direct(
        &[
            "preview",
            "--timezone",
            "utc",
            "--color",
            "always",
            "--json",
        ],
        directory.path(),
    ));
    let a: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(a, b);
    assert_eq!(a["schema_version"], 1);
    assert_eq!(a["summary"]["rename"], 1);
    assert_eq!(a["summary"]["mtime_dates"], 1);
    for output in [first, second] {
        assert!(output.stderr.is_empty());
        assert!(!output.stdout.contains(&0x1b));
    }
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn quiet_apply_suppresses_success_chatter_and_json_remains_machine_readable() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let output = successful(direct(&["-q", "-y", "--timezone", "utc"], directory.path()));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let json = successful(direct(&["--json", "--quiet"], directory.path()));
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert!(json.stderr.is_empty());
}

#[test]
fn bare_json_is_readonly_and_yes_json_returns_one_apply_result() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());
    let output = successful(direct(&["--json", "--timezone", "utc"], directory.path()));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["rows"][0]["action"], "rename");
    assert_eq!(snapshot(directory.path()), before);

    let output = successful(direct(
        &["--yes", "--json", "--timezone", "utc"],
        directory.path(),
    ));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["changed"], 1);
    assert!(parsed["transaction"].as_str().is_some());
    assert!(output.stderr.is_empty());
}

#[test]
fn argument_errors_exit_two_without_touching_the_library() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());
    for arguments in [
        vec!["--unknown-option"],
        vec!["--timezone", "jupiter"],
        vec!["--duplicates", "delete"],
        vec!["--quiet", "--verbose"],
        vec!["apply", "--color", "sometimes"],
        vec!["--dry-run", "apply", "--yes"],
        vec!["--yes", "preview"],
    ] {
        let output = direct(&arguments, directory.path());
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert_eq!(snapshot(directory.path()), before);
    }
}

#[test]
fn a_directory_named_like_a_subcommand_can_be_selected_with_double_dash() {
    let directory = TempDir::new().unwrap();
    let photos = directory.path().join("preview");
    fs::create_dir(&photos).unwrap();
    fixture(&photos.join("camera.jpg"), b"photo");
    let before = snapshot(directory.path());

    let output = successful(direct(
        &["--json", "--timezone", "utc", "--", "preview"],
        directory.path(),
    ));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["scanned"], 1);
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn history_is_readonly_for_pristine_and_partially_initialized_libraries() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    for partial_state in [false, true] {
        if partial_state {
            fs::create_dir(directory.path().join(".hizuke")).unwrap();
        }
        let before = snapshot(directory.path());
        successful(direct(&["history", "--json"], directory.path()));
        assert_eq!(snapshot(directory.path()), before);
    }
    let missing = directory.path().join("nonexistent");
    assert!(!run(&["history", "--json"], &missing).status.success());
    assert!(!missing.exists());
}

#[test]
fn undo_preview_and_transaction_detail_do_not_change_images_or_journals() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("camera.jpg"), b"photo");
    successful(direct(&["-y", "--timezone", "utc"], directory.path()));
    let history = successful(direct(&["history", "--json"], directory.path()));
    let records: serde_json::Value = serde_json::from_slice(&history.stdout).unwrap();
    let transaction = records[0]["id"].as_str().unwrap();
    let before = snapshot(directory.path());

    let detail = successful(direct(
        &["history", "--transaction", transaction, "--json"],
        directory.path(),
    ));
    let preview = successful(direct(
        &["undo", "--dry-run", "--yes", "--json"],
        directory.path(),
    ));
    let a: serde_json::Value = serde_json::from_slice(&detail.stdout).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(a, b);
    assert_eq!(a["entries"][0]["original"], "camera.jpg");
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn separately_managed_child_library_is_excluded_from_parent_recursion() {
    let directory = TempDir::new().unwrap();
    let child = directory.path().join("child");
    fs::create_dir(&child).unwrap();
    fixture(&child.join("child.jpg"), b"child photo");
    successful(run(&["apply", "--yes", "--timezone", "utc"], &child));
    let child_before = snapshot(&child);
    fixture(&directory.path().join("parent.jpg"), b"parent photo");

    successful(run(
        &["apply", "--yes", "--recursive", "--timezone", "utc"],
        directory.path(),
    ));
    assert_eq!(snapshot(&child), child_before);
    assert!(directory.path().join(format!("{STAMP}.jpg")).exists());
    // A child adopted first stays independently undoable after adopting its parent.
    successful(run(&["undo", "--yes"], &child));
    assert_eq!(fs::read(child.join("child.jpg")).unwrap(), b"child photo");
}

#[test]
fn a_managed_parent_prevents_adopting_an_unmanaged_child() {
    let directory = TempDir::new().unwrap();
    fixture(&directory.path().join("parent.jpg"), b"parent photo");
    successful(run(
        &["apply", "--yes", "--timezone", "utc"],
        directory.path(),
    ));
    let child = directory.path().join("child");
    fs::create_dir(&child).unwrap();
    fixture(&child.join("child.jpg"), b"child photo");
    let before = snapshot(directory.path());

    let output = run(&["apply", "--yes", "--timezone", "utc"], &child);
    assert!(!output.status.success());
    assert_eq!(snapshot(directory.path()), before);
}

#[test]
fn json_errors_are_one_structured_document_without_terminal_output() {
    let directory = TempDir::new().unwrap();
    let missing = directory.path().join("missing");
    let output = run(&["preview", "--json", "--color", "always"], &missing);
    assert_eq!(output.status.code(), Some(1));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["error"]["exit_code"], 1);
    assert!(output.stderr.is_empty());
    assert!(!output.stdout.contains(&0x1b));
}
