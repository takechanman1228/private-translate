//! The third PDF output, "for papers (BabelDOC)": the external `babeldoc-engine` lays out the
//! translated PDF and asks the app's own model for each paragraph through the `babeldoc` crate.
//!
//! The crate calls a translate function on a thread of its own and waits for each answer; the
//! model (`Worker`) answers with events on the UI thread. A run therefore has three parts:
//!
//! - a background thread that runs [`babeldoc::translate_pdf`] and sends [`Message`]s: one
//!   `Translate` per paragraph (with a channel for the answer), progress, and `Done` at the end
//!   (with the PDF already read through [`babeldoc::Report::open_output`], never a path);
//! - the view, which receives those messages on the UI thread, submits each paragraph to the
//!   worker as one unit (the built-in PDF's unit settings) and routes the worker's events back;
//! - [`Router`], the UI-independent part that pairs requests, job ids and answers, so that a
//!   stale job's events, a cancel, a failed unit and a failed runtime each get the right answer.
//!
//! While a run lasts, the file job holds the worker ([`crate::jobs::Jobs::start_external`]):
//! each paragraph has a new job id, and the run stays the file job between them.
//!
//! Nothing here logs or formats document text. The log gets counts, stage names, fixed reason
//! codes and times only. The translate function never panics with text in its message (it
//! formats nothing), and catches any panic and answers it as a fatal failure.
//!
//! macOS only: elsewhere the settings row and the choice are not shown.

use std::sync::mpsc;
use std::time::Duration;

use babeldoc::{Availability, Cancel, Job, Outcome, Profile, StopSignal, TranslateError};

use super::*;

/// Whether this platform offers the output at all.
pub(in crate::view) const SUPPORTED: bool = cfg!(target_os = "macos");
/// How often the translate function checks the stop signals while it waits for an answer.
const ANSWER_POLL: Duration = Duration::from_millis(50);
/// After a cancel, how long the engine gets to clean up before the bridge kills its group.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long a close or quit waits for a stopped run to clean up (the bridge kills the engine
/// after [`STOP_GRACE`] and gives its pipes 5 seconds more), before it closes anyway.
pub(in crate::view) const CLOSE_WAIT: Duration = Duration::from_secs(20);
/// The app's own folder for the runs' private folders, in its data folder.
const WORK_FOLDER: &str = "babeldoc-work";

/// The PDF output in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::view) enum PdfOutput {
    Bilingual,
    Layout,
    /// For papers (BabelDOC).
    Papers,
}

impl PdfOutput {
    /// The output the saved choices give: the third one only while the engine checks as usable
    /// (and on a supported platform); otherwise the two old choices, from `pdf_layout`.
    pub(in crate::view) fn effective(
        pdf_layout: bool,
        pdf_babeldoc: bool,
        engine_ready: bool,
    ) -> Self {
        if pdf_babeldoc && engine_ready && SUPPORTED {
            Self::Papers
        } else if pdf_layout {
            Self::Layout
        } else {
            Self::Bilingual
        }
    }

    /// The choices offered, in order, with their label keys.
    pub(in crate::view) fn choices(engine_ready: bool) -> Vec<(Self, &'static str)> {
        let mut choices = vec![
            (Self::Bilingual, "files.pdf_bilingual"),
            (Self::Layout, "files.pdf_layout"),
        ];
        if engine_ready && SUPPORTED {
            choices.push((Self::Papers, "files.pdf_babeldoc"));
        }
        choices
    }

    /// Stores this choice the way the settings keep it.
    pub(in crate::view) fn store(self, settings: &mut Settings) {
        settings.pdf_babeldoc = self == Self::Papers;
        if self != Self::Papers {
            settings.pdf_layout = self == Self::Layout;
        }
    }

    pub(in crate::view) fn label(self) -> &'static str {
        match self {
            Self::Bilingual => "files.pdf_bilingual",
            Self::Layout => "files.pdf_layout",
            Self::Papers => "files.pdf_babeldoc",
        }
    }
}

/// What the check of the configured engine found.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(in crate::view) enum EngineStatus {
    /// No engine is configured.
    #[default]
    Unset,
    Checking,
    Checked(Availability),
}

/// The engine check, run once at start and whenever the path changes (it starts a process, so
/// never while rendering). Results of an older check are ignored.
#[derive(Default)]
pub(in crate::view) struct EngineState {
    generation: u64,
    status: EngineStatus,
    /// The last run's id (see [`Run`]).
    runs: u64,
}

impl EngineState {
    fn next_run(&mut self) -> u64 {
        self.runs += 1;
        self.runs
    }

    pub(in crate::view) fn status(&self) -> &EngineStatus {
        &self.status
    }

    pub(in crate::view) fn ready(&self) -> bool {
        SUPPORTED && self.status == EngineStatus::Checked(Availability::Ready)
    }

    /// Starts a check of `path` (none: the engine was removed). Returns the generation to
    /// finish it with, when there is something to check.
    fn begin(&mut self, path: Option<&Path>) -> Option<u64> {
        self.generation += 1;
        if path.is_some() && SUPPORTED {
            self.status = EngineStatus::Checking;
            Some(self.generation)
        } else {
            self.status = EngineStatus::Unset;
            None
        }
    }

    /// Records a result; false when a newer check (or a removal) replaced this one.
    fn finish(&mut self, generation: u64, availability: Availability) -> bool {
        if generation != self.generation {
            return false;
        }
        self.status = EngineStatus::Checked(availability);
        true
    }
}

/// A short fixed code for the log.
fn availability_code(availability: &Availability) -> &'static str {
    match availability {
        Availability::Ready => "ready",
        Availability::NotFound => "not_found",
        Availability::NotExecutable => "not_executable",
        Availability::WrongVersion { .. } => "wrong_version",
        Availability::AssetsMissing => "assets_missing",
        Availability::Unusable { reason } => reason,
        Availability::Unsupported => "unsupported",
    }
}

/// The settings row's status text.
fn status_text(status: &EngineStatus) -> String {
    match status {
        EngineStatus::Unset => t!("settings.babeldoc_unset"),
        EngineStatus::Checking => t!("settings.babeldoc_checking"),
        EngineStatus::Checked(Availability::Ready) => t!("settings.babeldoc_ready"),
        EngineStatus::Checked(Availability::NotFound) => t!("settings.babeldoc_not_found"),
        EngineStatus::Checked(Availability::NotExecutable) => {
            t!("settings.babeldoc_not_executable")
        }
        EngineStatus::Checked(Availability::WrongVersion { .. }) => {
            t!("settings.babeldoc_wrong_version")
        }
        EngineStatus::Checked(Availability::AssetsMissing) => {
            t!("settings.babeldoc_assets_missing")
        }
        EngineStatus::Checked(Availability::Unusable { .. } | Availability::Unsupported) => {
            t!("settings.babeldoc_unusable")
        }
    }
    .into_owned()
}

// --- Routing requests and answers (UI-independent) ----------------------------------------

/// The answer to one paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::view) enum Answer {
    Text(String),
    /// This paragraph only: the engine keeps its source text and goes on.
    Unit,
    /// The run cannot go on (cancelled, or the model failed).
    Fatal,
}

impl From<Answer> for Result<String, TranslateError> {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Text(text) => Ok(text),
            Answer::Unit => Err(TranslateError::Unit),
            Answer::Fatal => Err(TranslateError::Fatal),
        }
    }
}

pub(in crate::view) type Reply = mpsc::SyncSender<Answer>;

/// What the background thread sends to the UI thread.
pub(in crate::view) enum Message {
    /// Translate this paragraph and answer on `reply`.
    Translate {
        text: String,
        reply: Reply,
    },
    /// BabelDOC's overall progress, 0 to 100.
    Progress(f32),
    Done(Box<Finished>),
}

impl Finished {
    /// The end of a run whose thread went away without sending one.
    fn lost() -> Self {
        Self {
            outcome: None,
            pdf: None,
            elapsed: Duration::ZERO,
        }
    }
}

/// Turns what the UI receives from a run's thread into messages, ending with exactly one
/// `Done`: a channel that closes before `Done` (the thread panicked outside the translate
/// function) ends the run as a fixed failure through the same completion path.
#[derive(Default)]
pub(in crate::view) struct Receiver {
    done: bool,
}

impl Receiver {
    pub(in crate::view) fn next(
        &mut self,
        received: Result<Message, async_channel::RecvError>,
    ) -> Option<Message> {
        if self.done {
            return None;
        }
        match received {
            Ok(message) => {
                self.done = matches!(message, Message::Done(_));
                Some(message)
            }
            Err(async_channel::RecvError) => {
                self.done = true;
                Some(Message::Done(Box::new(Finished::lost())))
            }
        }
    }
}

/// How a run ended, as the UI needs it.
pub(in crate::view) struct Finished {
    /// `None`: the run's thread ended without a result (it panicked).
    pub outcome: Option<Outcome>,
    /// The translated PDF, read on the background thread (`Ok` and `Partial` only).
    pub pdf: Option<Result<Vec<u8>, std::io::ErrorKind>>,
    pub elapsed: Duration,
}

/// Where a worker event goes while a run owns the worker.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::view) enum Routed {
    /// Not the paragraph in flight: the view handles it as before (it ignores other jobs).
    Ignored,
    /// The paragraph in flight's event, handled here.
    Consumed,
    /// The model's runtime failed: the paragraph in flight was answered as fatal and the run
    /// stops; the view must still handle the event.
    RuntimeFailed,
}

struct InFlight {
    reply: Reply,
    job: Option<JobId>,
    result: Option<UnitResult>,
}

/// Pairs paragraph requests with worker jobs and answers them. See the module documentation.
#[derive(Default)]
pub(in crate::view) struct Router {
    in_flight: Option<InFlight>,
    stopping: bool,
}

impl Router {
    /// A new paragraph. True when it should be submitted to the worker (then call
    /// [`Router::submitted`]); false when it was answered at once because the run is stopping.
    pub(in crate::view) fn request(&mut self, reply: Reply) -> bool {
        // The engine asks one paragraph at a time; an older one still open is answered first.
        self.answer(Answer::Fatal);
        if self.stopping {
            let _ = reply.try_send(Answer::Fatal);
            return false;
        }
        self.in_flight = Some(InFlight {
            reply,
            job: None,
            result: None,
        });
        true
    }

    pub(in crate::view) fn submitted(&mut self, job: JobId) {
        if let Some(in_flight) = &mut self.in_flight {
            in_flight.job = Some(job);
        }
    }

    /// Stops the run: the paragraph in flight is answered as fatal now, and so is every later
    /// request.
    pub(in crate::view) fn cancel(&mut self) {
        self.stopping = true;
        self.answer(Answer::Fatal);
    }

    pub(in crate::view) fn stopping(&self) -> bool {
        self.stopping
    }

    fn answer(&mut self, answer: Answer) {
        if let Some(in_flight) = self.in_flight.take() {
            // The thread may already have given up waiting (it was stopped): nothing to do.
            let _ = in_flight.reply.try_send(answer);
        }
    }

    pub(in crate::view) fn event(&mut self, event: &Event) -> Routed {
        if let Event::RuntimeFailed(_) = event {
            self.cancel();
            return Routed::RuntimeFailed;
        }
        let Some(in_flight) = &mut self.in_flight else {
            return Routed::Ignored;
        };
        let Some(active) = in_flight.job else {
            return Routed::Ignored;
        };
        match event {
            Event::Unit { job, index, result } if *job == active => {
                if *index == 0 {
                    in_flight.result = Some(result.clone());
                }
                Routed::Consumed
            }
            Event::Finished { job, stats } if *job == active => {
                let answer = if stats.stop_reason == StopReason::Cancelled {
                    Answer::Fatal
                } else {
                    match in_flight.result.take() {
                        Some(UnitResult::Complete { text, .. }) => Answer::Text(text),
                        // Cut, too long, invalid, or no result: this paragraph only.
                        _ => Answer::Unit,
                    }
                };
                self.answer(answer);
                Routed::Consumed
            }
            Event::Failed { job, .. } if *job == active => {
                self.answer(Answer::Fatal);
                Routed::Consumed
            }
            Event::Chunk { job, .. }
            | Event::Token { job, .. }
            | Event::Segment { job, .. }
            | Event::Rewind { job, .. }
            | Event::Retry { job, .. }
            | Event::ParagraphFallback { job, .. }
            | Event::Cut { job, .. }
                if *job == active =>
            {
                Routed::Consumed
            }
            _ => Routed::Ignored,
        }
    }
}

// --- The background side ----------------------------------------------------------------

/// The translate function the bridge calls, on its endpoint thread: hands the paragraph to the
/// UI thread and waits for the answer, checking both stop signals on every turn of the wait.
fn translate_one(
    messages: &async_channel::Sender<Message>,
    cancel: &Cancel,
    text: &str,
    signal: StopSignal<'_>,
) -> Result<String, TranslateError> {
    let stopped = || cancel.is_cancelled() || signal.is_stopped();
    if stopped() {
        return Err(TranslateError::Fatal);
    }
    let (reply, answer) = mpsc::sync_channel(1);
    let request = Message::Translate {
        text: text.to_owned(),
        reply,
    };
    if messages.send_blocking(request).is_err() {
        return Err(TranslateError::Fatal);
    }
    loop {
        if stopped() {
            return Err(TranslateError::Fatal);
        }
        match answer.recv_timeout(ANSWER_POLL) {
            Ok(answer) => return answer.into(),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(TranslateError::Fatal),
        }
    }
}

/// The app's folder for the runs' private folders: made by the app, mode `0700`, in its data
/// folder (the preview profile's with `PRIVATE_TRANSLATE_LOCAL_PREVIEW`), never the shared temporary
/// folder. The bridge refuses it unless it is private (see `babeldoc::run`).
fn work_parent() -> std::io::Result<PathBuf> {
    make_work_parent(appdata::settings::data_file(WORK_FOLDER))
}

/// Makes (or keeps) `dir` as the app's private folder for the runs: see [`work_parent`].
fn make_work_parent(dir: PathBuf) -> std::io::Result<PathBuf> {
    if !dir.is_absolute() {
        return Err(std::io::ErrorKind::NotFound.into());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        // A folder, not a link to one; kept at 0700 whatever it was left at.
        if !std::fs::symlink_metadata(&dir)?.is_dir() {
            return Err(std::io::ErrorKind::InvalidInput.into());
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Runs `job` to its end on the calling (background) thread and sends the result.
fn run(mut job: Job, messages: async_channel::Sender<Message>, cancel: Cancel) {
    // Folders a crashed process left in the work folder are not removed here: nothing tells them
    // from the live run of another instance that shares the data folder.
    match work_parent() {
        Ok(parent) => {
            job.work_parent = Some(parent);
        }
        Err(e) => {
            crate::app_log!("babeldoc: no work folder ({:?})", e.kind());
            let finished = Finished {
                outcome: Some(Outcome::WorkDirFailed(e.kind())),
                pdf: None,
                elapsed: Duration::ZERO,
            };
            let _ = messages.send_blocking(Message::Done(Box::new(finished)));
            return;
        }
    }
    let translate = |text: &str, signal: StopSignal<'_>| {
        // Nothing in `translate_one` formats text, so a panic there cannot carry any; caught
        // here all the same, and answered as a fixed failure.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            translate_one(&messages, &cancel, text, signal)
        }))
        .unwrap_or(Err(TranslateError::Fatal))
    };
    let report = babeldoc::translate_pdf(
        &job,
        translate,
        |progress| {
            let _ = messages.try_send(Message::Progress(progress.overall));
        },
        &cancel,
    );
    let pdf = match report.outcome {
        Outcome::Ok(_) | Outcome::Partial(_) => Some(report.open_output().and_then(|mut file| {
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes).map(|_| bytes)
        })),
        _ => None,
    }
    .map(|read| read.map_err(|e| e.kind()));
    let finished = Finished {
        outcome: Some(report.outcome.clone()),
        pdf,
        elapsed: report.elapsed,
    };
    // Counts only: what the endpoint answered, and the engine's own breakdown of its units.
    let units = match &report.outcome {
        Outcome::Ok(saved) | Outcome::Partial(saved) => {
            let u = saved.units;
            format!(
                "; engine units {} total, {} translated, {} failed (unparsed {}, placeholders {}, \
                 incomplete {}, empty {}, invalid {}, connection {}), {} without letters",
                u.total,
                u.translated,
                u.failed,
                u.unparsed_requests,
                u.placeholder_mismatch,
                u.incomplete_answers,
                u.empty_answers,
                u.invalid_answers,
                u.connection_errors,
                u.skipped_no_letters
            )
        }
        _ => String::new(),
    };
    crate::app_log!(
        "babeldoc: {} exit {:?}, endpoint {} translated, {} unit failures, {} fatal, {} rejected, \
         {} dropped, {:.1?}{}{}",
        outcome_code(finished.outcome.as_ref()),
        report.exit_code,
        report.endpoint.translated,
        report.endpoint.unit_failures,
        report.endpoint.fatal_failures,
        report.endpoint.rejected,
        report.endpoint.dropped,
        report.elapsed,
        units,
        if report.leftover.is_some() || report.engine_leftover_folder().is_some() {
            ", a work folder was left"
        } else {
            ""
        }
    );
    // Dropping the report removes the run's private folder.
    drop(report);
    let _ = messages.send_blocking(Message::Done(Box::new(finished)));
}

/// A fixed code per outcome, for the log.
fn outcome_code(outcome: Option<&Outcome>) -> &'static str {
    let Some(outcome) = outcome else {
        return "thread_lost";
    };
    match outcome {
        Outcome::Ok(_) => "ok",
        Outcome::Partial(_) => "partial",
        Outcome::Refused { code } => code,
        Outcome::TranslatorFailed { code } => code,
        Outcome::AssetsMissing => "assets_missing",
        Outcome::BabeldocFailed { code, .. } => code,
        Outcome::Interrupted => "interrupted",
        Outcome::Crashed { .. } => "crashed",
        Outcome::InvalidResult(_) => "invalid_result",
        Outcome::SpawnFailed(_) => "spawn_failed",
        Outcome::EndpointFailed(_) => "endpoint_failed",
        Outcome::WorkDirFailed(_) => "work_dir_failed",
        Outcome::WorkParentNotPrivate => "work_parent_not_private",
        Outcome::Unsupported => "unsupported",
    }
}

// --- From a finished run to the panel (UI-independent) ------------------------------------

/// The panel's result for a finished run: a translation to save, or a notice.
pub(in crate::view) fn conclude(
    finished: &Finished,
    cancelled: bool,
    runtime_failed: bool,
) -> Result<CompletedFile, FileNotice> {
    if cancelled {
        return Err(FileNotice::text("files.cancelled"));
    }
    if runtime_failed {
        return Err(FileNotice::failure(
            "files.translation_failed",
            "the model stopped".into(),
        ));
    }
    let Some(outcome) = &finished.outcome else {
        return Err(FileNotice::failure(
            "files.babeldoc_failed",
            "thread_lost".into(),
        ));
    };
    let saved = match outcome {
        Outcome::Ok(saved) | Outcome::Partial(saved) => saved,
        Outcome::Refused { code } => return Err(refusal_notice(code)),
        Outcome::TranslatorFailed { code } => {
            return Err(FileNotice::failure(
                "files.translation_failed",
                (*code).to_owned(),
            ));
        }
        Outcome::AssetsMissing => return Err(FileNotice::text("files.babeldoc_assets_missing")),
        Outcome::Interrupted => return Err(FileNotice::text("files.cancelled")),
        // The executable was moved or lost its exec bit since it was checked.
        Outcome::SpawnFailed(
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied,
        ) => return Err(FileNotice::text("files.babeldoc_engine_gone")),
        // Codes and kinds only: `Outcome`'s `Debug` carries no text from the engine.
        other => {
            return Err(FileNotice::failure(
                "files.babeldoc_failed",
                format!("{other:?}"),
            ));
        }
    };
    let bytes = match &finished.pdf {
        Some(Ok(bytes)) => bytes.clone(),
        Some(Err(kind)) => {
            return Err(FileNotice::failure(
                "files.babeldoc_failed",
                format!("reading the PDF: {kind:?}"),
            ));
        }
        None => {
            return Err(FileNotice::failure(
                "files.babeldoc_failed",
                "no PDF".into(),
            ));
        }
    };
    let untranslated = usize::try_from(saved.units.failed).unwrap_or(usize::MAX);
    Ok(CompletedFile {
        output: FileOutput::PdfBytes(Arc::new(bytes)),
        has_changes: saved.units.translated > 0,
        review: FileReview {
            partial_translation: untranslated > 0,
            untranslated,
            ..Default::default()
        },
    })
}

/// The notice for a PDF the engine refused, reusing the built-in PDF's texts where they mean
/// the same.
fn refusal_notice(code: &str) -> FileNotice {
    FileNotice::text(match code {
        "encrypted" => "files.pdf_password",
        "editing_restricted" => "files.pdf_edit_restricted",
        "protected" => "files.pdf_protected",
        "signed" | "interactive_form" => "files.pdf_interactive",
        "too_many_pages" | "too_large" => "files.pdf_limit",
        "scanned_pdf" => "files.babeldoc_scanned",
        "already_translated" => "files.babeldoc_already_translated",
        "not_pdf" | "unreadable" => "files.babeldoc_unreadable",
        _ => {
            return FileNotice::failure("files.babeldoc_failed", code.to_owned());
        }
    })
}

/// Whether a PDF read for `read_as` must be read again for `now` (`idle`: nothing is being
/// translated, shown as a result, or batched). Not a PDF (`None`): never.
pub(in crate::view) fn stale_read(read_as: Option<PdfOutput>, now: PdfOutput, idle: bool) -> bool {
    read_as.is_some_and(|read_as| read_as != now) && idle
}

// --- Languages --------------------------------------------------------------------------

/// The engine's code for an app language code (`ja(easy)` is Japanese to BabelDOC).
fn engine_code(code: &str) -> Option<String> {
    let code = code.strip_suffix("(easy)").unwrap_or(code);
    let mut parts = code.split('-');
    let first = parts.next()?;
    let valid = (2..=3).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_alphabetic())
        && parts
            .all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()));
    valid.then(|| code.to_owned())
}

/// The source language the engine is given (it needs one), which the model is then given too.
/// An explicit choice is used as is. With "detect", the app decides as for the built-in PDF:
/// Japanese when the document reads as Japanese. Anything else: none, and the user is asked to
/// choose the source (guessing among Latin-script languages sent Dutch as English).
pub(in crate::view) fn engine_source(chosen: Option<&str>, japanese: bool) -> Option<String> {
    match chosen {
        Some(code) => engine_code(code),
        None if japanese => Some("ja".into()),
        None => None,
    }
}

/// Gives the model the source the engine got, so both translate from the same language.
pub(in crate::view) fn with_source(
    mut options: engine::pfml::TranslationOptions,
    source: &str,
) -> Option<engine::pfml::TranslationOptions> {
    options.source = Some(engine::pfml::LangCode::new(source).ok()?);
    Some(options)
}

// --- Closing during a run (UI-independent) ---------------------------------------------

/// A close (or quit) asked during a run, waiting for the run's end (its engine reaped and its
/// private folder removed) or for [`CLOSE_WAIT`], whichever comes first: the first of the two
/// takes it, so the window closes once.
#[derive(Default, Debug)]
pub(in crate::view) struct PendingClose(Option<bool>);

impl PendingClose {
    /// Records a close (`quit` false) or quit; a quit wins over a close asked before or after.
    /// True for the first ask (which starts the wait).
    fn ask(&mut self, quit: bool) -> bool {
        let first = self.0.is_none();
        self.0 = Some(quit || self.0.unwrap_or(false));
        first
    }

    /// While a close waits, a new ask is added to it (true); otherwise nothing happens (false).
    fn join(&mut self, quit: bool) -> bool {
        if self.0.is_none() {
            return false;
        }
        self.ask(quit);
        true
    }

    /// The close to do now (`Some(quit)`), once.
    fn take(&mut self) -> Option<bool> {
        self.0.take()
    }
}

// --- The view -----------------------------------------------------------------------------

/// A run in progress, owned by the file panel.
pub(in crate::view) struct Run {
    /// Tells this run's messages from an older run's.
    id: u64,
    router: Router,
    cancel: Cancel,
    options: engine::pfml::TranslationOptions,
    runtime_failed: bool,
}

impl Run {
    pub(in crate::view) fn cancel(&mut self) {
        self.cancel.cancel();
        self.router.cancel();
    }
}

impl TranslatorView {
    /// The PDF output in effect for new reads.
    pub(in crate::view) fn pdf_output(&self) -> PdfOutput {
        PdfOutput::effective(
            self.settings.pdf_layout,
            self.settings.pdf_babeldoc,
            self.external_engine.ready(),
        )
    }

    /// Whether the batch tab offers the source menu for BabelDOC (PLaMo detects each file's
    /// language, but BabelDOC needs one): before a batch starts, with the third output in
    /// effect. Other files in the batch still detect their language.
    pub(in crate::view) fn batch_source_for_babeldoc(&self) -> bool {
        self.batch.work.settings().is_none()
            && (self.languages.automatic
                || self.languages.profile == engine::llama::ModelProfile::PlamoPfml)
            && self.pdf_output() == PdfOutput::Papers
            && ::files::pdf::supports_pdf(self.languages.target(&self.settings).resolve_for(false))
            && self.batch.work.rows().iter().any(|row| {
                matches!(row.state, batch::State::Waiting)
                    && path_format(&row.path) == Some(FileFormat::Pdf)
            })
    }

    /// Checks the configured engine on a background thread (see [`EngineState`]).
    pub(in crate::view) fn check_babeldoc_engine(&mut self, cx: &mut Context<Self>) {
        let path = self.settings.babeldoc_engine.clone();
        let Some(generation) = self.external_engine.begin(path.as_deref()) else {
            self.refresh_pdf_output(cx);
            cx.notify();
            return;
        };
        let path = path.expect("begin returns a generation only with a path");
        cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let availability = cx
                .background_spawn(async move { babeldoc::check(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                crate::app_log!(
                    "babeldoc engine check: {} in {:.1?}",
                    availability_code(&availability),
                    started.elapsed()
                );
                if this.external_engine.finish(generation, availability) {
                    this.refresh_pdf_output(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Lets the user pick the engine's executable, then checks it.
    fn choose_babeldoc_engine(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("settings.babeldoc_choose").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.set_babeldoc_engine(Some(path), cx));
            }
        })
        .detach();
    }

    pub(in crate::view) fn set_babeldoc_engine(
        &mut self,
        path: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.settings.babeldoc_engine = path;
        self.settings.save();
        self.check_babeldoc_engine(cx);
    }

    /// Reads the open PDF again when the output in effect changed under it (the engine was
    /// removed, or became usable), unless it is being translated or has a result, or a batch
    /// (which keeps the output it started with) is shown. A notice on the panel is kept.
    pub(in crate::view) fn refresh_pdf_output(&mut self, cx: &mut Context<Self>) {
        let Some((path, document)) = &self.files.loaded else {
            return;
        };
        let idle =
            !self.file_operation_busy() && self.files.completed.is_none() && !self.batch.visible();
        if stale_read(document.pdf_output(), self.pdf_output(), idle) {
            let path = path.clone();
            let kept = self.files.message.take();
            self.read_file_keeping(path, kept, cx);
        }
    }

    /// Check these before asking for a model or starting a model switch.
    pub(super) fn babeldoc_preflight(
        &self,
        pdf: Option<PdfOutput>,
        source: Option<&str>,
        target: &str,
        in_batch: bool,
    ) -> Result<(), FileNotice> {
        if pdf != Some(PdfOutput::Papers) || !::files::pdf::supports_pdf(target) {
            return Ok(());
        }
        if self.settings.babeldoc_engine.is_none() || !self.external_engine.ready() {
            return Err(FileNotice::text("files.babeldoc_unavailable"));
        }
        if source.is_none() {
            return Err(FileNotice::text(if in_batch {
                "files.babeldoc_choose_source_batch"
            } else {
                "files.babeldoc_choose_source"
            }));
        }
        Ok(())
    }

    /// Starts the run for the loaded `PdfExternal` document (from [`Self::submit_file`]).
    pub(in crate::view) fn start_external_pdf(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, FileDocument::PdfExternal(doc))) = self.files.loaded.clone() else {
            return;
        };
        let target = request.options.target.as_str().to_owned();
        // A target the PDF writers cannot set goes to the built-in text output, as before.
        if !::files::pdf::supports_pdf(&target) {
            self.files.loaded = Some((path, FileDocument::Pdf(doc)));
            self.submit_file(request, window, cx);
            return;
        }
        if self.languages.profile != engine::llama::ModelProfile::PlamoPfml {
            return self.external_not_started(FileNotice::text("files.babeldoc_plamo"), window, cx);
        }
        self.files.completed = None;
        self.files.progress = None;
        self.files.cancelling = false;
        self.files.message = None;
        self.files.details_open = false;
        self.files.saved = None;
        self.files.target = target.clone();
        let engine = self
            .settings
            .babeldoc_engine
            .clone()
            .filter(|_| self.external_engine.ready());
        let chosen = request.options.source.as_ref().map(|s| s.as_str());
        let source = engine_source(chosen, self.files.source_hint.japanese);
        let in_batch = self.batch.work.active().is_some();
        let (engine, source, target) = match (engine, source, engine_code(&target)) {
            (Some(engine), Some(source), Some(target)) => (engine, source, target),
            (None, _, _) => {
                return self.external_not_started(
                    FileNotice::text("files.babeldoc_unavailable"),
                    window,
                    cx,
                );
            }
            (_, None, _) => {
                return self.external_not_started(
                    FileNotice::text(if in_batch {
                        "files.babeldoc_choose_source_batch"
                    } else {
                        "files.babeldoc_choose_source"
                    }),
                    window,
                    cx,
                );
            }
            (_, _, None) => {
                return self.external_not_started(
                    FileNotice::failure("files.babeldoc_failed", "target language".into()),
                    window,
                    cx,
                );
            }
        };
        // Both the engine and model must use the same source.
        let Some(options) = with_source(request.options, &source) else {
            return self.external_not_started(
                FileNotice::failure("files.babeldoc_failed", "source language".into()),
                window,
                cx,
            );
        };
        // The paper profile, since the built-in outputs cover business documents. The run's
        // private folder goes in the app's own folder (`work_parent`, on the run's thread).
        let mut job = Job::new(engine, path, source, target, Profile::Paper);
        job.stop_grace = STOP_GRACE;
        let (sender, messages) = async_channel::unbounded();
        let cancel = Cancel::new();
        let thread = {
            let cancel = cancel.clone();
            std::thread::Builder::new()
                .name("babeldoc-run".into())
                .spawn(move || run(job, sender, cancel))
        };
        if let Err(error) = thread {
            return self.external_not_started(
                FileNotice::failure("files.babeldoc_failed", format!("{:?}", error.kind())),
                window,
                cx,
            );
        }
        // The options' shape only (codes, counts), never the context or glossary text.
        crate::app_log!(
            "babeldoc: started ({} units read by the app; source {}, target {}, form {:?}, \
             context {} bytes, glossary {} entries)",
            doc.units().len(),
            options.source.as_ref().map_or("none", |s| s.as_str()),
            options.target.as_str(),
            options.form,
            options.context.len(),
            options.glossary.len()
        );
        let id = self.external_engine.next_run();
        self.files.external = Some(Run {
            id,
            router: Router::default(),
            cancel,
            options,
            runtime_failed: false,
        });
        self.jobs.start_external();
        cx.spawn_in(window, async move |this, cx| {
            let mut receiver = Receiver::default();
            while let Some(message) = receiver.next(messages.recv().await) {
                if this
                    .update_in(cx, |this, window, cx| {
                        this.external_message(id, message, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// The run could not start: the notice, and the batch moves on.
    fn external_not_started(
        &mut self,
        notice: FileNotice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.files.message = Some(notice);
        if let Some(id) = self.batch.work.active() {
            self.finish_batch_translation(id, window, cx);
        }
        cx.notify();
    }

    fn external_message(
        &mut self,
        id: u64,
        message: Message,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.files.external.as_ref().is_some_and(|run| run.id == id);
        match message {
            Message::Translate { reply, .. } if !current => {
                let _ = reply.try_send(Answer::Fatal);
            }
            // Another run's progress or end (it was replaced): nothing of this run's.
            Message::Progress(_) | Message::Done(_) if !current => {}
            Message::Translate { text, reply } => {
                let Some(run) = &mut self.files.external else {
                    let _ = reply.try_send(Answer::Fatal);
                    return;
                };
                if run.router.request(reply) {
                    let job = self.worker.translate_units(
                        run.options.clone(),
                        plain_unit_request(vec![text], PdfOutput::Papers),
                    );
                    run.router.submitted(job);
                }
            }
            Message::Progress(overall) => {
                if self.files.external.is_some() {
                    let percent = (overall.clamp(0.0, 100.0).round() as usize).min(100);
                    self.files.progress = Some((percent, 100));
                    if let Some(id) = self.batch.work.active() {
                        self.batch.work.progress(id, self.files.progress);
                    }
                    cx.notify();
                }
            }
            Message::Done(finished) => self.external_done(*finished, window, cx),
        }
    }

    fn external_done(&mut self, finished: Finished, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.files.external.take() else {
            return;
        };
        let cancelled = self.files.cancelling || run.router.stopping() && !run.runtime_failed;
        match conclude(&finished, cancelled, run.runtime_failed) {
            Ok(completed) => {
                if !completed.has_changes {
                    self.files.message = Some(FileNotice::text("files.no_changes"));
                }
                self.files.completed = Some(completed);
            }
            Err(notice) => self.files.message = Some(notice),
        }
        crate::app_log!(
            "babeldoc: finished as {} in {:.1?}",
            outcome_code(finished.outcome.as_ref()),
            finished.elapsed
        );
        let resume = self.jobs.finish_external().unwrap_or(false);
        self.files.cancelling = false;
        // The engine could not be started: it was moved or lost its exec bit since the check.
        // Check again, so that the next file is offered the built-in outputs (a running batch
        // keeps its output; its next rows then say that the engine is not ready).
        if matches!(finished.outcome, Some(Outcome::SpawnFailed(_))) {
            self.check_babeldoc_engine(cx);
        }
        // A verified model update that waited for this run (`ReplaceStep::Wait`): installed before
        // a pending close too, which would otherwise drop it with the window.
        let close = self.pending_close.take();
        if close.is_some() {
            self.batch.work.cancel(true);
            self.jobs.finish_batch();
            self.deferred_model_use = None;
        }
        self.install_deferred_model_updates(window, cx);
        if let Some(quit) = close {
            // The run has cleaned up (its engine reaped, its private folder removed).
            crate::app_log!("babeldoc: the stopped run is cleaned up; closing");
            self.close_now(quit, window, cx);
            return;
        }
        if let Some(id) = self.batch.work.active() {
            self.finish_batch_translation(id, window, cx);
        }
        if resume && !run.runtime_failed && self.model_ready {
            self.translate_now(window, cx);
        }
        cx.notify();
    }

    /// Closing or quitting during a run: stops it and keeps the close pending until the run's
    /// end (its engine reaped and private folder removed), or [`CLOSE_WAIT`] at most. True when
    /// the close was deferred (there is a run).
    pub(in crate::view) fn close_after_run(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(run) = &mut self.files.external else {
            return false;
        };
        run.cancel();
        self.files.cancelling = true;
        self.worker.cancel();
        if self.pending_close.ask(quit) {
            crate::app_log!("babeldoc: closing after the run stops");
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(CLOSE_WAIT).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Some(quit) = this.pending_close.take() {
                        crate::app_log!("babeldoc: the stopped run did not end in time; closing");
                        this.close_now(quit, window, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
        true
    }

    /// Whether a close is waiting for a run (see [`Self::close_after_run`]); a quit asked
    /// meanwhile turns it into a quit.
    pub(in crate::view) fn close_pending(&mut self, quit: bool) -> bool {
        self.pending_close.join(quit)
    }

    /// Routes a worker event while a run owns the worker. `None`: no run.
    pub(in crate::view) fn external_event(&mut self, event: &Event) -> Option<bool> {
        let run = self.files.external.as_mut()?;
        Some(match run.router.event(event) {
            Routed::Ignored => false,
            Routed::Consumed => true,
            Routed::RuntimeFailed => {
                run.runtime_failed = true;
                run.cancel.cancel();
                // The view handles the failure as well (model state, status).
                false
            }
        })
    }
}

/// The settings row (macOS only).
pub(in crate::view) fn settings_row(
    path: Option<PathBuf>,
    status: EngineStatus,
    muted: Hsla,
    view: WeakEntity<TranslatorView>,
) -> Option<AnyElement> {
    if !SUPPORTED {
        return None;
    }
    let ready = status == EngineStatus::Checked(Availability::Ready);
    let has_path = path.is_some();
    let choose_view = view.clone();
    Some(
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .text_sm()
                    .child(
                        div()
                            .w(scaled(140.))
                            .flex_none()
                            .child(t!("settings.babeldoc").into_owned()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .when(ready, |row| row.child(Icon::new(IconName::Check)))
                                    .child(status_text(&status)),
                            )
                            .children(path.map(|path| {
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(path.display().to_string())
                            }))
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .child(
                                        Button::new("babeldoc-choose")
                                            .small()
                                            .outline()
                                            .label(t!("settings.babeldoc_choose").into_owned())
                                            .disabled(status == EngineStatus::Checking)
                                            .on_click(move |_, window, cx| {
                                                let _ = choose_view.update(cx, |this, cx| {
                                                    this.choose_babeldoc_engine(cx)
                                                });
                                                window.refresh();
                                            }),
                                    )
                                    .when(has_path, |row| {
                                        row.child(
                                            Button::new("babeldoc-remove")
                                                .small()
                                                .ghost()
                                                .label(t!("settings.babeldoc_remove").into_owned())
                                                .on_click(move |_, window, cx| {
                                                    let _ = view.update(cx, |this, cx| {
                                                        this.set_babeldoc_engine(None, cx)
                                                    });
                                                    window.refresh();
                                                }),
                                        )
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("settings.babeldoc_note").into_owned()),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // The glob above brings GPUI's `test` attribute; the standard one is meant.
    use core::prelude::v1::test;
    use engine::TranslateStats;

    fn finished_event(job: JobId, stop_reason: StopReason) -> Event {
        Event::Finished {
            job,
            stats: TranslateStats {
                prompt_tokens: 0,
                generated_tokens: 0,
                prompt_duration: Duration::ZERO,
                generation_duration: Duration::ZERO,
                stop_reason,
                retries: 0,
                cut: 0,
                prompt_leaks: 0,
            },
        }
    }

    fn complete(job: JobId, text: &str) -> Event {
        Event::Unit {
            job,
            index: 0,
            result: UnitResult::Complete {
                text: text.into(),
                retried: false,
            },
        }
    }

    fn ask(router: &mut Router, job: JobId) -> mpsc::Receiver<Answer> {
        let (reply, answer) = mpsc::sync_channel(1);
        assert!(router.request(reply));
        router.submitted(job);
        answer
    }

    #[test]
    fn each_paragraph_is_answered_from_its_own_job_only() {
        let mut router = Router::default();
        let first = ask(&mut router, 7);
        // An older job's events (a text job, or a paragraph already answered) are not taken.
        assert_eq!(router.event(&complete(6, "stale")), Routed::Ignored);
        assert_eq!(
            router.event(&finished_event(6, StopReason::Finished)),
            Routed::Ignored
        );
        assert!(first.try_recv().is_err());
        assert_eq!(
            router.event(&Event::Token {
                job: 7,
                text: "x".into()
            }),
            Routed::Consumed
        );
        assert_eq!(router.event(&complete(7, "訳")), Routed::Consumed);
        assert_eq!(
            router.event(&finished_event(7, StopReason::Finished)),
            Routed::Consumed
        );
        assert_eq!(first.try_recv().unwrap(), Answer::Text("訳".into()));
        // The answered job's late events are no longer this run's.
        assert_eq!(
            router.event(&finished_event(7, StopReason::Finished)),
            Routed::Ignored
        );
        let second = ask(&mut router, 8);
        assert_eq!(router.event(&complete(7, "late")), Routed::Ignored);
        router.event(&complete(8, "second"));
        router.event(&finished_event(8, StopReason::Finished));
        assert_eq!(second.try_recv().unwrap(), Answer::Text("second".into()));
    }

    #[test]
    fn a_failed_unit_fails_that_paragraph_only() {
        for result in [
            Some(UnitResult::Cut),
            Some(UnitResult::TooLong),
            Some(UnitResult::Invalid),
            None,
        ] {
            let mut router = Router::default();
            let answer = ask(&mut router, 3);
            if let Some(result) = result {
                router.event(&Event::Unit {
                    job: 3,
                    index: 0,
                    result,
                });
            }
            router.event(&finished_event(3, StopReason::Finished));
            assert_eq!(answer.try_recv().unwrap(), Answer::Unit);
            // The run goes on.
            let next = ask(&mut router, 4);
            router.event(&complete(4, "ok"));
            router.event(&finished_event(4, StopReason::Finished));
            assert_eq!(next.try_recv().unwrap(), Answer::Text("ok".into()));
        }
    }

    #[test]
    fn a_failed_job_or_runtime_stops_the_run() {
        let mut router = Router::default();
        let answer = ask(&mut router, 5);
        let failed = Event::Failed {
            job: 5,
            error: "no model is loaded".into(),
        };
        assert_eq!(router.event(&failed), Routed::Consumed);
        assert_eq!(answer.try_recv().unwrap(), Answer::Fatal);

        let mut router = Router::default();
        let answer = ask(&mut router, 9);
        assert_eq!(
            router.event(&Event::RuntimeFailed("gone".into())),
            Routed::RuntimeFailed
        );
        assert_eq!(answer.try_recv().unwrap(), Answer::Fatal);
        // Later paragraphs are refused without reaching the worker.
        let (reply, later) = mpsc::sync_channel(1);
        assert!(!router.request(reply));
        assert_eq!(later.try_recv().unwrap(), Answer::Fatal);
    }

    #[test]
    fn a_cancel_answers_at_once_and_refuses_what_follows() {
        let mut router = Router::default();
        let answer = ask(&mut router, 11);
        router.cancel();
        assert_eq!(answer.try_recv().unwrap(), Answer::Fatal);
        // The cancelled job's own end changes nothing.
        assert_eq!(
            router.event(&finished_event(11, StopReason::Cancelled)),
            Routed::Ignored
        );
        let (reply, later) = mpsc::sync_channel(1);
        assert!(!router.request(reply));
        assert_eq!(later.try_recv().unwrap(), Answer::Fatal);

        // A job the worker cancelled (the stop button) is fatal, not a failed unit.
        let mut router = Router::default();
        let answer = ask(&mut router, 12);
        router.event(&complete(12, "partial"));
        router.event(&finished_event(12, StopReason::Cancelled));
        assert_eq!(answer.try_recv().unwrap(), Answer::Fatal);
    }

    #[test]
    fn the_translate_function_returns_once_stopped_while_waiting() {
        let (sender, messages) = async_channel::unbounded();
        let cancel = Cancel::new();
        let flag = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope
                .spawn(|| translate_one(&sender, &cancel, "a paragraph", StopSignal::new(&flag)));
            // The UI side got the request, but never answers; a steady stream of other messages
            // does not keep the cancel from being seen.
            let Ok(Message::Translate { text, reply }) = messages.recv_blocking() else {
                panic!("expected a request");
            };
            assert_eq!(text, "a paragraph");
            std::thread::sleep(Duration::from_millis(120));
            cancel.cancel();
            let started = Instant::now();
            assert_eq!(waiting.join().unwrap(), Err(TranslateError::Fatal));
            assert!(started.elapsed() < Duration::from_secs(1));
            drop(reply);
        });
        // Already stopped: no request is sent at all.
        assert_eq!(
            translate_one(&sender, &cancel, "x", StopSignal::new(&flag)),
            Err(TranslateError::Fatal)
        );
        assert!(messages.try_recv().is_err());
        // An answer comes back as the bridge's result.
        let cancel = Cancel::new();
        std::thread::scope(|scope| {
            let waiting =
                scope.spawn(|| translate_one(&sender, &cancel, "p", StopSignal::new(&flag)));
            let Ok(Message::Translate { reply, .. }) = messages.recv_blocking() else {
                panic!("expected a request");
            };
            reply.send(Answer::Unit).unwrap();
            assert_eq!(waiting.join().unwrap(), Err(TranslateError::Unit));
        });
    }

    #[test]
    fn the_output_in_effect_needs_a_usable_engine() {
        let papers = if SUPPORTED {
            PdfOutput::Papers
        } else {
            PdfOutput::Bilingual
        };
        assert_eq!(
            PdfOutput::effective(false, false, true),
            PdfOutput::Bilingual
        );
        assert_eq!(PdfOutput::effective(true, false, true), PdfOutput::Layout);
        assert_eq!(PdfOutput::effective(false, true, true), papers);
        // No engine, or one that does not check as usable: back to the saved two-way choice.
        assert_eq!(
            PdfOutput::effective(false, true, false),
            PdfOutput::Bilingual
        );
        assert_eq!(PdfOutput::effective(true, true, false), PdfOutput::Layout);
        assert_eq!(PdfOutput::choices(false).len(), 2);
        assert_eq!(
            PdfOutput::choices(true).len(),
            if SUPPORTED { 3 } else { 2 }
        );
        // Choosing keeps the layout choice under the third one.
        let mut settings = Settings {
            pdf_layout: true,
            ..Settings::default()
        };
        PdfOutput::Papers.store(&mut settings);
        assert!(settings.pdf_babeldoc && settings.pdf_layout);
        assert_eq!(
            PdfOutput::effective(settings.pdf_layout, settings.pdf_babeldoc, false),
            PdfOutput::Layout
        );
        PdfOutput::Bilingual.store(&mut settings);
        assert!(!settings.pdf_babeldoc && !settings.pdf_layout);
    }

    #[test]
    fn the_engine_check_ignores_stale_results() {
        let mut state = EngineState::default();
        assert_eq!(state.status(), &EngineStatus::Unset);
        let first = state.begin(Some(Path::new("/a")));
        let second = state.begin(Some(Path::new("/b")));
        if !SUPPORTED {
            assert_eq!((first, second), (None, None));
            return;
        }
        assert!(!state.finish(first.unwrap(), Availability::Ready));
        assert_eq!(state.status(), &EngineStatus::Checking);
        assert!(!state.ready());
        assert!(state.finish(second.unwrap(), Availability::NotFound));
        assert!(!state.ready());
        let third = state.begin(Some(Path::new("/c"))).unwrap();
        assert!(state.finish(third, Availability::Ready));
        assert!(state.ready());
        // Removing the engine makes a running check stale too.
        let fourth = state.begin(Some(Path::new("/c"))).unwrap();
        assert_eq!(state.begin(None), None);
        assert!(!state.finish(fourth, Availability::Ready));
        assert_eq!(state.status(), &EngineStatus::Unset);
    }

    /// With "detect", only a Japanese document gets a source; anything else is
    /// asked for (a share of English words read Dutch as English), and the model then gets the
    /// same source as the engine.
    #[test]
    fn the_source_language_is_chosen_or_japanese_or_asked_for() {
        assert_eq!(engine_source(Some("fr"), false).as_deref(), Some("fr"));
        assert_eq!(engine_source(Some("nl"), true).as_deref(), Some("nl"));
        assert_eq!(
            engine_source(Some("ja(easy)"), false).as_deref(),
            Some("ja")
        );
        assert_eq!(engine_source(None, true).as_deref(), Some("ja"));
        // English, Dutch, anything not Japanese: the user chooses.
        assert_eq!(engine_source(None, false), None);
        assert_eq!(engine_code("zh-Hant").as_deref(), Some("zh-Hant"));
        assert_eq!(engine_code("--x"), None);

        let auto =
            engine::pfml::TranslationOptions::new(engine::pfml::LangCode::new("ja").unwrap());
        assert!(auto.source.is_none());
        let options = with_source(auto.clone(), "nl").unwrap();
        assert_eq!(options.source.as_ref().map(|s| s.as_str()), Some("nl"));
        assert_eq!(options.target, auto.target);
        assert!(with_source(auto, "not a code").is_none());
    }

    /// A run's thread that goes away without `Done` (a panic outside the
    /// translate function) still ends the run, once, as a fixed failure.
    #[test]
    fn a_run_thread_that_dies_still_ends_the_run() {
        let (sender, messages) = async_channel::unbounded::<Message>();
        let thread = std::thread::spawn(move || {
            sender.try_send(Message::Progress(12.0)).unwrap();
            panic!("the run failed");
        });
        assert!(thread.join().is_err());
        let mut receiver = Receiver::default();
        assert!(matches!(
            receiver.next(messages.recv_blocking()),
            Some(Message::Progress(_))
        ));
        let Some(Message::Done(finished)) = receiver.next(messages.recv_blocking()) else {
            panic!("expected the run to end");
        };
        assert!(finished.outcome.is_none());
        let Err(notice) = conclude(&finished, false, false) else {
            panic!("expected a failure");
        };
        assert_eq!(notice.details().as_deref(), Some("thread_lost"));
        assert_eq!(outcome_code(None), "thread_lost");
        // Nothing after the end.
        assert!(receiver.next(messages.recv_blocking()).is_none());

        // A normal end is passed on once, and the closed channel after it adds nothing.
        let (sender, messages) = async_channel::unbounded::<Message>();
        sender
            .try_send(Message::Done(Box::new(finished_ok())))
            .unwrap();
        drop(sender);
        let mut receiver = Receiver::default();
        assert!(matches!(
            receiver.next(messages.recv_blocking()),
            Some(Message::Done(_))
        ));
        assert!(receiver.next(messages.recv_blocking()).is_none());
    }

    /// A close or quit during a run waits; asking again joins it (a quit wins),
    /// and the close happens once, from whichever comes first, the run's end or the time limit.
    #[test]
    fn a_close_during_a_run_waits_and_happens_once() {
        let mut pending = PendingClose::default();
        // No close waiting: an ask does not join anything.
        assert!(!pending.join(true));
        assert_eq!(pending.take(), None);
        // The first ask starts the wait; a later one does not start another.
        assert!(pending.ask(false));
        assert!(!pending.ask(false));
        // Closing again while it waits is held back, and a quit turns it into a quit.
        assert!(pending.join(true));
        assert!(pending.join(false));
        assert_eq!(pending.take(), Some(true));
        // The run's end took it: the time limit finds nothing left to do.
        assert_eq!(pending.take(), None);
        assert!(!pending.join(false));

        let mut pending = PendingClose::default();
        assert!(pending.ask(true));
        assert!(!pending.ask(false));
        assert_eq!(pending.take(), Some(true));
        let mut pending = PendingClose::default();
        assert!(pending.ask(false));
        assert_eq!(pending.take(), Some(false));
    }

    /// The app's own parent for the runs is made private, kept private, and is
    /// never a link (the bridge then checks it, ACL included).
    #[cfg(unix)]
    #[test]
    fn the_apps_work_folder_is_private_and_not_a_link() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("plamo-babeldoc-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("data").join(WORK_FOLDER);
        assert_eq!(make_work_parent(dir.clone()).unwrap(), dir);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        // Loosened since: made private again.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        make_work_parent(dir.clone()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        // A link in its place is refused, and what it leads to is left alone.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.join("data").join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(make_work_parent(link).is_err());
        assert_eq!(mode(&elsewhere), 0o755);
        assert!(make_work_parent(PathBuf::from("relative")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn finished_ok() -> Finished {
        finished(Outcome::Ok(saved(0, 1)), Some(Ok(b"%PDF".to_vec())))
    }

    /// A read that finished for another output than the one now in effect is
    /// read again, unless the panel is busy, has a result or shows a batch.
    #[test]
    fn a_pdf_read_for_another_output_is_read_again() {
        use PdfOutput::*;
        assert!(stale_read(Some(Bilingual), Papers, true));
        assert!(stale_read(Some(Papers), Layout, true));
        assert!(!stale_read(Some(Papers), Papers, true));
        assert!(!stale_read(Some(Bilingual), Papers, false));
        assert!(!stale_read(None, Papers, true));
    }

    /// An engine that was moved or lost its exec bit says so.
    #[test]
    fn an_engine_that_cannot_be_started_says_so() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied,
        ] {
            assert_eq!(
                key(conclude(
                    &finished(Outcome::SpawnFailed(kind), None),
                    false,
                    false
                )),
                "files.babeldoc_engine_gone"
            );
        }
        assert_eq!(
            key(conclude(
                &finished(Outcome::SpawnFailed(std::io::ErrorKind::OutOfMemory), None),
                false,
                false
            )),
            "files.babeldoc_failed"
        );
    }

    fn saved(units_failed: u64, translated: u64) -> babeldoc::run::Saved {
        babeldoc::run::Saved {
            output: PathBuf::from("/private/run/out/in.ja.pdf"),
            pages: Some(15),
            input_pages: 15,
            units: babeldoc::run::Units {
                total: translated + units_failed,
                translated,
                failed: units_failed,
                ..Default::default()
            },
            seconds: 1.0,
            nothing_to_translate: false,
        }
    }

    fn finished(outcome: Outcome, pdf: Option<Result<Vec<u8>, std::io::ErrorKind>>) -> Finished {
        Finished {
            outcome: Some(outcome),
            pdf,
            elapsed: Duration::from_secs(1),
        }
    }

    fn completed(result: Result<CompletedFile, FileNotice>) -> CompletedFile {
        match result {
            Ok(done) => done,
            Err(notice) => panic!("expected a translation, got {}", notice.summary()),
        }
    }

    fn key(result: Result<CompletedFile, FileNotice>) -> &'static str {
        match result {
            Err(FileNotice::Message { key, .. }) => key,
            Err(_) => "other notice",
            Ok(_) => "completed",
        }
    }

    #[test]
    fn each_ending_gives_its_result_or_notice() {
        let pdf = || Some(Ok(b"%PDF".to_vec()));
        let done = completed(conclude(
            &finished(Outcome::Ok(saved(0, 19)), pdf()),
            false,
            false,
        ));
        assert!(done.has_changes && !done.review.partial_translation);
        assert!(matches!(&done.output, FileOutput::PdfBytes(b) if b.as_slice() == b"%PDF"));

        let partial = completed(conclude(
            &finished(Outcome::Partial(saved(4, 15)), pdf()),
            false,
            false,
        ));
        assert!(partial.review.partial_translation);
        assert_eq!(partial.review.untranslated, 4);

        let nothing = completed(conclude(
            &finished(Outcome::Ok(saved(0, 0)), pdf()),
            false,
            false,
        ));
        assert!(!nothing.has_changes);

        for (code, expected) in [
            ("encrypted", "files.pdf_password"),
            ("editing_restricted", "files.pdf_edit_restricted"),
            ("protected", "files.pdf_protected"),
            ("signed", "files.pdf_interactive"),
            ("interactive_form", "files.pdf_interactive"),
            ("too_many_pages", "files.pdf_limit"),
            ("too_large", "files.pdf_limit"),
            ("scanned_pdf", "files.babeldoc_scanned"),
            ("already_translated", "files.babeldoc_already_translated"),
            ("unreadable", "files.babeldoc_unreadable"),
            ("output_exists", "files.babeldoc_failed"),
        ] {
            let outcome = Outcome::Refused {
                code: Box::leak(code.to_owned().into_boxed_str()),
            };
            assert_eq!(
                key(conclude(&finished(outcome, None), false, false)),
                expected,
                "{code}"
            );
        }
        let cases = [
            (
                Outcome::AssetsMissing,
                false,
                false,
                "files.babeldoc_assets_missing",
            ),
            (Outcome::Interrupted, false, false, "files.cancelled"),
            (
                Outcome::TranslatorFailed {
                    code: "translation_failed",
                },
                true,
                false,
                "files.cancelled",
            ),
            (
                Outcome::TranslatorFailed {
                    code: "translation_failed",
                },
                false,
                true,
                "files.translation_failed",
            ),
            (
                Outcome::TranslatorFailed {
                    code: "translation_failed",
                },
                false,
                false,
                "files.translation_failed",
            ),
            (
                Outcome::Crashed { exit_code: Some(1) },
                false,
                false,
                "files.babeldoc_failed",
            ),
            (
                Outcome::WorkParentNotPrivate,
                false,
                false,
                "files.babeldoc_failed",
            ),
        ];
        for (outcome, cancelled, runtime, expected) in cases {
            assert_eq!(
                key(conclude(&finished(outcome, None), cancelled, runtime)),
                expected
            );
        }
        // A success whose PDF could not be read is a failure, not an empty translation.
        let unread = finished(
            Outcome::Ok(saved(0, 3)),
            Some(Err(std::io::ErrorKind::NotFound)),
        );
        assert_eq!(
            key(conclude(&unread, false, false)),
            "files.babeldoc_failed"
        );
        // A cancel wins over a success that raced it.
        assert_eq!(
            key(conclude(
                &finished(Outcome::Ok(saved(0, 3)), pdf()),
                true,
                false
            )),
            "files.cancelled"
        );
    }

    #[test]
    fn notices_carry_codes_only() {
        let outcome = Outcome::BabeldocFailed {
            code: "cleanup_failed",
            leftover_folder: true,
        };
        let Err(notice) = conclude(&finished(outcome, None), false, false) else {
            panic!("expected a notice");
        };
        let details = notice.details().unwrap();
        assert!(details.contains("cleanup_failed"), "{details}");
        assert!(!details.contains('/'), "{details}");
    }

    #[test]
    fn the_unit_request_matches_the_built_in_pdf() {
        let request = plain_unit_request(vec!["one".into()], PdfOutput::Papers);
        assert_eq!(request.units.len(), 1);
        assert!(!request.units[0].indivisible);
        assert!(request.tuning.reuse_translations);
        assert_eq!(request.max_unit_tokens, MAX_UNIT_TOKENS);
        assert_eq!(request.max_history_tokens, 0);
        assert!(!(request.validate)(0, "  "));
        assert!((request.validate)(0, "訳"));
    }
}
