use super::*;
use crate::view::ModelChoice;
use batch::{Batch, State};
use std::collections::{HashMap, HashSet};

pub(super) struct BatchSettings {
    request: RequestSettings,
    pdf_output: PdfOutput,
    /// The source chosen at the start, for BabelDOC's PDFs only.
    pdf_source: SourceChoice,
    automatic: bool,
    formats: &'static [FileFormat],
}

impl BatchSettings {
    fn pdf_output(&self) -> PdfOutput {
        self.pdf_output
    }

    fn request_for(&self, format: FileFormat) -> RequestSettings {
        let mut request = self.request.clone();
        request.source = self.source_for(format);
        request
    }

    fn source_for(&self, format: FileFormat) -> SourceChoice {
        if format == FileFormat::Pdf
            && self.pdf_output == PdfOutput::Papers
            && ::files::pdf::supports_pdf(self.request.target.resolve_for(false))
        {
            self.pdf_source
        } else {
            self.request.source
        }
    }
}

pub(super) struct SavedFile {
    path: PathBuf,
    review: FileReview,
    notice: Option<FileNotice>,
    encoding: Option<&'static str>,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Saved,
    Review,
    Failed,
    Stopped,
    /// The batch's model cannot translate the file's format.
    Skipped,
}

impl SavedFile {
    fn needs_review(&self) -> bool {
        self.notice.as_ref().is_some_and(|n| !n.success())
            || self.review.partial_translation
            || self.review.show_document_counts
                && (self.review.kept_as_source > 0 || self.review.simplified_paragraphs > 0)
            || self.review.update_toc
            || self.review.word_overflow > 0
            || self.review.check_slides
            || self.review.pdf_text
            || self.review.check_pdf
            || !self.review.powerpoint_frames.is_empty()
    }

    fn warning(&self) -> Option<String> {
        if let Some(FileNotice::Layout(layout)) = &self.notice
            && let Some(line) = layout.warning()
        {
            return Some(line);
        }
        let review = &self.review;
        if let Some(word) = review.word_warning() {
            return Some(word.into_owned());
        }
        [
            (review.pdf_text, "files.pdf_text_output"),
            (review.partial_translation, "files.batch_partial"),
            (review.update_toc, "files.update_toc"),
            (
                review.check_slides || !review.powerpoint_frames.is_empty(),
                "files.check_slides",
            ),
            (review.check_pdf, "files.check_pdf"),
        ]
        .into_iter()
        .find(|(needed, _)| *needed)
        .map(|(_, key)| t!(key).into_owned())
    }
}

fn outcome(state: &State<SavedFile, FileNotice>) -> Option<Outcome> {
    match state {
        State::Saved(saved) if saved.needs_review() => Some(Outcome::Review),
        State::Saved(_) => Some(Outcome::Saved),
        State::Failed(FileNotice::ModelFormats { .. }) => Some(Outcome::Skipped),
        State::Failed(_) => Some(Outcome::Failed),
        State::Cancelled => Some(Outcome::Stopped),
        _ => None,
    }
}

// Retain at most one failed document alongside the sequential worker.
struct UnsavedBatchFile {
    source: PathBuf,
    doc: FileDocument,
    done: CompletedFile,
    target: String,
}

impl UnsavedBatchFile {
    fn save(&self, destination: Option<&Path>) -> Result<SavedFile, FileNotice> {
        save_translation(
            &self.source,
            &self.doc,
            &self.done.output,
            &self.target,
            destination,
        )
        .map(|(path, summary)| SavedFile {
            path,
            notice: summary.map(FileNotice::Layout),
            review: self.done.review.clone(),
            encoding: self.doc.encoding(),
        })
        .map_err(|error| FileNotice::error(&error, Operation::Save))
    }
}

enum ListAction {
    Clear,
    Remove(usize),
    Replace(batch_input::Selection),
    Close { quit: bool },
    Restart,
}

impl ListAction {
    fn recovery_at_risk(&self, retained: Option<usize>) -> Option<usize> {
        retained.filter(|id| match self {
            Self::Remove(row) => row == id,
            Self::Clear | Self::Replace(_) | Self::Close { .. } | Self::Restart => true,
        })
    }
}

#[derive(Default)]
pub(in crate::view) struct BatchPanel {
    pub(super) work: Batch<BatchSettings, SavedFile, FileNotice>,
    expanded: Option<usize>,
    recovery: Option<(usize, Arc<UnsavedBatchFile>)>,
    retrying: Option<usize>,
    after_single_save: Option<batch_input::Selection>,
    interrupted: bool,
    routing: bool,
    routes: HashMap<usize, (FileInput, FileRoute)>,
    upgrade_suggestions: HashSet<usize>,
    pending_request: Option<Request>,
    input_notice: Option<FileNotice>,
    selection: batch_input::SelectionNotes,
}

impl BatchPanel {
    fn retain_failed(
        &mut self,
        id: usize,
        file: Arc<UnsavedBatchFile>,
        error: FileNotice,
    ) -> FileNotice {
        if self.recovery.is_some() {
            return match error.details() {
                Some(detail) => FileNotice::failure("files.recovery_limit", detail),
                None => FileNotice::text("files.recovery_limit"),
            };
        }
        self.recovery = Some((id, file));
        error
    }

    pub(super) fn retry_busy(&self) -> bool {
        self.retrying.is_some()
    }
    pub(super) fn has_recovery(&self) -> bool {
        self.recovery.is_some()
    }
    pub(in crate::view) fn fixed_settings(&self) -> Option<&RequestSettings> {
        self.work.settings().map(|s| &s.request)
    }

    pub(super) fn selection_lines(&self) -> Vec<String> {
        self.selection.lines()
    }

    fn clear(&mut self) {
        self.work.clear();
        self.routes.clear();
        self.upgrade_suggestions.clear();
        self.pending_request = None;
        self.recovery = None;
        self.interrupted = false;
        self.expanded = None;
        self.input_notice = None;
        self.selection = Default::default();
    }

    pub(in crate::view) fn summary(&self) -> String {
        let rows = self.work.rows();
        let total = rows.len();
        if let Some(current) = rows
            .iter()
            .position(|row| Some(row.id) == self.work.active())
        {
            return t!("files.batch_progress", current = current + 1, total = total).into_owned();
        }
        let (mut saved, mut review, mut failed, mut stopped, mut skipped) = (0, 0, 0, 0, 0);
        for row in rows {
            match outcome(&row.state) {
                Some(Outcome::Saved) => saved += 1,
                Some(Outcome::Review) => {
                    saved += 1;
                    review += 1;
                }
                Some(Outcome::Failed) => failed += 1,
                Some(Outcome::Stopped) => stopped += 1,
                Some(Outcome::Skipped) => skipped += 1,
                None => {}
            }
        }
        let waiting = total - saved - failed - stopped - skipped;
        let mut parts = vec![if self.work.settings().is_none() && saved == 0 {
            t!("files.batch_waiting", total = total, waiting = waiting).into_owned()
        } else {
            t!("files.batch_finished", total = total, saved = saved).into_owned()
        }];
        if waiting > 0 && (self.work.settings().is_some() || saved > 0) {
            parts.push(t!("files.remaining_waiting", count = waiting).into_owned());
        }
        for (count, key) in [
            (
                review,
                if review == 1 {
                    "files.batch_review_one"
                } else {
                    "files.batch_review"
                },
            ),
            (failed, "files.batch_failed"),
            (stopped, "files.batch_stopped"),
            (skipped, "files.batch_skipped"),
        ] {
            if count > 0 {
                parts.push(t!(key, count = count).into_owned());
            }
        }
        parts.join("　")
    }

    pub(in crate::view) fn visible(&self) -> bool {
        !self.work.rows().is_empty()
    }

    /// The next file to translate. A file in a format the batch's model cannot translate is
    /// skipped with a notice naming the formats it can, and the queue goes on.
    fn next_translatable(&mut self) -> Option<(usize, PathBuf)> {
        let settings = self.work.settings()?;
        if settings.automatic {
            return self.work.next();
        }
        let formats = settings.formats;
        loop {
            let (id, path) = self.work.next()?;
            if let Some(format) = path_format(&path)
                && !formats.contains(&format)
            {
                crate::app_log!("batch: skipped a file in a format the model cannot translate");
                self.work
                    .finish(id, Err(FileNotice::model_formats(formats, format)));
            } else {
                return Some((id, path));
            }
        }
    }
}

#[path = "batch_input.rs"]
mod batch_input;

impl TranslatorView {
    fn appends_selection(&self) -> bool {
        self.batch.visible()
            && (self.jobs.busy()
                || self.batch.has_recovery()
                || self.batch.work.pending()
                || self.batch.work.settings().is_none())
    }

    pub(super) fn can_add_files(&self) -> bool {
        !self.picking_files
            && !self.batch.retry_busy()
            && (self.batch.visible() || !self.file_operation_busy())
    }

    pub(super) fn show_input_notice(&mut self, notice: FileNotice) {
        if self.batch.visible() {
            self.batch.input_notice = Some(notice);
        } else {
            self.files.message = Some(notice);
        }
    }

    pub(in crate::view) fn accept_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_add_files() || paths.is_empty() || window.has_active_dialog(cx) {
            return;
        }
        self.file_tab = true;
        self.picking_files = true;
        let existing = if self.appends_selection() {
            self.batch
                .work
                .rows()
                .iter()
                .map(|row| row.path.clone())
                .collect()
        } else {
            Vec::new()
        };
        cx.spawn_in(window, async move |this, cx| {
            let selection = cx
                .background_spawn(async move { batch_input::expand_paths(paths, existing) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.picking_files = false;
                let replacing = !this.batch.visible()
                    && (selection.inputs.len() > 1 || selection.grouped)
                    && !selection.inputs.is_empty()
                    && this.files.saved.is_none()
                    && this
                        .files
                        .completed
                        .as_ref()
                        .is_some_and(|done| done.has_changes);
                if replacing {
                    this.confirm_batch_selection(selection, window, cx);
                } else {
                    this.apply_selection(selection, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn confirm_batch_selection(
        &self,
        selection: batch_input::Selection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(selection)));
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let save = pending.clone();
            let discard = pending.clone();
            let save_view = view.clone();
            let discard_view = view.clone();
            dialog.title(t!("files.unsaved").into_owned()).footer(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("cancel-selection")
                            .label(t!("files.cancel_selection").into_owned())
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("discard-selection")
                            .label(t!("files.discard_continue").into_owned())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                if let Some(selection) = discard.borrow_mut().take() {
                                    let _ = discard_view.update(cx, |this, cx| {
                                        this.apply_selection(selection, window, cx)
                                    });
                                }
                            }),
                    )
                    .child(
                        file_action(Button::new("save-selection"), false, cx)
                            .label(t!("files.save").into_owned())
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                let _ = save_view.update(cx, |this, cx| {
                                    this.batch.after_single_save = save.borrow_mut().take();
                                    this.save_file(false, window, cx);
                                });
                            }),
                    ),
            )
        });
    }

    pub(super) fn after_single_save(
        &mut self,
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(selection) = self.batch.after_single_save.take()
            && saved
        {
            self.apply_selection(selection, window, cx);
        }
    }

    fn apply_selection(
        &mut self,
        selection: batch_input::Selection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if selection.inputs.is_empty() {
            self.batch.selection = selection.notes;
            self.resume_batch_after_input(window, cx);
            cx.notify();
        } else if self.appends_selection() {
            self.populate_selection(selection, true, window, cx);
        } else {
            self.request_list_action(ListAction::Replace(selection), window, cx);
        }
    }

    fn populate_selection(
        &mut self,
        selection: batch_input::Selection,
        append: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let batch_input::Selection {
            inputs,
            notes,
            grouped,
        } = selection;
        if self.batch.has_recovery() && !self.jobs.busy() && !self.batch.work.pending() {
            self.batch.work.edit_settings();
        }
        if !append {
            self.batch.clear();
            self.batch.selection = notes;
            if inputs.len() == 1 && !grouped {
                let (path, error) = inputs.into_iter().next().unwrap();
                if let Some(error) = error {
                    self.files.message = Some(error);
                } else {
                    self.read_file(path, cx);
                }
                cx.notify();
                return;
            }
            self.files = FilePanel::default();
        } else {
            self.batch.selection = notes;
        }
        self.batch.input_notice = None;
        for (path, error) in inputs {
            self.batch.work.add(path, error);
        }
        self.resume_batch_after_input(window, cx);
        cx.notify();
    }

    pub(super) fn resume_batch_after_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picking_files || self.batch.routing {
            return;
        }
        if self.jobs.busy() && self.batch.visible() && self.batch.work.active().is_none() {
            self.next_batch_file(window, cx);
        }
    }

    pub(super) fn start_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.batch.retry_busy()
            || self.batch.work.settings().is_some()
            || !self.batch.work.pending()
        {
            return;
        }
        let automatic = self.languages.automatic;
        let pdf_output = self.pdf_output();
        let papers = self.batch_source_for_babeldoc();
        let source = if !papers && (automatic || self.requires_language_pair()) {
            self.languages.source
        } else {
            SourceChoice::Detect
        };
        let settings = BatchSettings {
            request: self.request_settings(source),
            pdf_output,
            pdf_source: self.languages.source,
            automatic,
            formats: self.capabilities().files,
        };
        self.batch.routes.clear();
        self.required_model = None;
        self.batch.interrupted = false;
        self.batch.work.start(settings);
        self.jobs.start_batch();
        if !automatic {
            self.next_batch_file(window, cx);
            return;
        }
        let waiting: Vec<_> = self
            .batch
            .work
            .rows()
            .iter()
            .filter(|row| matches!(row.state, State::Waiting))
            .map(|row| (row.id, row.path.clone()))
            .collect();
        self.batch.routing = true;
        cx.spawn_in(window, async move |this, cx| {
            let inputs = cx
                .background_spawn(async move {
                    waiting
                        .into_iter()
                        .map(|(id, path)| {
                            let input = FileDocument::read(&path, pdf_output)
                                .map_err(|error| FileNotice::error(&error, Operation::Open))
                                .and_then(|doc| {
                                    if let Some(notice) = doc.empty_notice() {
                                        return Err(notice);
                                    }
                                    Ok(FileInput::new(&doc, doc.source_hint()))
                                });
                            (id, input)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.batch.routing = false;
                let Some(settings) = this.batch.work.settings() else {
                    return;
                };
                let installed = this
                    .active_model
                    .as_ref()
                    .and_then(ModelChoice::entry)
                    .map(|entry| entry.id());
                let mut routes = HashMap::new();
                let mut failures = Vec::new();
                for (id, input) in inputs {
                    let Some(row) = this.batch.work.row(id) else {
                        continue;
                    };
                    if !matches!(row.state, State::Waiting) {
                        continue;
                    }
                    match input.map_err(FileRequirement::from).and_then(|input| {
                        this.route_file(input, &settings.request_for(input.format), true, true, cx)
                            .map(|route| (input, route))
                    }) {
                        Ok(route) => {
                            routes.insert(id, route);
                        }
                        Err(requirement) => {
                            failures.push((id, path_format(&row.path), requirement))
                        }
                    }
                }
                // Retain the decisions and expose failed preflights before the first translation.
                for (id, format, requirement) in failures {
                    this.show_batch_requirement(format, &requirement, cx);
                    if let Some(row) = this.batch.work.row_mut(id) {
                        row.state = State::Failed(requirement.notice);
                    }
                }
                this.batch.work.prioritize(|id| {
                    let model = routes
                        .get(&id)
                        .and_then(|(_, route)| route.entry)
                        .map(CatalogEntry::id);
                    (
                        model.is_none() || model != installed,
                        model.unwrap_or("\u{10ffff}"),
                    )
                });
                this.batch.routes = routes;
                this.next_batch_file(window, cx);
            });
        })
        .detach();
    }

    fn next_batch_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.batch.routing || self.batch.work.active().is_some() {
            return;
        }
        self.files = FilePanel::default();
        self.batch.pending_request = None;
        if self.runtime_error.is_some() || !self.model_ready && !self.languages.automatic {
            self.batch.interrupted = true;
            self.batch.work.edit_settings();
            self.jobs.finish_batch();
            self.install_deferred_model_updates(window, cx);
            cx.notify();
            return;
        }
        let Some((id, path)) = self.batch.next_translatable() else {
            if self.picking_files {
                return;
            }
            self.batch.routes.clear();
            if self.jobs.finish_batch() {
                self.translate_now(window, cx);
            }
            self.install_deferred_model_updates(window, cx);
            cx.notify();
            return;
        };
        let pdf = self.batch.work.settings().unwrap().pdf_output();
        self.files.io_busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    ::files::check_batch_destination(&path, |probe, error| {
                        crate::app_log!(
                            "could not remove batch write probe {}: {error}",
                            probe.display()
                        );
                    })
                    .map_err(|e| FileNotice::batch_preflight_error(&e))?;
                    let doc = FileDocument::read(&path, pdf)
                        .map_err(|e| FileNotice::error(&e, Operation::Open))?;
                    if let Some(error) = doc.empty_notice() {
                        return Err(error);
                    }
                    let source_hint = doc.source_hint();
                    Ok((path, doc, source_hint))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.files.io_busy = false;
                match result {
                    Ok((path, doc, source_hint)) if this.batch.work.progress(id, None) => {
                        this.files.loaded = Some((path, doc));
                        this.files.source_hint = source_hint;
                        this.submit_batch_document(id, window, cx);
                    }
                    other => {
                        let error = other
                            .err()
                            .unwrap_or_else(|| FileNotice::text("files.translation_failed"));
                        this.batch.work.finish(id, Err(error));
                        this.next_batch_file(window, cx);
                    }
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub(in crate::view) fn batch_load_failed(
        &mut self,
        id: usize,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stopping = self
            .batch
            .work
            .row(id)
            .is_some_and(|row| matches!(row.state, State::Stopping));
        self.batch.work.finish(
            id,
            Err(FileNotice::failure(
                "files.translation_failed",
                error.to_owned(),
            )),
        );
        if stopping && self.model_ready {
            self.next_batch_file(window, cx);
            return;
        }
        self.batch.interrupted = true;
        self.batch.work.edit_settings();
        if self.jobs.finish_batch() {
            self.translate_now(window, cx);
        }
        cx.notify();
    }

    fn show_batch_requirement(
        &mut self,
        format: Option<FileFormat>,
        requirement: &FileRequirement,
        cx: &App,
    ) {
        if self.required_model.is_none()
            && requirement.suggestion.is_some()
            && (format.is_some_and(FileFormat::is_document)
                || !self.all_installed_models_failed(cx))
        {
            self.required_model = requirement.suggestion;
            self.required_format = format;
        }
    }

    fn submit_batch_document(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, doc)) = &self.files.loaded else {
            return;
        };
        let settings = self.batch.work.settings().unwrap();
        let input = FileInput::new(doc, self.files.source_hint);
        let request_settings = settings.request_for(input.format);
        let route = match self.batch.routes.remove(&id) {
            Some((planned, route)) if planned == input => Ok(route),
            Some(_) => Err("files.batch_source_changed".into()),
            // Files added after the batch started are routed before their translation begins.
            None => self.route_file(input, &request_settings, settings.automatic, true, cx),
        };
        let route = match route {
            Ok(route) => route,
            Err(requirement) => {
                self.show_batch_requirement(Some(input.format), &requirement, cx);
                self.batch.work.finish_deferred(id, requirement.notice);
                self.next_batch_file(window, cx);
                return;
            }
        };
        if route.suggestion.is_some() {
            self.batch.upgrade_suggestions.insert(id);
        }
        let entry = route.entry;
        let request = route.request(request_settings, doc, input.hint);
        if let Some(entry) = entry {
            let choice = ModelChoice::Catalog(entry);
            if !self.model_ready || self.active_model.as_ref() != Some(&choice) {
                self.remember_rollback(&choice);
                self.batch.pending_request = Some(request);
                self.batch_on_load = Some(id);
                self.load_model(choice, window, cx);
                return;
            }
        }
        self.submit_file(request, window, cx);
    }

    pub(in crate::view) fn resume_batch_document(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = self.batch.pending_request.take()
            && self.batch.work.progress(id, None)
        {
            self.submit_file(request, window, cx);
        } else {
            self.batch
                .work
                .finish(id, Err(FileNotice::text("files.translation_failed")));
            self.next_batch_file(window, cx);
        }
    }

    pub(super) fn finish_batch_translation(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(done) = self.files.completed.take()
            && done.has_changes
            && self.batch.work.saving(id)
        {
            let (source, doc) = self.files.loaded.take().unwrap();
            let file = Arc::new(UnsavedBatchFile {
                source,
                doc,
                done,
                target: std::mem::take(&mut self.files.target),
            });
            self.files.io_busy = true;
            cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_spawn(async move { file.save(None).map_err(|error| (error, file)) })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| {
                    let result =
                        result.map_err(|(error, file)| this.batch.retain_failed(id, file, error));
                    this.batch.work.finish(id, result);
                    this.next_batch_file(window, cx);
                });
            })
            .detach();
        } else {
            let error = self
                .files
                .message
                .take()
                .unwrap_or_else(|| FileNotice::text("files.no_changes"));
            self.batch.work.finish(id, Err(error));
            self.next_batch_file(window, cx);
        }
        cx.notify();
    }

    pub(super) fn confirm_recovery_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_list_action(ListAction::Close { quit }, window, cx);
    }

    pub(in crate::view) fn request_update_restart(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.file_operation_busy() && !self.batch.retry_busy() {
            self.request_list_action(ListAction::Restart, window, cx);
        }
    }

    /// Quits, and has the prepared version swap itself in and start.
    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if self.file_operation_busy() {
            return;
        }
        let InstallState::Ready(version) = self.install else {
            return;
        };
        let Some(Ok(installation)) = &self.installation else {
            return;
        };
        let installation = installation.clone();
        // The Restart action has already asked about this unsaved file, if there is one.
        let confirmed_recovery = self
            .batch
            .recovery
            .as_ref()
            .map(|(_, file)| Arc::clone(file));
        self.install = InstallState::Verifying(version);
        cx.notify();
        self._install_task = cx.spawn(async move |this, cx| {
            let verified = cx
                .background_spawn(async move { update::apply::verify_start(&installation) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let result = verified.and_then(|update| {
                    // Work may have started while Gatekeeper was assessing the update.
                    let new_recovery = this.batch.recovery.as_ref().is_some_and(|(_, file)| {
                        confirmed_recovery
                            .as_ref()
                            .is_none_or(|confirmed| !Arc::ptr_eq(file, confirmed))
                    });
                    if this.file_operation_busy() || this.batch.retry_busy() || new_recovery {
                        this.install = InstallState::Ready(version);
                        cx.notify();
                        return Ok(());
                    }
                    update.start()?;
                    crate::app_log!("quitting to install the update to {version}");
                    cx.quit();
                    Ok(())
                });
                if let Err(e) = result {
                    crate::app_log!("cannot start the update to {version}: {e}");
                    this.install = InstallState::Failed(e.to_string());
                    cx.notify();
                }
            });
        });
    }

    fn request_list_action(
        &mut self,
        action: ListAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) || self.picking_files || self.batch.retry_busy() {
            return;
        }
        let Some(id) = action.recovery_at_risk(self.batch.recovery.as_ref().map(|(id, _)| *id))
        else {
            self.finish_list_action(action, false, window, cx);
            return;
        };
        let pending = std::rc::Rc::new(std::cell::RefCell::new(Some(action)));
        let view = cx.entity().downgrade();
        let name = saved_filename(&self.batch.work.row(id).unwrap().path);
        let stopping =
            self.jobs.busy() && matches!(*pending.borrow(), Some(ListAction::Close { .. }));
        window.open_dialog(cx, move |dialog, _, cx| {
            let discard = pending.clone();
            let save = pending.clone();
            let discard_view = view.clone();
            let save_view = view.clone();
            dialog
                .title(t!("files.unsaved").into_owned())
                .child(name.clone())
                .when(stopping, |dialog| {
                    dialog.child(t!("files.batch_close").into_owned())
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("cancel-recovery")
                                .label(t!("files.cancel_selection").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("discard-recovery")
                                .label(t!("files.discard_continue").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    if let Some(action) = discard.borrow_mut().take() {
                                        let _ = discard_view.update(cx, |this, cx| {
                                            this.finish_list_action(action, false, window, cx)
                                        });
                                    }
                                }),
                        )
                        .child(
                            file_action(Button::new("save-recovery"), false, cx)
                                .label(t!("files.save_as").into_owned())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = save_view.update(cx, |this, cx| {
                                        this.save_batch_as(id, save.borrow_mut().take(), window, cx)
                                    });
                                }),
                        ),
                )
        });
    }

    fn finish_list_action(
        &mut self,
        action: ListAction,
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            ListAction::Clear => {
                if self.jobs.busy() {
                    return;
                }
                self.batch.clear();
                self.files = Default::default();
                cx.notify();
            }
            ListAction::Remove(id) => self.remove_row(id, saved, cx),
            ListAction::Replace(selection) => self.populate_selection(selection, false, window, cx),
            ListAction::Close { quit } => self.finish_file_close(quit, window, cx),
            ListAction::Restart => self.restart_to_update(cx),
        }
    }

    fn save_batch_as(
        &mut self,
        id: usize,
        action: Option<ListAction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.picking_files
            || self.batch.retry_busy()
            || !self
                .batch
                .work
                .row(id)
                .is_some_and(|r| matches!(r.state, State::Failed(_)))
        {
            return;
        }
        let Some((_, file)) = self.batch.recovery.as_ref().filter(|(row, _)| *row == id) else {
            return;
        };
        let file = file.clone();
        let suggested = ::files::translation_path(
            &file.source,
            &file.target,
            matches!(file.doc, FileDocument::PdfLayout(_)),
        )
        .ok();
        let picker = cx.prompt_for_new_path(
            file.source.parent().unwrap_or(Path::new(".")),
            suggested
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|s| s.to_str()),
        );
        self.batch.retrying = Some(id);
        cx.spawn_in(window, async move |this, cx| {
            let result = match picker.await {
                Ok(Ok(Some(path))) => Some(
                    cx.background_spawn(async move { file.save(Some(&path)) })
                        .await,
                ),
                Ok(Err(e)) => Some(Err(FileNotice::failure(
                    "files.picker_failed",
                    e.to_string(),
                ))),
                _ => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.batch.retrying = None;
                if let Some(row) = this.batch.work.row_mut(id)
                    && matches!(row.state, State::Failed(_))
                    && let Some(result) = result
                {
                    match result {
                        Ok(saved) => {
                            this.batch.recovery = None;
                            row.state = State::Saved(saved);
                            if let Some(action) = action {
                                this.finish_list_action(action, true, window, cx);
                            }
                        }
                        Err(error) => row.state = State::Failed(error),
                    };
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn stop_batch(&mut self, id: Option<usize>, cx: &mut Context<Self>) {
        if let Some(id) = id {
            if !self.batch.work.cancel_one(id) {
                return;
            }
        } else {
            self.batch.work.cancel(true);
        }
        if self.jobs.file_running() {
            self.cancel_file(cx);
        }
        cx.notify();
    }

    fn remove_row(&mut self, id: usize, just_saved: bool, cx: &mut Context<Self>) {
        if self.picking_files || self.batch.retrying == Some(id) {
            return;
        }
        if !self.batch.work.remove(id, just_saved) {
            return;
        }
        self.batch.upgrade_suggestions.remove(&id);
        if self
            .batch
            .recovery
            .as_ref()
            .is_some_and(|(row, _)| *row == id)
        {
            self.batch.recovery = None;
        }
        let rows = self.batch.work.rows();
        if self.batch.work.settings().is_none()
            && let [row] = rows
            && matches!(row.state, State::Waiting)
        {
            let path = row.path.clone();
            self.batch.clear();
            self.read_file(path, cx);
        }
        cx.notify();
    }

    pub(super) fn render_batch(&self, cx: &mut Context<Self>) -> AnyElement {
        let batch = &self.batch.work;
        let running = self.jobs.busy();
        let draft = batch.settings().is_none();
        let pdf_text = !::files::pdf::supports_pdf(
            batch
                .settings()
                .map_or(self.languages.target(&self.settings), |s| s.request.target)
                .resolve_for(false),
        );
        let selected = batch
            .settings()
            .map_or(self.pdf_output(), |s| s.pdf_output());
        let pdf_choices = PdfOutput::choices(self.external_engine.ready());
        let translates_pdf = self.languages.automatic
            || batch
                .settings()
                .map_or(self.capabilities().files, |s| s.formats)
                .contains(&FileFormat::Pdf);
        let mut panel = v_flex()
            .w_full()
            .max_w(scaled(FILE_PANEL_WIDTH))
            .mx_auto()
            .gap_2();
        let actions = h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("add-files")
                    .ghost()
                    .label(
                        t!(if self.appends_selection() {
                            "files.add"
                        } else {
                            "files.new_list"
                        })
                        .into_owned(),
                    )
                    .disabled(self.picking_files || self.batch.retry_busy())
                    .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
            )
            .when(draft && batch.pending(), |bar| {
                bar.child(
                    file_action(
                        Button::new("start-batch").label(t!("files.start_batch").into_owned()),
                        self.picking_files,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.start_file(window, cx))),
                )
            })
            .when(running, |bar| {
                bar.child(
                    Button::new("stop-all")
                        .danger()
                        .outline()
                        .label(t!("files.stop_all").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| this.stop_batch(None, cx))),
                )
            })
            .when(!running, |bar| {
                bar.child(
                    Button::new("clear-list")
                        .ghost()
                        .label(t!("files.clear_list").into_owned())
                        .disabled(self.picking_files || self.batch.retry_busy())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.request_list_action(ListAction::Clear, window, cx);
                        })),
                )
            });
        panel = panel.child(actions).child(
            div()
                .text_sm()
                .child(t!("files.batch_save_hint").into_owned()),
        );
        if translates_pdf
            && batch.rows().iter().any(|r| {
                r.path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
            })
        {
            panel = panel.child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .child(t!("files.batch_pdf_output").into_owned())
                    .map(|bar| {
                        if pdf_text {
                            bar.child("TXT")
                        } else if draft {
                            bar.children(pdf_choices.into_iter().enumerate().map(
                                |(index, (choice, key))| {
                                    Button::new(("batch-format", index))
                                        .small()
                                        .outline()
                                        .selected(selected == choice)
                                        .label(format!(
                                            "{}{}",
                                            if choice == selected { "✓ " } else { "" },
                                            t!(key)
                                        ))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if this.batch.work.settings().is_some() {
                                                return;
                                            }
                                            choice.store(&mut this.settings);
                                            this.settings.save();
                                            cx.notify();
                                        }))
                                },
                            ))
                        } else {
                            bar.child(div().text_sm().child(t!(selected.label()).into_owned()))
                        }
                    }),
            );
        }
        if self.batch.interrupted {
            panel = panel.child(
                div()
                    .text_sm()
                    .child(t!("files.batch_runtime_stopped").into_owned()),
            );
        }
        panel = panel.children(self.batch.selection.lines());
        if let Some(notice) = &self.batch.input_notice {
            panel = panel.child(notice.summary()).children(notice.details());
        }
        panel = panel.child(div().font_semibold().child(self.batch.summary()));
        let offer_7b = !self
            .models
            .read(cx)
            .store()
            .state(&catalog::HY_MT2_7B)
            .is_usable();
        for row in batch.rows() {
            let id = row.id;
            let name = saved_filename(&row.path);
            let status = match &row.state {
                State::Waiting => t!("files.waiting"),
                State::Reading => t!("files.reading"),
                State::Stopping => t!("files.stopping"),
                State::Translating(None) => t!("files.starting"),
                State::Translating(Some((done, _)))
                    if self.files.external.is_some() && batch.active() == Some(id) =>
                {
                    t!("files.babeldoc_progress", percent = done)
                }
                State::Translating(Some((done, total))) => {
                    t!("files.progress", done = done, total = total)
                }
                State::Saving => t!("files.saving"),
                State::Saved(saved) => t!("files.saved", name = saved_filename(&saved.path)),
                State::Failed(FileNotice::ModelFormats { .. }) => t!("files.skipped"),
                State::Failed(_) => t!("files.failed"),
                State::Cancelled => t!("files.stopped"),
            };
            let mut header = h_flex()
                .flex_wrap()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(140.))
                        .truncate()
                        .font_medium()
                        .child(name),
                )
                .child(div().text_sm().child(status.into_owned()));
            let mut body = v_flex()
                .id(("batch-row", id))
                .gap_1()
                .px_3()
                .py_2()
                .border_1()
                .rounded_lg()
                .border_color(cx.theme().border)
                .flex_shrink_0();
            match &row.state {
                State::Waiting | State::Failed(_) | State::Cancelled => {
                    header = header.child(
                        Button::new(("remove", id))
                            .disabled(self.picking_files || self.batch.retry_busy())
                            .small()
                            .ghost()
                            .label(t!("files.remove").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_list_action(ListAction::Remove(id), window, cx)
                            })),
                    );
                }
                State::Reading | State::Translating(_) => {
                    header = header.child(
                        Button::new(("stop", id))
                            .small()
                            .ghost()
                            .label(t!("files.stop_one").into_owned())
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.stop_batch(Some(id), cx)),
                            ),
                    );
                }
                State::Saved(_) => {
                    header = header
                        .child(
                            Button::new(("open", id))
                                .small()
                                .ghost()
                                .label(t!("files.open_saved").into_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(State::Saved(saved)) =
                                        this.batch.work.row(id).map(|r| &r.state)
                                    {
                                        cx.open_with_system(&saved.path);
                                    }
                                })),
                        )
                        .child(
                            Button::new(("reveal", id))
                                .small()
                                .ghost()
                                .label(t!("files.reveal").into_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(State::Saved(saved)) =
                                        this.batch.work.row(id).map(|r| &r.state)
                                    {
                                        cx.reveal_path(&saved.path);
                                    }
                                })),
                        );
                }
                _ => {}
            }
            if self
                .batch
                .recovery
                .as_ref()
                .is_some_and(|(row, _)| *row == id)
            {
                header = header.child(
                    Button::new(("retry-save", id))
                        .small()
                        .outline()
                        .label(t!("files.save_as").into_owned())
                        .disabled(self.batch.retry_busy())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.save_batch_as(id, None, window, cx)
                        })),
                );
            }
            let notice = match &row.state {
                State::Failed(e) => Some(e),
                State::Saved(s) => s.notice.as_ref(),
                _ => None,
            };
            let warning = match &row.state {
                State::Saved(saved) => saved.warning(),
                _ => None,
            };
            let expandable = notice.is_some_and(|n| n.details().is_some())
                || matches!(&row.state, State::Saved(s)
                    if s.encoding.is_some() || warning.is_some() || s.review.has_file_counts());
            let expanded = self.batch.expanded == Some(id);
            if expandable {
                header = header.child(
                    Button::new(("details", id))
                        .small()
                        .ghost()
                        .label(
                            t!(if expanded {
                                "files.hide_details"
                            } else {
                                "files.show_details"
                            })
                            .into_owned(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.batch.work.row(id).is_some_and(|r| {
                                matches!(r.state, State::Saved(_) | State::Failed(_))
                            }) {
                                return;
                            }
                            this.batch.expanded = (this.batch.expanded != Some(id)).then_some(id);
                            cx.notify();
                        })),
                );
            }
            body = body.child(header);
            if offer_7b
                && (self.batch.upgrade_suggestions.contains(&id)
                    || self
                        .batch
                        .routes
                        .get(&id)
                        .is_some_and(|(_, route)| route.suggestion.is_some()))
                && let Some(format) = path_format(&row.path)
            {
                body = body.child(
                    div()
                        .id(("document-model-suggestion", id))
                        .child(self.render_download(&catalog::HY_MT2_7B, Some(format), cx)),
                );
            }
            if pdf_text
                && translates_pdf
                && row
                    .path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
                && !matches!(row.state, State::Saved(_))
            {
                body = body.child(
                    div()
                        .text_sm()
                        .child(t!("files.pdf_text_output").into_owned()),
                );
            }
            if let Some(line) = &warning {
                body = body
                    .border_color(cx.theme().warning)
                    .child(div().text_sm().child(format!("⚠ {line}")));
            }
            if let Some(notice) = notice {
                if expanded || matches!(row.state, State::Failed(_)) {
                    body = body.child(div().text_sm().child(notice.summary()));
                }
                if expanded {
                    body = body.children(notice.details().map(|d| div().text_sm().child(d)));
                }
            }
            if expanded && let State::Saved(saved) = &row.state {
                body = body.children(
                    saved
                        .review
                        .guidance()
                        .into_iter()
                        .filter(|line| Some(line) != warning.as_ref())
                        .map(|line| div().text_sm().child(line)),
                );
                if let Some(encoding) = saved.encoding {
                    body = body.child(
                        div()
                            .text_sm()
                            .child(t!("files.encoding", encoding = encoding).into_owned()),
                    );
                }
                if !saved.review.powerpoint_frames.is_empty() {
                    body = body.child(
                        Button::new(("review-slides", id))
                            .small()
                            .ghost()
                            .label(
                                t!(
                                    "files.pptx_review",
                                    count = saved.review.powerpoint_frames.len()
                                )
                                .into_owned(),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(State::Saved(saved)) =
                                    this.batch.work.row(id).map(|r| &r.state)
                                {
                                    powerpoint_review::open(
                                        saved.review.powerpoint_frames.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    );
                }
            }
            panel = panel.child(body);
        }
        div()
            .id("batch-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_5()
            .py_4()
            .child(panel)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn checks_the_retained_row_for_each_destructive_action() {
        use super::ListAction::*;
        for action in [
            Clear,
            Remove(7),
            Replace(Default::default()),
            Close { quit: false },
            Close { quit: true },
            Restart,
        ] {
            assert_eq!(action.recovery_at_risk(Some(7)), Some(7));
            assert_eq!(action.recovery_at_risk(None), None);
        }
        assert_eq!(Remove(8).recovery_at_risk(Some(7)), None);
        assert_eq!(Remove(8).recovery_at_risk(None), None);
    }

    #[test]
    fn recovers_only_one_failed_translation_without_overwriting() {
        use super::*;
        let root = std::env::temp_dir().join(format!("plamo-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("notes.txt");
        std::fs::write(&source, b"original").unwrap();
        ::files::check_batch_destination(&source, |_, e| panic!("{e}")).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let file = Arc::new(UnsavedBatchFile {
            source: source.clone(),
            doc: FileDocument::read(&source, super::PdfOutput::Bilingual)
                .ok()
                .unwrap(),
            target: "ja".into(),
            done: CompletedFile {
                has_changes: true,
                output: FileOutput::Text("translation".into()),
                review: FileReview::default(),
            },
        });
        let mut panel = BatchPanel::default();
        let error = file.save(Some(&source)).err().unwrap();
        panel.retain_failed(7, file.clone(), error);
        let notice = panel.retain_failed(8, file, FileNotice::text("files.save_failed"));
        assert!(matches!(
            notice,
            FileNotice::Message {
                key: "files.recovery_limit",
                ..
            }
        ));
        let (id, kept) = panel.recovery.take().unwrap();
        assert_eq!(id, 7);
        let destination = root.join("other.txt");
        kept.save(Some(&destination)).ok().unwrap();
        assert_eq!(std::fs::read(destination).unwrap(), b"translation");
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        assert!(
            ::files::check_batch_destination(&source.join("no-folder"), |_, e| panic!("{e}"))
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classifies_terminal_rows_and_keeps_active_rows_out_of_totals() {
        use super::*;
        let saved = |review| {
            State::Saved(SavedFile {
                path: "result".into(),
                review,
                encoding: None,
                notice: None,
            })
        };
        assert_eq!(outcome(&saved(FileReview::default())), Some(Outcome::Saved));
        for review in [
            FileReview {
                word_overflow: 1,
                ..Default::default()
            },
            FileReview {
                partial_translation: true,
                ..Default::default()
            },
            FileReview {
                update_toc: true,
                ..Default::default()
            },
            FileReview {
                check_slides: true,
                ..Default::default()
            },
            FileReview {
                check_pdf: true,
                ..Default::default()
            },
        ] {
            assert_eq!(outcome(&saved(review)), Some(Outcome::Review));
        }
        assert_eq!(
            outcome(&State::Failed(FileNotice::text("files.failed"))),
            Some(Outcome::Failed)
        );
        assert_eq!(outcome(&State::Cancelled), Some(Outcome::Stopped));
        for state in [
            State::Waiting,
            State::Reading,
            State::Translating(None),
            State::Stopping,
            State::Saving,
        ] {
            assert_eq!(outcome(&state), None);
        }
    }

    #[test]
    fn a_worker_failure_keeps_its_diagnostic_and_releases_only_that_row() {
        use super::{Batch, Event, FileEvent, FilePanel, State};
        let mut batch = Batch::<(), (), _>::default();
        batch.add("first.txt".into(), None);
        batch.add("second.txt".into(), None);
        batch.start(());
        let (id, _) = batch.next().unwrap();
        batch.progress(id, Some((1, 2)));
        let mut file = FilePanel::default();
        let failed = Event::Failed {
            job: 7,
            error: "test diagnostic".into(),
        };
        assert!(matches!(file.receive(8, &failed), FileEvent::Other));
        assert!(matches!(file.receive(7, &failed), FileEvent::Finished));
        assert!(file.completed.is_none());
        batch.finish(id, Err(file.message.take().unwrap()));
        let State::Failed(notice) = &batch.rows()[0].state else {
            panic!("missing failure")
        };
        assert_eq!(notice.details().as_deref(), Some("test diagnostic"));
        assert_eq!(batch.next().unwrap().0, 1);
    }

    fn external_settings(profile: engine::llama::ModelProfile) -> super::BatchSettings {
        use super::PdfOutput;
        use crate::lang::{SourceChoice, StyleChoice, TargetChoice};
        super::BatchSettings {
            automatic: false,
            pdf_output: PdfOutput::Bilingual,
            pdf_source: SourceChoice::Lang("en"),
            formats: profile.capabilities().files,
            request: super::RequestSettings {
                profile,
                source: SourceChoice::Lang("en"),
                target: TargetChoice::Lang("ja"),
                style: StyleChoice::Polite,
                document: Default::default(),
                context: String::new(),
                glossary: Vec::new(),
            },
        }
    }

    #[test]
    fn a_batch_skips_only_the_formats_its_model_cannot_translate() {
        use super::*;
        use engine::llama::ModelProfile;
        let _lock = crate::lang::TEST_LOCALE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = rust_i18n::locale().to_string();
        rust_i18n::set_locale("en");
        let names = ["a.pdf", "b.txt", "c.docx", "d.md", "e.pptx", "f.txt"];
        for (profile, formats, translated) in [
            (
                ModelProfile::PlamoPfml,
                ModelProfile::PlamoPfml.capabilities().files,
                &names[..],
            ),
            (
                ModelProfile::HyMt2,
                catalog::HY_MT2_7B.capabilities().files,
                &names[..],
            ),
            (
                ModelProfile::HyMt2,
                catalog::HY_MT2.capabilities().files,
                &names[..],
            ),
            (
                ModelProfile::IndexTranslate,
                catalog::INDEX_TRANSLATE.capabilities().files,
                &["b.txt", "c.docx", "d.md", "f.txt"][..],
            ),
        ] {
            let mut panel = BatchPanel::default();
            for name in names {
                panel.work.add(name.into(), None);
            }
            // Nothing runs before the batch starts with its settings.
            assert!(panel.next_translatable().is_none());
            let mut settings = external_settings(profile);
            settings.formats = formats;
            panel.work.start(settings);
            let mut done = Vec::new();
            while let Some((id, path)) = panel.next_translatable() {
                done.push(path.clone());
                assert!(panel.work.progress(id, None));
                // A skipped row never becomes the active one.
                assert_eq!(panel.work.active(), Some(id));
                panel
                    .work
                    .finish(id, Err(FileNotice::text("files.no_changes")));
            }
            assert_eq!(
                done,
                translated.iter().map(PathBuf::from).collect::<Vec<_>>()
            );
            let skipped = names.len() - translated.len();
            for row in panel.work.rows() {
                let expected = if translated.contains(&row.path.to_str().unwrap()) {
                    Outcome::Failed
                } else {
                    Outcome::Skipped
                };
                assert_eq!(outcome(&row.state), Some(expected), "{profile:?}");
                if let State::Failed(notice @ FileNotice::ModelFormats { .. }) = &row.state {
                    let formats = notice::format_names(formats);
                    assert!(notice.summary().contains(&formats));
                }
            }
            let summary = panel.summary();
            assert_eq!(summary.contains("Skipped:"), skipped > 0, "{summary}");
            if skipped > 0 {
                assert!(
                    summary.contains(&format!("Skipped: {skipped}")),
                    "{summary}"
                );
            }
            assert!(!summary.contains("Waiting"), "{summary}");
        }
        rust_i18n::set_locale(&previous);
    }

    #[test]
    fn an_external_batch_uses_the_chosen_source_and_no_style() {
        use super::FileDocument;
        use engine::llama::ModelProfile;
        let fixed = external_settings(ModelProfile::HyMt2);
        let doc = ::files::Document::open("A short note.".as_bytes(), "sample.txt").unwrap();
        let doc = FileDocument::Text(std::sync::Arc::new(doc));
        let request = doc.request(&fixed.request, doc.source_hint());
        assert_eq!(request.options.source.as_ref().unwrap().as_str(), "en");
        assert_eq!(request.options.target.as_str(), "ja");
        assert_eq!(request.options.form, None);
    }

    #[test]
    fn consented_document_routes_recommend_plamo_when_neither_model_is_installed() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let plamo = catalog::find_entry("plamo-3-translate-2b-Q4_K_M").unwrap();
        let hy = &catalog::HY_MT2_7B;
        let mut settings = external_settings(ModelProfile::PlamoPfml);
        settings.automatic = true;
        settings.request.source = SourceChoice::Detect;
        settings.request.target = TargetChoice::Lang("ja");

        for (format, pdf) in [
            (FileFormat::Word, None),
            (FileFormat::PowerPoint, None),
            (FileFormat::Pdf, Some(PdfOutput::Bilingual)),
        ] {
            let input = FileInput {
                hint: FileSource {
                    detected: Some("en"),
                    japanese: false,
                },
                format,
                pdf,
            };
            for (installed, consent, expected_model, expected_suggestion) in [
                (&[][..], true, None, Some(plamo)),
                (&[hy.id()][..], true, Some(hy), None),
                (&[plamo.id(), hy.id()][..], true, Some(plamo), None),
                (&[][..], false, None, Some(hy)),
            ] {
                let choose = |models, source, target| {
                    let entries = match models {
                        FileModels::Plamo => [plamo],
                        FileModels::External => [hy],
                        FileModels::All => unreachable!("documents select by model family"),
                    };
                    select(
                        Request {
                            source,
                            target: Some(target),
                            format: Some(format),
                            system_memory: u64::MAX,
                            quality: QualityTier::Standard,
                            plamo_consent: consent,
                            bidirectional: false,
                            selected: None,
                        },
                        &entries,
                        installed,
                    )
                };
                for request in [&settings.request, &settings.request_for(format)] {
                    let availability = FileModelAvailability {
                        fits_7b: true,
                        installed_7b: installed.contains(&hy.id()),
                        installed_small: false,
                        index_blocked_by_context: false,
                    };
                    match input.automatic_route(request, availability, choose) {
                        Ok(route) => {
                            assert_eq!(route.entry, expected_model);
                            assert!(expected_suggestion.is_none());
                        }
                        Err(requirement) => {
                            assert!(expected_model.is_none());
                            assert_eq!(requirement.suggestion, expected_suggestion);
                            assert!(matches!(
                                requirement.notice,
                                FileNotice::Message { key, .. }
                                    if key == "files.file_model_required"
                            ));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn index_markdown_has_no_optional_seven_b_download() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let settings = external_settings(ModelProfile::IndexTranslate);
        let input = FileInput {
            hint: FileSource {
                detected: Some("en"),
                japanese: false,
            },
            format: FileFormat::Markdown,
            pdf: None,
        };
        let index = &catalog::INDEX_TRANSLATE;
        let large = &catalog::HY_MT2_7B;
        let entries = [large, &catalog::HY_MT2, index];
        for gib in [8, 16] {
            let memory = gib << 30;
            let choose = |_, source, target| {
                select(
                    Request {
                        source,
                        target: Some(target),
                        format: Some(input.format),
                        system_memory: memory,
                        quality: QualityTier::Standard,
                        plamo_consent: false,
                        bidirectional: false,
                        selected: None,
                    },
                    &entries,
                    &[index.id()],
                )
            };
            for request in [&settings.request, &settings.request_for(input.format)] {
                let route = input
                    .automatic_route(
                        request,
                        FileModelAvailability {
                            fits_7b: selection::fits_memory_budget(large.bytes, memory),
                            installed_7b: false,
                            installed_small: false,
                            index_blocked_by_context: true,
                        },
                        choose,
                    )
                    .ok()
                    .expect("installed Index can translate Markdown");
                assert_eq!(route.entry, Some(index));
                assert_eq!(route.source.code(), Some("en"));
                assert!(route.suggestion.is_none());
            }
        }
    }

    #[test]
    fn index_routes_word_and_keeps_pdf_gated() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let settings = external_settings(ModelProfile::IndexTranslate);
        let index = &catalog::INDEX_TRANSLATE;
        let large = &catalog::HY_MT2_7B;
        let entries = [large, &catalog::HY_MT2, index];
        for (format, output) in [
            (FileFormat::Word, None),
            (FileFormat::Pdf, Some(PdfOutput::Bilingual)),
            (FileFormat::Pdf, Some(PdfOutput::Layout)),
            (FileFormat::Pdf, Some(PdfOutput::Papers)),
        ] {
            let input = FileInput {
                hint: FileSource {
                    detected: Some("en"),
                    japanese: false,
                },
                format,
                pdf: output,
            };
            let choose = |models, source, target| {
                let applicable: &[_] = match models {
                    FileModels::External => &entries,
                    FileModels::Plamo => &[],
                    FileModels::All => unreachable!("documents select by model family"),
                };
                select(
                    Request {
                        source,
                        target: Some(target),
                        format: Some(format),
                        system_memory: 16 << 30,
                        quality: QualityTier::Standard,
                        plamo_consent: false,
                        bidirectional: false,
                        selected: None,
                    },
                    applicable,
                    &[index.id()],
                )
            };
            for request in [&settings.request, &settings.request_for(format)] {
                let result = input.automatic_route(
                    request,
                    FileModelAvailability {
                        fits_7b: true,
                        installed_7b: false,
                        installed_small: format == FileFormat::Word,
                        index_blocked_by_context: format == FileFormat::Word,
                    },
                    choose,
                );
                if output == Some(PdfOutput::Papers) {
                    assert!(matches!(
                        result.err().unwrap().notice,
                        FileNotice::Message {
                            key: "files.babeldoc_plamo",
                            ..
                        }
                    ));
                } else if format == FileFormat::Pdf {
                    assert!(matches!(
                        result.err().unwrap().notice,
                        FileNotice::Message {
                            key: "files.file_model_required",
                            ..
                        }
                    ));
                } else {
                    let route = result.ok().expect("Index supports Word");
                    assert_eq!(route.entry, Some(index));
                    assert_eq!(route.source.code(), Some("en"));
                    assert_eq!(route.suggestion, Some(large));
                }
            }
        }
    }

    #[test]
    fn document_offers_distinguish_installed_7b_from_usable_models() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let small = &catalog::HY_MT2;
        let large = &catalog::HY_MT2_7B;
        let mut settings = external_settings(ModelProfile::HyMt2);
        settings.request.target = TargetChoice::Lang("ja");
        for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
            let input = FileInput {
                hint: FileSource {
                    detected: Some("en"),
                    japanese: false,
                },
                format,
                pdf: (format == FileFormat::Pdf).then_some(PdfOutput::Bilingual),
            };
            for (gib, installed_7b, usable, expected_entry, expected_suggestion, expected_notice) in [
                (16, false, &[small.id()][..], Some(small), Some(large), None),
                (8, false, &[small.id()][..], Some(small), None, None),
                (16, true, &[small.id()][..], Some(small), None, None),
                (
                    16,
                    true,
                    &[large.id(), small.id()][..],
                    Some(large),
                    None,
                    None,
                ),
                (
                    16,
                    true,
                    &[][..],
                    None,
                    Some(large),
                    Some("files.hy_unavailable"),
                ),
                (
                    8,
                    true,
                    &[][..],
                    None,
                    Some(small),
                    Some("files.file_model_required"),
                ),
            ] {
                let memory = gib << 30;
                for request in [&settings.request, &settings.request_for(format)] {
                    let availability = FileModelAvailability {
                        fits_7b: selection::fits_memory_budget(large.bytes, memory),
                        installed_7b,
                        installed_small: usable.contains(&small.id()),
                        index_blocked_by_context: false,
                    };
                    let route =
                        input.automatic_route(request, availability, |models, source, target| {
                            let entries: &[_] = match models {
                                FileModels::Plamo => &[],
                                FileModels::External => &[large, small],
                                FileModels::All => unreachable!("documents select by model family"),
                            };
                            select(
                                Request {
                                    source,
                                    target: Some(target),
                                    format: Some(format),
                                    system_memory: memory,
                                    quality: QualityTier::Standard,
                                    plamo_consent: false,
                                    bidirectional: false,
                                    selected: None,
                                },
                                entries,
                                usable,
                            )
                        });
                    match route {
                        Ok(route) => {
                            assert_eq!(route.entry, expected_entry);
                            assert_eq!(route.suggestion, expected_suggestion);
                            assert!(expected_notice.is_none());
                        }
                        Err(requirement) => {
                            assert!(expected_entry.is_none());
                            assert_eq!(requirement.suggestion, expected_suggestion);
                            assert!(matches!(
                                requirement.notice,
                                FileNotice::Message { key, .. } if Some(key) == expected_notice
                            ));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn blocked_index_markdown_explains_the_required_change_in_single_and_batch() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let input = FileInput {
            hint: FileSource {
                detected: Some("en"),
                japanese: false,
            },
            format: FileFormat::Markdown,
            pdf: None,
        };
        let index = &catalog::INDEX_TRANSLATE;
        let hy = &catalog::HY_MT2;
        let mut settings = external_settings(ModelProfile::IndexTranslate);
        settings.automatic = true;
        for use_glossary in [false, true] {
            settings.request.context.clear();
            if use_glossary {
                settings.request.glossary.push(engine::pfml::GlossaryEntry {
                    source: "library".into(),
                    target: "図書館".into(),
                });
            } else {
                settings.request.context = "A library catalogue".into();
            }
            for request in [&settings.request, &settings.request_for(input.format)] {
                for consent in [false, true] {
                    for (installed, expected_notice) in [
                        (&[index.id()][..], Some("files.index_markdown_context_auto")),
                        (&[][..], Some("files.file_model_required")),
                        (&[index.id(), hy.id()][..], None),
                    ] {
                        let result = input.automatic_route(
                            request,
                            FileModelAvailability {
                                fits_7b: true,
                                installed_7b: false,
                                installed_small: installed.contains(&hy.id()),
                                index_blocked_by_context: installed.contains(&index.id()),
                            },
                            |models, source, target| {
                                select(
                                    Request {
                                        source,
                                        target: Some(target),
                                        format: Some(input.format),
                                        system_memory: 16 << 30,
                                        quality: QualityTier::Standard,
                                        plamo_consent: consent,
                                        bidirectional: false,
                                        selected: None,
                                    },
                                    &models.entries(false),
                                    installed,
                                )
                            },
                        );
                        match result {
                            Ok(route) => {
                                assert_eq!(route.entry, Some(hy));
                                assert_eq!(expected_notice, None);
                            }
                            Err(requirement) => {
                                assert!(matches!(
                                    requirement.notice,
                                    FileNotice::Message { key, .. } if Some(key) == expected_notice
                                ));
                                assert!(requirement.suggestion.is_some());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn batch_pre_routing_matches_the_single_file_for_document_languages() {
        use super::*;
        use engine::selection::{QualityTier, Request, select};

        let plamo = catalog::find_entry("plamo-3-translate-2b-Q4_K_M").unwrap();
        let mut settings = external_settings(ModelProfile::HyMt2);
        settings.automatic = true;
        // PLaMo continues to use the user's source choice. Hy-MT2 uses the detected source.
        settings.request.source = SourceChoice::Lang("de");
        settings.request.target = TargetChoice::Lang("en");
        let installed = [plamo.id(), catalog::HY_MT2_7B.id(), catalog::HY_MT2.id()];
        for (format, pdf, detected, consent, expected_model, expected_source, expected_error) in [
            (
                FileFormat::Word,
                None,
                Some("pt"),
                false,
                Some(installed[1]),
                Some("pt"),
                None,
            ),
            (
                FileFormat::PowerPoint,
                None,
                Some("pt"),
                true,
                Some(installed[0]),
                Some("de"),
                None,
            ),
            (
                FileFormat::Pdf,
                Some(PdfOutput::Bilingual),
                Some("ko"),
                false,
                Some(installed[1]),
                Some("ko"),
                None,
            ),
            (
                FileFormat::Pdf,
                Some(PdfOutput::Layout),
                None,
                false,
                Some(installed[2]),
                Some("de"),
                None,
            ),
            (
                FileFormat::PowerPoint,
                None,
                Some("zh-hant"),
                false,
                None,
                None,
                Some("files.hy_direction"),
            ),
            (
                FileFormat::Pdf,
                Some(PdfOutput::Papers),
                Some("pt"),
                false,
                None,
                None,
                Some("files.babeldoc_plamo"),
            ),
        ] {
            settings.pdf_output = pdf.unwrap_or(PdfOutput::Bilingual);
            settings.pdf_source = settings.request.source;
            let input = FileInput {
                hint: FileSource {
                    detected,
                    japanese: false,
                },
                format,
                pdf,
            };
            let memory = if pdf == Some(PdfOutput::Layout) {
                8 << 30
            } else {
                u64::MAX
            };
            let fits_7b = selection::fits_memory_budget(catalog::HY_MT2_7B.bytes, memory);
            let choose = |models, source, target| {
                let all = [plamo, &catalog::HY_MT2_7B, &catalog::HY_MT2];
                let entries = match models {
                    FileModels::All => &all[..],
                    FileModels::Plamo => &all[..1],
                    FileModels::External => &all[1..],
                };
                select(
                    Request {
                        source,
                        target: Some(target),
                        format: Some(format),
                        system_memory: memory,
                        quality: QualityTier::Standard,
                        plamo_consent: consent,
                        bidirectional: false,
                        selected: None,
                    },
                    entries,
                    &installed,
                )
            };
            let route_snapshot = |result: Result<FileRoute, FileRequirement>| match result {
                Ok(route) => (route.entry.map(CatalogEntry::id), route.source.code(), None),
                Err(error) => match error.notice {
                    FileNotice::Message { key, .. } => (None, None, Some(key)),
                    _ => panic!("expected a preflight message"),
                },
            };
            let availability = || FileModelAvailability {
                fits_7b,
                installed_7b: true,
                installed_small: format == FileFormat::Pdf,
                index_blocked_by_context: false,
            };
            let single = input.automatic_route(&settings.request, availability(), choose);
            let batch =
                input.automatic_route(&settings.request_for(format), availability(), choose);
            assert_eq!(
                route_snapshot(single),
                (expected_model, expected_source, expected_error)
            );
            assert_eq!(
                route_snapshot(batch),
                (expected_model, expected_source, expected_error)
            );
        }
    }

    #[test]
    fn a_papers_batch_keeps_its_explicit_source_only_for_paper_pdf_rows() {
        use super::{FileFormat, PdfOutput, SourceChoice, TargetChoice};
        let mut settings = external_settings(engine::llama::ModelProfile::PlamoPfml);
        settings.automatic = true;
        settings.pdf_output = PdfOutput::Papers;
        settings.pdf_source = SourceChoice::Lang("fr");
        settings.request.source = SourceChoice::Detect;
        assert_eq!(
            settings.source_for(FileFormat::Pdf),
            SourceChoice::Lang("fr")
        );
        for format in [
            FileFormat::Txt,
            FileFormat::Markdown,
            FileFormat::Word,
            FileFormat::PowerPoint,
        ] {
            assert_eq!(settings.source_for(format), SourceChoice::Detect);
        }
        settings.request.target = TargetChoice::Lang("ar");
        assert_eq!(settings.source_for(FileFormat::Pdf), SourceChoice::Detect);
    }

    #[test]
    fn each_document_resolves_its_own_language_from_the_original_settings() {
        use super::{BatchPanel, BatchSettings, FileDocument, PdfOutput, RequestSettings};
        use crate::lang::{SourceChoice, StyleChoice, TargetChoice};
        use engine::pfml::GlossaryEntry;
        let settings = |layout, style, context: &str| BatchSettings {
            automatic: false,
            pdf_output: if layout {
                PdfOutput::Layout
            } else {
                PdfOutput::Bilingual
            },
            pdf_source: SourceChoice::Detect,
            formats: engine::llama::ModelProfile::PlamoPfml.capabilities().files,
            request: RequestSettings {
                profile: engine::llama::ModelProfile::PlamoPfml,
                source: SourceChoice::Detect,
                target: TargetChoice::JapaneseEnglish,
                style,
                document: Default::default(),
                context: context.into(),
                glossary: vec![GlossaryEntry {
                    source: "example".into(),
                    target: context.into(),
                }],
            },
        };
        let mut panel = BatchPanel::default();
        panel
            .work
            .start(settings(true, StyleChoice::Polite, "original"));
        panel
            .work
            .start(settings(false, StyleChoice::Plain, "next"));
        let fixed = panel.work.settings().unwrap();
        assert_eq!(fixed.pdf_output(), PdfOutput::Layout);
        for (source, target) in [("This is English.", "ja"), ("これは日本語です。", "en")]
        {
            let doc = ::files::Document::open(source.as_bytes(), "sample.txt").unwrap();
            let doc = FileDocument::Text(std::sync::Arc::new(doc));
            let request = doc.request(&fixed.request, doc.source_hint());
            assert_eq!(request.options.target.as_str(), target);
            assert!(request.options.source.is_none());
            assert_eq!(request.options.context, "original");
            assert_eq!(request.options.glossary[0].target, "original");
            if target == "ja" {
                assert_eq!(request.options.form, StyleChoice::Polite.form());
            }
        }
    }
}
