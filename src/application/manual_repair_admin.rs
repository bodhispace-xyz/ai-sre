//! Handles bounded local operator requests using kernel peer credentials, never caller-supplied identity.

use crate::reasoning::storage::JournalStore;
use std::{
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[derive(serde::Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Acknowledge {
        handoff_digest: String,
    },
    Inspect {
        artifact: String,
    },
    Recover {
        artifact: String,
        expected_request_digest: String,
        reason: String,
    },
}

struct AuthenticatedRequest {
    request: Request,
    uid: u32,
    gid: u32,
}

async fn read_request(
    stream: &mut UnixStream,
    operator_gid: Option<u32>,
) -> io::Result<AuthenticatedRequest> {
    let peer = stream.peer_cred()?;
    if peer.uid() != 0 && operator_gid != Some(peer.gid()) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        let size = stream.read_u32().await? as usize;
        if size == 0 || size > 4096 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut bytes = vec![0; size];
        stream.read_exact(&mut bytes).await?;
        serde_json::from_slice::<Request>(&bytes)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    Ok(AuthenticatedRequest {
        request,
        uid: peer.uid(),
        gid: peer.gid(),
    })
}

impl AuthenticatedRequest {
    fn execute(self, journal: &mut JournalStore) -> io::Result<Vec<u8>> {
        let response = match self.request {
            Request::Acknowledge { handoff_digest } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| io::Error::from(io::ErrorKind::Other))?
                    .as_secs();
                let acknowledged = journal
                    .acknowledge_manual_handoff(&handoff_digest, self.uid, self.gid, now)
                    .map_err(storage_error)?;
                serde_json::json!({"acknowledged": acknowledged, "approved": false, "dispatched": false, "readiness": "not_assessed"})
            }
            Request::Inspect { artifact } => {
                let data = journal
                    .manual_repair_artifact(&artifact)
                    .map_err(storage_error)?;
                let expected = journal
                    .manual_validation_attempt(&artifact)
                    .map_err(storage_error)?
                    .map(|attempt| crate::gitops::receipt::digest(attempt.request_json.as_bytes()));
                let handoff_digest = data
                    .as_ref()
                    .and_then(|artifact| artifact.handoff_json.as_ref())
                    .map(|wire| crate::gitops::receipt::digest(wire.as_bytes()));
                let acknowledgement = handoff_digest
                    .as_ref()
                    .map(|digest| journal.manual_handoff_acknowledgement(digest))
                    .transpose()
                    .map_err(storage_error)?
                    .flatten();
                serde_json::json!({
                    "handoff_digest": handoff_digest,
                    "acknowledgement": acknowledgement,
                    "cleanup_status": data.as_ref().and_then(|artifact| artifact.attempt.as_ref()).map(|attempt| attempt.cleanup_status()).unwrap_or("unknown"),
                    "artifact": data,
                    "expected_request_digest": expected,
                    "history": journal.manual_validation_history(&artifact).map_err(storage_error)?,
                    "readiness": "not_assessed",
                    "dispatched": false,
                })
            }
            Request::Recover {
                artifact,
                expected_request_digest,
                reason,
            } => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| io::Error::from(io::ErrorKind::Other))?
                    .as_secs();
                let recovered = journal
                    .recover_manual_validation(
                        &artifact,
                        &expected_request_digest,
                        self.uid,
                        self.gid,
                        &reason,
                        now,
                    )
                    .map_err(storage_error)?;
                serde_json::json!({"recovered":recovered,"dispatched":false,"readiness":"not_assessed"})
            }
        };
        let bytes = serde_json::to_vec(&response)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(bytes)
    }
}

async fn write_response(stream: &mut UnixStream, bytes: &[u8]) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(bytes).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

// Test harness for the same authentication, execution, and delivery stages used by the service.
#[cfg(test)]
async fn serve_connection(
    journal: &mut JournalStore,
    stream: &mut UnixStream,
    group: Option<u32>,
) -> io::Result<()> {
    let request = read_request(stream, group).await?;
    let response = request.execute(journal)?;
    write_response(stream, &response).await
}

fn storage_error(_: crate::reasoning::storage::JournalStoreError) -> io::Error {
    io::Error::other("operator journal operation failed")
}

/// Owns the optional local socket and removes only the inode this process created.
pub(crate) struct AdminListener {
    listener: tokio::net::UnixListener,
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
    pub(crate) operator_gid: Option<u32>,
}

impl AdminListener {
    pub(crate) fn from_environment() -> io::Result<Option<Self>> {
        let Some(path) = std::env::var_os("AI_SRE_ADMIN_SOCKET") else {
            return Ok(None);
        };
        let gid = std::env::var("AI_SRE_OPERATORS_GID")
            .ok()
            .map(|value| {
                value
                    .parse::<u32>()
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            })
            .transpose()?;
        Self::bind(std::path::PathBuf::from(path), gid).map(Some)
    }

    fn bind(path: std::path::PathBuf, operator_gid: Option<u32>) -> io::Result<Self> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        let uid = rustix::process::geteuid().as_raw();
        for ancestor in parent.ancestors() {
            let metadata = std::fs::symlink_metadata(ancestor)?;
            let root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
            if !metadata.is_dir()
                || ![0, uid].contains(&metadata.uid())
                || (metadata.mode() & 0o022 != 0 && !root_sticky)
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        // A sticky shared directory is acceptable above, but never as the socket's direct parent.
        if std::fs::metadata(parent)?.mode() & 0o022 != 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let listener = tokio::net::UnixListener::bind(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        let bound = Self {
            listener,
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            operator_gid,
        };
        if let Some(gid) = operator_gid {
            std::os::unix::fs::chown(&bound.path, None, Some(gid))?;
        }
        std::fs::set_permissions(&bound.path, std::fs::Permissions::from_mode(0o660))?;
        Ok(bound)
    }
}

impl Drop for AdminListener {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Decoded, authenticated command; only synchronous journal work runs in the incident worker.
pub(crate) struct AdminCommand {
    request: AuthenticatedRequest,
    reply: tokio::sync::oneshot::Sender<io::Result<Vec<u8>>>,
    deadline: tokio::time::Instant,
}

impl AdminCommand {
    pub(crate) fn execute(self, journal: &mut JournalStore) {
        // A timed-out queued request does not gain authority later. Delivery can still fail
        // after commit, so the operator must inspect history when the outcome is unknown.
        if tokio::time::Instant::now() < self.deadline && !self.reply.is_closed() {
            let result = self.request.execute(journal);
            let _ = self.reply.send(result);
        }
    }
}

/// Bounded socket I/O owner; dropping the worker cancels clients and removes the owned socket.
pub(crate) struct AdminService {
    commands: tokio::sync::mpsc::Receiver<AdminCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl AdminService {
    pub(crate) fn start(listener: AdminListener) -> Self {
        let (sender, commands) = tokio::sync::mpsc::channel(4);
        let task = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.listener.accept(), if clients.len() < 4 => {
                        let Ok((mut stream, _)) = connection else { break; };
                        let sender = sender.clone();
                        let group = listener.operator_gid;
                        clients.spawn(async move {
                            let request = read_request(&mut stream, group).await?;
                            let (reply, receiver) = tokio::sync::oneshot::channel();
                            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                            sender.try_send(AdminCommand { request, reply, deadline }).map_err(|_| io::Error::other("operator queue unavailable"))?;
                            let response = tokio::time::timeout_at(deadline, receiver).await
                                .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
                                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))??;
                            write_response(&mut stream, &response).await
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self { commands, task }
    }
}

impl Drop for AdminService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn next(service: &mut Option<AdminService>) -> Option<AdminCommand> {
    match service {
        Some(service) => service.commands.recv().await,
        None => std::future::pending().await,
    }
}

/// Runs the actual CLI against the queued operator service in Linux acceptance tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) async fn acceptance_cli(
    journal: &mut JournalStore,
    arguments: &[&str],
) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "u9-flow-admin-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.join("admin.sock");
    let mut service = AdminService::start(
        AdminListener::bind(socket.clone(), Some(rustix::process::getegid().as_raw())).unwrap(),
    );
    let mut client = tokio::process::Command::new(
        std::env::var("U9_ADMIN_CLI").expect("supply the built operator CLI"),
    );
    client
        .arg(arguments[0])
        .arg(&socket)
        .args(&arguments[1..])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(20), client.output());
    tokio::pin!(output);
    let result = loop {
        tokio::select! {
            command = service.commands.recv() => command.unwrap().execute(journal),
            result = &mut output => break result.unwrap().unwrap(),
        }
    };
    drop(service);
    tokio::task::yield_now().await;
    assert!(!socket.exists());
    std::fs::remove_dir(root).unwrap();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn acknowledgement_authenticates_before_lookup_and_rejects_supplied_identity() {
        // Given an acknowledgement request with no caller-controlled identity or approval fields.
        let bytes = br#"{"operation":"acknowledge","handoff_digest":"missing"}"#;
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client.write_u32(bytes.len() as u32).await.unwrap();
        client.write_all(bytes).await.unwrap();
        let peer = server.peer_cred().unwrap();
        // When the existing kernel-credential gate admits the peer and looks up the exact handoff.
        let authenticated = read_request(&mut server, Some(peer.gid())).await.unwrap();
        assert_eq!(authenticated.uid, peer.uid());
        assert_eq!(authenticated.gid, peer.gid());
        let mut store = JournalStore::open(":memory:").unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&authenticated.execute(&mut store).unwrap()).unwrap();
        // Then an unknown handoff is refused without approval, dispatch, or a forged audit identity.
        assert_eq!(response["acknowledged"], false);
        assert_eq!(response["approved"], false);
        assert_eq!(response["dispatched"], false);
        assert!(store.journal().entries().is_empty());
        assert!(
            serde_json::from_str::<Request>(
                r#"{"operation":"acknowledge","handoff_digest":"h","uid":0}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn expired_queued_recovery_cannot_mutate_even_before_reply_timeout_is_polled() {
        // Given an authenticated recovery whose queue deadline has passed but its reply is still open.
        let mut journal = JournalStore::open(":memory:").unwrap();
        let wire = r#"{"nonce":"expired-queue","expires_at":1}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        let (reply, _receiver) = tokio::sync::oneshot::channel();
        let command = AdminCommand {
            request: AuthenticatedRequest {
                uid: 0,
                gid: 0,
                request: Request::Recover {
                    artifact: "candidate".into(),
                    expected_request_digest: crate::gitops::receipt::digest(wire.as_bytes()),
                    reason: "worker inspected".into(),
                },
            },
            reply,
            deadline: tokio::time::Instant::now(),
        };
        // When the incident worker finally dequeues the command.
        command.execute(&mut journal);
        // Then the active attempt remains untouched and no recovery is recorded.
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_some()
        );
        assert!(
            journal
                .manual_validation_history("candidate")
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires disposable Linux root, setpriv, and U9_ADMIN_CLI pointing to the built client"]
    async fn linux_cross_user_cli_enforces_kernel_identity_and_audits_recovery() {
        use std::os::unix::fs::PermissionsExt;
        // Given the real listener, a compiled CLI, and a root-owned disposable directory.
        assert_eq!(
            rustix::process::geteuid().as_raw(),
            0,
            "run this acceptance test as root in the disposable VM"
        );
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("u9-cross-user-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cli = root.join("ai-sre-admin");
        std::fs::copy(
            std::env::var("U9_ADMIN_CLI").expect("build and supply the actual CLI"),
            &cli,
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = root.join("admin.sock");
        let mut service =
            AdminService::start(AdminListener::bind(socket.clone(), Some(501)).unwrap());
        let path = root.join("journal.sqlite");
        let mut journal = JournalStore::open(&path).unwrap();
        let wire = r#"{"nonce":"cross-user","expires_at":1}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        let expected = crate::gitops::receipt::digest(wire.as_bytes());
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = service.commands.recv() => command.unwrap().execute(&mut journal),
                    _ = &mut stopped => break,
                }
            }
            journal
        });
        async fn invoke(
            cli: &std::path::Path,
            socket: &std::path::Path,
            uid: &str,
            gid: &str,
            groups: &[&str],
            operation: &[&str],
        ) -> std::process::Output {
            let mut command = tokio::process::Command::new("/usr/bin/setpriv");
            command
                .args(["--reuid", uid, "--regid", gid])
                .args(groups)
                .arg("--")
                .arg(cli)
                .arg(operation[0])
                .arg(socket)
                .args(&operation[1..]);
            command.kill_on_drop(true);
            tokio::time::timeout(Duration::from_secs(20), command.output())
                .await
                .unwrap()
                .unwrap()
        }
        // When a non-operator uses the actual CLI, filesystem access is denied first.
        let denied = invoke(
            &cli,
            &socket,
            "65534",
            "65534",
            &["--clear-groups"],
            &["inspect", "candidate"],
        )
        .await;
        assert!(!denied.status.success());
        // Supplementary operator membership passes mode 0660 but must fail kernel primary-GID authorization.
        let supplementary = invoke(
            &cli,
            &socket,
            "65534",
            "65534",
            &["--groups", "501"],
            &["recover", "candidate", &expected, "must not be admitted"],
        )
        .await;
        assert!(!supplementary.status.success());
        let root_read = invoke(
            &cli,
            &socket,
            "0",
            "0",
            &["--clear-groups"],
            &["inspect", "candidate"],
        )
        .await;
        assert!(root_read.status.success());
        let recovered = invoke(
            &cli,
            &socket,
            "501",
            "501",
            &["--clear-groups"],
            &["recover", "candidate", &expected, "worker inspected"],
        )
        .await;
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        let replay = invoke(
            &cli,
            &socket,
            "501",
            "501",
            &["--clear-groups"],
            &["recover", "candidate", &expected, "stale replay"],
        )
        .await;
        assert!(!replay.status.success());
        // Then exactly the primary-group operator is recorded, with no mutation by denied clients.
        stop.send(()).unwrap();
        let journal = worker.await.unwrap();
        let history = journal.manual_validation_history("candidate").unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].operator_uid, 501);
        assert_eq!(history[0].operator_gid, 501);
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
        drop(journal);
        tokio::task::yield_now().await;
        assert!(!socket.exists());
        std::fs::remove_file(cli).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn incomplete_client_does_not_hold_up_a_complete_operator_request() {
        use std::os::unix::fs::PermissionsExt;
        // Given one authenticated client that sends no frame and another with a complete request.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("u9-admin-isolation-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("admin.sock");
        let gid = rustix::process::getegid().as_raw();
        let mut service =
            AdminService::start(AdminListener::bind(path.clone(), Some(gid)).unwrap());
        let _slow_client = UnixStream::connect(&path).await.unwrap();
        let mut client = UnixStream::connect(&path).await.unwrap();
        let bytes = br#"{"operation":"inspect","artifact":"missing"}"#;
        client.write_u32(bytes.len() as u32).await.unwrap();
        client.write_all(bytes).await.unwrap();
        // When the journal worker waits for decoded commands, not socket bytes.
        let command = tokio::time::timeout(Duration::from_secs(1), service.commands.recv())
            .await
            .unwrap()
            .unwrap();
        let mut journal = JournalStore::open(":memory:").unwrap();
        command.execute(&mut journal);
        // Then the complete request finishes before the slow client's five-second read deadline.
        let length = tokio::time::timeout(Duration::from_secs(1), client.read_u32())
            .await
            .unwrap()
            .unwrap();
        let mut response = vec![0; length as usize];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&response).unwrap()["readiness"],
            "not_assessed"
        );
        drop(service);
        tokio::task::yield_now().await;
        assert!(!path.exists());
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn disconnected_recovery_remains_auditable_after_restart() {
        // Given a durable uncertain attempt and a client that disconnects after sending recovery.
        let root = std::env::temp_dir().join(format!(
            "u9-admin-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("journal.sqlite");
        let mut journal = JournalStore::open(&path).unwrap();
        let wire = r#"{"nonce":"disconnect","expires_at":1}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let gid = server.peer_cred().unwrap().gid();
        let bytes = serde_json::to_vec(&serde_json::json!({"operation":"recover", "artifact":"candidate", "expected_request_digest":crate::gitops::receipt::digest(wire.as_bytes()), "reason":"remote worker inspected"})).unwrap();
        // When reply delivery fails and the journal is reopened by a new process lifetime.
        let (result, ()) = tokio::join!(
            serve_connection(&mut journal, &mut server, Some(gid)),
            async {
                client.write_u32(bytes.len() as u32).await.unwrap();
                client.write_all(&bytes).await.unwrap();
                drop(client);
            }
        );
        assert!(result.is_err());
        drop(journal);
        let journal = JournalStore::open(&path).unwrap();
        // Then the committed audit survives; a missing reply is not treated as a rollback.
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            journal.manual_validation_history("candidate").unwrap()[0].request_json,
            wire
        );
        drop(journal);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn listener_refuses_existing_socket_and_cleans_up_its_own_socket() {
        use std::os::unix::fs::PermissionsExt;
        // Given a private deployment-owned socket directory.
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("u9-socket-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("admin.sock");
        let listener = AdminListener::bind(path.clone(), None).unwrap();
        // When another listener tries to use the same path.
        assert!(AdminListener::bind(path.clone(), None).is_err());
        assert!(path.exists());
        // Then it leaves the original intact, and the owner removes it on normal shutdown.
        drop(listener);
        assert!(!path.exists());
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn supplied_operator_identity_cannot_override_kernel_credentials() {
        // Given an operator connection whose JSON tries to supply a privileged identity.
        let mut journal = crate::reasoning::storage::JournalStore::open(":memory:").unwrap();
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let gid = server.peer_cred().unwrap().gid();
        let bytes = br#"{"operation":"inspect","artifact":"candidate","uid":0}"#;
        client.write_u32(bytes.len() as u32).await.unwrap();
        client.write_all(bytes).await.unwrap();
        // When the framed request is decoded after kernel authentication.
        let error = serve_connection(&mut journal, &mut server, Some(gid))
            .await
            .unwrap_err();
        // Then unknown identity fields are rejected rather than trusted or silently ignored.
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn non_operator_is_rejected_before_request_input() {
        // Given a non-root peer with no admitted operator group.
        let mut journal = crate::reasoning::storage::JournalStore::open(":memory:").unwrap();
        let (_client, mut server) = tokio::net::UnixStream::pair().unwrap();
        if server.peer_cred().unwrap().uid() == 0 {
            return;
        }
        // When it connects without sending any bytes.
        let error = serve_connection(&mut journal, &mut server, None)
            .await
            .unwrap_err();
        // Then authorization fails immediately, before parsing or touching journal state.
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn local_operator_recovery_is_audited_and_does_not_dispatch_work() {
        // Given an expired uncertain request and a connected local operator.
        let mut journal = crate::reasoning::storage::JournalStore::open(":memory:").unwrap();
        let wire = r#"{"nonce":"old","expires_at":1}"#;
        journal
            .reserve_manual_validation("candidate", wire)
            .unwrap();
        let (mut client, mut server) = tokio::net::UnixStream::pair().unwrap();
        let peer = server.peer_cred().unwrap();
        let request = serde_json::json!({"operation":"recover", "artifact":"candidate", "expected_request_digest":crate::gitops::receipt::digest(wire.as_bytes()), "reason":"worker checked; retry requested"});
        // When the authenticated request is handled through the Unix transport.
        let client_work = async {
            let bytes = serde_json::to_vec(&request).unwrap();
            client.write_u32(bytes.len() as u32).await.unwrap();
            client.write_all(&bytes).await.unwrap();
            let length = client.read_u32().await.unwrap();
            let mut response = vec![0; length as usize];
            client.read_exact(&mut response).await.unwrap();
            serde_json::from_slice::<serde_json::Value>(&response).unwrap()
        };
        let (handled, response) = tokio::join!(
            serve_connection(&mut journal, &mut server, Some(peer.gid())),
            client_work
        );
        handled.unwrap();
        // Then only retry eligibility changes; the old attempt and kernel identity remain auditable.
        assert_eq!(response["recovered"], true);
        assert_eq!(response["dispatched"], false);
        let history = journal.manual_validation_history("candidate").unwrap();
        assert_eq!(history[0].operator_uid, peer.uid());
        assert_eq!(history[0].request_json, wire);
        assert!(
            journal
                .manual_validation_attempt("candidate")
                .unwrap()
                .is_none()
        );
    }
}
