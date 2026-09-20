//! Builds a private bounded validation tree from immutable Git blobs, never a dirty checkout.
//!
//! Only fixed read-only Git builtins run on the host. Hooks, filters, archive
//! commands, submodules, and symlinks are not executed or followed. The caller
//! owns repository selection and must refresh its base evidence before handoff.

use super::{
    it_tools_image::{PILOT_PATH, render},
    receipt::digest,
    validate::valid_sha,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FILES: usize = 4096;
const MAX_DATA: usize = 64 * 1024 * 1024;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

/// An owned copy of tracked regular files with the pilot scalar deterministically replaced.
/// Dropping it removes only its generated temporary tree.
pub struct RepositorySnapshot {
    root: PathBuf,
    root_identity: (u64, u64),
    original: String,
    pilot: String,
    base: String,
    digest: String,
}

/// A snapshot failure yields no tree eligible for sandbox validation.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// Git or filesystem I/O failed or exceeded its deadline/size limit.
    #[error("repository snapshot could not be captured")]
    Unavailable,
    /// The repository, file modes, paths, or base revision are outside policy.
    #[error("repository snapshot violates scope")]
    OutsideScope,
}

impl RepositorySnapshot {
    /// Captures at most 4096 regular files and 64 MiB from the exact current HEAD.
    /// Repository selection is deployment-owned. No fetch, checkout, hook, or filter runs.
    pub async fn capture(
        repository: &Path,
        base: &str,
        image: &str,
    ) -> Result<Self, SnapshotError> {
        if !repository.is_absolute() || !valid_sha(base) {
            return Err(SnapshotError::OutsideScope);
        }
        let head = git(repository, &["rev-parse", "--verify", "HEAD"], &[], 128).await?;
        if std::str::from_utf8(&head).ok().map(str::trim) != Some(base) {
            return Err(SnapshotError::OutsideScope);
        }
        let tree = git(
            repository,
            &["ls-tree", "-r", "-z", "--full-tree", base],
            &[],
            4 * 1024 * 1024,
        )
        .await?;
        let mut entries = Vec::new();
        let mut batch = Vec::new();
        for record in tree.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let record = std::str::from_utf8(record).map_err(|_| SnapshotError::OutsideScope)?;
            let (header, name) = record.split_once('\t').ok_or(SnapshotError::OutsideScope)?;
            let fields: Vec<_> = header.split(' ').collect();
            if fields.len() != 3
                || !["100644", "100755"].contains(&fields[0])
                || fields[1] != "blob"
                || !valid_sha(fields[2])
                || !safe_path(name)
                || entries.len() >= MAX_FILES
            {
                return Err(SnapshotError::OutsideScope);
            }
            entries.push((name.to_owned(), fields[0] == "100755", fields[2].to_owned()));
            batch.extend_from_slice(fields[2].as_bytes());
            batch.push(b'\n');
        }
        let data = git(repository, &["cat-file", "--batch"], &batch, MAX_DATA).await?;
        let root = private_directory()?;
        let metadata = fs::symlink_metadata(&root).map_err(|_| SnapshotError::Unavailable)?;
        let mut snapshot = Self {
            root,
            root_identity: (metadata.dev(), metadata.ino()),
            original: String::new(),
            pilot: String::new(),
            base: base.into(),
            digest: digest(format!("{}:{}:{}", digest(&tree), digest(&data), image).as_bytes()),
        };
        let mut offset = 0usize;
        let mut source_directories = BTreeSet::new();
        for (name, executable, oid) in entries {
            let newline = data
                .get(offset..)
                .and_then(|v| v.iter().position(|b| *b == b'\n'))
                .ok_or(SnapshotError::Unavailable)?
                + offset;
            let header = std::str::from_utf8(&data[offset..newline])
                .map_err(|_| SnapshotError::Unavailable)?;
            let fields: Vec<_> = header.split(' ').collect();
            if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
                return Err(SnapshotError::Unavailable);
            }
            let length: usize = fields[2].parse().map_err(|_| SnapshotError::Unavailable)?;
            offset = newline + 1;
            let end = offset
                .checked_add(length)
                .filter(|end| *end < data.len())
                .ok_or(SnapshotError::Unavailable)?;
            if data[end] != b'\n' {
                return Err(SnapshotError::Unavailable);
            }
            let mut contents = &data[offset..end];
            if name == PILOT_PATH {
                if length > 256 * 1024 {
                    return Err(SnapshotError::OutsideScope);
                }
                snapshot.original = std::str::from_utf8(contents)
                    .map_err(|_| SnapshotError::OutsideScope)?
                    .into();
                snapshot.pilot =
                    render(&snapshot.original, image).map_err(|_| SnapshotError::OutsideScope)?;
                contents = snapshot.pilot.as_bytes();
            }
            let destination = snapshot.root.join("source").join(name);
            fs::create_dir_all(destination.parent().ok_or(SnapshotError::OutsideScope)?)
                .map_err(|_| SnapshotError::Unavailable)?;
            source_directories.extend(
                destination
                    .parent()
                    .into_iter()
                    .flat_map(Path::ancestors)
                    .take_while(|directory| *directory != snapshot.root)
                    .map(Path::to_path_buf),
            );
            fs::write(&destination, contents).map_err(|_| SnapshotError::Unavailable)?;
            fs::set_permissions(
                destination,
                fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
            )
            .map_err(|_| SnapshotError::Unavailable)?;
            offset = end + 1;
        }
        if snapshot.original.is_empty() || offset != data.len() {
            return Err(SnapshotError::OutsideScope);
        }
        // The container uses a different UID. Normalize only the mounted subtree;
        // the outer directory stays 0700 so other host users cannot reach it.
        for directory in source_directories {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755))
                .map_err(|_| SnapshotError::Unavailable)?;
        }
        let head_after = git(repository, &["rev-parse", "--verify", "HEAD"], &[], 128).await?;
        if head_after != head {
            return Err(SnapshotError::OutsideScope);
        }
        Ok(snapshot)
    }

    /// Original pilot file read from the immutable Git object.
    pub fn original_source(&self) -> &str {
        &self.original
    }
    /// Pilot contents after the deterministic one-field replacement.
    pub fn pilot_source(&self) -> &str {
        &self.pilot
    }
    /// Captured local HEAD revision; remote freshness is checked separately at handoff.
    pub fn base_sha(&self) -> &str {
        &self.base
    }
    /// Identity binding tracked paths/modes, actual blob bytes, and the replacement image.
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub(crate) fn source_directory(&self) -> PathBuf {
        self.root.join("source")
    }
}

impl Drop for RepositorySnapshot {
    fn drop(&mut self) {
        // Recovery may already have removed this directory. Never remove a replacement at its path.
        if fs::symlink_metadata(&self.root).is_ok_and(|metadata| {
            metadata.is_dir() && (metadata.dev(), metadata.ino()) == self.root_identity
        }) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn safe_path(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 512
        && !name.split('/').any(|part| matches!(part, "" | "." | ".."))
        && !name.bytes().any(|b| b < 32 || b == 127 || b == b'\\')
        && Path::new(name)
            .components()
            .all(|part| matches!(part, Component::Normal(p) if p != ".git"))
}

fn private_directory() -> Result<PathBuf, SnapshotError> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SnapshotError::Unavailable)?
        .as_nanos();
    private_directory_at(suffix)
}

fn private_directory_at(suffix: u128) -> Result<PathBuf, SnapshotError> {
    let root = std::env::temp_dir().join(format!(
        "ai-sre-u9-snapshot-{}-{suffix}-{}",
        std::process::id(),
        NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
    ));
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root)
        .map_err(|_| SnapshotError::Unavailable)?;
    Ok(root)
}

async fn git(
    repository: &Path,
    args: &[&str],
    input: &[u8],
    limit: usize,
) -> Result<Vec<u8>, SnapshotError> {
    let mut child = tokio::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(repository)
        .args(args)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // A missing promisor object must not trigger a fetch or a host credential helper.
        // The empty protocol allowlist also denies transports on older Git versions.
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| SnapshotError::Unavailable)?;
    let mut stdin = child.stdin.take().ok_or(SnapshotError::Unavailable)?;
    let stdout = child.stdout.take().ok_or(SnapshotError::Unavailable)?;
    tokio::time::timeout(Duration::from_secs(10), async {
        let write = async {
            stdin.write_all(input).await?;
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let read = async {
            let mut bytes = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (_, bytes, status) =
            tokio::try_join!(write, read, child.wait()).map_err(|_| SnapshotError::Unavailable)?;
        if !status.success() || bytes.len() > limit {
            return Err(SnapshotError::Unavailable);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| SnapshotError::Unavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_created_in_the_same_clock_tick_get_distinct_private_directories() {
        // Given two snapshots starting in the same clock tick in the same process.
        let tick = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let first = private_directory_at(tick).unwrap();

        // When both request exclusively created temporary directories.
        let second = private_directory_at(tick);
        fs::remove_dir(&first).unwrap();

        // Then neither collides with nor reuses the other's private source directory.
        let second = second.expect("same-tick snapshots must have unique names");
        assert_ne!(first, second);
        assert_eq!(
            fs::metadata(&second).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir(second).unwrap();
    }
}
