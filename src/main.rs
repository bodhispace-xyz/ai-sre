//! Thin process entry point for the AI SRE service.
//!
//! The binary loads and validates secret-free policy, binds the listener, and
//! delegates all incident workflow decisions to the application shell.

use std::{env, path::PathBuf};

use ai_sre::{
    application,
    config::{AppConfig, ConfigLoadError},
};
use thiserror::Error;
use tokio::net::TcpListener;

#[derive(Debug, Error)]
enum MainError {
    #[error("configuration failed to load")]
    Config(#[from] ConfigLoadError),
    #[error("application failed")]
    Application(#[from] application::ApplicationError),
    #[error("listener could not bind")]
    Bind(#[from] std::io::Error),
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
    let listener = TcpListener::bind(application::listener_address()?).await?;
    application::serve(config, listener).await?;
    Ok(())
}
