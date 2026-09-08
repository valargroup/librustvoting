//! Owned projections for wire consumers that cannot share Rust string storage.
use super::{ObservationAttribution, ObservationOutcome};

/// Plain owned-string projection for wire and FFI consumers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ObservationRecord {
    pub id: u64,
    pub parent_id: Option<u64>,
    /// SDK-authored stage name, never caller data or a request URL.
    /// Labels and nesting may evolve across SDK versions; consumers must
    /// tolerate unknown stages rather than depend on an exact phase sequence.
    pub stage: String,
    pub attribution: ObservationAttribution,
    pub started_after_us: u64,
    pub elapsed_us: u64,
    pub outcome: ObservationOutcome,
    /// Stable error category only; never the error's free-form message.
    pub error_kind: Option<String>,
    pub http_status: Option<u16>,
    /// Configured endpoint ordinal, when known. URLs and request paths are excluded.
    pub endpoint_index: Option<u32>,
    /// One-based network attempt within its parent operation; absent on ordinary stages.
    pub attempt: Option<u32>,
}

/// Plain owned-string projection for wire and FFI consumers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ObservationSummary {
    pub stage: String,
    pub attribution: ObservationAttribution,
    pub calls: u64,
    pub outcome: ObservationOutcome,
    /// Sum of stage durations, not invocation wall time.
    pub cumulative_elapsed_us: u64,
}

/// Plain owned-string projection for wire and FFI consumers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct OperationObservability {
    pub operation: String,
    /// Wall-clock anchor in microseconds since the Unix epoch. Durations stay monotonic.
    pub started_at_unix_us: u64,
    pub round_id: Option<String>,
    pub elapsed_us: u64,
    pub outcome: ObservationOutcome,
    pub records: Vec<ObservationRecord>,
    pub summaries: Vec<ObservationSummary>,
    pub records_dropped: u64,
    /// Measurements omitted because their summary group could not be admitted.
    /// Counts updates, not distinct omitted groups.
    pub summary_updates_dropped: u64,
    /// Stage starts omitted because the concurrent timer limit was reached.
    pub active_stages_dropped: u64,
}
impl From<&super::ObservationRecord> for ObservationRecord {
    fn from(record: &super::ObservationRecord) -> Self {
        Self {
            id: record.id,
            parent_id: record.parent_id,
            stage: record.stage.to_string(),
            attribution: record.attribution,
            started_after_us: record.started_after_us,
            elapsed_us: record.elapsed_us,
            outcome: record.outcome,
            error_kind: record.error_kind.as_ref().map(ToString::to_string),
            http_status: record.http_status,
            endpoint_index: record.endpoint_index,
            attempt: record.attempt,
        }
    }
}
impl From<&super::ObservationSummary> for ObservationSummary {
    fn from(summary: &super::ObservationSummary) -> Self {
        Self {
            stage: summary.stage.to_string(),
            attribution: summary.attribution,
            outcome: summary.outcome,
            calls: summary.calls,
            cumulative_elapsed_us: summary.cumulative_elapsed_us,
        }
    }
}
impl From<&super::OperationObservability> for OperationObservability {
    fn from(report: &super::OperationObservability) -> Self {
        Self {
            operation: report.operation.clone(),
            round_id: report.round_id.clone(),
            started_at_unix_us: report.started_at_unix_us,
            elapsed_us: report.elapsed_us,
            outcome: report.outcome,
            records: report.records.iter().map(Into::into).collect(),
            summaries: report.summaries.iter().map(Into::into).collect(),
            records_dropped: report.records_dropped,
            summary_updates_dropped: report.summary_updates_dropped,
            active_stages_dropped: report.active_stages_dropped,
        }
    }
}
