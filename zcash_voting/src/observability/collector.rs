use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    ObservabilityOptions, ObservationAttribution, ObservationOutcome, ObservationRecord,
    ObservationSummary, OperationObservability,
};

type SummaryKey = (&'static str, ObservationAttribution, ObservationOutcome);

pub(super) fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub(super) struct Collector {
    pub(super) started: Instant,
    pub(super) round_id: Option<String>,
    pub(super) limits: ObservabilityOptions,
    pub(super) next_id: u64,
    pub(super) active: BTreeMap<u64, (&'static str, ObservationRecord, Instant)>,
    records: Vec<ObservationRecord>,
    summaries: BTreeMap<SummaryKey, ObservationSummary>,
    // Only SDK-authored static vocabulary is interned; no attribution or caller data.
    labels: BTreeMap<&'static str, Arc<str>>,
    started_at_unix_us: u64,
    records_dropped: u64,
    summary_updates_dropped: u64,
    pub(super) active_stages_dropped: u64,
    pub(super) frozen: bool,
}

impl Collector {
    pub(super) fn new(limits: ObservabilityOptions) -> Self {
        Self {
            started: Instant::now(),
            started_at_unix_us: micros(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            ),
            round_id: None,
            limits,
            next_id: 0,
            active: BTreeMap::new(),
            records: Vec::new(),
            summaries: BTreeMap::new(),
            labels: BTreeMap::new(),
            records_dropped: 0,
            summary_updates_dropped: 0,
            active_stages_dropped: 0,
            frozen: false,
        }
    }

    pub(super) fn label(&mut self, label: &'static str) -> Arc<str> {
        Arc::clone(self.labels.entry(label).or_insert_with(|| Arc::from(label)))
    }

    pub(super) fn retain(&mut self, stage: &'static str, record: ObservationRecord) {
        let key = (stage, record.attribution, record.outcome);
        if !self.summaries.contains_key(&key)
            && self.summaries.len() < self.limits.max_summary_groups
        {
            self.summaries.insert(
                key,
                ObservationSummary {
                    stage: Arc::clone(&record.stage),
                    attribution: record.attribution,
                    outcome: record.outcome,
                    calls: 0,
                    cumulative_elapsed_us: 0,
                },
            );
        }
        if let Some(summary) = self.summaries.get_mut(&key) {
            summary.calls = summary.calls.saturating_add(1);
            summary.cumulative_elapsed_us = summary
                .cumulative_elapsed_us
                .saturating_add(record.elapsed_us);
        } else {
            self.summary_updates_dropped = self.summary_updates_dropped.saturating_add(1);
        }
        // Reserve detail capacity in start order, including unfinished parents.
        if record.id < self.limits.max_records as u64 {
            self.records.push(record);
        } else {
            self.records_dropped = self.records_dropped.saturating_add(1);
        }
    }

    pub(super) fn snapshot(
        &mut self,
        operation: &'static str,
        round_id: Option<&str>,
        outcome: ObservationOutcome,
    ) -> OperationObservability {
        self.frozen = true;
        let now = Instant::now();
        for (_, (stage, mut record, started)) in std::mem::take(&mut self.active) {
            record.elapsed_us = micros(now.saturating_duration_since(started));
            self.retain(stage, record);
        }
        let mut records = std::mem::take(&mut self.records);
        records.sort_by_key(|record| record.id);
        OperationObservability {
            operation: operation.to_owned(),
            started_at_unix_us: self.started_at_unix_us,
            round_id: round_id.map(str::to_owned).or_else(|| self.round_id.take()),
            elapsed_us: micros(now.saturating_duration_since(self.started)),
            outcome,
            records,
            summaries: std::mem::take(&mut self.summaries).into_values().collect(),
            records_dropped: self.records_dropped,
            summary_updates_dropped: self.summary_updates_dropped,
            active_stages_dropped: self.active_stages_dropped,
        }
    }
}
