//! Read-only, deterministic planning and exact duplicate detection.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::engine::Operation;
use crate::metadata::{Scan, byte_equal};

#[derive(Clone, Debug, Serialize)]
pub struct DuplicateGroup {
    pub id: usize,
    pub files: Vec<PathBuf>,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug)]
pub enum DuplicateChoice {
    Pending,
    KeepAll,
    Skip,
    /// Zero-based index in DuplicateGroup.files.
    Keep(usize),
}

#[derive(Debug, Serialize)]
pub struct PlanRow {
    pub source: PathBuf,
    pub target: Option<PathBuf>,
    pub action: &'static str,
    pub timestamp: String,
    pub time_source: String,
    pub size: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub schema_version: u32,
    pub root: PathBuf,
    pub scanned: usize,
    pub skipped_non_images: usize,
    pub skipped_directories: usize,
    pub duplicates: Vec<DuplicateGroup>,
    pub pending_groups: usize,
    pub rows: Vec<PlanRow>,
    pub warnings: Vec<String>,
    pub summary: PlanSummary,
    #[serde(skip)]
    pub operations: Vec<Operation>,
}

#[derive(Debug, Default, Serialize)]
pub struct PlanSummary {
    pub rename: usize,
    pub quarantine: usize,
    pub unchanged: usize,
    pub skipped_duplicates: usize,
    pub awaiting_choice: usize,
    pub exif_dates: usize,
    pub mp4_dates: usize,
    pub mtime_dates: usize,
    pub total_bytes: u64,
    pub quarantined_bytes: u64,
}

/// Hashes only narrow candidates. A complete byte comparison decides equality.
pub fn duplicate_groups(scan: &Scan) -> Result<Vec<DuplicateGroup>> {
    let mut buckets: BTreeMap<(u64, &str), Vec<usize>> = BTreeMap::new();
    for (index, file) in scan.images.iter().enumerate() {
        crate::control::check_cancelled()?;
        buckets
            .entry((file.fingerprint.size, &file.fingerprint.sha256))
            .or_default()
            .push(index);
    }
    let mut result = Vec::new();
    for ((size, sha256), indices) in buckets {
        crate::control::check_cancelled()?;
        if indices.len() < 2 {
            continue;
        }
        let mut exact_groups: Vec<Vec<usize>> = Vec::new();
        for index in indices {
            let mut found = false;
            for group in &mut exact_groups {
                if byte_equal(
                    &scan.root.join(&scan.images[group[0]].path),
                    &scan.root.join(&scan.images[index].path),
                )? {
                    group.push(index);
                    found = true;
                    break;
                }
            }
            if !found {
                exact_groups.push(vec![index]);
            }
        }
        for group in exact_groups.into_iter().filter(|g| g.len() > 1) {
            result.push(DuplicateGroup {
                id: 0,
                files: group.iter().map(|&i| scan.images[i].path.clone()).collect(),
                size,
                sha256: sha256.to_owned(),
            });
        }
    }
    result.sort_by(|a, b| a.files[0].cmp(&b.files[0]));
    for (index, group) in result.iter_mut().enumerate() {
        group.id = index + 1;
    }
    Ok(result)
}

/// Existing correct names (including collision suffixes) stay stable on reruns.
fn correct_name(path: &Path, stamp: &str) -> bool {
    let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
        return false;
    };
    if stem == stamp {
        return true;
    }
    let Some(suffix) = stem.strip_prefix(stamp).and_then(|s| s.strip_prefix(" (")) else {
        return false;
    };
    let Some(number) = suffix.strip_suffix(')') else {
        return false;
    };
    !number.starts_with('0') && !number.is_empty() && number.bytes().all(|c| c.is_ascii_digit())
}

pub fn build_plan(
    scan: &Scan,
    groups: &[DuplicateGroup],
    choices: &BTreeMap<usize, DuplicateChoice>,
) -> Result<Plan> {
    let mut policies = BTreeMap::new();
    let mut pending_groups = 0;
    for group in groups {
        let choice = choices
            .get(&group.id)
            .copied()
            .unwrap_or(DuplicateChoice::Pending);
        if let DuplicateChoice::Keep(index) = choice {
            ensure!(index < group.files.len(), "invalid duplicate keeper index");
        }
        if matches!(choice, DuplicateChoice::Pending) {
            pending_groups += 1;
        }
        for (index, file) in group.files.iter().enumerate() {
            let action = match choice {
                DuplicateChoice::Pending => "needs_decision",
                DuplicateChoice::Skip => "skip",
                DuplicateChoice::Keep(keeper) if keeper != index => "quarantine",
                _ => "rename",
            };
            policies.insert(file, action);
        }
    }

    // Reserve even names that will be vacated: external/non-image files, directories,
    // dangling symlinks, and stationary files must never be overwritten. ASCII-folded
    // keys also avoid .JPG/.jpg destination collisions on case-insensitive volumes.
    let mut occupied: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for file in &scan.images {
        crate::control::check_cancelled()?;
        let parent = file.path.parent().unwrap_or(Path::new(""));
        if occupied.contains_key(parent) {
            continue;
        }
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(scan.root.join(parent))? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 sibling name in {}", parent.display()))?;
            names.insert(name.to_ascii_lowercase());
        }
        occupied.insert(parent.to_owned(), names);
    }

    // Remember the next free suffix for each timestamp/extension. Starting each
    // image at zero would be quadratic for large same-second bursts.
    let mut next_suffix: BTreeMap<(PathBuf, String, String), u64> = BTreeMap::new();
    let mut plan = Plan {
        schema_version: 1,
        root: scan.root.clone(),
        scanned: scan.images.len(),
        skipped_non_images: scan.skipped,
        skipped_directories: scan.skipped_directories,
        duplicates: groups.to_vec(),
        pending_groups,
        rows: Vec::new(),
        warnings: scan.warnings.clone(),
        summary: PlanSummary::default(),
        operations: Vec::new(),
    };
    for file in &scan.images {
        crate::control::check_cancelled()?;
        let stamp = file.timestamp.format("%Y-%m-%d %H.%M.%S").to_string();
        let policy = policies.get(&file.path).copied().unwrap_or("rename");
        let (action, target) = match policy {
            "needs_decision" | "skip" | "quarantine" => (policy, None),
            _ if correct_name(&file.path, &stamp) => ("unchanged", Some(file.path.clone())),
            _ => {
                let parent = file.path.parent().unwrap_or(Path::new(""));
                let extension = file
                    .path
                    .extension()
                    .and_then(|v| v.to_str())
                    .context("image extension is not UTF-8")?;
                let names = occupied
                    .get_mut(parent)
                    .context("missing parent reservation")?;
                let suffix = next_suffix
                    .entry((
                        parent.to_owned(),
                        stamp.clone(),
                        extension.to_ascii_lowercase(),
                    ))
                    .or_default();
                let target = loop {
                    let name = if *suffix == 0 {
                        format!("{stamp}.{extension}")
                    } else {
                        format!("{stamp} ({suffix}).{extension}")
                    };
                    if names.insert(name.to_ascii_lowercase()) {
                        *suffix = suffix
                            .checked_add(1)
                            .context("too many filename collisions")?;
                        break parent.join(name);
                    }
                    *suffix = suffix
                        .checked_add(1)
                        .context("too many filename collisions")?;
                };
                ("rename", Some(target))
            }
        };
        if matches!(action, "rename" | "quarantine") {
            plan.operations.push(Operation {
                source: file.path.clone(),
                target: target.clone(),
                expected: file.fingerprint.clone(),
            });
        }
        match action {
            "rename" => plan.summary.rename += 1,
            "quarantine" => {
                plan.summary.quarantine += 1;
                plan.summary.quarantined_bytes = plan
                    .summary
                    .quarantined_bytes
                    .checked_add(file.fingerprint.size)
                    .context("total size overflow")?;
            }
            "skip" => plan.summary.skipped_duplicates += 1,
            "needs_decision" => plan.summary.awaiting_choice += 1,
            _ => plan.summary.unchanged += 1,
        }
        if file.time_source == "mtime" {
            plan.summary.mtime_dates += 1;
        } else if file.time_source.starts_with("MP4 ") {
            plan.summary.mp4_dates += 1;
        } else {
            plan.summary.exif_dates += 1;
        }
        plan.summary.total_bytes = plan
            .summary
            .total_bytes
            .checked_add(file.fingerprint.size)
            .context("total size overflow")?;
        plan.rows.push(PlanRow {
            source: file.path.clone(),
            target,
            action,
            timestamp: stamp,
            time_source: file.time_source.clone(),
            size: file.fingerprint.size,
            warnings: file.warnings.clone(),
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognize_only_our_exact_names() {
        let stamp = "2024-01-02 03.04.05";
        for name in ["2024-01-02 03.04.05.JPG", "2024-01-02 03.04.05 (2).jpg"] {
            assert!(correct_name(Path::new(name), stamp));
        }
        for name in [
            "2024-01-02 03.04.05 (0).jpg",
            "2024-01-02 03.04.05 (01).jpg",
            "2024-01-02 03.04.05 ().jpg",
            "2024-01-02 03.04.05 edited.jpg",
        ] {
            assert!(!correct_name(Path::new(name), stamp));
        }
    }
}
