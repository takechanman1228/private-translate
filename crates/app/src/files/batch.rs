//! Ordered file work. Only the active row owns a reader, worker job or writer.
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub(super) enum State<R, E> {
    Waiting,
    Reading,
    Translating(Option<(usize, usize)>),
    Stopping,
    Saving,
    Saved(R),
    Failed(E),
    Cancelled,
}

pub(super) struct Row<R, E> {
    pub id: usize,
    pub path: PathBuf,
    pub state: State<R, E>,
}

pub(super) struct Batch<S, R, E> {
    rows: Vec<Row<R, E>>,
    next_id: usize,
    settings: Option<S>,
    active: Option<usize>,
}

impl<S, R, E> Default for Batch<S, R, E> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            next_id: 0,
            settings: None,
            active: None,
        }
    }
}

impl<S, R, E> Batch<S, R, E> {
    pub fn rows(&self) -> &[Row<R, E>] {
        &self.rows
    }
    pub fn settings(&self) -> Option<&S> {
        self.settings.as_ref()
    }
    pub fn pending(&self) -> bool {
        self.active.is_some() || self.rows.iter().any(|r| matches!(r.state, State::Waiting))
    }
    pub fn add(&mut self, path: PathBuf, error: Option<E>) {
        self.rows.push(Row {
            id: self.next_id,
            path,
            state: error.map_or(State::Waiting, State::Failed),
        });
        self.next_id += 1;
    }
    pub fn row(&self, id: usize) -> Option<&Row<R, E>> {
        self.rows.iter().find(|row| row.id == id)
    }

    pub fn row_mut(&mut self, id: usize) -> Option<&mut Row<R, E>> {
        self.rows.iter_mut().find(|row| row.id == id)
    }

    pub fn edit_settings(&mut self) {
        self.settings = None;
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.settings = None;
        self.active = None;
    }

    pub fn remove(&mut self, id: usize, just_saved: bool) -> bool {
        let count = self.rows.len();
        self.rows.retain(|r| {
            r.id != id
                || !(just_saved && matches!(r.state, State::Saved(_))
                    || matches!(
                        r.state,
                        State::Waiting | State::Failed(_) | State::Cancelled
                    ))
        });
        self.rows.len() < count
    }
    pub fn start(&mut self, settings: S) {
        self.settings.get_or_insert(settings);
    }
    pub fn prioritize(&mut self, mut rank: impl FnMut(usize) -> (bool, &'static str)) {
        self.rows.sort_by_key(|row| rank(row.id));
    }

    pub fn next(&mut self) -> Option<(usize, PathBuf)> {
        self.settings.as_ref()?;
        if self.active.is_some() {
            return None;
        }
        let row = self
            .rows
            .iter_mut()
            .find(|r| matches!(r.state, State::Waiting))?;
        self.active = Some(row.id);
        row.state = State::Reading;
        Some((row.id, row.path.clone()))
    }
    fn active_row(&mut self, id: usize) -> Option<&mut Row<R, E>> {
        if self.active != Some(id) {
            return None;
        }
        self.rows.iter_mut().find(|r| r.id == id)
    }
    pub fn active(&self) -> Option<usize> {
        self.active
    }
    pub fn progress(&mut self, id: usize, progress: Option<(usize, usize)>) -> bool {
        let Some(row) = self.active_row(id) else {
            return false;
        };
        if !matches!(row.state, State::Reading | State::Translating(_)) {
            return false;
        }
        row.state = State::Translating(progress);
        true
    }
    pub fn saving(&mut self, id: usize) -> bool {
        let Some(row) = self.active_row(id) else {
            return false;
        };
        if !matches!(row.state, State::Translating(_)) {
            return false;
        }
        row.state = State::Saving;
        true
    }
    pub fn finish_deferred(&mut self, id: usize, error: E) {
        self.finish(id, Err(error));
        if let Some(index) = self.rows.iter().position(|row| row.id == id) {
            let row = self.rows.remove(index);
            self.rows.push(row);
        }
    }

    pub fn cancel_one(&mut self, id: usize) -> bool {
        let Some(row) = self.active_row(id) else {
            return false;
        };
        if !matches!(row.state, State::Reading | State::Translating(_)) {
            return false;
        }
        row.state = State::Stopping;
        true
    }
    pub fn cancel(&mut self, all: bool) {
        for row in &mut self.rows {
            match row.state {
                State::Waiting if all => row.state = State::Cancelled,
                State::Reading | State::Translating(_) => row.state = State::Stopping,
                _ => {}
            }
        }
    }
    /// Release ownership only after the reader/worker/writer has returned.
    pub fn finish(&mut self, id: usize, result: Result<R, E>) {
        let Some(row) = self.active_row(id) else {
            return;
        };
        row.state = if matches!(row.state, State::Stopping) {
            State::Cancelled
        } else {
            match result {
                Ok(saved) => State::Saved(saved),
                Err(error) => State::Failed(error),
            }
        };
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processes_grouped_models_with_the_loaded_model_first() {
        let mut batch = Batch::<(), (), ()>::default();
        for name in ["first.txt", "report.pdf", "second.txt", "appendix.pdf"] {
            batch.add(name.into(), None);
        }
        batch.start(());
        let models = ["hy", "plamo", "hy", "plamo"];
        batch.prioritize(|id| (models[id] != "plamo", models[id]));
        for expected in [1, 3, 0, 2] {
            let (id, _) = batch.next().unwrap();
            assert_eq!(id, expected);
            batch.finish(id, Ok(()));
        }
    }

    #[test]
    fn selections_returning_after_stop_can_run_and_old_ids_cannot_cancel_them() {
        let mut batch = Batch::<(), (), ()>::default();
        batch.add("a".into(), None);
        batch.add("b".into(), None);
        batch.start(());
        let (a, _) = batch.next().unwrap();
        batch.cancel(true);
        batch.finish(a, Err(()));
        batch.add("returned by picker".into(), None);
        let (new, _) = batch.next().unwrap();
        assert!(!batch.cancel_one(a));
        assert!(batch.progress(new, None));
        assert!(batch.remove(a, false));
        assert!(!batch.remove(new, false));
        batch.finish(new, Ok(()));
        assert!(!batch.remove(new, false));
        assert!(batch.remove(new, true));
        batch.clear();
        batch.add("new list".into(), None);
        assert!(!batch.remove(a, false));
    }

    #[test]
    fn runs_in_order_with_one_snapshot_and_one_active_file() {
        let mut b = Batch::default();
        for name in ["a", "b", "c"] {
            b.add(name.into(), None);
        }
        assert!(b.next().is_none());
        b.start(String::from("first"));
        let (a, _) = b.next().unwrap();
        assert!(b.next().is_none());
        assert!(b.progress(a, Some((2, 3))));
        b.start(String::from("changed"));
        b.add("d".into(), None);
        assert_eq!(b.settings().unwrap(), "first");
        b.remove(a, false);
        b.remove(2, false);
        assert_eq!(b.rows().len(), 3);
        b.finish(63, Err("stale"));
        assert_eq!(b.active(), Some(a));
        assert!(b.saving(a));
        b.cancel(false); // file publication already owns this row
        b.finish(a, Ok("saved path"));
        assert_eq!(b.rows()[0].state, State::Saved("saved path"));
        let (b_id, _) = b.next().unwrap();
        b.finish(b_id, Err("open"));
        assert_eq!(b.rows()[1].state, State::Failed("open"));
        let (d, _) = b.next().unwrap();
        assert_eq!(d, 3);
        b.finish(d, Err("translation"));
        assert!(!b.pending());
    }

    #[test]
    fn cancellation_waits_for_ack_and_preserves_finished_rows() {
        let mut b = Batch::default();
        for i in 0..5 {
            b.add(i.to_string().into(), (i == 0).then_some("unsupported"));
        }
        b.start(());
        let (id, _) = b.next().unwrap();
        b.cancel(false);
        assert!(!b.progress(id, None));
        assert!(b.next().is_none());
        b.finish(id, Err("read returned"));
        let (next, _) = b.next().unwrap();
        assert!(!b.cancel_one(id));
        assert!(b.progress(next, None));
        b.cancel(true);
        assert!(!b.saving(next));
        b.finish(next, Ok(()));
        assert!(!b.pending());
        assert_eq!(b.rows()[0].state, State::Failed("unsupported"));
        assert!(b.rows()[1..].iter().all(|r| r.state == State::Cancelled));
    }
}
