//! Contracts for server-owned Git, deployment-history, and health context.

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use ai_sre::{
    adapters::context::{
        ContextError, ReadOnlyContext, ReadOnlyContextConfig, ReadOnlyRunner, deployment_history,
        git_desired_state, health_plan,
    },
    reasoning::evidence::{EvidenceBoard, EvidenceSource},
    reasoning::tools::{ContextTool, ToolCall, ToolResultClass, parse_tool_call},
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

#[tokio::test]
async fn context_facade_keeps_health_aliases_and_git_repository_server_owned() {
    // Given a facade configured with one canonical repository and health alias.
    let binary = fixture_script("printf '{\"status\":\"up\"}'");
    let mut health_commands = BTreeMap::new();
    health_commands.insert("api".to_owned(), Vec::new());
    let context = ReadOnlyContext::new(
        ReadOnlyRunner::new(Duration::from_secs(1), 128, 1),
        ReadOnlyContextConfig {
            git_binary: binary.clone(),
            git_repository: "/srv/repo".to_owned(),
            health_binary: binary,
            health_commands,
        },
    )
    .expect("valid context config");
    let mut board = EvidenceBoard::default();

    // When the configured health alias is requested and an unknown alias is attempted.
    let known = context.health(&mut board, "api").await;
    let unknown = context.health(&mut board, "admin").await;

    // Then only the server-owned alias can reach the process boundary.
    assert!(known.is_ok());
    assert_eq!(unknown, Err(ContextError::InvalidPlan));
    assert_eq!(board.records()[0].source, EvidenceSource::Health);
}

#[test]
fn provider_names_map_to_the_read_only_context_capabilities() {
    // Given the three non-Grafana context tool names exposed to providers.
    let desired = parse_tool_call("d", "read_desired_state", r#"{"query":"HEAD\napi.yml"}"#);
    let history = parse_tool_call("h", "read_deployment_history", r#"{"query":"api.yml"}"#);
    let health = parse_tool_call("c", "read_health", r#"{"query":"api"}"#);
    let discover = parse_tool_call("o", "discover_observability", r#"{"query":"overview"}"#);

    // When provider metadata crosses the strict tool parser.
    // Then each name maps to an allowlisted capability with no command fields.
    assert_eq!(
        desired.expect("desired").tool,
        ContextTool::ReadDesiredState
    );
    assert_eq!(
        history.expect("history").tool,
        ContextTool::ReadDeploymentHistory
    );
    assert_eq!(health.expect("health").tool, ContextTool::ReadHealth);
    assert_eq!(
        discover.expect("discover").tool,
        ContextTool::DiscoverObservability
    );
}

#[tokio::test]
async fn context_tool_execution_rejects_grafana_capabilities_at_the_wrong_boundary() {
    // Given a server-owned context facade and a Grafana query routed to it accidentally.
    let binary = fixture_script("printf 'unused'");
    let context = ReadOnlyContext::new(
        ReadOnlyRunner::new(Duration::from_secs(1), 128, 1),
        ReadOnlyContextConfig {
            git_binary: binary.clone(),
            git_repository: "/srv/repo".to_owned(),
            health_binary: binary,
            health_commands: BTreeMap::new(),
        },
    )
    .expect("valid context config");
    let mut board = EvidenceBoard::default();
    let result = context
        .execute_tool(
            &mut board,
            &ToolCall {
                call_id: "logs".to_owned(),
                tool: ContextTool::QueryLogs,
                query: "{app=\"api\"}".to_owned(),
            },
        )
        .await;

    // Then the adapter refuses cross-capability dispatch without process execution.
    assert_eq!(result.class, ToolResultClass::Denied);
    assert!(board.records().is_empty());
}

#[tokio::test]
async fn failed_health_reads_leave_explicit_failure_evidence() {
    // Given a configured but unknown health alias.
    let binary = fixture_script("printf 'unused'");
    let context = ReadOnlyContext::new(
        ReadOnlyRunner::new(Duration::from_secs(1), 128, 1),
        ReadOnlyContextConfig {
            git_binary: binary.clone(),
            git_repository: "/srv/repo".to_owned(),
            health_binary: binary,
            health_commands: BTreeMap::new(),
        },
    )
    .expect("valid context config");
    let mut board = EvidenceBoard::default();
    let call = ToolCall {
        call_id: "health-1".to_owned(),
        tool: ContextTool::ReadHealth,
        query: "api".to_owned(),
    };

    // When the adapter rejects the alias before process execution.
    let result = context.execute_tool(&mut board, &call).await;

    // Then the failure remains explicit, bounded, and attributable to health.
    assert_eq!(result.class, ToolResultClass::Denied);
    let record = &board.records()[0];
    assert_eq!(record.source, EvidenceSource::Health);
    assert_eq!(
        record.metadata.status,
        ai_sre::reasoning::evidence::EvidenceStatus::Rejected
    );
}
