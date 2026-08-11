//! GIVEN/WHEN/THEN contracts for the disposable typed gateway protocol.

use ai_sre::adapters::gateway::protocol::{GatewayError, GatewayOperation, GatewayProtocol};

#[test]
fn gateway_accepts_one_canonical_typed_request_and_rejects_replay() {
    // Given a fresh gateway protocol state and one canonical receipt request.
    let mut protocol = GatewayProtocol::default();
    let request = "AI-SRE/1\treceipt\tattempt-001\thomelab-target\tread-receipt";

    // When the request is parsed twice.
    let first = protocol.parse(request).expect("canonical request");
    let replay = protocol.parse(request);

    // Then the first request is typed and the replay is rejected.
    assert_eq!(first.operation, GatewayOperation::Receipt);
    assert_eq!(replay, Err(GatewayError::Replay));
}

#[test]
fn gateway_rejects_shell_syntax_unknown_targets_and_bad_framing() {
    // Given requests attempting shell syntax, an unknown target, and bad framing.
    let cases = [
        "AI-SRE/1\texecute\tattempt-002\thomelab-target\tdeploy; rm -rf /",
        "AI-SRE/1\treceipt\tattempt-003\tunknown-target\tread",
        "AI-SRE/1\treceipt\tattempt-004\thomelab-target",
    ];

    // When each request crosses the parser boundary.
    let mut protocol = GatewayProtocol::default();
    let results = cases
        .iter()
        .map(|request| protocol.parse(request))
        .collect::<Vec<_>>();

    // Then no request reaches a target adapter.
    assert_eq!(results[0], Err(GatewayError::NotAllowed));
    assert_eq!(results[1], Err(GatewayError::NotAllowed));
    assert_eq!(results[2], Err(GatewayError::InvalidFraming));
}
