//! Persists one worker's launch intent and excludes competing owners of its state directory.

use super::SandboxError;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) struct JobStore {
    database: Connection,
    // This separate SQLite write lock stays held for the lifetime of the owner.
    _owner: Connection,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PendingJob {
    pub name: String,
    pub token: String,
    snapshot: SnapshotLease,
    pub container_id: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotLease {
    root: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
}

impl PendingJob {
    pub fn remove_snapshot(&self) -> Result<(), SandboxError> {
        self.snapshot.validate_path()?;
        let metadata = match fs::symlink_metadata(&self.snapshot.root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return self.snapshot.sync_parent();
            }
            Err(_) => return Err(SandboxError::Unavailable),
        };
        if !metadata.is_dir()
            || metadata.mode() & 0o077 != 0
            || metadata.dev() != self.snapshot.device
            || metadata.ino() != self.snapshot.inode
            || metadata.uid() != self.snapshot.uid
        {
            return Err(SandboxError::Unavailable);
        }
        fs::remove_dir_all(&self.snapshot.root).map_err(|_| SandboxError::Unavailable)?;
        self.snapshot.sync_parent()
    }
}

impl SnapshotLease {
    fn sync_parent(&self) -> Result<(), SandboxError> {
        fs::File::open(self.root.parent().ok_or(SandboxError::Unavailable)?)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| SandboxError::Unavailable)
    }
    fn capture(root: &Path) -> Result<Self, SandboxError> {
        let metadata = fs::symlink_metadata(root).map_err(|_| SandboxError::Unavailable)?;
        if !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
            return Err(SandboxError::Unavailable);
        }
        let lease = Self {
            root: fs::canonicalize(root).map_err(|_| SandboxError::Unavailable)?,
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
        };
        lease.validate_path()?;
        Ok(lease)
    }

    fn validate_path(&self) -> Result<(), SandboxError> {
        let parent =
            fs::canonicalize(std::env::temp_dir()).map_err(|_| SandboxError::Unavailable)?;
        if self.root.parent() != Some(parent.as_path())
            || !self
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("ai-sre-u9-snapshot-")
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
        {
            return Err(SandboxError::Unavailable);
        }
        Ok(())
    }
}

impl JobStore {
    pub fn claim_request(&mut self, nonce: &str) -> Result<(), SandboxError> {
        self.database
            .execute_batch("CREATE TABLE IF NOT EXISTS requests (nonce TEXT PRIMARY KEY);")
            .map_err(|_| SandboxError::Unavailable)?;
        let count: i64 = self
            .database
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .map_err(|_| SandboxError::Unavailable)?;
        // Bounded retention fails closed rather than silently forgetting replay protection.
        if count >= 4096 {
            return Err(SandboxError::Unavailable);
        }
        self.database
            .execute("INSERT INTO requests VALUES(?1)", [nonce])
            .map_err(|_| SandboxError::Failed)?;
        Ok(())
    }

    pub fn bind_runtime(&mut self, identity: &str) -> Result<(), SandboxError> {
        self.database.execute_batch("CREATE TABLE IF NOT EXISTS runtime (singleton INTEGER PRIMARY KEY CHECK(singleton=1), identity TEXT NOT NULL);").map_err(|_| SandboxError::Unavailable)?;
        let stored: Option<String> = self
            .database
            .query_row(
                "SELECT identity FROM runtime WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| SandboxError::Unavailable)?;
        match stored {
            Some(stored) if stored == identity => Ok(()),
            Some(_) => Err(SandboxError::Unavailable),
            None if self.pending()?.is_some() => Err(SandboxError::Unavailable),
            None => {
                self.database
                    .execute("INSERT INTO runtime VALUES(1, ?1)", [identity])
                    .map_err(|_| SandboxError::Unavailable)?;
                Ok(())
            }
        }
    }

    pub fn open(directory: &Path) -> Result<Self, SandboxError> {
        let metadata = fs::symlink_metadata(directory).map_err(|_| SandboxError::Unavailable)?;
        if !directory.is_absolute() || !metadata.is_dir() || metadata.mode() & 0o077 != 0 {
            return Err(SandboxError::Unavailable);
        }
        let directory = fs::canonicalize(directory).map_err(|_| SandboxError::Unavailable)?;
        for ancestor in directory.ancestors().skip(1) {
            let ancestor = fs::metadata(ancestor).map_err(|_| SandboxError::Unavailable)?;
            let protected_sticky = ancestor.uid() == 0 && ancestor.mode() & 0o1000 != 0;
            if (ancestor.uid() != 0 && ancestor.uid() != metadata.uid())
                || (ancestor.mode() & 0o022 != 0 && !protected_sticky)
            {
                return Err(SandboxError::Unavailable);
            }
        }
        let owner = open_database(&directory.join("owner.sqlite"), metadata.uid())?;
        owner
            .busy_timeout(Duration::ZERO)
            .map_err(|_| SandboxError::Unavailable)?;
        owner
            .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .map_err(|_| SandboxError::Unavailable)?;
        let database = open_database(&directory.join("jobs.sqlite"), metadata.uid())?;
        database.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), namespace TEXT NOT NULL, sequence INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS active (singleton INTEGER PRIMARY KEY CHECK(singleton=1), name TEXT NOT NULL, token TEXT NOT NULL, artifact TEXT NOT NULL, snapshot TEXT NOT NULL, image TEXT NOT NULL, lease TEXT NOT NULL, container_id TEXT);").map_err(|_| SandboxError::Unavailable)?;
        database
            .execute(
                "INSERT OR IGNORE INTO identity VALUES(1, ?1, 0)",
                [nonce()?],
            )
            .map_err(|_| SandboxError::Unavailable)?;
        fs::File::open(&directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| SandboxError::Unavailable)?;
        Ok(Self {
            database,
            _owner: owner,
        })
    }

    pub fn pending(&self) -> Result<Option<PendingJob>, SandboxError> {
        self.database
            .query_row(
                "SELECT name, token, lease, container_id FROM active WHERE singleton=1",
                [],
                |row| {
                    Ok(PendingJob {
                        name: row.get(0)?,
                        token: row.get(1)?,
                        snapshot: serde_json::from_str(&row.get::<_, String>(2)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                        container_id: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|_| SandboxError::Unavailable)
    }

    pub fn begin(
        &mut self,
        artifact: &str,
        snapshot: &str,
        image: &str,
        snapshot_root: &Path,
    ) -> Result<PendingJob, SandboxError> {
        let tx = self
            .database
            .transaction()
            .map_err(|_| SandboxError::Unavailable)?;
        let (namespace, sequence): (String, i64) = tx
            .query_row(
                "SELECT namespace, sequence FROM identity WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| SandboxError::Unavailable)?;
        let sequence = sequence
            .checked_add(1)
            .filter(|n| *n > 0)
            .ok_or(SandboxError::Unavailable)?;
        let job = PendingJob {
            name: format!("u9-{namespace}-{sequence}"),
            token: nonce()?,
            snapshot: SnapshotLease::capture(snapshot_root)?,
            container_id: None,
        };
        tx.execute(
            "INSERT INTO active VALUES(1, ?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                job.name,
                job.token,
                artifact,
                snapshot,
                image,
                serde_json::to_string(&job.snapshot).map_err(|_| SandboxError::Unavailable)?
            ],
        )
        .map_err(|_| SandboxError::Unavailable)?;
        tx.execute(
            "UPDATE identity SET sequence=?1 WHERE singleton=1",
            [sequence],
        )
        .map_err(|_| SandboxError::Unavailable)?;
        tx.commit().map_err(|_| SandboxError::Unavailable)?;
        Ok(job)
    }

    pub fn clear(&mut self, job: &PendingJob) -> Result<(), SandboxError> {
        let count = self
            .database
            .execute(
                "DELETE FROM active WHERE singleton=1 AND name=?1 AND token=?2",
                params![job.name, job.token],
            )
            .map_err(|_| SandboxError::Unavailable)?;
        if count != 1 {
            return Err(SandboxError::Unavailable);
        }
        Ok(())
    }

    pub fn remember_container(&mut self, job: &PendingJob, id: &str) -> Result<(), SandboxError> {
        let count = self.database.execute("UPDATE active SET container_id=?1 WHERE singleton=1 AND name=?2 AND token=?3 AND (container_id IS NULL OR container_id=?1)", params![id, job.name, job.token]).map_err(|_| SandboxError::Unavailable)?;
        if count != 1 {
            return Err(SandboxError::Unavailable);
        }
        Ok(())
    }
}

fn nonce() -> Result<String, SandboxError> {
    let mut bytes = [0; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|_| SandboxError::Unavailable)?;
    Ok(crate::gitops::receipt::digest(&bytes)[7..].to_owned())
}

fn open_database(path: &Path, uid: u32) -> Result<Connection, SandboxError> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file.sync_all().map_err(|_| SandboxError::Unavailable)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(SandboxError::Unavailable),
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| SandboxError::Unavailable)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(SandboxError::Unavailable);
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| SandboxError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn runtime_binding_survives_reopen_and_rejects_another_store() {
        // Given a private journal enrolled for one runtime storage identity.
        let root = std::env::temp_dir().join(format!(
            "u9-binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut jobs = JobStore::open(&root).unwrap();
        jobs.bind_runtime("original-store").unwrap();
        jobs.claim_request("request-one").unwrap();
        drop(jobs);
        // When a new owner attempts to reuse that journal for different storage.
        let mut jobs = JobStore::open(&root).unwrap();
        assert!(jobs.bind_runtime("different-store").is_err());
        // Then the original binding remains usable and cannot be silently replaced.
        jobs.bind_runtime("original-store").unwrap();
        // Then reconnecting cannot execute an already admitted request a second time.
        assert!(jobs.claim_request("request-one").is_err());
        jobs.claim_request("request-two").unwrap();
        drop(jobs);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abrupt_process_exit_releases_owner_but_preserves_launch_intent() {
        if let Some(directory) = std::env::var_os("U9_JOB_EXIT_STATE") {
            let mut jobs = JobStore::open(Path::new(&directory)).unwrap();
            jobs.begin(
                "artifact",
                "snapshot",
                "image",
                Path::new(&std::env::var_os("U9_JOB_EXIT_SNAPSHOT").unwrap()),
            )
            .unwrap();
            // Simulate process loss: no Rust destructors or graceful cleanup run.
            std::process::exit(23);
        }
        // Given an isolated child process that owns durable state and records a launch.
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let state = std::env::temp_dir().join(format!("u9-jobs-exit-{suffix}"));
        let snapshot = std::env::temp_dir().join(format!("ai-sre-u9-snapshot-exit-{suffix}"));
        for path in [&state, &snapshot] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // When that process exits abruptly while still holding its owner lock.
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gitops::sandbox::jobs::tests::abrupt_process_exit_releases_owner_but_preserves_launch_intent"])
            .env("U9_JOB_EXIT_STATE", &state).env("U9_JOB_EXIT_SNAPSHOT", &snapshot)
            .status().unwrap();
        assert_eq!(status.code(), Some(23));
        // Then a new process owner can reconcile the exact surviving launch record.
        let mut jobs = JobStore::open(&state).unwrap();
        let pending = jobs.pending().unwrap().unwrap();
        pending.remove_snapshot().unwrap();
        jobs.clear(&pending).unwrap();
        drop(jobs);
        assert!(!snapshot.exists());
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn launch_intent_survives_reopen_and_excludes_another_owner() {
        // Given a private state directory and an exclusively owned job journal.
        let root = std::env::temp_dir().join(format!(
            "u9-jobs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut jobs = JobStore::open(&root).unwrap();
        assert!(JobStore::open(&root).is_err());
        let snapshot = std::env::temp_dir().join(format!(
            "ai-sre-u9-snapshot-store-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&snapshot).unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        // When a launch is recorded and the process releases its owner without completing it.
        let first = jobs
            .begin("artifact", "snapshot", "image", &snapshot)
            .unwrap();
        assert!(jobs.begin("other", "other", "other", &snapshot).is_err());
        drop(jobs);
        // Then a new owner must recover that exact intent before allocating another name.
        let mut jobs = JobStore::open(&root).unwrap();
        assert_eq!(jobs.pending().unwrap().unwrap(), first);
        jobs.clear(&first).unwrap();
        let next = jobs
            .begin("artifact", "snapshot", "image", &snapshot)
            .unwrap();
        assert_ne!(first.name, next.name);
        drop(jobs);
        // Given an unrelated directory that has replaced the original snapshot pathname.
        let retained = root.join("retained-snapshot");
        fs::rename(&snapshot, &retained).unwrap();
        fs::create_dir(&snapshot).unwrap();
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o700)).unwrap();
        // Then cleanup refuses the replacement; only the original inode can be reclaimed.
        assert!(next.remove_snapshot().is_err());
        assert!(snapshot.exists());
        fs::remove_dir(&snapshot).unwrap();
        fs::rename(retained, &snapshot).unwrap();
        next.remove_snapshot().unwrap();
        // Given a hard-linked ownership database, opening another owner must fail closed.
        fs::hard_link(root.join("owner.sqlite"), root.join("owner-link")).unwrap();
        assert!(JobStore::open(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
