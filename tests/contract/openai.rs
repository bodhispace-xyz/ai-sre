//! Contract tests for secret-safe OpenAI OAuth session handling.

use std::{fs, os::unix::fs::PermissionsExt};

use ai_sre::adapters::llm::openai::{
    AuthCache, CacheError, RefreshFailure, RefreshSession, classify_rig_status,
};

#[test]
fn refresh_session_rotates_without_exposing_tokens_in_debug_output() {
    // Given a refresh session loaded from a deployment-projected credential.
    let mut session = RefreshSession::new("refresh-token-before");

    // When the provider returns a replacement refresh token.
    session.rotate("refresh-token-after");

    // Then debugging the session reveals neither the old nor new secret.
    let debug = format!("{session:?}");
    assert!(!debug.contains("refresh-token-before"));
    assert!(!debug.contains("refresh-token-after"));
}

#[test]
fn refresh_failure_is_classified_without_vendor_error_text() {
    // Given a provider rejection that requires an interactive reauthentication.
    let failure = RefreshFailure::ReauthenticationRequired;

    // When the failure crosses the adapter boundary.
    let debug = format!("{failure:?}");

    // Then only the safe classification is observable by core logic.
    assert_eq!(failure, RefreshFailure::ReauthenticationRequired);
    assert!(!debug.contains("token"));
}

#[test]
fn cache_replaces_refresh_tokens_atomically_and_reloads_them() {
    // Given a private cache path and a newly rotated refresh session.
    let directory = std::env::current_dir()
        .expect("current directory")
        .join("target")
        .join(format!("ai-sre-openai-cache-{}", std::process::id()));
    let path = directory.join("auth.json");
    let cache = AuthCache::new(&path);

    // When the session is stored and loaded again.
    cache
        .store(&RefreshSession::new("rotated-refresh-token"))
        .expect("store cache");
    let loaded = cache
        .load()
        .expect("load cache")
        .expect("cache should exist");

    // Then the token is recoverable, private, and no temporary file remains.
    assert_eq!(loaded, RefreshSession::new("rotated-refresh-token"));
    assert_eq!(
        fs::metadata(&path)
            .expect("cache metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_dir(&directory).expect("cache directory").count(),
        1
    );
    fs::remove_dir_all(directory).expect("remove cache fixture");
}

#[test]
fn malformed_cache_fails_closed_without_returning_secret_content() {
    // Given a cache file containing malformed JSON and a secret-looking value.
    let directory = std::env::current_dir()
        .expect("current directory")
        .join("target")
        .join(format!("ai-sre-openai-invalid-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("create cache directory");
    let path = directory.join("auth.json");
    fs::write(&path, br#"{\"refresh_token\": "secret"}"#).expect("write malformed cache");

    // When the cache is loaded.
    let result = AuthCache::new(&path).load();

    // Then only the safe invalid-cache classification is returned.
    assert_eq!(result, Err(CacheError::Invalid));
    fs::remove_dir_all(directory).expect("remove cache fixture");
}

#[test]
fn rig_http_statuses_map_to_safe_openai_failure_classes() {
    // Given representative Rig HTTP outcomes from the provider boundary.
    let statuses = [Some(401), Some(429), Some(503), None];

    // When statuses are classified without retaining response bodies.
    let failures = statuses
        .iter()
        .map(|status| classify_rig_status(*status))
        .collect::<Vec<_>>();

    // Then authentication and transient failures stay distinct and secret-free.
    assert_eq!(
        failures[0],
        ai_sre::reasoning::coordinator::FailureClass::AuthenticationRequired
    );
    assert_eq!(
        failures[1],
        ai_sre::reasoning::coordinator::FailureClass::RateLimited
    );
    assert!(failures[2..].iter().all(|failure| {
        *failure == ai_sre::reasoning::coordinator::FailureClass::TemporarilyUnavailable
    }));
}
