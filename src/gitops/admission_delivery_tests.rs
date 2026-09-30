//! Tests the homelab Python receiver against Rust admission using isolated synthetic receipts.

use super::*;
use crate::gitops::{inbox::InboxLimits, receipt::ReceiptWire};
use std::{fs, io, path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, Command},
};

// Cover pipe backpressure and process exit with one deadline, then confirm child cleanup.
async fn bounded_delivery(child: &mut Child, bytes: &[u8], deadline: Duration) -> io::Result<bool> {
    let delivery = tokio::time::timeout(deadline, async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing delivery stdin"))?;
        stdin.write_all(bytes).await?;
        drop(stdin);
        Ok(child.wait().await?.success())
    })
    .await;
    match delivery {
        Ok(Ok(success)) => Ok(success),
        failure => {
            tokio::time::timeout(Duration::from_secs(3), child.kill())
                .await
                .map_err(|_| io::Error::other("delivery child cleanup unconfirmed"))??;
            match failure {
                Ok(Err(error)) => Err(error),
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "receipt delivery deadline exceeded",
                )),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

#[tokio::test]
async fn stalled_delivery_input_times_out_and_reaps_the_child() {
    // Given a direct child that never reads stdin and a payload larger than its pipe capacity.
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // When delivery hits its deadline while writing the payload, not just while waiting for exit.
    let result = bounded_delivery(
        &mut child,
        &vec![b'x'; 2 * 1024 * 1024],
        Duration::from_millis(100),
    )
    .await;
    // Then timeout is explicit and cleanup has reaped the child before returning.
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(child.try_wait().unwrap().is_some());
}

#[tokio::test]
async fn stalled_delivery_exit_times_out_and_reaps_the_child() {
    // Given a child that accepts the empty input but does not exit within the delivery deadline.
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // When input has finished and waiting for process exit consumes the deadline.
    let result = bounded_delivery(&mut child, b"", Duration::from_millis(100)).await;
    // Then the same bound covers exit and confirms child cleanup before returning.
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(child.try_wait().unwrap().is_some());
}

#[tokio::test]
async fn completed_delivery_preserves_success_and_failure_status() {
    // Given ordinary child success and rejection, neither of which should become a timeout.
    for (binary, expected) in [("/bin/true", true), ("/bin/false", false)] {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // When the bounded delivery waits for the actual exit with an empty input.
        let result = bounded_delivery(&mut child, b"", Duration::from_secs(3)).await;
        // Then exit status is preserved and the child is reaped.
        assert_eq!(result.unwrap(), expected);
        assert!(child.try_wait().unwrap().is_some());
    }
}

#[tokio::test]
#[ignore = "requires root in a disposable Linux VM and U9_RECEIPT_PRODUCER"]
async fn python_delivery_preserves_revocation_for_rust_admission() {
    assert!(rustix::process::geteuid().is_root());
    // Given the actual homelab receiver and a protected disposable destination.
    let producer = PathBuf::from(std::env::var_os("U9_RECEIPT_PRODUCER").unwrap());
    assert!(producer.is_absolute());
    let root = PathBuf::from(format!("/root/u9-python-delivery-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("receipts")).unwrap();
    let inbox = ReceiptInbox::new(root.join("receipts"), InboxLimits::default()).unwrap();
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let original = crate::gitops::receipt::fixture().wire;
    let bundle = |receipts: &[ReceiptWire]| {
        serde_json::json!({
            "checkpoint": {
                "schema":"ai-sre/repair-checkpoint/v1", "repository":"bodhispace-xyz/bodhispace-homelab",
                "base":"a".repeat(40), "deployment_id":original.deployment_id,
                "observed_at":at, "expires_at":at+300,
                "receipt_digests":receipts.iter().map(ReceiptWire::digest).collect::<Vec<_>>()
            },
            "receipts":receipts
        })
    };
    let deliver = |value: serde_json::Value| {
        let mut child = Command::new("/usr/bin/python3")
            .arg(&producer)
            .arg("deliver")
            .arg(&root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        async move {
            bounded_delivery(
                &mut child,
                &serde_json::to_vec(&value).unwrap(),
                Duration::from_secs(30),
            )
            .await
            .unwrap()
        }
    };
    let initial = bundle(std::slice::from_ref(&original));
    assert!(deliver(initial.clone()).await);
    assert_eq!(
        read(&root.join("checkpoint.json"), &inbox, at, 300)
            .unwrap()
            .receipts
            .len(),
        1
    );

    // When trusted delivery adds a revocation, then attempts to replay the earlier bundle.
    let mut revoked = original.clone();
    revoked.revoked = true;
    let current = bundle(&[original.clone(), revoked]);
    assert!(deliver(current).await);
    let checkpoint = fs::read(root.join("checkpoint.json")).unwrap();
    assert!(!deliver(initial).await);

    // Then Rust still sees the complete revoked set and the checkpoint has not rolled back.
    assert_eq!(fs::read(root.join("checkpoint.json")).unwrap(), checkpoint);
    let admission = read(&root.join("checkpoint.json"), &inbox, at, 300).unwrap();
    assert_eq!(admission.receipts.len(), 2);
    assert!(
        admission
            .receipts
            .iter()
            .any(|receipt| receipt.wire.revoked)
    );
    assert!(read(&root.join("checkpoint.json"), &inbox, at + 300, 300).is_err());
    fs::remove_dir_all(root).unwrap();
}
