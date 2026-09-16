//! Durable, non-overwriting filesystem transactions.
//!
//! Every image move has a write-ahead journal entry. Snapshots are immutable;
//! publishing the next journal snapshot is itself an exclusive rename. Recovery
//! first resolves the one possible in-flight move, then stages all files before
//! restoring original paths. No image is deleted, including duplicate images.
//!
//! The advisory lock coordinates hizuke processes, not arbitrary writers.
//! Fingerprints detect observed changes, but cannot stop another process from
//! modifying a file between validation and rename. Local filesystems providing
//! atomic rename and truthful fsync are required for the durability guarantees;
//! cloud clients and network filesystems may offer weaker semantics.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::metadata::{Fingerprint, fingerprint};

pub const STATE_DIR: &str = ".hizuke";
/// Storage compatibility only: reuse an existing legacy collection in place.
pub const LEGACY_STATE_DIR: &str = ".imgrename";
// Preserve coordination with older clients; this is a compatibility identifier.
#[cfg(windows)]
const LEGACY_TREE_MUTEX: &str = "Global\\imgrename-directory-trees-v1";

pub fn is_state_dir_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(STATE_DIR) || name.eq_ignore_ascii_case(LEGACY_STATE_DIR)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub source: PathBuf,
    /// None quarantines a redundant file within the transaction, without deletion.
    pub target: Option<PathBuf>,
    pub expected: Fingerprint,
}

#[derive(Debug, Serialize)]
pub struct TransactionSummary {
    pub id: String,
    pub status: String,
    pub operations: usize,
}

#[derive(Debug, Serialize)]
pub struct TransactionDetail {
    pub id: String,
    pub status: String,
    pub entries: Vec<RestoreEntry>,
}

#[derive(Debug, Serialize)]
pub struct RestoreEntry {
    /// Last durably recorded location, relative to the collection root.
    pub current: PathBuf,
    pub original: PathBuf,
    /// An interrupted atomic move may already have reached this other location.
    pub alternate: Option<PathBuf>,
    pub quarantined: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Applying,
    Committed,
    Undoing,
    Recovering,
    Undone,
    RolledBack,
}

impl Status {
    fn incomplete(self) -> bool {
        matches!(self, Self::Applying | Self::Undoing | Self::Recovering)
    }
    fn name(self) -> &'static str {
        match self {
            Self::Applying => "applying",
            Self::Committed => "committed",
            Self::Undoing => "undoing",
            Self::Recovering => "recovering",
            Self::Undone => "undone",
            Self::RolledBack => "rolled_back",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Intent {
    Apply,
    Undo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Location {
    Original,
    Staged,
    Final,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    index: usize,
    from: Location,
    to: Location,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    revision: u64,
    id: String,
    status: Status,
    intent: Intent,
    operations: Vec<Operation>,
    locations: Vec<Location>,
    pending: Option<Pending>,
    /// Legacy v0.1 journals did not have a separate durable revision witness.
    #[serde(default)]
    witnessed: bool,
    #[serde(skip)]
    witnessed_revision: u64,
    #[serde(skip)]
    location_change: Option<LocationChange>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationChange {
    index: usize,
    location: Location,
}

/// The first record stores the plan once. Later records contain constant-size
/// state deltas, avoiding quadratic journal storage for large photo libraries.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    version: u32,
    revision: u64,
    id: String,
    status: Status,
    intent: Intent,
    pending: Option<Pending>,
    location_change: Option<LocationChange>,
    #[serde(default)]
    witnessed: bool,
}

struct Store {
    root: PathBuf,
    transactions: PathBuf,
    _lock: Option<File>,
    _tree_lock: TreeLock,
}

impl Store {
    fn open(root: &Path) -> Result<Self> {
        let root = canonical_root(root)?;
        let tree_lock = lock_tree(&root, false)?;
        let state = select_state_directory(&root)?;
        check_ancestor_ownership(&root)?;
        ensure_directory(&state)?;
        let lock_path = state.join("lock");
        let lock = match open_exclusive(&lock_path) {
            Ok(file) => {
                file.sync_all()?;
                sync_directory(&state)?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                open_regular(&lock_path, true)?
            }
            Err(error) => return Err(error).context("cannot create transaction lock"),
        };
        lock.try_lock()
            .context("another hizuke process holds this directory's lock")?;
        let transactions = state.join("transactions");
        ensure_directory(&transactions)?;
        Ok(Self {
            root,
            transactions,
            _lock: Some(lock),
            _tree_lock: tree_lock,
        })
    }

    /// Never creates or changes filesystem entries, even for incomplete state.
    fn open_readonly(root: &Path) -> Result<Option<Self>> {
        let root = canonical_root(root)?;
        let tree_lock = lock_tree(&root, true)?;
        let state = select_state_directory(&root)?;
        if !exists(&state)? {
            return Ok(None);
        }
        verify_components(&root, &state, false)?;
        ensure!(
            fs::symlink_metadata(&state)?.is_dir(),
            "state is not a directory"
        );
        let lock_path = state.join("lock");
        let lock = if exists(&lock_path)? {
            let lock = open_regular(&lock_path, false)?;
            lock.try_lock_shared()
                .context("another hizuke process is changing this directory")?;
            Some(lock)
        } else {
            None
        };
        let transactions = state.join("transactions");
        verify_components(&root, &transactions, true)?;
        Ok(Some(Self {
            root,
            transactions,
            _lock: lock,
            _tree_lock: tree_lock,
        }))
    }

    fn transaction(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let path = self.transactions.join(id);
        verify_components(&self.root, &path, false)?;
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "transaction is not a directory"
        );
        Ok(path)
    }

    fn ids(&self) -> Result<Vec<String>> {
        verify_components(&self.root, &self.transactions, true)?;
        if !exists(&self.transactions)? {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&self.transactions)? {
            let entry = entry?;
            let id = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 transaction directory"))?;
            self.transaction(&id)?;
            ids.push(id);
        }
        ids.sort();
        Ok(ids)
    }

    fn load(&self, id: &str) -> Result<Option<Journal>> {
        let directory = self.transaction(id)?;
        let mut snapshots = Vec::new();
        for entry in fs::read_dir(&directory)? {
            crate::control::check_cancelled()?;
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 transaction entry"))?;
            ensure!(
                !entry.file_type()?.is_symlink(),
                "symlink in transaction: {}",
                entry.path().display()
            );
            if let Some(revision) = snapshot_revision(&name) {
                snapshots.push((revision, entry.path()));
            }
        }
        snapshots.sort_by_key(|(revision, _)| *revision);
        let proofs = read_witness(&directory)?;
        ensure!(
            proofs.len() <= snapshots.len(),
            "transaction journal tail is missing; durable HEAD records later revisions"
        );
        let mut snapshots = snapshots.into_iter();
        let Some((revision, path)) = snapshots.next() else {
            for name in ["staging", "duplicates"] {
                let archived = directory.join(name);
                if exists(&archived)? {
                    ensure!(
                        fs::symlink_metadata(&archived)?.is_dir(),
                        "invalid transaction archive"
                    );
                    ensure!(
                        fs::read_dir(&archived)?.next().is_none(),
                        "transaction journal is missing but archived files remain; refusing to discard their recovery history"
                    );
                }
            }
            return Ok(None);
        };
        ensure!(
            revision == 1,
            "initial transaction journal is missing; refusing to guess"
        );
        let mut data = Vec::new();
        open_regular(&path, false)?.read_to_end(&mut data)?;
        verify_witness(&proofs, revision, &data)?;
        let mut journal: Journal = serde_json::from_slice(&data)
            .with_context(|| format!("invalid journal {}; refusing to guess", path.display()))?;
        ensure!(
            journal.version == 1 && journal.id == id && journal.revision == revision,
            "journal identity/version does not match its path"
        );
        validate_operations(&journal.operations)?;
        let mut counts = LocationCounts::new(&journal.locations);
        validate_journal_state(&journal, &counts)?;
        ensure!(
            counts.original == journal.locations.len(),
            "initial journal must locate every file at its original path"
        );
        ensure!(
            journal.status == Status::Applying
                || (journal.status == Status::RolledBack && journal.operations.is_empty()),
            "invalid initial transaction status"
        );
        let mut witness_start = journal.witnessed.then_some(1);
        for (revision, path) in snapshots {
            crate::control::check_cancelled()?;
            ensure!(
                revision
                    == journal
                        .revision
                        .checked_add(1)
                        .context("journal revision overflow")?,
                "transaction journal sequence has a gap; refusing to guess"
            );
            data.clear();
            open_regular(&path, false)?.read_to_end(&mut data)?;
            verify_witness(&proofs, revision, &data)?;
            let event: Event = serde_json::from_slice(&data).with_context(|| {
                format!("invalid journal {}; refusing to guess", path.display())
            })?;
            ensure!(
                event.version == 1 && event.id == id && event.revision == revision,
                "journal event identity/version does not match its path"
            );
            validate_transition(&journal, &event, &counts)?;
            if event.witnessed {
                witness_start.get_or_insert(revision);
                journal.witnessed = true;
            }
            if let Some(change) = event.location_change {
                ensure!(
                    change.index < journal.locations.len(),
                    "invalid location event"
                );
                counts.moved(journal.locations[change.index], change.location);
                journal.locations[change.index] = change.location;
            }
            journal.revision = revision;
            journal.status = event.status;
            journal.intent = event.intent;
            journal.pending = event.pending;
        }
        validate_journal_state(&journal, &counts)?;
        if let Some(first) = witness_start {
            // A crash may publish ONE record before its witness. No subsequent
            // image mutation can begin until the witness itself has been flushed.
            let secured = (proofs.len() as u64).max(first - 1);
            ensure!(
                journal.revision <= secured + 1,
                "transaction HEAD witness is missing or truncated before earlier durable moves"
            );
        }
        journal.witnessed_revision = proofs.len() as u64;
        Ok(Some(journal))
    }

    fn save(&self, journal: &mut Journal) -> Result<()> {
        let directory = self.transaction(&journal.id)?;
        let revision = journal
            .revision
            .checked_add(1)
            .context("journal revision overflow")?;
        let final_path = directory.join(format!("journal-{revision:020}.json"));
        let temporary = directory.join(format!(".snapshot-{}", unique_id()));
        let mut file = open_exclusive(&temporary)?;
        if journal.revision == 0 {
            let mut initial = journal.clone();
            initial.revision = revision;
            initial.witnessed = true;
            serde_json::to_writer(&mut file, &initial)?;
        } else {
            serde_json::to_writer(
                &mut file,
                &Event {
                    version: 1,
                    revision,
                    id: journal.id.clone(),
                    status: journal.status,
                    intent: journal.intent,
                    pending: journal.pending.clone(),
                    location_change: journal.location_change.clone(),
                    witnessed: true,
                },
            )?;
        }
        file.write_all(b"\n")?;
        file.sync_all()?;
        rename_no_replace(&temporary, &final_path)?;
        sync_directory(&directory)?;
        append_witnesses(&directory, journal.witnessed_revision, revision)?;
        journal.revision = revision;
        journal.witnessed = true;
        journal.witnessed_revision = revision;
        journal.location_change = None;
        Ok(())
    }

    fn assert_finished(&self) -> Result<()> {
        for id in self.ids()? {
            let journal = self.load(&id)?;
            ensure!(
                journal.is_some_and(|journal| !journal.status.incomplete()),
                "transaction {id} is unfinished; run hizuke recover first"
            );
        }
        Ok(())
    }

    fn location(&self, journal: &Journal, index: usize, location: Location) -> Result<PathBuf> {
        let operation = &journal.operations[index];
        let path = match location {
            Location::Original => self.root.join(&operation.source),
            Location::Staged => self
                .transaction(&journal.id)?
                .join("staging")
                .join(index.to_string()),
            Location::Final => match &operation.target {
                Some(target) => self.root.join(target),
                None => self
                    .transaction(&journal.id)?
                    .join("duplicates")
                    .join(index.to_string()),
            },
        };
        verify_components(&self.root, &path, true)?;
        Ok(path)
    }

    fn check_file(&self, path: &Path, expected: &Fingerprint) -> Result<()> {
        verify_components(&self.root, path, false)?;
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "expected a regular file: {}",
            path.display()
        );
        ensure!(
            &fingerprint(path)? == expected,
            "file changed since planning; preserved without modification: {}",
            path.display()
        );
        Ok(())
    }

    fn move_file(&self, journal: &mut Journal, index: usize, to: Location) -> Result<()> {
        crate::control::check_cancelled()?;
        ensure!(journal.pending.is_none(), "unresolved journal move");
        let from = journal.locations[index];
        if from == to {
            return Ok(());
        }
        let source = self.location(journal, index, from)?;
        let target = self.location(journal, index, to)?;
        self.check_file(&source, &journal.operations[index].expected)?;
        ensure!(
            !exists(&target)?,
            "destination already exists; nothing overwritten: {}",
            target.display()
        );
        journal.pending = Some(Pending { index, from, to });
        self.save(journal)?;
        // Repeat validation after journal fsync, which can take appreciable time.
        self.check_file(&source, &journal.operations[index].expected)?;
        verify_components(&self.root, &target, true)?;
        rename_no_replace(&source, &target)
            .with_context(|| format!("cannot move {} to {}", source.display(), target.display()))?;
        sync_directory(source.parent().context("source has no parent")?)?;
        if source.parent() != target.parent() {
            sync_directory(target.parent().context("destination has no parent")?)?;
        }
        journal.locations[index] = to;
        journal.location_change = Some(LocationChange {
            index,
            location: to,
        });
        journal.pending = None;
        self.save(journal)?;
        Ok(())
    }

    fn resolve_pending(&self, journal: &mut Journal) -> Result<()> {
        let Some(pending) = journal.pending.clone() else {
            return Ok(());
        };
        let source = self.location(journal, pending.index, pending.from)?;
        let target = self.location(journal, pending.index, pending.to)?;
        let expected = &journal.operations[pending.index].expected;
        let source_matches = exists(&source)? && self.check_file(&source, expected).is_ok();
        let target_matches = exists(&target)? && self.check_file(&target, expected).is_ok();
        ensure!(
            !(source_matches && target_matches),
            "ambiguous interrupted move: both {} and {} match; preserved both",
            source.display(),
            target.display()
        );
        if source_matches {
            journal.locations[pending.index] = pending.from;
        } else if target_matches {
            journal.locations[pending.index] = pending.to;
        } else {
            bail!(
                "cannot locate unchanged file for interrupted move from {} to {}; preserved journal for retry",
                source.display(),
                target.display()
            );
        }
        journal.location_change = Some(LocationChange {
            index: pending.index,
            location: journal.locations[pending.index],
        });
        journal.pending = None;
        self.save(journal)
    }

    fn verify_restorable(&self, journal: &Journal) -> Result<()> {
        let mut occupied = HashSet::new();
        for index in 0..journal.operations.len() {
            crate::control::check_cancelled()?;
            let operation = &journal.operations[index];
            check_nested_ownership(&self.root, &self.root.join(&operation.source))?;
            if let Some(target) = &operation.target {
                check_nested_ownership(&self.root, &self.root.join(target))?;
            }
            let path = self.location(journal, index, journal.locations[index])?;
            self.check_file(&path, &operation.expected)?;
            occupied.insert(path);
        }
        for index in 0..journal.operations.len() {
            let original = self.location(journal, index, Location::Original)?;
            ensure!(
                !exists(&original)? || occupied.contains(&original),
                "original path is occupied by another file; nothing overwritten: {}",
                original.display()
            );
        }
        Ok(())
    }

    fn restore(&self, journal: &mut Journal) -> Result<()> {
        crate::control::check_cancelled()?;
        self.resolve_pending(journal)?;
        let directory = self.transaction(&journal.id)?;
        ensure_directory(&directory.join("staging"))?;
        ensure_directory(&directory.join("duplicates"))?;
        // Check every item and all original destinations before moving any file.
        self.verify_restorable(journal)?;
        journal.status = Status::Recovering;
        self.save(journal)?;
        for index in 0..journal.operations.len() {
            self.move_file(journal, index, Location::Staged)?;
        }
        for index in 0..journal.operations.len() {
            self.move_file(journal, index, Location::Original)?;
        }
        journal.status = if journal.intent == Intent::Undo {
            Status::Undone
        } else {
            Status::RolledBack
        };
        self.save(journal)
    }
}

/// Apply the entire plan or preserve a recoverable journal on failure.
pub fn apply(root: &Path, operations: &[Operation]) -> Result<Option<String>> {
    apply_with_progress(root, operations, &|_, _| {})
}

pub fn apply_with_progress(
    root: &Path,
    operations: &[Operation],
    progress: &dyn Fn(usize, usize),
) -> Result<Option<String>> {
    crate::control::check_cancelled()?;
    validate_operations(operations)?;
    let store = Store::open(root)?;
    store.assert_finished()?;
    if operations.is_empty() {
        return Ok(None);
    }
    let sources: HashSet<_> = operations
        .iter()
        .map(|operation| operation.source.as_path())
        .collect();
    let total = operations
        .len()
        .checked_mul(2)
        .context("too many operations")?;
    progress(0, total);
    for operation in operations {
        crate::control::check_cancelled()?;
        let source = store.root.join(&operation.source);
        check_nested_ownership(&store.root, &source)?;
        store.check_file(&source, &operation.expected)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                fs::symlink_metadata(&source)?.dev() == fs::metadata(&store.transactions)?.dev(),
                "file is on another filesystem; transactions require one filesystem: {}",
                source.display()
            );
        }
        if let Some(target) = &operation.target {
            let path = store.root.join(target);
            check_nested_ownership(&store.root, &path)?;
            verify_components(&store.root, &path, true)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    fs::metadata(path.parent().context("destination has no parent")?)?.dev()
                        == fs::metadata(&store.transactions)?.dev(),
                    "destination is on another filesystem: {}",
                    path.display()
                );
            }
            ensure!(
                !exists(&path)? || sources.contains(target.as_path()),
                "destination already exists; nothing overwritten: {}",
                path.display()
            );
        }
    }
    let id = unique_id();
    let directory = store.transactions.join(&id);
    fs::create_dir(&directory)?;
    sync_directory(&store.transactions)?;
    let mut journal = Journal {
        version: 1,
        revision: 0,
        id: id.clone(),
        status: Status::Applying,
        intent: Intent::Apply,
        operations: operations.to_vec(),
        locations: vec![Location::Original; operations.len()],
        pending: None,
        location_change: None,
        witnessed: true,
        witnessed_revision: 0,
    };
    store.save(&mut journal)?;
    let result = (|| -> Result<()> {
        ensure_directory(&directory.join("staging"))?;
        ensure_directory(&directory.join("duplicates"))?;
        for index in 0..operations.len() {
            store.move_file(&mut journal, index, Location::Staged)?;
            progress(index + 1, total);
        }
        for index in 0..operations.len() {
            store.move_file(&mut journal, index, Location::Final)?;
            progress(operations.len() + index + 1, total);
        }
        journal.status = Status::Committed;
        store.save(&mut journal)
    })();
    if let Err(error) = result {
        // Reload from disk: the final journal publication may have succeeded even
        // if the directory fsync reported an error.
        let rollback = crate::control::without_cancellation(|| {
            store.load(&id).and_then(|journal| {
                let mut journal = journal.context("transaction journal disappeared")?;
                store.restore(&mut journal)
            })
        });
        return match rollback {
            Ok(()) => Err(error).context(format!("transaction {id} failed and original names were restored")),
            Err(rollback_error) => Err(error).context(format!(
                "transaction {id} stopped; files and journal are preserved. Run hizuke recover. Recovery detail: {rollback_error:#}")),
        };
    }
    Ok(Some(id))
}

/// Restore the originals from a committed transaction, including quarantined files.
pub fn undo(root: &Path, id: Option<&str>) -> Result<String> {
    crate::control::check_cancelled()?;
    let store = Store::open(root)?;
    store.assert_finished()?;
    let id = select(
        &store,
        id,
        |journal| journal.status == Status::Committed,
        "no committed transaction is available to undo",
    )?;
    let mut journal = store.load(&id)?.context("transaction has no journal")?;
    ensure!(
        journal.status == Status::Committed,
        "transaction {id} is not committed"
    );
    store.verify_restorable(&journal)?;
    journal.intent = Intent::Undo;
    journal.status = Status::Undoing;
    store.save(&mut journal)?;
    store.restore(&mut journal).with_context(|| {
        format!("undo of {id} stopped; files are preserved; run hizuke recover")
    })?;
    Ok(id)
}

/// Restore original names after an interrupted apply or undo. Safe to retry.
pub fn recover(root: &Path, id: Option<&str>) -> Result<String> {
    crate::control::check_cancelled()?;
    let store = Store::open(root)?;
    let id = match id {
        Some(id) => {
            store.transaction(id)?;
            id.to_owned()
        }
        None => {
            let mut selected = None;
            for id in store.ids()?.into_iter().rev() {
                if store
                    .load(&id)?
                    .is_none_or(|journal| journal.status.incomplete())
                {
                    selected = Some(id);
                    break;
                }
            }
            selected.context("no unfinished transaction needs recovery")?
        }
    };
    let Some(mut journal) = store.load(&id)? else {
        // A crash before the first journal publication cannot have moved images.
        let mut journal = Journal {
            version: 1,
            revision: 0,
            id: id.clone(),
            status: Status::RolledBack,
            intent: Intent::Apply,
            operations: Vec::new(),
            locations: Vec::new(),
            pending: None,
            location_change: None,
            witnessed: true,
            witnessed_revision: 0,
        };
        store.save(&mut journal)?;
        return Ok(id);
    };
    ensure!(
        journal.status.incomplete(),
        "transaction {id} is already {}; use undo for a committed transaction",
        journal.status.name()
    );
    store.restore(&mut journal).with_context(|| {
        format!("recovery of {id} stopped; files and journal are preserved for retry")
    })?;
    Ok(id)
}

pub fn history(root: &Path) -> Result<Vec<TransactionSummary>> {
    let Some(store) = Store::open_readonly(root)? else {
        return Ok(Vec::new());
    };
    store
        .ids()?
        .into_iter()
        .map(|id| {
            let journal = store.load(&id)?;
            Ok(TransactionSummary {
                id,
                status: journal
                    .as_ref()
                    .map_or("preparing", |journal| journal.status.name())
                    .to_owned(),
                operations: journal.map_or(0, |journal| journal.operations.len()),
            })
        })
        .collect()
}

/// Read a concrete restoration preview without modifying state or image files.
pub fn inspect(root: &Path, id: &str) -> Result<TransactionDetail> {
    let store = Store::open_readonly(root)?.context("this directory has no transaction history")?;
    let Some(journal) = store.load(id)? else {
        return Ok(TransactionDetail {
            id: id.to_owned(),
            status: "preparing".into(),
            entries: Vec::new(),
        });
    };
    let mut entries = Vec::with_capacity(journal.operations.len());
    for (index, operation) in journal.operations.iter().enumerate() {
        let current = store.location(&journal, index, journal.locations[index])?;
        let alternate = journal
            .pending
            .as_ref()
            .filter(|pending| pending.index == index)
            .map(|pending| store.location(&journal, index, pending.to))
            .transpose()?;
        entries.push(RestoreEntry {
            current: current.strip_prefix(&store.root)?.to_owned(),
            original: operation.source.clone(),
            alternate: alternate
                .map(|path| path.strip_prefix(&store.root).map(Path::to_owned))
                .transpose()?,
            quarantined: operation.target.is_none(),
        });
    }
    Ok(TransactionDetail {
        id: journal.id,
        status: journal.status.name().into(),
        entries,
    })
}

const WITNESS_BYTES: usize = 86; // 20 decimal revision digits, space, SHA256, newline.

fn read_witness(directory: &Path) -> Result<Vec<String>> {
    let path = directory.join("HEAD");
    if !exists(&path)? {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    open_regular(&path, false)?.read_to_end(&mut bytes)?;
    let mut proofs = Vec::new();
    for (index, frame) in bytes.chunks_exact(WITNESS_BYTES).enumerate() {
        crate::control::check_cancelled()?;
        ensure!(
            frame[20] == b' '
                && frame[85] == b'\n'
                && frame[..20].iter().all(u8::is_ascii_digit)
                && frame[21..85].iter().all(u8::is_ascii_hexdigit),
            "invalid transaction HEAD witness"
        );
        let revision: u64 = std::str::from_utf8(&frame[..20])?.parse()?;
        ensure!(
            revision == index as u64 + 1,
            "transaction HEAD witness has a sequence gap"
        );
        proofs.push(std::str::from_utf8(&frame[21..85])?.to_owned());
    }
    // Only an incomplete LAST frame is ignored. save() repairs it while holding
    // the exclusive tree lock, before any later image move is authorized.
    Ok(proofs)
}

fn verify_witness(proofs: &[String], revision: u64, bytes: &[u8]) -> Result<()> {
    if let Some(expected) = proofs.get((revision - 1) as usize) {
        ensure!(
            *expected == format!("{:x}", Sha256::digest(bytes)),
            "transaction journal revision {revision} does not match its durable HEAD digest"
        );
    }
    Ok(())
}

fn append_witnesses(directory: &Path, secured: u64, revision: u64) -> Result<()> {
    ensure!(secured < revision, "invalid HEAD append revision");
    let path = directory.join("HEAD");
    let created = !exists(&path)?;
    let mut file = if !created {
        open_regular(&path, true)?
    } else {
        ensure!(secured == 0, "transaction HEAD witness disappeared");
        open_exclusive(&path)?
    };
    let length = secured
        .checked_mul(WITNESS_BYTES as u64)
        .context("HEAD size overflow")?;
    let actual_length = file.metadata()?.len();
    ensure!(
        actual_length >= length && actual_length < length + WITNESS_BYTES as u64,
        "transaction HEAD witness changed unexpectedly"
    );
    if actual_length != length {
        file.set_len(length)?;
    }
    file.seek(SeekFrom::Start(length))?;
    for next in secured + 1..=revision {
        let mut bytes = Vec::new();
        open_regular(&directory.join(format!("journal-{next:020}.json")), false)?
            .read_to_end(&mut bytes)?;
        writeln!(file, "{next:020} {:x}", Sha256::digest(&bytes))?;
    }
    file.sync_all()?;
    if created {
        sync_directory(directory)?;
    }
    Ok(())
}

struct LocationCounts {
    original: usize,
    finalized: usize,
}

impl LocationCounts {
    fn new(locations: &[Location]) -> Self {
        let mut counts = Self {
            original: 0,
            finalized: 0,
        };
        for location in locations {
            counts.original += usize::from(*location == Location::Original);
            counts.finalized += usize::from(*location == Location::Final);
        }
        counts
    }

    fn moved(&mut self, from: Location, to: Location) {
        self.original = self.original - usize::from(from == Location::Original)
            + usize::from(to == Location::Original);
        self.finalized = self.finalized - usize::from(from == Location::Final)
            + usize::from(to == Location::Final);
    }
}

fn validate_journal_state(journal: &Journal, counts: &LocationCounts) -> Result<()> {
    ensure!(
        journal.locations.len() == journal.operations.len(),
        "invalid journal locations"
    );
    if let Some(pending) = &journal.pending {
        ensure!(
            pending.index < journal.locations.len()
                && journal.locations[pending.index] == pending.from
                && pending.from != pending.to
                && journal.status.incomplete(),
            "invalid pending journal move"
        );
        validate_move(journal, pending, counts)?;
    }
    match journal.status {
        Status::Applying | Status::Committed | Status::RolledBack => {
            ensure!(journal.intent == Intent::Apply, "invalid apply intent")
        }
        Status::Undoing | Status::Undone => {
            ensure!(journal.intent == Intent::Undo, "invalid undo intent")
        }
        Status::Recovering => {}
    }
    match journal.status {
        Status::Committed => ensure!(
            journal.pending.is_none() && counts.finalized == journal.locations.len(),
            "committed journal has unfinished files"
        ),
        Status::Undone | Status::RolledBack => ensure!(
            journal.pending.is_none() && counts.original == journal.locations.len(),
            "restored journal has unfinished files"
        ),
        _ => {}
    }
    Ok(())
}

fn validate_move(journal: &Journal, pending: &Pending, counts: &LocationCounts) -> Result<()> {
    let permitted = match journal.status {
        Status::Applying => match (pending.from, pending.to) {
            (Location::Original, Location::Staged) => counts.finalized == 0,
            (Location::Staged, Location::Final) => counts.original == 0,
            _ => false,
        },
        Status::Undoing | Status::Recovering => match (pending.from, pending.to) {
            (Location::Original | Location::Final, Location::Staged) => true,
            (Location::Staged, Location::Original) => counts.finalized == 0,
            _ => false,
        },
        _ => false,
    };
    ensure!(
        permitted,
        "journal contains an invalid move for its transaction phase"
    );
    Ok(())
}

fn validate_transition(journal: &Journal, event: &Event, counts: &LocationCounts) -> Result<()> {
    ensure!(
        !journal.witnessed || event.witnessed,
        "journal cannot disable its durable HEAD witness"
    );
    if let Some(previous) = &journal.pending {
        let change = event
            .location_change
            .as_ref()
            .context("pending move resolved without a location event")?;
        ensure!(
            event.pending.is_none()
                && event.status == journal.status
                && event.intent == journal.intent
                && change.index == previous.index
                && (change.location == previous.from || change.location == previous.to),
            "invalid resolution of pending journal move"
        );
    } else if let Some(pending) = &event.pending {
        ensure!(
            event.location_change.is_none()
                && event.status == journal.status
                && event.intent == journal.intent
                && pending.index < journal.locations.len()
                && journal.locations[pending.index] == pending.from,
            "invalid pending journal transition"
        );
        validate_move(journal, pending, counts)?;
    } else {
        ensure!(
            event.location_change.is_none(),
            "journal location changed without a prior pending move"
        );
        let allowed = match (journal.status, event.status) {
            (Status::Applying, Status::Committed) => counts.finalized == journal.locations.len(),
            (Status::Committed, Status::Undoing) => {
                journal.intent == Intent::Apply && event.intent == Intent::Undo
            }
            (
                Status::Applying | Status::Committed | Status::Undoing | Status::Recovering,
                Status::Recovering,
            ) => true,
            (Status::Recovering, Status::Undone) => {
                journal.intent == Intent::Undo && counts.original == journal.locations.len()
            }
            (Status::Recovering, Status::RolledBack) => {
                journal.intent == Intent::Apply && counts.original == journal.locations.len()
            }
            _ => false,
        };
        ensure!(allowed, "invalid transaction status transition");
        if !(journal.status == Status::Committed && event.status == Status::Undoing) {
            ensure!(
                event.intent == journal.intent,
                "transaction intent changed unexpectedly"
            );
        }
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("cannot inspect directory {}", root.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "root must be a real directory, not a symlink: {}",
        root.display()
    );
    let root = root.canonicalize()?;
    ensure!(root.to_str().is_some(), "root path must be valid UTF-8");
    ensure!(
        !root.components().any(|component| component
            .as_os_str()
            .to_str()
            .is_some_and(is_state_dir_name)),
        "root cannot enter a reserved state directory ({STATE_DIR} or {LEGACY_STATE_DIR})"
    );
    Ok(root)
}

fn select_state_directory(root: &Path) -> Result<PathBuf> {
    let current = root.join(STATE_DIR);
    let legacy = root.join(LEGACY_STATE_DIR);
    let has_current = exists(&current)?;
    let has_legacy = exists(&legacy)?;
    ensure!(
        !(has_current && has_legacy),
        "both {STATE_DIR} and {LEGACY_STATE_DIR} exist in {}; refusing to merge or modify separate histories",
        root.display()
    );
    Ok(if has_legacy { legacy } else { current })
}

fn has_state_directory(root: &Path) -> Result<bool> {
    Ok(exists(&root.join(STATE_DIR))? || exists(&root.join(LEGACY_STATE_DIR))?)
}

fn check_ancestor_ownership(root: &Path) -> Result<()> {
    // A separately managed child remains independent if its parent is adopted later.
    if has_state_directory(root)? {
        return Ok(());
    }
    for ancestor in root.ancestors().skip(1) {
        ensure!(
            !has_state_directory(ancestor)?,
            "this directory belongs to the collection at {}; run hizuke from that root",
            ancestor.display()
        );
    }
    Ok(())
}

fn check_nested_ownership(root: &Path, path: &Path) -> Result<()> {
    let parent = path.parent().context("image has no parent")?;
    for directory in parent
        .ancestors()
        .take_while(|directory| *directory != root)
    {
        ensure!(directory.starts_with(root), "path escapes collection root");
        ensure!(
            !has_state_directory(directory)?,
            "{} belongs to a separate hizuke collection; process that collection directly",
            path.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
thread_local! { static WINDOWS_TREE_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

struct TreeLock {
    #[cfg(unix)]
    _directories: Vec<File>,
    #[cfg(windows)]
    mutex: *mut std::ffi::c_void,
}

fn lock_tree(root: &Path, readonly: bool) -> Result<TreeLock> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let ancestors: Vec<_> = root.ancestors().collect();
        let mut directories = Vec::with_capacity(ancestors.len());
        for directory in ancestors.into_iter().rev() {
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(directory)?;
            let result = if readonly || directory != root {
                file.try_lock_shared()
            } else {
                file.try_lock()
            };
            result.with_context(|| {
                format!(
                    "another hizuke process is using an overlapping directory tree near {}",
                    directory.display()
                )
            })?;
            directories.push(file);
        }
        Ok(TreeLock {
            _directories: directories,
        })
    }
    #[cfg(windows)]
    {
        let _ = (root, readonly);
        ensure!(
            !WINDOWS_TREE_HELD.get(),
            "this thread already holds a hizuke directory lock"
        );
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateMutexW(
                attributes: *const std::ffi::c_void,
                owner: i32,
                name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // A global named mutex conservatively serializes instances across roots
        // and sessions on Windows, where std cannot lock directory handles.
        let name: Vec<u16> = LEGACY_TREE_MUTEX.encode_utf16().chain(Some(0)).collect();
        // SAFETY: the null security descriptor and terminated name are valid.
        let mutex = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        ensure!(
            !mutex.is_null(),
            "cannot create directory coordination mutex: {}",
            std::io::Error::last_os_error()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            // SAFETY: CreateMutexW returned a live handle. Bounded waits allow
            // unrelated Windows instances and parallel tests to serialize.
            let result = unsafe { WaitForSingleObject(mutex, 100) };
            if result == 0 || result == 0x80 {
                WINDOWS_TREE_HELD.set(true);
                return Ok(TreeLock { mutex });
            }
            if result != 258
                || std::time::Instant::now() >= deadline
                || crate::control::is_cancelled()
            {
                // SAFETY: this branch does not own the mutex, but owns its handle.
                unsafe {
                    CloseHandle(mutex);
                }
                crate::control::check_cancelled()?;
                bail!("another hizuke process is active, or directory coordination is unavailable");
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, readonly);
        bail!("directory tree locking is unsupported on this operating system")
    }
}

#[cfg(windows)]
impl Drop for TreeLock {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ReleaseMutex(handle: *mut std::ffi::c_void) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        // SAFETY: this guard exclusively owns a successfully acquired mutex.
        unsafe {
            ReleaseMutex(self.mutex);
            CloseHandle(self.mutex);
        }
        WINDOWS_TREE_HELD.set(false);
    }
}

fn select(
    store: &Store,
    id: Option<&str>,
    predicate: impl Fn(&Journal) -> bool,
    empty: &str,
) -> Result<String> {
    if let Some(id) = id {
        store.transaction(id)?;
        return Ok(id.to_owned());
    }
    for id in store.ids()?.into_iter().rev() {
        if store.load(&id)?.is_some_and(|journal| predicate(&journal)) {
            return Ok(id);
        }
    }
    bail!("{empty}")
}

fn validate_operations(operations: &[Operation]) -> Result<()> {
    let mut sources = HashSet::new();
    let mut targets = HashSet::new();
    for operation in operations {
        validate_relative(&operation.source)?;
        ensure!(
            sources.insert(&operation.source),
            "source appears twice in plan: {}",
            operation.source.display()
        );
        if let Some(target) = &operation.target {
            validate_relative(target)?;
            ensure!(
                targets.insert(target),
                "destination appears twice in plan: {}",
                target.display()
            );
        }
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<()> {
    let text = path.to_str().context("image paths must be valid UTF-8")?;
    ensure!(
        !text.is_empty() && !path.is_absolute(),
        "expected a non-empty relative image path"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "image paths cannot contain '.', '..', or absolute prefixes: {}",
        path.display()
    );
    let normalized: PathBuf = path.components().collect();
    ensure!(
        normalized.as_os_str() == path.as_os_str(),
        "image path is not normalized: {}",
        path.display()
    );
    ensure!(
        !path.components().any(|component| component
            .as_os_str()
            .to_str()
            .is_some_and(is_state_dir_name)),
        "image path cannot enter a reserved state directory ({STATE_DIR} or {LEGACY_STATE_DIR})"
    );
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid transaction identifier"
    );
    Ok(())
}

fn snapshot_revision(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("journal-")?.strip_suffix(".json")?;
    (digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn unique_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{:020}-{:09}-{}-{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

fn verify_components(root: &Path, path: &Path, allow_missing_leaf: bool) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .context("path escapes image directory")?;
    let components: Vec<_> = relative.components().collect();
    let mut current = root.to_owned();
    let root_metadata = fs::symlink_metadata(root)?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "image root changed or became a symlink"
    );
    for (index, component) in components.iter().enumerate() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe path component"
        );
        current.push(component.as_os_str());
        let is_leaf = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "refusing symlink: {}",
                    current.display()
                );
                if !is_leaf {
                    ensure!(
                        metadata.is_dir(),
                        "parent is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error)
                if allow_missing_leaf
                    && is_leaf
                    && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", current.display()));
            }
        }
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {
            sync_directory(path.parent().context("directory has no parent")?)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).with_context(|| format!("cannot create {}", path.display()));
        }
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "expected a real directory: {}",
        path.display()
    );
    Ok(())
}

fn open_exclusive(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options.open(path)
}

fn open_regular(path: &Path, writable: bool) -> Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "expected regular state file: {}",
        path.display()
    );
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    ensure!(opened.is_file(), "state entry is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            !writable || opened.nlink() == 1,
            "refusing to write a hard-linked state file: {}",
            path.display()
        );
    }
    Ok(file)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut options = OpenOptions::new();
        use std::os::unix::fs::OpenOptionsExt;
        options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options
            .open(path)?
            .sync_all()
            .with_context(|| format!("cannot sync directory {}", path.display()))?;
    }
    // Windows does not expose portable directory fsync through std. File journal
    // snapshots are flushed, but power-loss durability is filesystem dependent.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn rename_no_replace(source: &Path, target: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        // SAFETY: both C strings are valid and live for the duration of the call.
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("exclusive rename failed (no fallback overwrite is permitted)");
        }
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let mut source: Vec<u16> = source.as_os_str().encode_wide().collect();
        let mut target: Vec<u16> = target.as_os_str().encode_wide().collect();
        ensure!(
            !source.contains(&0) && !target.contains(&0),
            "NUL in file path"
        );
        source.push(0);
        target.push(0);
        // No MOVEFILE_REPLACE_EXISTING: an occupied destination must fail.
        // SAFETY: both UTF-16 strings are terminated and live through the call.
        let result = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) };
        if result == 0 {
            return Err(std::io::Error::last_os_error()).context("exclusive rename failed");
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (source, target);
        bail!("atomic no-replace rename is unsupported on this operating system")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn put(root: &Path, name: &str, contents: &[u8]) {
        fs::write(root.join(name), contents).unwrap();
    }
    fn operation(root: &Path, source: &str, target: Option<&str>) -> Operation {
        Operation {
            source: source.into(),
            target: target.map(Into::into),
            expected: fingerprint(&root.join(source)).unwrap(),
        }
    }
    fn bytes(root: &Path, name: &str) -> Vec<u8> {
        fs::read(root.join(name)).unwrap()
    }

    #[test]
    fn refuses_overwrite_and_preserves_both_files() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"first");
        put(root.path(), "b.jpg", b"unrelated");
        assert!(
            apply(
                root.path(),
                &[operation(root.path(), "a.jpg", Some("b.jpg"))]
            )
            .is_err()
        );
        assert_eq!(bytes(root.path(), "a.jpg"), b"first");
        assert_eq!(bytes(root.path(), "b.jpg"), b"unrelated");
    }

    #[test]
    fn exclusive_rename_never_replaces_even_identical_contents() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a", b"same");
        put(root.path(), "b", b"same");
        assert!(rename_no_replace(&root.path().join("a"), &root.path().join("b")).is_err());
        assert_eq!(bytes(root.path(), "a"), b"same");
        assert_eq!(bytes(root.path(), "b"), b"same");
    }

    #[test]
    fn cycle_is_transactional_and_undoable() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"one");
        put(root.path(), "b.jpg", b"two");
        let operations = [
            operation(root.path(), "a.jpg", Some("b.jpg")),
            operation(root.path(), "b.jpg", Some("a.jpg")),
        ];
        let id = apply(root.path(), &operations).unwrap().unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"two");
        assert_eq!(bytes(root.path(), "b.jpg"), b"one");
        assert_eq!(undo(root.path(), None).unwrap(), id);
        assert_eq!(bytes(root.path(), "a.jpg"), b"one");
        assert_eq!(bytes(root.path(), "b.jpg"), b"two");
        assert_eq!(history(root.path()).unwrap()[0].status, "undone");
    }

    #[test]
    fn quarantined_duplicates_are_restored_by_undo() {
        let root = TempDir::new().unwrap();
        put(root.path(), "one.jpg", b"same");
        put(root.path(), "two.jpg", b"same");
        let operations = [
            operation(root.path(), "one.jpg", Some("renamed.jpg")),
            operation(root.path(), "two.jpg", None),
        ];
        let id = apply(root.path(), &operations).unwrap().unwrap();
        assert!(!root.path().join("two.jpg").exists());
        assert_eq!(
            bytes(
                root.path(),
                &format!("{STATE_DIR}/transactions/{id}/duplicates/1")
            ),
            b"same"
        );
        undo(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "one.jpg"), b"same");
        assert_eq!(bytes(root.path(), "two.jpg"), b"same");
    }

    #[test]
    fn changed_files_block_apply_and_undo() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"before");
        let operation = operation(root.path(), "a.jpg", Some("b.jpg"));
        put(root.path(), "a.jpg", b"after");
        assert!(apply(root.path(), &[operation]).is_err());
        let id = apply(
            root.path(),
            &[self::operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        put(root.path(), "b.jpg", b"edited");
        assert!(undo(root.path(), Some(&id)).is_err());
        assert_eq!(bytes(root.path(), "b.jpg"), b"edited");
        assert_eq!(history(root.path()).unwrap()[0].status, "committed");
    }

    fn interrupted(root: &Path, did_move: bool) -> String {
        let store = Store::open(root).unwrap();
        let id = unique_id();
        let directory = store.transactions.join(&id);
        fs::create_dir(&directory).unwrap();
        ensure_directory(&directory.join("staging")).unwrap();
        ensure_directory(&directory.join("duplicates")).unwrap();
        let operation = operation(root, "a.jpg", Some("b.jpg"));
        let mut journal = Journal {
            version: 1,
            revision: 0,
            id: id.clone(),
            status: Status::Applying,
            intent: Intent::Apply,
            operations: vec![operation],
            locations: vec![Location::Original],
            pending: Some(Pending {
                index: 0,
                from: Location::Original,
                to: Location::Staged,
            }),
            location_change: None,
            witnessed: true,
            witnessed_revision: 0,
        };
        store.save(&mut journal).unwrap();
        if did_move {
            rename_no_replace(&root.join("a.jpg"), &directory.join("staging/0")).unwrap();
        }
        id
    }

    #[test]
    fn recovery_resolves_pending_moves_before_and_after_rename() {
        for did_move in [false, true] {
            let root = TempDir::new().unwrap();
            put(root.path(), "a.jpg", b"original");
            let id = interrupted(root.path(), did_move);
            let current = if did_move {
                root.path()
                    .join(format!("{STATE_DIR}/transactions/{id}/staging/0"))
            } else {
                root.path().join("a.jpg")
            };
            assert!(
                apply(
                    root.path(),
                    &[Operation {
                        source: "other.jpg".into(),
                        target: None,
                        expected: fingerprint(&current).unwrap()
                    }]
                )
                .is_err()
            );
            assert_eq!(recover(root.path(), None).unwrap(), id);
            assert_eq!(bytes(root.path(), "a.jpg"), b"original");
            assert_eq!(history(root.path()).unwrap()[0].status, "rolled_back");
        }
    }

    #[test]
    fn recovery_conflicts_preserve_every_file_and_allow_retry() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = interrupted(root.path(), true);
        put(root.path(), "a.jpg", b"new external file");
        assert!(recover(root.path(), Some(&id)).is_err());
        assert_eq!(bytes(root.path(), "a.jpg"), b"new external file");
        assert_eq!(
            bytes(
                root.path(),
                &format!("{STATE_DIR}/transactions/{id}/staging/0")
            ),
            b"original"
        );
        fs::rename(root.path().join("a.jpg"), root.path().join("external.jpg")).unwrap();
        recover(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert_eq!(bytes(root.path(), "external.jpg"), b"new external file");
    }

    #[test]
    fn unfinished_undo_restores_originals() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        {
            let store = Store::open(root.path()).unwrap();
            let mut journal = store.load(&id).unwrap().unwrap();
            journal.status = Status::Undoing;
            journal.intent = Intent::Undo;
            store.save(&mut journal).unwrap();
            store.move_file(&mut journal, 0, Location::Staged).unwrap();
        }
        recover(root.path(), None).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert_eq!(history(root.path()).unwrap()[0].status, "undone");
    }

    #[test]
    fn pending_final_move_replays_events_and_recovers_a_cycle() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"one");
        put(root.path(), "b.jpg", b"two");
        let operations = vec![
            operation(root.path(), "a.jpg", Some("b.jpg")),
            operation(root.path(), "b.jpg", Some("a.jpg")),
        ];
        let id;
        {
            let store = Store::open(root.path()).unwrap();
            id = unique_id();
            let directory = store.transactions.join(&id);
            fs::create_dir(&directory).unwrap();
            ensure_directory(&directory.join("staging")).unwrap();
            ensure_directory(&directory.join("duplicates")).unwrap();
            let mut journal = Journal {
                version: 1,
                revision: 0,
                id: id.clone(),
                status: Status::Applying,
                intent: Intent::Apply,
                operations,
                locations: vec![Location::Original; 2],
                pending: None,
                location_change: None,
                witnessed: true,
                witnessed_revision: 0,
            };
            store.save(&mut journal).unwrap();
            store.move_file(&mut journal, 0, Location::Staged).unwrap();
            store.move_file(&mut journal, 1, Location::Staged).unwrap();
            journal.pending = Some(Pending {
                index: 0,
                from: Location::Staged,
                to: Location::Final,
            });
            store.save(&mut journal).unwrap();
            rename_no_replace(&directory.join("staging/0"), &root.path().join("b.jpg")).unwrap();
        }
        recover(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"one");
        assert_eq!(bytes(root.path(), "b.jpg"), b"two");
    }

    #[test]
    fn missing_journal_event_is_an_error_without_touching_images() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        let event = root.path().join(format!(
            "{STATE_DIR}/transactions/{id}/journal-00000000000000000002.json"
        ));
        fs::rename(event, root.path().join("saved-event.json")).unwrap();
        assert!(undo(root.path(), Some(&id)).is_err());
        assert_eq!(bytes(root.path(), "b.jpg"), b"original");
        assert!(!root.path().join("a.jpg").exists());
    }

    #[test]
    fn history_is_read_only_even_for_empty_or_incomplete_state() {
        let root = TempDir::new().unwrap();
        assert!(history(root.path()).unwrap().is_empty());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        let state = root.path().join(STATE_DIR);
        fs::create_dir(&state).unwrap();
        assert!(history(root.path()).unwrap().is_empty());
        assert_eq!(fs::read_dir(&state).unwrap().count(), 0);
        fs::create_dir(state.join("transactions")).unwrap();
        fs::create_dir(state.join("transactions/preparing-test")).unwrap();
        let records = history(root.path()).unwrap();
        assert_eq!(records[0].status, "preparing");
        assert!(
            inspect(root.path(), "preparing-test")
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!state.join("lock").exists());
        assert_eq!(
            fs::read_dir(state.join("transactions/preparing-test"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn history_validates_roots_even_without_state() {
        let root = TempDir::new().unwrap();
        assert!(history(&root.path().join("absent")).is_err());
        put(root.path(), "file", b"not a directory");
        assert!(history(&root.path().join("file")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), root.path().join("link")).unwrap();
            assert!(history(&root.path().join("link")).is_err());
        }
    }

    #[test]
    fn inspect_exposes_restoration_paths_and_pending_alternative_without_writes() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = interrupted(root.path(), true);
        let directory = root.path().join(format!("{STATE_DIR}/transactions/{id}"));
        let count = fs::read_dir(&directory).unwrap().count();
        let detail = inspect(root.path(), &id).unwrap();
        assert_eq!(detail.entries[0].original, Path::new("a.jpg"));
        assert_eq!(detail.entries[0].current, Path::new("a.jpg"));
        assert_eq!(
            detail.entries[0].alternate.as_deref(),
            Some(Path::new(&format!(
                "{STATE_DIR}/transactions/{id}/staging/0"
            )))
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), count);
    }

    #[test]
    fn directory_locks_coordinate_overlapping_roots_in_both_directions() {
        let parent = TempDir::new().unwrap();
        let child = parent.path().join("child");
        fs::create_dir(&child).unwrap();
        {
            let _parent_guard = Store::open(parent.path()).unwrap();
            assert!(Store::open(&child).is_err());
        }
        let parent = TempDir::new().unwrap();
        let child = parent.path().join("child");
        fs::create_dir(&child).unwrap();
        {
            let _child_guard = Store::open(&child).unwrap();
            assert!(Store::open(parent.path()).is_err());
        }
        drop(Store::open(parent.path()).unwrap());
        // Existing independent child collections stay usable if a parent is adopted.
        drop(Store::open(&child).unwrap());
    }

    #[test]
    fn nested_collection_boundaries_prevent_stealing_images() {
        let parent = TempDir::new().unwrap();
        let child = parent.path().join("child");
        fs::create_dir(&child).unwrap();
        put(&child, "a.jpg", b"original");
        drop(Store::open(&child).unwrap());
        let operation = operation(parent.path(), "child/a.jpg", Some("child/b.jpg"));
        assert!(apply(parent.path(), &[operation]).is_err());
        assert_eq!(bytes(&child, "a.jpg"), b"original");
        assert!(apply(&child, &[self::operation(&child, "a.jpg", Some("b.jpg"))]).is_ok());
        let new_child = parent.path().join("new-child");
        fs::create_dir(&new_child).unwrap();
        assert!(Store::open(&new_child).is_err());
        assert!(!new_child.join(STATE_DIR).exists());
    }

    #[test]
    fn durable_witness_detects_removal_of_the_last_event() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        let directory = root.path().join(format!("{STATE_DIR}/transactions/{id}"));
        let latest = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| snapshot_revision(path.file_name().unwrap().to_str().unwrap()).is_some())
            .max()
            .unwrap();
        fs::rename(latest, root.path().join("removed-event.json")).unwrap();
        assert!(
            history(root.path())
                .unwrap_err()
                .to_string()
                .contains("tail")
        );
        assert!(undo(root.path(), Some(&id)).is_err());
        assert_eq!(bytes(root.path(), "b.jpg"), b"original");
    }

    fn remove_witness_for_legacy_fixture(directory: &Path) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if snapshot_revision(path.file_name().unwrap().to_str().unwrap()).is_some() {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value.as_object_mut().unwrap().remove("witnessed");
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        fs::remove_file(directory.join("HEAD")).unwrap();
    }

    #[test]
    fn legacy_v01_journals_are_readable_and_upgrade_when_undone() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        let directory = root.path().join(format!("{STATE_DIR}/transactions/{id}"));
        remove_witness_for_legacy_fixture(&directory);
        assert_eq!(history(root.path()).unwrap()[0].status, "committed");
        undo(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert!(directory.join("HEAD").exists());
        assert_eq!(history(root.path()).unwrap()[0].status, "undone");
    }

    #[test]
    fn invalid_legacy_state_transition_is_rejected_without_moving_files() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        let directory = root.path().join(format!("{STATE_DIR}/transactions/{id}"));
        remove_witness_for_legacy_fixture(&directory);
        let event = directory.join("journal-00000000000000000002.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&event).unwrap()).unwrap();
        value["pending"]["to"] = serde_json::json!("final");
        fs::write(event, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(history(root.path()).is_err());
        assert!(undo(root.path(), Some(&id)).is_err());
        assert_eq!(bytes(root.path(), "b.jpg"), b"original");
    }

    #[test]
    fn torn_last_witness_frame_can_be_repaired_without_losing_images() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap()
        .unwrap();
        let head = root
            .path()
            .join(format!("{STATE_DIR}/transactions/{id}/HEAD"));
        let file = OpenOptions::new().write(true).open(&head).unwrap();
        file.set_len(file.metadata().unwrap().len() - 30).unwrap();
        drop(file);
        assert_eq!(history(root.path()).unwrap()[0].status, "committed");
        undo(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert_eq!(fs::metadata(head).unwrap().len() % WITNESS_BYTES as u64, 0);
    }

    #[test]
    fn absent_journal_with_archived_files_is_never_treated_as_an_empty_transaction() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = interrupted(root.path(), true);
        let directory = root.path().join(format!("{STATE_DIR}/transactions/{id}"));
        fs::rename(directory.join("HEAD"), root.path().join("saved-HEAD")).unwrap();
        fs::rename(
            directory.join("journal-00000000000000000001.json"),
            root.path().join("saved-journal.json"),
        )
        .unwrap();
        assert!(history(root.path()).is_err());
        assert!(recover(root.path(), Some(&id)).is_err());
        assert_eq!(fs::read(directory.join("staging/0")).unwrap(), b"original");
        assert!(!directory.join("journal-00000000000000000001.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mutable_state_files_cannot_alias_other_files_through_hard_links() {
        let root = TempDir::new().unwrap();
        put(root.path(), "state", b"preserve");
        fs::hard_link(root.path().join("state"), root.path().join("photo.jpg")).unwrap();
        assert!(open_regular(&root.path().join("state"), true).is_err());
        assert!(open_regular(&root.path().join("state"), false).is_ok());
        assert_eq!(bytes(root.path(), "photo.jpg"), b"preserve");
    }

    #[test]
    fn existing_legacy_history_and_quarantine_are_restored_in_place() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"same");
        put(root.path(), "copy.jpg", b"same");
        let operations = [
            operation(root.path(), "a.jpg", Some("renamed.jpg")),
            operation(root.path(), "copy.jpg", None),
        ];
        let id = apply(root.path(), &operations).unwrap().unwrap();
        fs::rename(
            root.path().join(STATE_DIR),
            root.path().join(LEGACY_STATE_DIR),
        )
        .unwrap();
        assert_eq!(history(root.path()).unwrap()[0].status, "committed");
        let detail = inspect(root.path(), &id).unwrap();
        assert!(detail.entries[1].current.starts_with(LEGACY_STATE_DIR));
        undo(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"same");
        assert_eq!(bytes(root.path(), "copy.jpg"), b"same");
        assert!(!root.path().join(STATE_DIR).exists());
        assert_eq!(history(root.path()).unwrap()[0].status, "undone");
    }

    #[test]
    fn interrupted_legacy_transaction_recovers_without_migrating_state() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let id = interrupted(root.path(), true);
        fs::rename(
            root.path().join(STATE_DIR),
            root.path().join(LEGACY_STATE_DIR),
        )
        .unwrap();
        recover(root.path(), Some(&id)).unwrap();
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert_eq!(history(root.path()).unwrap()[0].status, "rolled_back");
        assert!(!root.path().join(STATE_DIR).exists());
    }

    #[test]
    fn new_transactions_reuse_the_sole_legacy_state_directory() {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join(LEGACY_STATE_DIR)).unwrap();
        assert!(history(root.path()).unwrap().is_empty());
        assert_eq!(
            fs::read_dir(root.path().join(LEGACY_STATE_DIR))
                .unwrap()
                .count(),
            0
        );
        put(root.path(), "a.jpg", b"original");
        apply(
            root.path(),
            &[operation(root.path(), "a.jpg", Some("b.jpg"))],
        )
        .unwrap();
        assert!(!root.path().join(STATE_DIR).exists());
        assert!(
            root.path()
                .join(LEGACY_STATE_DIR)
                .join("transactions")
                .is_dir()
        );
        assert_eq!(bytes(root.path(), "b.jpg"), b"original");
    }

    #[test]
    fn dual_state_directories_are_rejected_without_writing_either_history() {
        let root = TempDir::new().unwrap();
        for directory in [STATE_DIR, LEGACY_STATE_DIR] {
            fs::create_dir(root.path().join(directory)).unwrap();
            put(
                &root.path().join(directory),
                "sentinel",
                directory.as_bytes(),
            );
        }
        put(root.path(), "a.jpg", b"original");
        let operation = operation(root.path(), "a.jpg", Some("b.jpg"));
        assert!(
            history(root.path())
                .unwrap_err()
                .to_string()
                .contains("both")
        );
        assert!(inspect(root.path(), "example").is_err());
        assert!(apply(root.path(), &[operation]).is_err());
        assert!(undo(root.path(), None).is_err());
        assert!(recover(root.path(), None).is_err());
        for directory in [STATE_DIR, LEGACY_STATE_DIR] {
            let path = root.path().join(directory);
            assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
            assert_eq!(bytes(&path, "sentinel"), directory.as_bytes());
        }
        assert_eq!(bytes(root.path(), "a.jpg"), b"original");
        assert!(!root.path().join("b.jpg").exists());
    }

    #[test]
    fn legacy_state_boundaries_remain_reserved_and_independent() {
        assert!(is_state_dir_name(".HIZUKE"));
        assert!(is_state_dir_name(".IMGrENAME"));
        assert!(!is_state_dir_name("photos"));
        let root = TempDir::new().unwrap();
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        fs::create_dir(child.join(LEGACY_STATE_DIR)).unwrap();
        put(&child, "a.jpg", b"original");
        assert!(
            apply(
                root.path(),
                &[operation(root.path(), "child/a.jpg", Some("child/b.jpg"))]
            )
            .is_err()
        );
        assert!(apply(&child, &[operation(&child, "a.jpg", Some("b.jpg"))]).is_ok());
        let fresh = TempDir::new().unwrap();
        fs::create_dir(fresh.path().join(LEGACY_STATE_DIR)).unwrap();
        let subroot = fresh.path().join("new-child");
        fs::create_dir(&subroot).unwrap();
        assert!(Store::open(&subroot).is_err());
        assert!(!subroot.join(STATE_DIR).exists());
        assert!(canonical_root(&fresh.path().join(LEGACY_STATE_DIR)).is_err());
    }

    #[test]
    fn apply_reports_every_durable_move_in_order() {
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"one");
        put(root.path(), "b.jpg", b"two");
        let operations = [
            operation(root.path(), "a.jpg", Some("c.jpg")),
            operation(root.path(), "b.jpg", None),
        ];
        let events = std::cell::RefCell::new(Vec::new());
        apply_with_progress(root.path(), &operations, &|done, total| {
            events.borrow_mut().push((done, total))
        })
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec![(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]
        );
    }

    #[test]
    fn advisory_lock_blocks_other_mutators() {
        let root = TempDir::new().unwrap();
        let _first = Store::open(root.path()).unwrap();
        assert!(Store::open(root.path()).is_err());
    }

    #[test]
    fn rejects_escaping_or_unnormalized_paths() {
        for path in [
            "../photo.jpg",
            "/photo.jpg",
            "./photo.jpg",
            "sub/../photo.jpg",
            "sub//photo.jpg",
            ".hizuke/photo.jpg",
            ".imgrename/photo.jpg",
            "sub/.IMGrENAME/photo.jpg",
            "photo.jpg/",
        ] {
            assert!(
                validate_relative(Path::new(path)).is_err(),
                "accepted {path}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_sources_parents_and_state() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        put(root.path(), "a.jpg", b"original");
        let mut op = operation(root.path(), "a.jpg", Some("b.jpg"));
        symlink("a.jpg", root.path().join("link.jpg")).unwrap();
        op.source = "link.jpg".into();
        assert!(apply(root.path(), &[op]).is_err());
        let external = TempDir::new().unwrap();
        symlink(external.path(), root.path().join("parent")).unwrap();
        assert!(
            apply(
                root.path(),
                &[operation(root.path(), "a.jpg", Some("parent/b.jpg"))]
            )
            .is_err()
        );
        let fresh = TempDir::new().unwrap();
        put(fresh.path(), "a.jpg", b"original");
        symlink(external.path(), fresh.path().join(STATE_DIR)).unwrap();
        assert!(
            apply(
                fresh.path(),
                &[operation(fresh.path(), "a.jpg", Some("b.jpg"))]
            )
            .is_err()
        );
        assert_eq!(bytes(fresh.path(), "a.jpg"), b"original");
    }
}
