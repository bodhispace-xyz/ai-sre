//! Application orchestration for the shadow incident workflow.
//!
//! This shell owns dependency wiring, queue supervision, provider admission
//! policy, and notification delivery. Core reasoning modules remain focused
//! on typed state transitions and do not need vendor adapter types.

use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use thiserror::Error;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};

use crate::{
    adapters::{
        llm::{deepseek::DeepSeekClient, gemini::GeminiClient},
        ntfy::{NtfyConfig, NtfyPublisher},
    },
    bootstrap,
    config::AppConfig,
    observability::MetricsSnapshot,
    reasoning::{
        budget::Reservation,
        dispatcher::IncidentDispatcher,
        incident::AlertStatus,
        investigation::{LiveInvestigationInput, ShadowInvestigator, investigate_live},
        live::{LiveProvider, LiveProviders},
        runtime::IncidentRuntime,
        storage::{JournalStore, OutboxMessage},
    },
    transport::{AlertIntake, IntakeCommand, IntakeConfig},
};

/// Application startup and worker failures after configuration parsing.
#[derive(Debug, Error)]
pub enum ApplicationError {
    /// Dependency assembly failed.
    #[error("application bootstrap failed")]
    Bootstrap(#[from] bootstrap::BootstrapError),
    /// The listener address could not be parsed.
    #[error("listener address is invalid")]
    Address(#[from] std::net::AddrParseError),
    /// The listener could not bind.
    #[error("listener could not bind")]
    Bind(#[from] std::io::Error),
    /// The durable journal could not open.
    #[error("incident journal could not open")]
    Journal(#[from] crate::reasoning::storage::JournalStoreError),
    /// The HTTP intake stopped.
    #[error("HTTP intake stopped")]
    Intake(#[from] crate::transport::IntakeError),
    /// Intake authentication was not configured.
    #[error("AI_SRE_ALERTMANAGER_TOKEN must be configured")]
    MissingIntakeCredential,
    /// Operator notification delivery was not configured.
    #[error("NTFY_ENDPOINT and NTFY_TOPIC must be configured")]
    MissingNotificationConfig,
    /// The incident worker stopped after a durable processing failure.
    #[error("incident worker stopped after a durable processing failure")]
    WorkerFailed,
}

/// Builds dependencies and runs the supervised shadow worker.
pub async fn serve(config: AppConfig, listener: TcpListener) -> Result<(), ApplicationError> {
    let reasoning_config = config.reasoning.clone();
    let application = bootstrap::build(config)?;
    let journal_path = env::var_os("AI_SRE_JOURNAL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("state/ai-sre.sqlite"));
    let (sender, mut receiver) = mpsc::channel::<IntakeCommand>(64);
    let metrics = MetricsSnapshot::default();
    let worker_metrics = metrics.clone();
    let (worker_failed, worker_failed_rx) = oneshot::channel();
    let mut dispatcher = IncidentDispatcher::new(JournalStore::open(journal_path)?);
    let gemini = gated_api_provider(
        "gemini",
        "gemini-2.5-flash",
        "GEMINI_API_KEY",
        "GEMINI_GATE",
        "GEMINI_PRICE_CATALOG",
    )
    .map(GeminiClient::new);
    let deepseek = gated_api_provider(
        "deepseek",
        "deepseek-chat",
        "DEEPSEEK_API_KEY",
        "DEEPSEEK_GATE",
        "DEEPSEEK_PRICE_CATALOG",
    )
    .map(DeepSeekClient::new);
    let ntfy = match (env::var("NTFY_ENDPOINT").ok(), env::var("NTFY_TOPIC").ok()) {
        (Some(endpoint), Some(topic)) => Some(NtfyPublisher::new(
            NtfyConfig { endpoint, topic },
            env::var("NTFY_TOKEN").ok(),
        )),
        _ => return Err(ApplicationError::MissingNotificationConfig),
    };
    let rig_gate_accepted = env::var("RIG_GATE").ok().as_deref() == Some("accepted");

    let worker = tokio::spawn(async move {
        drain_outbox(&mut dispatcher, ntfy.as_ref()).await;
        worker_metrics
            .replace_from(dispatcher.journal().journal())
            .await;
        let pending = dispatcher.pending_investigations();
        let mut startup_command = (!pending.is_empty()).then(|| {
            let (acknowledged, _ignored_ack) = oneshot::channel();
            IntakeCommand {
                batch: crate::transport::IntakeBatch { incidents: pending },
                acknowledged,
            }
        });
        'worker: loop {
            let startup = startup_command.is_some();
            let Some(command) = (match startup_command.take() {
                Some(command) => Some(command),
                None => receiver.recv().await,
            }) else {
                break;
            };
            let incidents = match if startup {
                Ok(command.batch.incidents)
            } else {
                dispatcher.process_new(command.batch)
            } {
                Ok(incidents) => {
                    let _ = command.acknowledged.send(Ok(()));
                    incidents
                }
                Err(error) => {
                    let _ = command.acknowledged.send(Err(()));
                    eprintln!("incident dispatch stopped: {error}");
                    let _ = worker_failed.send(());
                    break;
                }
            };
            worker_metrics
                .replace_from(dispatcher.journal().journal())
                .await;
            for incident in incidents {
                if !matches!(incident.status, AlertStatus::Firing) {
                    continue;
                }
                let service = incident
                    .labels
                    .get("service")
                    .map(String::as_str)
                    .unwrap_or("unknown");
                let Some(queries) = ShadowInvestigator::queries_for_service(service) else {
                    continue;
                };
                let mut runtime = match IncidentRuntime::new(reasoning_config.clone()) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!("incident runtime failed: {error}");
                        continue;
                    }
                };
                let start_at_ms = runtime.elapsed_ms();
                let run_id = format!("{}-{}", incident.incident_id, now_ms());
                let global_budget_admitted = reserve_paid_budget(
                    dispatcher.journal_mut(),
                    &run_id,
                    &reasoning_config,
                    gemini.is_some() as u64 + deepseek.is_some() as u64,
                );
                let paid_provider_count = gemini.is_some() as u64 + deepseek.is_some() as u64;
                let result = investigate_live(LiveInvestigationInput {
                    grafana: &application.grafana,
                    read_only: Some(&application.read_only),
                    journal: dispatcher.journal_mut(),
                    signal: &incident,
                    run_id: &run_id,
                    runtime: &mut runtime,
                    providers: LiveProviders {
                        openai: rig_gate_accepted
                            .then_some((&application.openai_oauth, &application.openai_cache))
                            .as_ref()
                            .map(|provider| provider as &dyn LiveProvider),
                        gemini: global_budget_admitted
                            .then_some(gemini.as_ref())
                            .flatten()
                            .map(|provider| provider as &dyn LiveProvider),
                        deepseek: global_budget_admitted
                            .then_some(deepseek.as_ref())
                            .flatten()
                            .map(|provider| provider as &dyn LiveProvider),
                    },
                    reservation: Reservation {
                        provider_calls: 1,
                        tokens: 4_000,
                        evidence_queries: 0,
                        cost_micro_usd: 250_000,
                    },
                    queries,
                    start_at_ms,
                })
                .await;
                if paid_provider_count > 0 && global_budget_admitted {
                    let actual_cost = runtime.journal().project();
                    let actual_cost = (actual_cost.unknown_cost_attempts == 0)
                        .then_some(actual_cost.known_cost_micro_usd);
                    if let Err(error) = dispatcher
                        .journal_mut()
                        .reconcile_cost(&run_id, actual_cost)
                    {
                        eprintln!("cost reservation reconciliation failed: {error}");
                        let _ = worker_failed.send(());
                        break 'worker;
                    }
                }
                if result.is_ok() {
                    let outbox = result.as_ref().ok().map(|result| OutboxMessage {
                        delivery_id: format!("{}:report", result.incident_id),
                        body: crate::adapters::ntfy::render_message(result),
                    });
                    if let Err(error) =
                        dispatcher.mark_completed_with_outbox(&incident.incident_id, outbox)
                    {
                        eprintln!("incident completion journal failed: {error}");
                        let _ = worker_failed.send(());
                        break 'worker;
                    }
                }
                drain_outbox(&mut dispatcher, ntfy.as_ref()).await;
                worker_metrics
                    .replace_from(dispatcher.journal().journal())
                    .await;
            }
        }
    });

    let intake_token = env::var("AI_SRE_ALERTMANAGER_TOKEN")
        .map_err(|_| ApplicationError::MissingIntakeCredential)?;
    let intake_next_token = env::var("AI_SRE_ALERTMANAGER_NEXT_TOKEN").ok();
    let intake = AlertIntake::new(IntakeConfig::default())
        .with_bearer_tokens(intake_token, intake_next_token)
        .serve_with_metrics(listener, sender, metrics);
    tokio::pin!(intake);
    tokio::pin!(worker);
    tokio::select! {
        result = &mut intake => {
            worker.abort();
            result.map_err(ApplicationError::from)
        },
        result = &mut worker => {
            eprintln!("incident worker exited unexpectedly: {result:?}");
            Err(ApplicationError::WorkerFailed)
        },
        _ = worker_failed_rx => {
            worker.abort();
            Err(ApplicationError::WorkerFailed)
        }
    }
}

async fn drain_outbox(dispatcher: &mut IncidentDispatcher, ntfy: Option<&NtfyPublisher>) {
    let Some(ntfy) = ntfy else { return };
    let pending = match dispatcher.journal().pending_outbox() {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("notification outbox read failed: {error}");
            return;
        }
    };
    for message in pending {
        match ntfy.publish_message(&message.body).await {
            Ok(()) => {
                if let Err(error) = dispatcher
                    .journal_mut()
                    .mark_outbox_delivered(&message.delivery_id, now_ms())
                {
                    eprintln!("notification outbox acknowledgement failed: {error}");
                }
            }
            Err(error) => eprintln!("ntfy publication failed: {error}"),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn gated_api_provider(
    provider: &str,
    model: &str,
    key: &str,
    gate: &str,
    price_catalog: &str,
) -> Option<String> {
    let api_key = env::var(key).ok()?;
    if env::var(gate).ok().as_deref() != Some("accepted") {
        return None;
    }
    let catalog = env::var(price_catalog).ok()?;
    if !valid_price_catalog(&catalog, provider, model, now_ms()) {
        return None;
    }
    global_cost_limits()?;
    Some(api_key)
}

#[derive(Debug, Deserialize)]
struct PriceCatalog {
    provider: String,
    model: String,
    version: String,
    valid_until_ms: u64,
    micro_usd_per_1k_tokens: u64,
}

fn valid_price_catalog(raw: &str, provider: &str, model: &str, now_ms: u64) -> bool {
    let Ok(catalog) = serde_json::from_str::<PriceCatalog>(raw) else {
        return false;
    };
    catalog.provider == provider
        && catalog.model == model
        && !catalog.model.trim().is_empty()
        && !catalog.version.trim().is_empty()
        && catalog.valid_until_ms > now_ms
        && catalog.micro_usd_per_1k_tokens > 0
}

fn global_cost_limits() -> Option<(u64, u64, String, String)> {
    let daily = env::var("AI_SRE_DAILY_COST_LIMIT_MICRO_USD")
        .ok()?
        .parse::<u64>()
        .ok()?;
    let monthly = env::var("AI_SRE_MONTHLY_COST_LIMIT_MICRO_USD")
        .ok()?
        .parse::<u64>()
        .ok()?;
    let day = env::var("AI_SRE_BUDGET_DAY").ok()?;
    let month = env::var("AI_SRE_BUDGET_MONTH").ok()?;
    (daily > 0 && monthly > 0 && !day.trim().is_empty() && !month.trim().is_empty())
        .then_some((daily, monthly, day, month))
}

fn reserve_paid_budget(
    journal: &mut JournalStore,
    run_id: &str,
    reasoning_config: &crate::reasoning::coordinator::ReasoningConfig,
    paid_provider_count: u64,
) -> bool {
    let Some((daily, monthly, day, month)) = global_cost_limits() else {
        return paid_provider_count == 0;
    };
    let Some(incident_ceiling) = reasoning_config.budget.max_cost_micro_usd else {
        return false;
    };
    let amount = 250_000_u64.saturating_mul(paid_provider_count);
    if amount == 0 {
        return true;
    }
    let scopes = [
        (format!("incident:{}", run_id), incident_ceiling),
        (format!("day:{day}"), daily),
        (format!("month:{month}"), monthly),
    ];
    let scope_refs = scopes
        .iter()
        .map(|(scope, ceiling)| (scope.as_str(), *ceiling))
        .collect::<Vec<_>>();
    journal
        .reserve_cost(run_id, amount, &scope_refs, now_ms())
        .is_ok()
}

/// Parses a configured listener address for callers that keep binding outside
/// the application service.
pub fn listener_address() -> Result<SocketAddr, ApplicationError> {
    env::var("AI_SRE_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
        .parse()
        .map_err(ApplicationError::Address)
}

#[cfg(test)]
mod tests {
    use super::valid_price_catalog;

    #[test]
    fn price_catalog_requires_provider_identity_price_and_freshness() {
        // Given a catalog with explicit provider/model/version and a future expiry.
        let catalog = r#"{
            "provider":"gemini",
            "model":"gemini-2.5-flash",
            "version":"2026-08-11",
            "valid_until_ms":2000,
            "micro_usd_per_1k_tokens":40
        }"#;

        // When the admission gate validates it against the current time.
        let accepted = valid_price_catalog(catalog, "gemini", "gemini-2.5-flash", 1000);
        let wrong_provider = valid_price_catalog(catalog, "deepseek", "gemini-2.5-flash", 1000);
        let wrong_model = valid_price_catalog(catalog, "gemini", "gemini-2.0-flash", 1000);
        let expired = valid_price_catalog(catalog, "gemini", "gemini-2.5-flash", 3000);

        // Then only the fresh, provider-matching catalog is accepted.
        assert!(accepted);
        assert!(!wrong_provider);
        assert!(!wrong_model);
        assert!(!expired);
    }
}
