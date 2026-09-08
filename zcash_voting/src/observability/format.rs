use std::fmt;

use super::{ObservationOutcome, OperationObservability};

impl fmt::Display for ObservationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Pending => "pending",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::NoWork => "no_work",
            Self::Reused => "reused",
            Self::Unfinished => "unfinished",
            Self::PossiblyDispatched => "possibly_dispatched",
        })
    }
}

/// Formats bounded summary rows; detailed records and free-form error text are omitted.
impl fmt::Display for OperationObservability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {} ({} us, started_at_unix_us={})",
            self.operation, self.outcome, self.elapsed_us, self.started_at_unix_us
        )?;
        if let Some(round) = &self.round_id {
            write!(f, " round={round}")?;
        }
        let mut summaries = self.summaries.iter().collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            (left.attribution, &left.stage, left.outcome).cmp(&(
                right.attribution,
                &right.stage,
                right.outcome,
            ))
        });
        for summary in summaries {
            write!(f, "\n  {}", summary.stage)?;
            if let Some(bundle) = summary.attribution.bundle_index {
                write!(f, " bundle={bundle}")?;
            }
            if let Some(proposal) = summary.attribution.proposal_id {
                write!(f, " proposal={proposal}")?;
            }
            if let Some(share) = summary.attribution.share_index {
                write!(f, " share={share}")?;
            }
            write!(
                f,
                ": {} calls={} total={} us",
                summary.outcome, summary.calls, summary.cumulative_elapsed_us
            )?;
        }
        write!(
            f,
            "\n  omitted: records={} summary_updates={} active_stages={}",
            self.records_dropped, self.summary_updates_dropped, self.active_stages_dropped
        )
    }
}
