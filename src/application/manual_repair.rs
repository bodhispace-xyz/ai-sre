//! Coordinates remote validation and durable manual handoff without granting GitHub write authority.
//!
//! The caller supplies journal-derived qualification and deployment-owned repository/worker policy.
//! Missing qualification, changed source, transport failure, and stale evidence remain non-ready.

use crate::{
    gitops::{
        artifact::ManualRepairCandidate,
        handoff::{HandoffPolicy, ManualHandoffRequest, ManualRepairHandoff},
        sandbox::{RemoteJob, SandboxError, SshValidator},
        snapshot::RepositorySnapshot,
    },
    reasoning::{journal::JournalContext, storage::JournalStore},
};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Trusted incident selection; source, replacement image, and evidence digest are derived internally.
pub struct IncidentRepairRequest<'a> {
    /// Protected deployment completion to resolve in the journal.
    pub deployment_id: &'a str,
    /// Server-owned incident/run scope whose durable evidence must exist.
    pub context: &'a JournalContext,
    /// Deployment-owned read-only local mirror, never a model-selected path.
    pub repository: &'a Path,
    /// Immutable base observed by the caller's trusted repository adapter.
    pub base: &'a str,
}

/// Rebuilds a candidate from current qualification, scoped evidence, and committed source.
/// Repeated preparation is idempotent; it never restores authority by deserializing candidate JSON.
pub async fn prepare_incident_repair(
    journal: &mut JournalStore,
    request: &IncidentRepairRequest<'_>,
) -> Result<Option<ManualRepairCandidate>, SandboxError> {
    let Some(qualified) = journal
        .qualified_deployment(request.deployment_id, now()?)
        .map_err(|_| SandboxError::Failed)?
    else {
        return Ok(None);
    };
    let Some(evidence) = journal
        .repair_evidence_digest(request.context)
        .map_err(|_| SandboxError::Failed)?
    else {
        return Ok(None);
    };
    let snapshot = RepositorySnapshot::capture(request.repository, request.base, qualified.image())
        .await
        .map_err(|_| SandboxError::Failed)?;
    journal
        .prepare_manual_candidate(&crate::gitops::artifact::ManualRepairRequest {
            deployment_id: request.deployment_id,
            incident_id: &request.context.incident_id,
            run_id: &request.context.run_id,
            expected_base: request.base,
            current_base: snapshot.base_sha(),
            evidence_digest: &evidence,
            source: snapshot.original_source(),
            now: now()?,
        })
        .map_err(|_| SandboxError::Failed)
}

/// Runs the explicit incident-to-manual-handoff flow. Not connected to production alert intake.
/// Missing authority remains recommendation-only; failures retain the existing durable attempt.
pub async fn run_incident_repair(
    journal: &mut JournalStore,
    worker: &mut SshValidator,
    request: &IncidentRepairRequest<'_>,
    policy: &HandoffPolicy,
) -> Result<Option<ManualRepairHandoff>, SandboxError> {
    let Some(candidate) = prepare_incident_repair(journal, request).await? else {
        return Ok(None);
    };
    validate_manual_repair(
        journal,
        worker,
        ManualValidationRequest {
            candidate: &candidate,
            context: request.context,
            repository: request.repository,
            base: request.base,
            policy,
        },
    )
    .await
}

/// Server-owned inputs for one explicit, previously prepared manual repair candidate.
pub struct ManualValidationRequest<'a> {
    /// Candidate already committed to the incident journal.
    pub candidate: &'a ManualRepairCandidate,
    /// Original incident/run binding.
    pub context: &'a JournalContext,
    /// Deployment-owned local read-only mirror, kept fresh by an external trusted synchronizer.
    pub repository: &'a Path,
    /// Exact base observed with the candidate's evidence.
    pub base: &'a str,
    /// Independently enrolled validation policy.
    pub policy: &'a HandoffPolicy,
}

/// Validates remotely, rechecks the local committed source, then atomically journals operator handoff.
/// This function does not fetch Git, publish artifacts, open PRs, or mutate deployment state.
/// Await it to completion; cancellation never grants artifact readiness.
pub async fn validate_manual_repair(
    journal: &mut JournalStore,
    worker: &mut SshValidator,
    request: ManualValidationRequest<'_>,
) -> Result<Option<ManualRepairHandoff>, SandboxError> {
    let artifact = request.candidate.artifact_digest();
    if journal
        .manual_validation_attempt(&artifact)
        .map_err(|_| SandboxError::Failed)?
        .is_some()
    {
        return Err(SandboxError::AttemptExists);
    }
    let snapshot =
        RepositorySnapshot::capture(request.repository, request.base, request.candidate.image())
            .await
            .map_err(|_| SandboxError::Failed)?;
    let job = RemoteJob::new(request.candidate, &snapshot, request.policy, now()?)?;
    let wire = serde_json::to_string(&job).map_err(|_| SandboxError::Failed)?;
    // Commit before any SSH side effect. The unique candidate key also serializes competing callers.
    if !journal
        .reserve_manual_validation(&artifact, &wire)
        .map_err(|_| SandboxError::Failed)?
    {
        return Err(SandboxError::AttemptExists);
    }
    let receipt = receive_validation(
        journal,
        &artifact,
        &wire,
        worker.validate(&job, request.policy),
    )
    .await?;
    // Re-capture HEAD after remote work; stale/moved source cannot become a ready artifact.
    let current =
        RepositorySnapshot::capture(request.repository, request.base, request.candidate.image())
            .await
            .map_err(|_| SandboxError::Failed)?;
    if current.digest() != snapshot.digest() {
        return Err(SandboxError::Failed);
    }
    journal
        .finalize_manual_repair(&ManualHandoffRequest {
            candidate: request.candidate,
            validation: &receipt,
            context: request.context,
            current_base: current.base_sha(),
            now: now()?,
            policy: request.policy,
        })
        .map_err(|_| SandboxError::Failed)
}

fn now() -> Result<u64, SandboxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SandboxError::Unavailable)
}

// The future is the external transport boundary. Dropping it cannot run async cleanup or
// establish a remote result; the committed reservation remains the durable unknown outcome.
async fn receive_validation(
    journal: &mut JournalStore,
    artifact: &str,
    wire: &str,
    response: impl std::future::Future<
        Output = Result<crate::gitops::sandbox::ValidationReceipt, SandboxError>,
    >,
) -> Result<crate::gitops::sandbox::ValidationReceipt, SandboxError> {
    let started = tokio::time::Instant::now();
    match response.await {
        Ok(receipt) => {
            journal
                .complete_manual_validation(&receipt, wire, Some(started.elapsed()))
                .map_err(|_| SandboxError::Failed)?;
            Ok(receipt)
        }
        Err(error) => {
            journal
                .record_manual_validation_failure(artifact, wire, started.elapsed())
                .map_err(|_| SandboxError::Failed)?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests;
