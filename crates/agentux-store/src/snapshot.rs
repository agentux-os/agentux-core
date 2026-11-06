//! Source inventory and durable storage primitives for node snapshot checkpoints.
//!
//! This module deliberately does not expose a wire checkpoint: required
//! lifecycle producers and some gateway-owned inventories still lack an
//! authoritative durable source, so complete checkpoint coverage cannot be
//! claimed.

use std::time::Duration;

use agentux_api::{
    HarnessLimit, NodeAlertState, NodeState, PermissionRequest, Run, Session,
    SnapshotProjectionType,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MIN_RETENTION: Duration = Duration::from_secs(15 * 60);
pub const MAX_PAGE_BYTES: usize = 1_048_576;
pub const MAX_PAGES: usize = 10_000;
pub const MAX_SNAPSHOT_BYTES: usize = 67_108_864;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    Approval,
    Artifact,
    Command,
    Gate,
    Harness,
    Limit,
    Node,
    NodeAlert,
    Run,
    Session,
    Task,
}

impl ProjectionKind {
    pub const ALL: [Self; 11] = [
        Self::Approval,
        Self::Artifact,
        Self::Command,
        Self::Gate,
        Self::Harness,
        Self::Limit,
        Self::Node,
        Self::NodeAlert,
        Self::Run,
        Self::Session,
        Self::Task,
    ];

    pub const fn source_kind(self) -> SourceKind {
        match self {
            Self::Approval | Self::Limit | Self::Run | Self::Session => SourceKind::Sourced,
            Self::Gate => SourceKind::Derived,
            Self::Harness | Self::Node | Self::NodeAlert => SourceKind::Sourced,
            Self::Artifact | Self::Command | Self::Task => SourceKind::Unavailable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Sourced,
    Derived,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnavailableProjection {
    pub kind: ProjectionKind,
    pub reason: String,
}

pub const fn checkpoint_capability_enabled() -> bool {
    false
}

pub fn unavailable_projections() -> Vec<UnavailableProjection> {
    ProjectionKind::ALL.into_iter().filter_map(|kind| {
        let reason = match kind {
            ProjectionKind::Command => Some(
                "durable storage exists, but the authenticated gateway command lifecycle producer is not connected",
            ),
            ProjectionKind::Artifact => Some(
                "typed pull request artifacts are projected, but the generic artifact lifecycle producer is not connected",
            ),
            ProjectionKind::Task => Some(
                "durable storage exists, but the typed task lifecycle producer is not connected",
            ),
            _ => None,
        }?;
        Some(UnavailableProjection { kind, reason: reason.into() })
    }).collect()
}

pub type BusinessProjectionKind = SnapshotProjectionType;

/// Explicit, complete state written by the lifecycle owner of a projection.
/// `state.currentState` is the protocol's complete current-state object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusinessProjection {
    pub projection_type: BusinessProjectionKind,
    pub id: String,
    pub organization_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub state: Value,
}

/// Durable current state or removal marker with the event sequence that
/// publishes the change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BusinessProjectionEntry {
    pub projection_type: BusinessProjectionKind,
    pub id: String,
    pub organization_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    pub updated_sequence: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at_sequence: Option<i64>,
}

/// A local harness projection and its last state/removal sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessProjectionEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    pub updated_sequence: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at_sequence: Option<i64>,
}

/// Complete local node state and the event sequence that published it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeProjectionEntry {
    pub state: NodeState,
    pub updated_sequence: i64,
}

/// Current alert state or retained removal tombstone with its last sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeAlertProjectionEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<NodeAlertState>,
    pub updated_sequence: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at_sequence: Option<i64>,
}

impl BusinessProjection {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() || self.organization_id.trim().is_empty() {
            return Err("projection id and organizationId are required".into());
        }
        let (field, family) = match self.projection_type {
            BusinessProjectionKind::Artifact => ("artifactId", "artifact"),
            BusinessProjectionKind::Task => ("taskId", "task"),
            BusinessProjectionKind::Command => ("commandId", "command"),
        };
        if self.state.get(field).and_then(Value::as_str) != Some(self.id.as_str()) {
            return Err(format!(
                "{family} state identity does not match projection id"
            ));
        }
        if self.state.get("organizationId").and_then(Value::as_str)
            != Some(self.organization_id.as_str())
        {
            return Err("projection state organizationId does not match its owner".into());
        }
        self.state
            .get("currentState")
            .and_then(Value::as_object)
            .filter(|current| !current.is_empty())
            .ok_or("projection state requires a complete currentState object")?;
        if matches!(
            self.projection_type,
            BusinessProjectionKind::Task | BusinessProjectionKind::Command
        ) && self
            .state
            .get("status")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(format!("{family} state requires a non-empty status"));
        }
        let state_run_id = self.state.get("runId").and_then(Value::as_str);
        if state_run_id != self.run_id.as_deref() {
            return Err(format!("{family} state and indexed runId must agree"));
        }
        if self.projection_type == BusinessProjectionKind::Task && state_run_id.is_none() {
            return Err("task state requires runId".into());
        }
        Ok(())
    }
}

/// Opaque immutable JSON bytes. This is intentionally not a protocol page;
/// callers cannot claim coverage until every projection has a source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaterializedSnapshot {
    pub organization_id: String,
    pub node_id: String,
    pub checkpoint_id: String,
    pub snapshot_id: String,
    pub captured_event_head: i64,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableCapture {
    pub organization_id: String,
    pub captured_event_head: i64,
    pub runs: Vec<Run>,
    pub approvals: Vec<PermissionRequest>,
    pub sessions: Vec<Session>,
    pub limits: Vec<HarnessLimit>,
    /// Durable local harness inventory and retained removals.
    pub harnesses: Vec<HarnessProjectionEntry>,
    /// Local operational state of this Core installation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeProjectionEntry>,
    /// Local Core-owned alerts and retained removals.
    #[serde(default)]
    pub node_alerts: Vec<NodeAlertProjectionEntry>,
    /// Gate state is derived solely from the durable run checks and attempt counters.
    pub gates: Vec<Value>,
    /// Explicitly written business state and retained tombstones for the
    /// three durable lifecycle owners.
    pub business_projections: Vec<BusinessProjectionEntry>,
    /// The complete contract remains unavailable while any entry is present.
    pub unavailable: Vec<UnavailableProjection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageError {
    Missing,
    Expired,
    ScopeMismatch,
    InvalidPageSize,
}

impl MaterializedSnapshot {
    pub fn page(
        &self,
        organization_id: &str,
        node_id: &str,
        page_index: usize,
        page_size: usize,
        now_ms: i64,
    ) -> Result<Vec<u8>, PageError> {
        if self.organization_id != organization_id || self.node_id != node_id {
            return Err(PageError::ScopeMismatch);
        }
        if now_ms >= self.expires_at_ms {
            return Err(PageError::Expired);
        }
        if page_size == 0 || page_size > MAX_PAGE_BYTES || page_index >= MAX_PAGES {
            return Err(PageError::InvalidPageSize);
        }
        let bytes = serde_json::to_vec(&self.payload).map_err(|_| PageError::Missing)?;
        if bytes.len().div_ceil(page_size) > MAX_PAGES {
            return Err(PageError::InvalidPageSize);
        }
        let start = page_index
            .checked_mul(page_size)
            .ok_or(PageError::InvalidPageSize)?;
        if start >= bytes.len() && !(start == 0 && bytes.is_empty()) {
            return Err(PageError::Missing);
        }
        Ok(bytes[start..bytes.len().min(start.saturating_add(page_size))].to_vec())
    }
}
