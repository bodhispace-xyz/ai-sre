//! Process entry point for the AI SRE service.
//!
//! Startup loads secret-free policy, validates it before binding network I/O,
//! and delegates webhook handling to the bounded transport module. This binary
//! owns no incident, authorization, or mutation decisions.

use std::{env, net::SocketAddr, path::PathBuf};

use ai_sre::{
    adapters::{
        llm::{deepseek::DeepSeekClient, gemini::GeminiClient},
        ntfy::{NtfyConfig, NtfyPublisher},
    },
    bootstrap,
    config::{AppConfig, ConfigLoadError},
    reasoning::{
        budget::Reservation,
        dispatcher::IncidentDispatcher,
        incident::AlertStatus,
        investigation::{LiveInvestigationInput, ShadowInvestigator, investigate_live},
        live::LiveProviders,
        runtime::IncidentRuntime,
        storage::{JournalStore, OutboxMessage},
    },
    transport::{AlertIntake, IntakeCommand, IntakeConfig},
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
enum MainError {
    #[error("configuration failed to load")]
    Config(#[from] ConfigLoadError),
    #[error("application bootstrap failed")]
    Bootstrap(#[from] bootstrap::BootstrapError),
    #[error("listener address is invalid")]
    Address(#[from] std::net::AddrParseError),
    #[error("listener could not bind")]
    Bind(#[from] std::io::Error),
    #[error("HTTP intake stopped")]
    Intake(#[from] ai_sre::transport::IntakeError),
    #[error("incident journal could not open")]
    Journal(#[from] ai_sre::reasoning::storage::JournalStoreError),
    #[error("AI_SRE_ALERTMANAGER_TOKEN must be configured")]
    MissingIntakeCredential,
    #[error("NTFY_ENDPOINT and NTFY_TOPIC must be configured")]
    MissingNotificationConfig,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), MainError> {
    let config = match env::var_os("AI_SRE_CONFIG") {
        Some(path) => AppConfig::from_path(PathBuf::from(path))?,
        None => {
            let config = AppConfig::default();
            config.validate().map_err(ConfigLoadError::Validation)?;
            config
        }
    };
    let reasoning_config = config.reasoning.clone();
    let application = bootstrap::build(config)?;
    let address = env::var("AI_SRE_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let listener = TcpListener::bind(address.parse::<SocketAddr>()?).await?;
    let journal_path = env::var_os("AI_SRE_JOURNAL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("state/ai-sre.sqlite"));
    let (sender, mut receiver) = mpsc::channel::<IntakeCommand>(64);
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
        _ => return Err(MainError::MissingNotificationConfig),
    };
    tokio::spawn(async move {
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
                        openai: (env::var("RIG_GATE").ok().as_deref() == Some("accepted"))
                            .then_some((&application.openai_oauth, &application.openai_cache)),
                        gemini: gemini.as_ref(),
                        deepseek: deepseek.as_ref(),
                    },
                    reservation: Reservation {
                        provider_calls: 1,
                        tokens: 4_000,
                        evidence_queries: 2,
                        cost_micro_usd: 250_000,
                    },
                    queries,
                    start_at_ms,
                })
                .await;
                if result.is_ok() {
                    let outbox = result.as_ref().ok().map(|result| OutboxMessage {
                        delivery_id: format!("{}:report", result.incident_id),
                        body: ai_sre::adapters::ntfy::render_message(result),
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
    let intake_token =
        env::var("AI_SRE_ALERTMANAGER_TOKEN").map_err(|_| MainError::MissingIntakeCredential)?;
    let intake_next_token = env::var("AI_SRE_ALERTMANAGER_NEXT_TOKEN").ok();
    AlertIntake::new(IntakeConfig::default())
        .with_bearer_tokens(intake_token, intake_next_token)
        .serve(listener, sender)
        .await?;
    Ok(())
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
                    .mark_outbox_delivered(&message.delivery_id, 0)
                {
                    eprintln!("notification outbox acknowledgement failed: {error}");
                }
            }
            Err(error) => eprintln!("ntfy publication failed: {error}"),
        }
    }
}

fn gated_api_provider(key: &str, gate: &str, price_catalog: &str) -> Option<String> {
    let api_key = env::var(key).ok()?;
    (env::var(gate).ok().as_deref() == Some("accepted"))
        .then(|| env::var(price_catalog).ok())
        .flatten()?;
    Some(api_key)
}
