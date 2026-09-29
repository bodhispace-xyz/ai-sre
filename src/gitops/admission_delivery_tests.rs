//! Tests the homelab Python receiver against Rust admission using isolated synthetic receipts.

use super::*;
use crate::gitops::{inbox::InboxLimits, receipt::ReceiptWire};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

#[test]
#[ignore = "requires root in a disposable Linux VM and U9_RECEIPT_PRODUCER"]
fn python_delivery_preserves_revocation_for_rust_admission() {
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
    let deliver = |value: &serde_json::Value| {
        let mut child = Command::new("/usr/bin/python3")
            .arg(&producer)
            .arg("deliver")
            .arg(&root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(value).unwrap())
            .unwrap();
        child.wait().unwrap().success()
    };
    let initial = bundle(std::slice::from_ref(&original));
    assert!(deliver(&initial));
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
    assert!(deliver(&current));
    let checkpoint = fs::read(root.join("checkpoint.json")).unwrap();
    assert!(!deliver(&initial));

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
