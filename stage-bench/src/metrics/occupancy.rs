//! How many of something were in flight at once.
//!
//! Concurrency cannot be read off a summary. Two runs that each spent 500
//! seconds of accumulated request time are indistinguishable by total and
//! completely different in wall time if one held four requests open and the
//! other thirty-two. This computes the difference by sweeping the intervals.

/// Peak and average concurrency over one set of intervals.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Occupancy {
    /// Intervals the sweep covered.
    pub samples: usize,
    /// Most intervals open at any one instant.
    pub peak: usize,
    /// Sum of interval durations divided by the wall span, idle gaps included.
    ///
    /// Deliberately not the mean of the sweep's levels: a run that is idle for
    /// half its span really did average half the occupancy, and a metric that
    /// ignored the gap would report the busy stretch as the whole run.
    pub average: f64,
    /// First interval start to last interval end, in microseconds.
    pub wall_span_us: u64,
    /// Sum of every interval's duration, in microseconds.
    pub cumulative_us: u64,
}

/// One measured interval, half-open as `[start, start + elapsed)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub start_us: u64,
    pub elapsed_us: u64,
}

impl Interval {
    pub fn end_us(&self) -> u64 {
        self.start_us.saturating_add(self.elapsed_us)
    }
}

/// Sweeps `intervals` for peak and average concurrency.
///
/// Ends are processed before starts at an equal timestamp, so an interval that
/// finishes exactly as the next begins is not counted as two in flight. The
/// opposite convention inflates every peak by one at a busy boundary, which is
/// the failure mode most likely to be believed.
pub fn sweep(intervals: &[Interval]) -> Occupancy {
    if intervals.is_empty() {
        return Occupancy::default();
    }

    let mut boundaries: Vec<(u64, i32)> = Vec::with_capacity(intervals.len() * 2);
    let mut cumulative_us: u64 = 0;
    let mut first_start = u64::MAX;
    let mut last_end = 0;
    for interval in intervals {
        boundaries.push((interval.start_us, 1));
        boundaries.push((interval.end_us(), -1));
        cumulative_us = cumulative_us.saturating_add(interval.elapsed_us);
        first_start = first_start.min(interval.start_us);
        last_end = last_end.max(interval.end_us());
    }
    // `-1` sorts before `1`, so ends are applied first at an equal timestamp.
    boundaries.sort_unstable();

    let mut open = 0i32;
    let mut peak = 0i32;
    for (_, delta) in &boundaries {
        open += delta;
        peak = peak.max(open);
    }

    let wall_span_us = last_end.saturating_sub(first_start);
    let average = if wall_span_us == 0 {
        // Every interval was instantaneous, or there was exactly one of zero
        // length. Reporting the sample count is honest; dividing by zero is not.
        intervals.len() as f64
    } else {
        cumulative_us as f64 / wall_span_us as f64
    };

    Occupancy {
        samples: intervals.len(),
        peak: usize::try_from(peak).unwrap_or(0),
        average,
        wall_span_us,
        cumulative_us,
    }
}
