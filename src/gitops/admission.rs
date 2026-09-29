//! Binds manual repair admission to a short-lived, protected source and receipt checkpoint.
//!
//! A trusted synchronizer attests upstream HEAD and complete receipt/revocation delivery.
//! Local Git state or a successful directory scan cannot create that attestation.

use super::{
    inbox::ReceiptInbox,
    receipt::{ProtectedReceipt, ReceiptError, read_protected},
    validate::valid_sha,
};
use serde::Deserialize;
use std::path::Path;

/// A verified local checkpoint plus the exact protected receipts it names.
pub(crate) struct Admission {
    pub base: String,
    pub deployment_id: String,
    pub receipts: Vec<ProtectedReceipt>,
    pub observed_at: u64,
    pub expires_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: String,
    repository: String,
    base: String,
    deployment_id: String,
    observed_at: u64,
    expires_at: u64,
    receipt_digests: Vec<String>,
}

impl Checkpoint {
    fn bind(
        self,
        receipts: Vec<ProtectedReceipt>,
        now: u64,
        max_age: u64,
    ) -> Result<Admission, ReceiptError> {
        let mut actual: Vec<_> = receipts.iter().map(|r| r.wire.digest()).collect();
        actual.sort();
        let mut expected = self.receipt_digests;
        expected.sort();
        if self.schema != "ai-sre/repair-checkpoint/v1"
            || self.repository != "bodhispace-xyz/bodhispace-homelab"
            || !valid_sha(&self.base)
            || !(1..=300).contains(&max_age)
            || now < self.observed_at
            || now >= self.expires_at
            || self
                .expires_at
                .checked_sub(self.observed_at)
                .is_none_or(|age| age > max_age)
            || actual != expected
            || expected.is_empty()
            || expected.windows(2).any(|pair| pair[0] == pair[1])
            || !receipts
                .iter()
                .any(|r| r.deployment_id() == self.deployment_id)
        {
            return Err(ReceiptError::InvalidFields);
        }
        Ok(Admission {
            base: self.base,
            deployment_id: self.deployment_id,
            receipts,
            observed_at: self.observed_at,
            expires_at: self.expires_at,
        })
    }
}

pub(crate) fn read(
    path: &Path,
    inbox: &ReceiptInbox,
    now: u64,
    max_age: u64,
) -> Result<Admission, ReceiptError> {
    let before = read_protected(path, 512 * 1024)?;
    let checkpoint: Checkpoint =
        serde_json::from_slice(&before).map_err(|_| ReceiptError::InvalidFields)?;
    let receipts = inbox.read()?;
    // Publication during this read requires a new attempt, never mixing two checkpoints.
    if before != read_protected(path, 512 * 1024)? {
        return Err(ReceiptError::Unprotected);
    }
    checkpoint.bind(receipts, now, max_age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint() -> Checkpoint {
        Checkpoint {
            schema: "ai-sre/repair-checkpoint/v1".into(),
            repository: "bodhispace-xyz/bodhispace-homelab".into(),
            base: "a".repeat(40),
            deployment_id: "deployment-123-1".into(),
            observed_at: 220,
            expires_at: 240,
            receipt_digests: vec![super::super::receipt::fixture().wire.digest()],
        }
    }

    #[test]
    fn receipt_checkpoint_digest_matches_the_python_producer_contract() {
        // Given the shared synthetic acceptance receipt, regardless of file whitespace.
        let wire: super::super::receipt::ReceiptWire =
            serde_json::from_str(include_str!("../../tests/fixtures/u9-sandbox/receipt.json"))
                .unwrap();
        // When Rust computes the canonical receipt identity used by producer checkpoints.
        let digest = wire.digest();
        // Then it matches the independently serialized Python producer fixture.
        assert_eq!(
            digest,
            "sha256:c7694ad5693ff4ee55d57c392d9e9755ce88787c7fe332dccfdf4161feb56352"
        );
    }

    #[test]
    fn checkpoint_requires_current_time_and_the_complete_receipt_set() {
        // Given a protected publisher's checkpoint naming one exact receipt.
        let receipt = || vec![super::super::receipt::fixture()];
        // When time or delivered evidence differs from that bounded checkpoint.
        assert!(checkpoint().bind(receipt(), 219, 30).is_err());
        assert!(checkpoint().bind(receipt(), 240, 30).is_err());
        assert!(checkpoint().bind(receipt(), 221, 10).is_err());
        assert!(checkpoint().bind(Vec::new(), 221, 30).is_err());
        let mut revoked = super::super::receipt::fixture();
        revoked.wire.revoked = true;
        assert!(checkpoint().bind(vec![revoked], 221, 30).is_err());
        // Then only a fresh checkpoint with the exact delivered set permits candidate preparation.
        let admitted = checkpoint().bind(receipt(), 221, 30).unwrap();
        assert_eq!(admitted.base, "a".repeat(40));
        assert_eq!(admitted.receipts.len(), 1);
    }

    #[test]
    fn checkpoint_rejects_duplicate_receipts_and_foreign_selection() {
        // Given valid receipts but an ambiguous set or a selected deployment absent from it.
        let mut duplicate = checkpoint();
        duplicate
            .receipt_digests
            .push(duplicate.receipt_digests[0].clone());
        let receipts = vec![
            super::super::receipt::fixture(),
            super::super::receipt::fixture(),
        ];
        assert!(duplicate.bind(receipts, 221, 30).is_err());
        let mut foreign = checkpoint();
        foreign.deployment_id = "other".into();
        // When binding selection to delivered evidence, unknown deployment identities fail closed.
        assert!(
            foreign
                .bind(vec![super::super::receipt::fixture()], 221, 30)
                .is_err()
        );
    }
}
