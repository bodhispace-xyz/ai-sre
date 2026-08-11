//! Contracts for server-owned Git, deployment-history, and health context.

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use ai_sre::{
    adapters::context::{
        ContextError, ReadOnlyRunner, deployment_history, git_desired_state, health_plan,
    },
    reasoning::evidence::{EvidenceBoard, EvidenceSource},
};

fn fixture_script(body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ai-sre-context-contract-{}-{}.sh",
        std::process::id(),
        body.len()
    ));
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fixture script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .expect("make fixture script executable");
    path
}

#[test]
fn git_and_history_plans_have_fixed_read_only_shapes() {
    // Given server-owned selectors for one repository and desired-state path.
    let git = git_desired_state("/usr/bin/git", "/srv/repo", "HEAD", "services/api.yml")
        .expect("valid git plan");
    let history = deployment_history("/usr/bin/git", "/srv/repo", "services/api.yml", 500)
        .expect("valid history plan");

    // When plans are constructed by the adapter.
    let git_debug = format!("{git:?}");
    let history_debug = format!("{history:?}");

    // Then the plan is bounded and the requested history is capped.
    assert!(git_debug.contains("GitDesiredState"));
    assert!(history_debug.contains("DeploymentHistory"));
    assert!(history_debug.contains("-n50"));
}

#[test]
fn model_text_cannot_create_a_relative_or_empty_command_plan() {
    // Given untrusted values attempting to replace the server-owned executable.
    let relative = health_plan("git", vec!["status".into()], "gatus");
    let empty = health_plan("/usr/bin/gatus", vec![], " ");

    // When the typed plan boundary validates them.
    // Then both requests fail before process creation.
    assert_eq!(relative, Err(ContextError::InvalidPlan));
    assert_eq!(empty, Err(ContextError::InvalidPlan));
}

#[test]
fn git_selectors_cannot_become_options_or_escape_the_repository() {
    // Given revision and path values that attempt option injection or traversal.
    let option = git_desired_state("/usr/bin/git", "/srv/repo", "--upload-pack=evil", "api.yml");
    let traversal = git_desired_state("/usr/bin/git", "/srv/repo", "HEAD", "../secrets");

    // When the typed Git boundary validates the selectors.
    // Then both plans are rejected before Git can run.
    assert_eq!(option, Err(ContextError::InvalidPlan));
    assert_eq!(traversal, Err(ContextError::InvalidPlan));
}

#[tokio::test]
async fn successful_read_only_output_is_committed_to_the_correct_source() {
    // Given a server-owned health command that emits a bounded JSON snapshot.
    let binary = fixture_script("printf '{\"status\":\"up\"}'");
    let plan = health_plan(&binary, Vec::new(), "api").expect("valid health plan");
    let runner = ReadOnlyRunner::new(Duration::from_secs(1), 128, 1);
    let mut board = EvidenceBoard::default();

    // When the read-only runner executes the plan.
    let evidence_id = runner.execute(&mut board, &plan).await.expect("evidence");

    // Then the output is immutable evidence from the health capability.
    assert_eq!(evidence_id, "evidence-0001");
    assert_eq!(board.records()[0].source, EvidenceSource::Health);
    assert_eq!(board.records()[0].payload, br#"{"status":"up"}"#);
}
