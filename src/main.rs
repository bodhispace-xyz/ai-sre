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
        storage::JournalStore,
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
    let gemini = env::var("GEMINI_API_KEY").ok().map(GeminiClient::new);
    let deepseek = env::var("DEEPSEEK_API_KEY").ok().map(DeepSeekClient::new);
    let ntfy = match (env::var("NTFY_ENDPOINT").ok(), env::var("NTFY_TOPIC").ok()) {
        (Some(endpoint), Some(topic)) => Some(NtfyPublisher::new(
            NtfyConfig { endpoint, topic },
            env::var("NTFY_TOKEN").ok(),
        )),
        _ => None,
    };
    tokio::spawn(async move {
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
                let result = investigate_live(LiveInvestigationInput {
                    grafana: &application.grafana,
                    journal: dispatcher.journal_mut(),
                    signal: &incident,
                    runtime: &mut runtime,
                    providers: LiveProviders {
                        openai: Some((&application.openai_oauth, &application.openai_cache)),
                        gemini: gemini.as_ref(),
                        deepseek: deepseek.as_ref(),
                    },
                    reservation: Reservation {
                        provider_calls: 1,
                        tokens: 4_000,
                        evidence_queries: 2,
                        cost_micro_usd: 0,
                    },
                    queries,
                    start_at_ms: 0,
                })
                .await;
                if let (Some(ntfy), Ok(result)) = (ntfy.as_ref(), result) {
                    if let Err(error) = ntfy.publish(&result).await {
                        eprintln!("ntfy publication failed: {error}");
                    }
                }
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
