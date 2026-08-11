//! Canonical framing and validation for the target gateway stub.
//!
//! This parser performs no network or process I/O. It rejects arbitrary shell
//! text, unknown operations, unknown targets, malformed framing, and replayed
//! attempt identifiers before a gateway adapter can dispatch anything.

use std::collections::BTreeSet;

use thiserror::Error;

const VERSION: &str = "AI-SRE/1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Operations exposed by the gateway protocol.
pub enum GatewayOperation {
    /// Read a previously recorded target receipt.
    Receipt,
    /// Request a target-side execution.
    Execute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Validated request handed to a gateway adapter.
pub struct GatewayRequest {
    /// Requested gateway operation.
    pub operation: GatewayOperation,
    /// Unique identifier used to prevent replay.
    pub attempt_id: String,
    /// Allowlisted target name.
    pub target: String,
    /// Opaque, non-shell payload for the target adapter.
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
/// Reasons a gateway request cannot cross the protocol boundary.
pub enum GatewayError {
    #[error("gateway request framing is invalid")]
    /// The version, field count, or required field framing is invalid.
    InvalidFraming,
    #[error("gateway operation or target is not allowed")]
    /// The operation, target, or payload violates the allowlist.
    NotAllowed,
    #[error("gateway attempt identifier was already used")]
    /// The attempt identifier was already reserved by this protocol instance.
    Replay,
}

#[derive(Debug, Clone, Default)]
/// Stateful parser that reserves each accepted attempt identifier once.
pub struct GatewayProtocol {
    seen_attempts: BTreeSet<String>,
}

impl GatewayProtocol {
    /// Parse one canonical request and reserve its attempt identifier.
    pub fn parse(&mut self, input: &str) -> Result<GatewayRequest, GatewayError> {
        let fields = input.split('\t').collect::<Vec<_>>();
        if fields.len() != 5 || fields[0] != VERSION || fields.iter().any(|field| field.is_empty())
        {
            return Err(GatewayError::InvalidFraming);
        }

        let operation = match fields[1] {
            "receipt" => GatewayOperation::Receipt,
            "execute" => GatewayOperation::Execute,
            _ => return Err(GatewayError::NotAllowed),
        };
        if fields[3] != "homelab-target"
            || fields[4]
                .chars()
                .any(|character| matches!(character, '\n' | '\r' | ';' | '|' | '&'))
        {
            return Err(GatewayError::NotAllowed);
        }
        if !self.seen_attempts.insert(fields[2].to_owned()) {
            return Err(GatewayError::Replay);
        }

        Ok(GatewayRequest {
            operation,
            attempt_id: fields[2].to_owned(),
            target: fields[3].to_owned(),
            payload: fields[4].to_owned(),
        })
    }
}
