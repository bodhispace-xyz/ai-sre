//! Fixed SSH forced-command entry point for one bounded, credential-free validation job.
//!
//! Deployment supplies a protected configuration file and restricts the dedicated SSH key.
//! Request data never selects commands, repository paths, or runtime enrollment.

use ai_sre::gitops::sandbox::{WorkerConfig, serve_worker};
use std::{
    env, fs,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::Duration,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), &'static str> {
    if env::var("SSH_ORIGINAL_COMMAND").as_deref() != Ok("ai-sre-validator-v1") {
        return Err("unsupported worker command");
    }
    let path = PathBuf::from(
        env::var_os("AI_SRE_VALIDATOR_CONFIG").ok_or("missing worker configuration")?,
    );
    if !path.is_absolute() {
        return Err("invalid worker configuration path");
    }
    for ancestor in path.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|_| "worker configuration unavailable")?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.file_type().is_symlink()
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
    let config: WorkerConfig =
        serde_json::from_slice(&bytes).map_err(|_| "invalid worker configuration")?;
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
