use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use super::{
    collector::{micros, Collector},
    ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationRecord,
    OperationObservability, OperationReport,
};

/// Opaque context used only by SDK extension hooks.
/// Collection is configured at public `*_with_report` entry points.
#[doc(hidden)]
///
/// Public callers can select collection but cannot finalize timers or collectors.
/// Each standalone call owns its snapshot. SDK child contexts share that call's
/// collector across tasks without gaining ownership of finalization.
#[derive(Clone)]
pub struct ObservationScope {
    collector: Option<Arc<Mutex<Collector>>>,
    options: Option<ObservabilityOptions>,
    parent_id: Option<u64>,
    attribution: ObservationAttribution,
    attempt: Option<u32>,
    endpoint_index: Option<u32>,
    configured_helpers: Option<Arc<[String]>>,
}

impl std::fmt::Debug for ObservationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservationScope")
            .field("enabled", &self.options.is_some())
            .field("attribution", &self.attribution)
            .finish()
    }
}

impl ObservationScope {
    pub(crate) fn is_enabled(&self) -> bool {
        self.collector.is_some()
    }

    /// Creates a no-op scope without touching a clock or allocating.
    pub(crate) fn disabled() -> Self {
        Self {
            collector: None,
            options: None,
            parent_id: None,
            attribution: ObservationAttribution::default(),
            attempt: None,
            endpoint_index: None,
            configured_helpers: None,
        }
    }

    /// Creates reusable standalone-call options. Each public invocation starts
    /// its own collector, so sequential and concurrent reuse is safe.
    pub(crate) fn new(options: Option<ObservabilityOptions>) -> Self {
        Self {
            options,
            ..Self::disabled()
        }
    }

    pub(crate) fn invocation(&self) -> ObservationInvocation {
        match (&self.collector, self.options) {
            (None, Some(options)) => ObservationInvocation {
                scope: Self {
                    collector: Some(Arc::new(Mutex::new(Collector::new(options)))),
                    ..self.clone()
                },
                owns_collector: true,
            },
            _ => ObservationInvocation {
                scope: self.clone(),
                owns_collector: false,
            },
        }
    }

    /// Binds public round metadata once, without allocation when disabled.
    pub(crate) fn bind_round_id(&self, round_id: &str) {
        if let Some(collector) = &self.collector {
            let mut collection = collector
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if !collection.frozen && collection.round_id.is_none() {
                collection.round_id = Some(round_id.to_owned());
            }
        }
    }

    pub(crate) fn bind_round_bytes(&self, round_id: &[u8; 32]) {
        if self.collector.is_some() {
            self.bind_round_id(&hex::encode(round_id));
        }
    }

    /// Narrows attribution without beginning a timer. Changing bundles clears
    /// inherited proposal/share identity; changing proposals clears the share.
    pub(crate) fn attributed(&self, attribution: ObservationAttribution) -> Self {
        let same_bundle = attribution.bundle_index.is_none()
            || attribution.bundle_index == self.attribution.bundle_index;
        let same_proposal = same_bundle
            && (attribution.proposal_id.is_none()
                || attribution.proposal_id == self.attribution.proposal_id);
        Self {
            attribution: ObservationAttribution {
                bundle_index: attribution.bundle_index.or(self.attribution.bundle_index),
                proposal_id: attribution.proposal_id.or(if same_bundle {
                    self.attribution.proposal_id
                } else {
                    None
                }),
                share_index: attribution.share_index.or(if same_proposal {
                    self.attribution.share_index
                } else {
                    None
                }),
            },
            ..self.clone()
        }
    }

    /// Attributes work to the complete bundle, clearing a triggering member's
    /// proposal and share identity while retaining the collector and parent.
    pub(crate) fn for_bundle(&self, bundle_index: u32) -> Self {
        Self {
            attribution: ObservationAttribution {
                bundle_index: Some(bundle_index),
                proposal_id: None,
                share_index: None,
            },
            ..self.clone()
        }
    }

    /// Attributes a transport attempt and its nested measurements to a one-based index.
    pub(crate) fn attempt(&self, attempt: u32) -> Self {
        Self {
            attempt: Some(attempt),
            ..self.clone()
        }
    }

    /// Retains the validated fleet's original order only while collecting.
    /// URLs stay private; snapshots carry only zero-based ordinals.
    pub(crate) fn with_helper_fleet(&self, canonical_urls: &[String]) -> Self {
        Self {
            configured_helpers: self.collector.as_ref().map(|_| Arc::from(canonical_urls)),
            endpoint_index: None,
            ..self.clone()
        }
    }

    /// Binds one helper without inheriting another request's endpoint identity.
    pub(crate) fn for_helper(&self, server_url: &str) -> Self {
        let endpoint_index = self.configured_helpers.as_ref().and_then(|configured| {
            let canonical = crate::helper::url::canonicalize_helper_base_url(server_url).ok()?;
            configured
                .iter()
                .position(|url| url == &canonical)
                .and_then(|index| u32::try_from(index).ok())
        });
        Self {
            endpoint_index,
            ..self.clone()
        }
    }

    /// Starts an SDK-authored stage. Use static labels, never payload contents.
    pub(crate) fn stage(&self, stage: &'static str) -> ObservationStage {
        let Some(collector) = &self.collector else {
            return ObservationStage::disabled();
        };
        let mut collection = collector
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if collection.frozen {
            return ObservationStage::disabled();
        }
        if collection.active.len() >= collection.limits.max_active_stages {
            collection.active_stages_dropped = collection.active_stages_dropped.saturating_add(1);
            // Descendants can still be admitted later, under the nearest retained ancestor.
            return ObservationStage {
                scope: self.clone(),
                stage,
                id: None,
            };
        }
        let started = Instant::now();
        let id = collection.next_id;
        collection.next_id = collection.next_id.saturating_add(1);
        let record = ObservationRecord {
            id,
            parent_id: self.parent_id,
            stage: collection.label(stage),
            attribution: self.attribution,
            started_after_us: micros(started.saturating_duration_since(collection.started)),
            elapsed_us: 0,
            outcome: ObservationOutcome::Unfinished,
            error_kind: None,
            http_status: None,
            endpoint_index: self.endpoint_index,
            attempt: self.attempt,
        };
        collection.active.insert(id, (stage, record, started));
        ObservationStage {
            scope: Self {
                parent_id: Some(id),
                ..self.clone()
            },
            stage,
            id: Some(id),
        }
    }

    /// Times fallible SDK work, retaining its original error and a safe category.
    pub(crate) fn measure<T>(
        &self,
        stage: &'static str,
        work: impl FnOnce() -> Result<T, crate::VotingError>,
    ) -> Result<T, crate::VotingError> {
        let timer = self.stage(stage);
        let result = work();
        let outcome = if result.is_ok() {
            ObservationOutcome::Succeeded
        } else {
            ObservationOutcome::Failed
        };
        timer.finish(outcome, result.as_ref().err().map(super::voting_error_kind));
        result
    }

    /// Measures a downstream operation whose error is not an SDK error.
    pub(crate) fn measure_result<T, E>(
        &self,
        stage: &'static str,
        work: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let timer = self.stage(stage);
        let result = work();
        timer.finish(
            if result.is_ok() {
                ObservationOutcome::Succeeded
            } else {
                ObservationOutcome::Failed
            },
            result.as_ref().err().map(|_| "DownstreamFailure"),
        );
        result
    }
}

/// SDK-owned invocation boundary. Borrowed child contexts cannot finalize it.
/// This owner is deliberately not cloneable and completion consumes it.
pub(crate) struct ObservationInvocation {
    scope: ObservationScope,
    owns_collector: bool,
}

impl std::ops::Deref for ObservationInvocation {
    type Target = ObservationScope;
    fn deref(&self) -> &ObservationScope {
        &self.scope
    }
}

impl ObservationInvocation {
    pub(crate) fn scope(&self) -> &ObservationScope {
        &self.scope
    }

    pub(crate) fn complete<T>(
        self,
        operation: &'static str,
        outcome: ObservationOutcome,
        result: T,
    ) -> OperationReport<T> {
        self.report(operation, None, outcome, result)
    }

    pub(crate) fn finish(
        self,
        operation: &'static str,
        round_id: Option<&str>,
        outcome: ObservationOutcome,
    ) -> Option<OperationObservability> {
        if !self.owns_collector {
            return None;
        }
        let collector = self.scope.collector.as_ref()?;
        let mut collection = collector
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Some(collection.snapshot(operation, round_id, outcome))
    }

    pub(crate) fn report<T>(
        self,
        operation: &'static str,
        round_id: Option<&str>,
        outcome: ObservationOutcome,
        result: T,
    ) -> OperationReport<T> {
        OperationReport {
            result,
            observability: self.finish(operation, round_id, outcome),
        }
    }
}

/// RAII stage timer. Dropping an unfinished stage never claims success.
pub(crate) struct ObservationStage {
    scope: ObservationScope,
    stage: &'static str,
    id: Option<u64>,
}

impl ObservationStage {
    fn disabled() -> Self {
        Self {
            scope: ObservationScope::disabled(),
            stage: "",
            id: None,
        }
    }

    /// Child context for work belonging to this stage.
    pub(crate) fn scope(&self) -> &ObservationScope {
        &self.scope
    }

    /// Finishes a stage with a semantic outcome and a static error category.
    pub(crate) fn finish(mut self, outcome: ObservationOutcome, error_kind: Option<&'static str>) {
        self.complete(outcome, error_kind, None, None);
    }

    /// Records metadata for one SDK-visible transport attempt, excluding bodies/URLs.
    pub(crate) fn finish_http(
        mut self,
        outcome: ObservationOutcome,
        error_kind: Option<&'static str>,
        status: Option<u16>,
        endpoint_index: Option<u32>,
    ) {
        self.complete(outcome, error_kind, status, endpoint_index);
    }

    fn complete(
        &mut self,
        outcome: ObservationOutcome,
        error_kind: Option<&'static str>,
        status: Option<u16>,
        endpoint_index: Option<u32>,
    ) {
        let Some(id) = self.id.take() else {
            return;
        };
        let Some(collector) = &self.scope.collector else {
            return;
        };
        let mut collection = collector
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if collection.frozen {
            return;
        }
        if let Some((_, mut record, started)) = collection.active.remove(&id) {
            record.elapsed_us = micros(started.elapsed());
            record.outcome = outcome;
            record.error_kind = error_kind.map(|label| collection.label(label));
            record.http_status = status;
            record.endpoint_index = endpoint_index.or(record.endpoint_index);
            collection.retain(self.stage, record);
        }
    }
}

impl Drop for ObservationStage {
    fn drop(&mut self) {
        self.complete(ObservationOutcome::Unfinished, None, None, None);
    }
}
