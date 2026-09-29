//! Runs a fixed SSH validation job or local checks of the enrolled Linux worker.
//!
//! Deployment supplies a protected configuration file and restricts the dedicated SSH key.
//! Request data never selects commands, repository paths, or runtime enrollment.

use ai_sre::gitops::sandbox::{RootlessValidator, SandboxPlan, WorkerConfig, serve_worker};
use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), &'static str> {
    let check_enrollment = command_mode(
        &env::args().skip(1).collect::<Vec<_>>(),
        env::var("SSH_ORIGINAL_COMMAND").ok().as_deref(),
    )?;
    let path = PathBuf::from(
        env::var_os("AI_SRE_VALIDATOR_CONFIG").ok_or("missing worker configuration")?,
    );
    let config = read_configuration(&path)?;
    if check_enrollment {
        check_local_enrollment(&config).await?;
        println!(
            "{}",
            serde_json::json!({
                "schema":"ai-sre/worker-preflight/v1", "local_prerequisites":"passed",
                "qualification":"not_assessed", "upstream_freshness":"not_assessed",
                "worker_request":"not_created", "pending_jobs":"not_assessed"
            })
        );
        return Ok(());
    }
    serve(config).await
}

// A preflight is local-only. The SSH forced-command protocol remains one fixed request operation.
fn command_mode(args: &[String], original_command: Option<&str>) -> Result<bool, &'static str> {
    if args == ["--check-enrollment"] && original_command.is_none() {
        return Ok(true);
    }
    if !args.is_empty() || original_command != Some("ai-sre-validator-v1") {
        return Err("unsupported worker command");
    }
    Ok(false)
}

fn read_configuration(path: &Path) -> Result<WorkerConfig, &'static str> {
    if !path.is_absolute() {
        return Err("invalid worker configuration path");
    }
    for (index, ancestor) in path.ancestors().enumerate() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| "worker configuration unavailable")?;
        if metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
            || !configuration_component_type(metadata.file_type(), index == 0)
        {
            return Err("worker configuration is not protected");
        }
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| "worker configuration unavailable")?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "worker configuration unavailable")?;
    if bytes.len() > 16 * 1024 {
        return Err("worker configuration exceeds limit");
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid worker configuration")
}

async fn check_local_enrollment(config: &WorkerConfig) -> Result<(), &'static str> {
    if !cfg!(target_os = "linux") || rustix::process::geteuid().is_root() {
        return Err("preflight requires the unprivileged Linux worker identity");
    }
    if !(1..=3600).contains(&config.policy.max_validation_age_seconds)
        || !config
            .policy
            .runtime_digest
            .strip_prefix("sha256:")
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    {
        return Err("invalid worker enrollment policy");
    }
    SandboxPlan::new(
        &config.policy.validator_image,
        config.policy.sandbox_limits.clone(),
    )
    .map_err(|_| "invalid worker enrollment policy")?;
    check_worker_directory(&config.repository, false)?;
    check_worker_directory(&config.state, true)?;
    // Do not open the job store: that acquires ownership, creates tables, and may reconcile jobs.
    let mut runtime = RootlessValidator::connect(&config.runtime, &config.policy.runtime_digest)
        .await
        .map_err(|_| "worker runtime is unavailable")?;
    runtime
        .check_preloaded_image(&config.policy.validator_image)
        .await
        .map_err(|_| "enrolled validator image is not available locally")
}

fn check_worker_directory(path: &Path, private: bool) -> Result<(), &'static str> {
    if !path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("invalid worker directory");
    }
    let uid = rustix::process::geteuid().as_raw();
    for (index, ancestor) in path.ancestors().enumerate() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| "worker directory unavailable")?;
        let protected_sticky = index > 0 && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || (index == 0 && metadata.uid() != uid)
            || (metadata.uid() != 0 && metadata.uid() != uid)
            || (metadata.mode() & 0o022 != 0 && !protected_sticky)
            || (index == 0 && private && metadata.mode() & 0o077 != 0)
            || (index == 0 && metadata.mode() & 0o500 != 0o500)
            || (index == 0 && private && metadata.mode() & 0o700 != 0o700)
        {
            return Err("worker directory is not protected");
        }
    }
    Ok(())
}

async fn serve(config: WorkerConfig) -> Result<(), &'static str> {
    // Dedicated OS threads do not keep Tokio alive on an abandoned stdin/stdout pipe.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let result = (|| {
            let mut header = [0u8; 4];
            let mut input = std::io::stdin().lock();
            input
                .read_exact(&mut header)
                .map_err(|_| "invalid worker input")?;
            let length = u32::from_be_bytes(header) as usize;
            if length == 0 || length > 16 * 1024 {
                return Err("worker input exceeds limit");
            }
            let mut bytes = vec![0; length + 4];
            bytes[..4].copy_from_slice(&header);
            input
                .read_exact(&mut bytes[4..])
                .map_err(|_| "invalid worker input")?;
            Ok(bytes)
        })();
        let _ = sender.send(result);
    });
    let bytes = tokio::time::timeout(Duration::from_secs(10), receiver)
        .await
        .map_err(|_| "worker input timeout")?
        .map_err(|_| "worker input failed")??;
    let mut response = Vec::new();
    serve_worker(&config, &mut bytes.as_slice(), &mut response)
        .await
        .map_err(|_| "worker validation failed")?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut output = std::io::stdout().lock();
        let result = output.write_all(&response).and_then(|_| output.flush());
        let _ = sender.send(result);
    });
    tokio::time::timeout(Duration::from_secs(10), receiver)
        .await
        .map_err(|_| "worker output timeout")?
        .map_err(|_| "worker output failed")?
        .map_err(|_| "worker output failed")
}

// Check the leaf before opening it: a protected FIFO can still block indefinitely on open.
fn configuration_component_type(kind: fs::FileType, leaf: bool) -> bool {
    if leaf { kind.is_file() } else { kind.is_dir() }
}

#[cfg(test)]
mod tests {
    use super::{check_worker_directory, command_mode, configuration_component_type};
    use std::{fs, os::unix::fs::symlink, process::Command};

    #[test]
    fn enrollment_check_cannot_be_selected_through_the_ssh_protocol() {
        // Given the local check flag, a normal worker invocation, or an unsupported command.
        let check = vec!["--check-enrollment".to_owned()];
        // When command admission receives the SSH origin independently of process arguments.
        assert_eq!(command_mode(&check, None), Ok(true));
        assert_eq!(command_mode(&[], Some("ai-sre-validator-v1")), Ok(false));
        // Then SSH cannot invoke local diagnostics or fall through from an unknown command.
        for original in [Some("ai-sre-validator-v1"), Some("anything")] {
            assert!(command_mode(&check, original).is_err());
        }
        assert!(command_mode(&[], None).is_err());
        assert!(command_mode(&[], Some("anything")).is_err());
        assert!(command_mode(&["--unknown".into()], None).is_err());
    }

    #[test]
    fn enrollment_directory_checks_preserve_state_and_reject_unsafe_permissions() {
        use std::os::unix::fs::PermissionsExt;
        // Given a private worker-owned state directory and a readable local mirror.
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("u9-preflight-paths-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("retained-state"), "existing ledger").unwrap();
        assert!(check_worker_directory(&root, true).is_ok());
        // When preflight encounters sharing, missing access, or a symlink alias.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_worker_directory(&root, true).is_err());
        assert!(check_worker_directory(&root, false).is_ok());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_worker_directory(&root, false).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o300)).unwrap();
        assert!(check_worker_directory(&root, true).is_err());
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.join("alias");
        symlink(&root, &link).unwrap();
        assert!(check_worker_directory(&link, false).is_err());
        // Then no ownership database or request ledger was created or rewritten.
        assert_eq!(
            fs::read_to_string(root.join("retained-state")).unwrap(),
            "existing ledger"
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configuration_requires_a_regular_leaf_and_directory_ancestors() {
        // Given ordinary configuration data and special files that must never be opened as config.
        let root = std::env::temp_dir().join(format!("ai-sre-config-types-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("config"), "{}").unwrap();
        symlink(root.join("config"), root.join("link")).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(root.join("fifo"))
                .status()
                .unwrap()
                .success()
        );
        // When the same type gate used before configuration open examines each path component.
        let kind = |name: &str| fs::symlink_metadata(root.join(name)).unwrap().file_type();
        // Then only a regular leaf and directory ancestors qualify, without opening the FIFO.
        assert!(configuration_component_type(kind("config"), true));
        assert!(!configuration_component_type(kind("config"), false));
        assert!(configuration_component_type(kind(""), false));
        for name in ["", "link", "fifo"] {
            assert!(!configuration_component_type(kind(name), true));
        }
        fs::remove_dir_all(root).unwrap();
    }
}
