//! Contract tests for secret-safe OpenAI OAuth session handling.

use ai_sre::adapters::llm::openai::{RefreshFailure, RefreshSession};

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
