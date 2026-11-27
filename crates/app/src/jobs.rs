//! Ownership of the app's one inference worker. File jobs cannot be superseded by typing.

#[derive(Default)]
pub struct Jobs {
    file: Option<u64>,
    /// A BabelDOC run: one worker job per paragraph, the file job all the way through.
    external: bool,
    deferred_text: bool,
    batch: bool,
}

impl Jobs {
    pub fn file(&self) -> Option<u64> {
        self.file
    }
    pub fn busy(&self) -> bool {
        self.batch || self.file.is_some() || self.external
    }
    /// Whether one file is being translated (a worker job, or a BabelDOC run).
    pub fn file_running(&self) -> bool {
        self.file.is_some() || self.external
    }
    pub fn start_external(&mut self) {
        self.external = true;
    }
    /// Like [`Self::finish_file`] for a BabelDOC run.
    pub fn finish_external(&mut self) -> Option<bool> {
        if !self.external {
            return None;
        }
        self.external = false;
        Some(!self.batch && std::mem::take(&mut self.deferred_text))
    }
    pub fn start_batch(&mut self) {
        self.batch = true;
    }
    pub fn finish_batch(&mut self) -> bool {
        self.batch = false;
        std::mem::take(&mut self.deferred_text)
    }
    pub fn start_file(&mut self, job: u64) {
        self.file = Some(job);
    }
    pub fn defer_text(&mut self) -> bool {
        if self.busy() {
            self.deferred_text = true;
            true
        } else {
            false
        }
    }
    /// A stale completion must neither release a new job nor run its deferred text.
    pub fn finish_file(&mut self, job: u64) -> Option<bool> {
        if self.file != Some(job) {
            return None;
        }
        self.file = None;
        Some(!self.batch && std::mem::take(&mut self.deferred_text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_waits_through_batch_reads_saves_and_failures() {
        let mut jobs = Jobs::default();
        jobs.start_batch();
        assert!(jobs.defer_text());
        for job in [1, 2] {
            jobs.start_file(job);
            assert_eq!(jobs.finish_file(job), Some(false));
            assert!(jobs.busy());
        }
        assert!(jobs.finish_batch());
        assert!(!jobs.busy());
    }

    /// A BabelDOC run holds the worker through all of its paragraph jobs: their completions
    /// neither release it nor run the deferred text; only the run's end does.
    #[test]
    fn an_external_run_holds_the_worker_between_its_paragraphs() {
        let mut jobs = Jobs::default();
        jobs.start_external();
        assert!(jobs.busy() && jobs.file_running());
        assert!(jobs.defer_text());
        for paragraph_job in [4, 5, 6] {
            assert_eq!(jobs.finish_file(paragraph_job), None);
            assert!(jobs.busy());
        }
        assert_eq!(jobs.finish_external(), Some(true));
        assert!(!jobs.busy());
        assert_eq!(jobs.finish_external(), None);

        // In a batch, the text waits for the batch.
        jobs.start_batch();
        jobs.start_external();
        assert!(jobs.defer_text());
        assert_eq!(jobs.finish_external(), Some(false));
        assert!(jobs.busy());
        assert!(jobs.finish_batch());
    }

    #[test]
    fn stale_completion_does_not_release_the_worker() {
        let mut jobs = Jobs::default();
        jobs.start_file(3);
        assert!(jobs.defer_text());
        assert!(jobs.defer_text());
        assert_eq!(jobs.finish_file(2), None);
        assert!(jobs.busy());
        assert_eq!(jobs.finish_file(3), Some(true));
        assert!(!jobs.busy());
        assert!(!jobs.defer_text());
    }
}
