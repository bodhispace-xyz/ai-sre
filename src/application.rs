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
    let (worker_failed, worker_failed_rx) = oneshot::channel();
    let mut dispatcher = IncidentDispatcher::new(JournalStore::open(journal_path)?);
    let gemini = gated_api_provider("GEMINI_API_KEY", "GEMINI_GATE", "GEMINI_PRICE_CATALOG")
        .map(GeminiClient::new);
    let deepseek = gated_api_provider(
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
        while let Some(command) = receiver.recv().await {
            let incidents = match dispatcher.process_new(command.batch) {
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
                let result = investigate_live(LiveInvestigationInput {
                    grafana: &application.grafana,
                    journal: dispatcher.journal_mut(),
                    signal: &incident,
                    runtime: &mut runtime,
                    providers: LiveProviders {
                        openai: rig_gate_accepted
                            .then_some((&application.openai_oauth, &application.openai_cache))
                            .as_ref()
                            .map(|provider| provider as &dyn LiveProvider),
                        gemini: gemini
                            .as_ref()
                            .map(|provider| provider as &dyn LiveProvider),
                        deepseek: deepseek
                            .as_ref()
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
                if result.is_ok() {
                    let outbox = result.as_ref().ok().map(|result| OutboxMessage {
                        delivery_id: format!("{}:report", result.incident_id),
                        body: crate::adapters::ntfy::render_message(result),
                    });
                    if let Err(error) =
                        dispatcher.mark_completed_with_outbox(&incident.incident_id, outbox)
                    {
                        eprintln!("incident completion journal failed: {error}");
                    }
                }
                drain_outbox(&mut dispatcher, ntfy.as_ref()).await;
            }
        }
    });

    let intake_token = env::var("AI_SRE_ALERTMANAGER_TOKEN")
        .map_err(|_| ApplicationError::MissingIntakeCredential)?;
    let intake_next_token = env::var("AI_SRE_ALERTMANAGER_NEXT_TOKEN").ok();
    let intake = AlertIntake::new(IntakeConfig::default())
        .with_bearer_tokens(intake_token, intake_next_token)
        .serve(listener, sender);
    tokio::pin!(intake);
    tokio::select! {
        result = &mut intake => result.map_err(ApplicationError::from),
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

fn gated_api_provider(key: &str, gate: &str, price_catalog: &str) -> Option<String> {
    let api_key = env::var(key).ok()?;
    (env::var(gate).ok().as_deref() == Some("accepted"))
        .then(|| env::var(price_catalog).ok())
        .flatten()?;
    Some(api_key)
}

/// Parses a configured listener address for callers that keep binding outside
/// the application service.
pub fn listener_address() -> Result<SocketAddr, ApplicationError> {
    env::var("AI_SRE_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
        .parse()
        .map_err(ApplicationError::Address)
}
