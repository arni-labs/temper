//! The token behind a principal, when a trusted issuer verified one.

use serde::{Deserialize, Serialize};

/// Which kind of token a trusted issuer signed, as its claims tell them apart:
/// a token naming an `agent_type` is an agent's access token, acting for the
/// human `sub`; a token with only a `sub` is the human's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// The human's own token.
    Human,
    /// An agent's access token, acting for a human.
    Access,
}

impl TokenKind {
    /// The kind's name, as it is serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Access => "access",
        }
    }
}

/// Facts from a JWT whose signature verified against a registered trusted
/// issuer. Recorded with the moves the principal makes; never a Cedar input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedToken {
    /// The trusted issuer that verified the token: its verified `iss`, which
    /// is also the `TrustedIssuer` record's id.
    pub issuer: String,
    /// Whether the token is the human's own or an agent's access token.
    pub kind: TokenKind,
    /// The verified `client_id` claim, when the token carries one.
    pub client_id: Option<String>,
    /// The verified `grant_id` claim, when the token carries one.
    pub grant_id: Option<String>,
}
