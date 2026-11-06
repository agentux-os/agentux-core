//! Metadata and identity rules for Core-owned generic artifacts.

use serde::{Deserialize, Serialize};

pub const MAX_ARTIFACT_BYTES: u64 = 25 * 1024 * 1024;
pub const MIN_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// A digest and size produced only after Core's private store verifies and
/// durably commits the supplied bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedContent {
    pub(crate) sha256: String,
    pub(crate) byte_length: u64,
}

impl VerifiedContent {
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn byte_length(&self) -> u64 {
        self.byte_length
    }
}

/// A trusted in-process producer's explicit publication request. Content is
/// supplied separately as bytes; callers cannot provide paths, URLs or hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    pub organization_id: String,
    pub run_id: String,
    pub session_id: String,
    pub producer_id: String,
    pub producer_key: String,
    pub publication_id: String,
    pub name: String,
    pub media_type: String,
}

impl Publication {
    pub fn validate(&self) -> Result<(), String> {
        for (field, value, max) in [
            ("organizationId", &self.organization_id, 128),
            ("runId", &self.run_id, 128),
            ("sessionId", &self.session_id, 120),
            ("producerId", &self.producer_id, 128),
            ("producerKey", &self.producer_key, 256),
            ("publicationId", &self.publication_id, 128),
            ("name", &self.name, 256),
            ("mediaType", &self.media_type, 127),
        ] {
            if value.trim().is_empty()
                || value.chars().count() > max
                || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "{field} must be non-empty, bounded and contain no control characters"
                ));
            }
        }
        if !self.media_type.contains('/')
            || self.media_type.starts_with('/')
            || self.media_type.ends_with('/')
        {
            return Err("mediaType must be a valid type/subtype".into());
        }
        Ok(())
    }
}

/// Durable result of publication, suitable for retry and event delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedArtifact {
    pub artifact_id: String,
    pub organization_id: String,
    pub run_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub role_id: String,
    pub producer_id: String,
    pub producer_key: String,
    pub publication_id: String,
    pub revision: String,
    pub media_type: String,
    pub byte_length: u64,
    pub name: String,
    pub sequence: i64,
    pub created: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication() -> Publication {
        Publication {
            organization_id: "org-1".into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            producer_id: "agentux-artifact-tool".into(),
            producer_key: "report".into(),
            publication_id: "pub-1".into(),
            name: "Build report".into(),
            media_type: "text/markdown".into(),
        }
    }

    #[test]
    fn publication_requires_bounded_safe_metadata() {
        assert!(publication().validate().is_ok());
        let mut invalid = publication();
        invalid.name = "bad\nname".into();
        assert!(invalid.validate().is_err());
        let mut invalid = publication();
        invalid.media_type = "text".into();
        assert!(invalid.validate().is_err());
    }
}
