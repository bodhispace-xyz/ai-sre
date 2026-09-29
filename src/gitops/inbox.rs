//! Loads a bounded snapshot of protected deployment receipts without granting repair authority.
//!
//! The trusted publisher retains immutable final JSON files and publishes by atomic rename.
//! Temporary files are never receipts. Every final file is read on every scan so revocations
//! and contradictions cannot be hidden behind an in-memory filename cursor.

use super::receipt::{ProtectedReceipt, ReceiptError};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

/// Deployment-owned resource bounds for one inbox scan.
#[derive(Clone, Copy)]
pub struct InboxLimits {
    /// Maximum directory entries, including temporary files.
    pub entries: usize,
    /// Maximum total encoded bytes across final receipt files.
    pub bytes: u64,
}

impl Default for InboxLimits {
    fn default() -> Self {
        Self {
            entries: 256,
            bytes: 16 * 1024 * 1024,
        }
    }
}

/// A fixed local inbox selected by deployment configuration, never by a model.
#[derive(Clone)]
pub struct ReceiptInbox {
    path: PathBuf,
    limits: InboxLimits,
}

impl ReceiptInbox {
    /// Validates bounded configuration. Filesystem provenance is rechecked on each read.
    pub fn new(path: PathBuf, limits: InboxLimits) -> Result<Self, ReceiptError> {
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || limits.entries == 0
            || limits.entries > 4096
            || limits.bytes == 0
            || limits.bytes > 64 * 1024 * 1024
        {
            return Err(ReceiptError::Unprotected);
        }
        Ok(Self { path, limits })
    }

    /// Reads all final receipts or rejects the scan; a prefix is never a successful snapshot.
    /// Requires Linux root-owned directories without group/other writes, including ancestors.
    pub fn read(&self) -> Result<Vec<ProtectedReceipt>, ReceiptError> {
        if !cfg!(target_os = "linux") {
            return Err(ReceiptError::Unprotected);
        }
        scan(
            &self.path,
            self.limits,
            protected_directory,
            ProtectedReceipt::read,
        )
    }
}

// Retry only a changed snapshot. Stable malformed or unprotected input fails immediately.
// Each attempt drops its entire result before retrying; a partial set never reaches the journal.
fn scan(
    path: &Path,
    limits: InboxLimits,
    directory: impl Fn(&Path) -> Result<fs::Metadata, ReceiptError>,
    mut read: impl FnMut(&Path) -> Result<ProtectedReceipt, ReceiptError>,
) -> Result<Vec<ProtectedReceipt>, ReceiptError> {
    for _ in 0..3 {
        let before = directory(path)?;
        let result = final_paths(path, limits).and_then(|paths| {
            paths
                .iter()
                .map(|path| read(path))
                .collect::<Result<Vec<_>, _>>()
        });
        let after = directory(path)?;
        if identity(&before) != identity(&after) {
            continue;
        }
        return result;
    }
    Err(ReceiptError::Unprotected)
}

fn identity(metadata: &fs::Metadata) -> (u64, u64, i64, i64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn protected_directory(path: &Path) -> Result<fs::Metadata, ReceiptError> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|_| ReceiptError::Unprotected)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(ReceiptError::Unprotected);
        }
    }
    fs::symlink_metadata(path).map_err(|_| ReceiptError::Unprotected)
}

// Kept separate from provenance checks so selection and resource bounds can be tested without root.
fn final_paths(path: &Path, limits: InboxLimits) -> Result<Vec<PathBuf>, ReceiptError> {
    let mut paths = Vec::new();
    let mut bytes = 0u64;
    for (index, entry) in fs::read_dir(path)
        .map_err(|_| ReceiptError::Unprotected)?
        .enumerate()
    {
        if index >= limits.entries {
            return Err(ReceiptError::Unprotected);
        }
        let entry = entry.map_err(|_| ReceiptError::Unprotected)?;
        let name = entry.file_name();
        let name = name.to_str().ok_or(ReceiptError::Unprotected)?;
        // Matches the producer's private publication prefix; never parse partially written bytes.
        if name.starts_with(".receipt-") {
            continue;
        }
        let stem = name
            .strip_suffix(".json")
            .ok_or(ReceiptError::Unprotected)?;
        if stem.is_empty()
            || stem.len() > 192
            || !stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(ReceiptError::Unprotected);
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|_| ReceiptError::Unprotected)?;
        bytes = bytes
            .checked_add(metadata.len())
            .ok_or(ReceiptError::Unprotected)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 1024 * 1024
            || bytes > limits.bytes
        {
            return Err(ReceiptError::Unprotected);
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::symlink,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ai-sre-inbox-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn stable_invalid_receipt_discards_previously_read_evidence() {
        // Given a stable inbox with a valid receipt followed by an invalid receipt.
        let root = directory();
        fs::write(root.join("a.json"), "{}").unwrap();
        fs::write(root.join("b.json"), "{}").unwrap();
        let mut reads = 0;
        // When the complete scan fails after reading its valid prefix.
        let result = scan(
            &root,
            InboxLimits::default(),
            |path| fs::symlink_metadata(path).map_err(|_| ReceiptError::Unprotected),
            |_| {
                reads += 1;
                if reads == 1 {
                    Ok(crate::gitops::receipt::fixture())
                } else {
                    Err(ReceiptError::InvalidFields)
                }
            },
        );
        // Then no partial evidence is returned, and stable invalid input is not retried.
        assert!(matches!(result, Err(ReceiptError::InvalidFields)));
        assert_eq!(reads, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_publication_retries_the_whole_snapshot_with_a_fixed_limit() {
        // Given one final file and a producer publishing a second file during the first read.
        let root = directory();
        fs::write(root.join("a.json"), "{}").unwrap();
        let metadata =
            |path: &Path| fs::symlink_metadata(path).map_err(|_| ReceiptError::Unprotected);
        let mut reads = 0;
        let result = scan(&root, InboxLimits::default(), metadata, |_| {
            reads += 1;
            if reads == 1 {
                let publish_root = root.clone();
                std::thread::spawn(move || {
                    fs::write(publish_root.join(".receipt-new"), "{}").unwrap();
                    fs::rename(
                        publish_root.join(".receipt-new"),
                        publish_root.join("b.json"),
                    )
                    .unwrap();
                })
                .join()
                .unwrap();
            }
            Ok(crate::gitops::receipt::fixture())
        })
        .unwrap();
        // Then the first partial set is discarded and both final files appear in the stable retry.
        assert_eq!(reads, 3);
        assert_eq!(result.len(), 2);
        fs::remove_file(root.join("b.json")).unwrap();
        // When the producer keeps changing the directory, retries stop after three attempts.
        let mut attempts = 0;
        assert!(
            scan(&root, InboxLimits::default(), metadata, |_| {
                attempts += 1;
                fs::write(root.join(format!(".receipt-{attempts}")), "{}").unwrap();
                Ok(crate::gitops::receipt::fixture())
            })
            .is_err()
        );
        assert_eq!(attempts, 3);
        // Then stable malformed evidence is not retried as if it were a publication race.
        let mut failures = 0;
        assert!(
            scan(&root, InboxLimits::default(), metadata, |_| {
                failures += 1;
                Err(ReceiptError::InvalidFields)
            })
            .is_err()
        );
        assert_eq!(failures, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires root in a disposable Linux VM"]
    fn protected_linux_scan_ingests_revocation_and_rejects_unsafe_entries() {
        use crate::reasoning::storage::JournalStore;
        use std::os::unix::fs::PermissionsExt;
        assert!(rustix::process::geteuid().is_root());
        // Given a root-owned inbox with an atomically published complete receipt.
        let root = PathBuf::from(format!("/root/ai-sre-inbox-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let inbox = ReceiptInbox::new(root.clone(), InboxLimits::default()).unwrap();
        let mut receipt = crate::gitops::receipt::fixture();
        let id = receipt.deployment_id().to_owned();
        fs::write(
            root.join(".receipt-pending"),
            serde_json::to_vec(&receipt.wire).unwrap(),
        )
        .unwrap();
        assert!(inbox.read().unwrap().is_empty());
        fs::rename(root.join(".receipt-pending"), root.join("original.json")).unwrap();
        let mut journal = JournalStore::open(":memory:").unwrap();
        for record in inbox.read().unwrap() {
            journal.record_deployment(&record, 221).unwrap();
        }
        assert!(journal.qualified_deployment(&id, 221).unwrap().is_some());
        // When a separate publication revokes that identity, every final file is imported again.
        receipt.wire.revoked = true;
        fs::write(
            root.join("revoked.json"),
            serde_json::to_vec(&receipt.wire).unwrap(),
        )
        .unwrap();
        for record in inbox.read().unwrap() {
            journal.record_deployment(&record, 222).unwrap();
        }
        // Then revocation wins and unsafe file forms reject the complete snapshot before any open.
        assert!(journal.qualified_deployment(&id, 222).unwrap().is_none());
        fs::set_permissions(root.join("revoked.json"), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(inbox.read().is_err());
        fs::set_permissions(root.join("revoked.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(root.join("fifo.json"))
                .status()
                .unwrap()
                .success()
        );
        assert!(inbox.read().is_err());
        fs::remove_file(root.join("fifo.json")).unwrap();
        fs::write(root.join("broken.json"), "{").unwrap();
        assert!(inbox.read().is_err());
        fs::remove_file(root.join("broken.json")).unwrap();
        fs::hard_link(root.join("original.json"), root.join("linked.json")).unwrap();
        assert!(inbox.read().is_err());
        fs::remove_file(root.join("linked.json")).unwrap();
        assert_eq!(inbox.read().unwrap().len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_selects_complete_files_and_rejects_overflow_instead_of_returning_a_prefix() {
        // Given two published files and an incomplete producer temporary file.
        let root = directory();
        for name in ["b.json", "a.json", ".receipt-pending"] {
            fs::write(root.join(name), "{}").unwrap();
        }
        // When bounded enumeration takes a complete snapshot.
        let files = final_paths(
            &root,
            InboxLimits {
                entries: 3,
                bytes: 4,
            },
        )
        .unwrap();
        // Then only sorted final files qualify, and either resource overflow rejects the whole scan.
        assert_eq!(files, vec![root.join("a.json"), root.join("b.json")]);
        assert!(
            final_paths(
                &root,
                InboxLimits {
                    entries: 2,
                    bytes: 4
                }
            )
            .is_err()
        );
        assert!(
            final_paths(
                &root,
                InboxLimits {
                    entries: 3,
                    bytes: 3
                }
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_or_unexpected_entries_and_unprotected_roots_never_supply_receipts() {
        // Given a published-looking symlink and a directory selected by an ordinary user.
        let root = directory();
        fs::write(root.join("source"), "{}").unwrap();
        symlink(root.join("source"), root.join("event.json")).unwrap();
        // When selection or the complete production reader examines those paths.
        assert!(final_paths(&root, InboxLimits::default()).is_err());
        assert!(
            ReceiptInbox::new(root.clone(), InboxLimits::default())
                .unwrap()
                .read()
                .is_err()
        );
        fs::remove_file(root.join("source")).unwrap();
        // Then even without the unexpected name, the final symlink remains rejected.
        assert!(final_paths(&root, InboxLimits::default()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
