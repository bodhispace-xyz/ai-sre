//! Process entry point for the AI SRE service.
//!
//! Startup loads secret-free policy, validates it before binding network I/O,
//! and delegates webhook handling to the bounded transport module. This binary
//! owns no incident, authorization, or mutation decisions.

use std::{env, net::SocketAddr, path::PathBuf};

use ai_sre::{
    bootstrap,
    config::{AppConfig, ConfigLoadError},
    reasoning::{dispatcher::IncidentDispatcher, storage::JournalStore},
    transport::{AlertIntake, IntakeConfig},
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
    let _application = bootstrap::build(config)?;
    let address = env::var("AI_SRE_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let listener = TcpListener::bind(address.parse::<SocketAddr>()?).await?;
    let journal_path = env::var_os("AI_SRE_JOURNAL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/ai-sre-incidents.jsonl"));
    let (sender, mut receiver) = mpsc::channel(64);
    let mut dispatcher = IncidentDispatcher::new(JournalStore::open(journal_path)?);
    tokio::spawn(async move {
        while let Some(batch) = receiver.recv().await {
            if let Err(error) = dispatcher.process(batch) {
                eprintln!("incident dispatch stopped: {error}");
                break;
            }
        }
    });
    AlertIntake::new(IntakeConfig::default())
        .serve(listener, sender)
        .await?;
    Ok(())
}
