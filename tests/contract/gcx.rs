//! Contract tests for the bounded Grafana `gcx` process boundary.

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use ai_sre::adapters::grafana::gcx::{GcxQuery, GcxRunError, GcxRunner, QueryKind};

fn fixture_script(body: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ai-sre-gcx-contract-{}-{}.sh",
        std::process::id(),
        body.len()
    ));
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fixture script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .expect("make fixture script executable");
    path
}

#[test]
fn metrics_query_builds_only_the_fixed_read_only_subcommand() {
    // Given a PromQL expression and the policy-selected Prometheus datasource.
    let query = GcxQuery::metrics("rate(http_requests_total[5m])", "prometheus");

    // When the typed request is lowered to the child-process argument vector.
    let argv = query.argv();

    // Then it matches gcx's documented metrics-query syntax and fixed capability.
    assert_eq!(
        argv,
        vec![
            "metrics".to_owned(),
            "query".to_owned(),
            "rate(http_requests_total[5m])".to_owned(),
            "-d".to_owned(),
            "prometheus".to_owned(),
        ]
    );
    assert_eq!(query.kind(), QueryKind::Metrics);
}

#[test]
fn logs_query_cannot_select_an_arbitrary_gcx_command() {
    // Given a LogQL expression and the policy-selected Loki datasource.
    let query = GcxQuery::logs("{app=\"api\"}", "loki");

    // When the typed request is lowered to the child-process argument vector.
    let argv = query.argv();

    // Then only the documented logs-query subcommand is present, never gcx api.
    assert_eq!(argv.first().map(String::as_str), Some("logs"));
    assert_eq!(argv.get(1).map(String::as_str), Some("query"));
    assert!(!argv.iter().any(|argument| argument == "api"));
    assert_eq!(query.kind(), QueryKind::Logs);
}

#[test]
fn model_authored_queries_are_rejected_before_any_process_is_spawned() {
    // Given a query containing shell syntax and an arbitrary datasource URL.
    let query = GcxQuery::logs("{app=\"api\"}; cat /etc/passwd", "https://evil.example");

    // When the read-only query policy validates the request.
    let result = query.validate(4_096);

    // Then policy rejects it before the process boundary is reachable.
    assert_eq!(result, Err(GcxRunError::InvalidQuery));
}

#[test]
fn valid_logql_pipes_and_promql_selectors_remain_data() {
    // Given ordinary LogQL and PromQL syntax that includes operators.
    let logs = GcxQuery::logs("{app=\"api\"} |= \"error\"", "loki");
    let metrics = GcxQuery::metrics("rate(http_requests_total{job=\"api\"}[5m])", "prometheus");

    // When the bounded query policy validates both expressions.
    let logs_result = logs.validate(4_096);
    let metrics_result = metrics.validate(4_096);

    // Then query-language operators are accepted as data, not shell syntax.
    assert_eq!(logs_result, Ok(()));
    assert_eq!(metrics_result, Ok(()));
}

#[tokio::test]
async fn runner_returns_bounded_stdout_from_a_successful_process() {
    // Given a harmless fixture process and a small output budget.
    let binary = fixture_script("printf 'structured-result'");
    let runner = GcxRunner::new(&binary, Duration::from_secs(1), 64);

    // When the typed metrics query is executed without a shell.
    let result = runner.run(&GcxQuery::metrics("up", "prometheus")).await;

    // Then the bounded stdout is returned and the process succeeds.
    assert_eq!(
        result.expect("fixture should succeed").stdout,
        b"structured-result"
    );
    fs::remove_file(binary).expect("remove fixture script");
}

#[tokio::test]
async fn runner_rejects_output_that_exceeds_the_configured_limit() {
    // Given a fixture process whose output is larger than the allowed budget.
    let binary = fixture_script("printf '0123456789'");
    let runner = GcxRunner::new(&binary, Duration::from_secs(1), 4);

    // When the query is executed.
    let result = runner.run(&GcxQuery::logs("{app=\"api\"}", "loki")).await;

    // Then execution fails closed before oversized output enters context.
    assert_eq!(result, Err(GcxRunError::OutputLimitExceeded));
    fs::remove_file(binary).expect("remove fixture script");
}

#[tokio::test]
async fn runner_times_out_and_does_not_leak_child_work() {
    // Given a fixture process that sleeps beyond the incident tool deadline.
    let binary = fixture_script("sleep 1");
    let runner = GcxRunner::new(&binary, Duration::from_millis(10), 64);

    // When the query is executed.
    let result = runner.run(&GcxQuery::metrics("up", "prometheus")).await;

    // Then the child is terminated and the caller receives a timeout classification.
    assert_eq!(result, Err(GcxRunError::TimedOut));
    fs::remove_file(binary).expect("remove fixture script");
}

#[tokio::test]
async fn runner_classifies_nonzero_exit_without_returning_stderr_secrets() {
    // Given a fixture process that writes a secret-looking stderr value and exits 7.
    let binary = fixture_script("printf 'token=secret' >&2; exit 7");
    let runner = GcxRunner::new(&binary, Duration::from_secs(1), 64);

    // When the query is executed.
    let result = runner.run(&GcxQuery::logs("{app=\"api\"}", "loki")).await;

    // Then only the exit classification is exposed to the reasoning layer.
    assert_eq!(result, Err(GcxRunError::NonZeroExit(7)));
    fs::remove_file(binary).expect("remove fixture script");
}
