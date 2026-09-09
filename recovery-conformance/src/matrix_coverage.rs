//! Completion gate shared by the live fault matrices.

/// Counts of exercised cases and explicitly documented coverage exclusions.
#[derive(Clone, Copy, Debug)]
pub struct MatrixCoverage {
    pub attempted: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Only known unsupported cases may be excluded; never environment failures.
    pub excluded: usize,
}

impl MatrixCoverage {
    /// Rejects empty, failed, partially exercised, or unexpectedly skipped runs.
    /// An exclusion is reported but is never counted as a successful exercise.
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.attempted > self.excluded,
            "matrix exercised no runnable cases"
        );
        anyhow::ensure!(self.failed == 0, "matrix has {} failed cases", self.failed);
        anyhow::ensure!(
            self.skipped == self.excluded,
            "matrix skipped runnable cases"
        );
        anyhow::ensure!(
            self.passed + self.excluded == self.attempted,
            "matrix did not pass every runnable case"
        );
        Ok(())
    }
}
