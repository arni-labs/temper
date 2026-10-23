//! Why a publish did not complete, and so who must act on it.

/// Why a publish did not complete. The variant says who acts next: the caller retries, the caller fixes its request
/// or source, or the server's operator looks at a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishArtifactError {
    /// The source changed after it was authorized; the caller retries against current state.
    StaleAuthorization,
    /// The request or its source cannot be published as given: a key component is not one path segment, the source
    /// has no content, or its bytes do not match the content hash recorded for them.
    Rejected(String),
    /// The server could not complete the publish: storage, configuration or an index that has fallen behind.
    Failed(String),
}

impl std::fmt::Display for PublishArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleAuthorization => f.write_str(
                "publish source authorization became stale; retry against current state",
            ),
            Self::Rejected(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PublishArtifactError {}

/// Server-side steps report their failures as strings; any of those is the server's to fix.
impl From<String> for PublishArtifactError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}
