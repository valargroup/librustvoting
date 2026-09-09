//! Nearest-rank percentiles over completed durations.
//!
//! Nearest rank rather than interpolation: every value reported is a duration
//! some attempt actually took, which is what makes "p95 was 812 ms" a statement
//! about a request rather than about an average of two.

/// Latency percentiles over one set of samples, in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Percentiles {
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

impl Percentiles {
    /// Percentiles over `samples`, which this sorts.
    ///
    /// An empty set yields zeros rather than an absence: the caller reports the
    /// sample count beside these, so a row of zeros with zero calls is already
    /// unambiguous, and an `Option` here would spread through every table cell.
    pub fn of(samples: &mut [u64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        samples.sort_unstable();
        Self {
            p50_us: nearest_rank(samples, 50),
            p95_us: nearest_rank(samples, 95),
            p99_us: nearest_rank(samples, 99),
            max_us: *samples.last().unwrap_or(&0),
        }
    }
}

/// The `percentile`-th value by nearest rank, from sorted `samples`.
///
/// Rank is `ceil(percentile/100 * n)`, clamped into the slice. Computed in
/// integers so a large sample count cannot drift the rank by a float rounding.
fn nearest_rank(samples: &[u64], percentile: u64) -> u64 {
    let count = samples.len() as u64;
    let rank = (percentile * count).div_ceil(100).max(1);
    let index = usize::try_from(rank - 1)
        .unwrap_or(0)
        .min(samples.len() - 1);
    samples[index]
}
