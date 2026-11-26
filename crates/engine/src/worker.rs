//! Inference worker thread.
//!
//! All llama.cpp state lives on one dedicated thread. Callers send [`Command`]s through a
//! [`Worker`] handle and receive [`Event`]s through a callback.
//!
//! Jobs supersede each other: submitting a translation cancels the one in flight, and queued
//! jobs that are already stale are skipped. That keeps auto-translate responsive while the
//! user types. Every job ends with one [`Event::Finished`] or [`Event::Failed`], a skipped one
//! with [`StopReason::Cancelled`].

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use pfml::TranslationOptions;

use crate::document::{
    DocumentOptions, Output, Progress, translate_document, translate_file_text, translate_segments,
};
use crate::llama::{
    Device, GpuOffload, Model, ModelOptions, ModelProfile, Runtime, RuntimeOptions, SessionOptions,
};
use crate::models::CatalogEntry;
use crate::units::{UnitRequest, UnitResult, translate_units};
use crate::{StopReason, TranslateStats};

/// Identifies a translation job. Ids increase monotonically per worker.
pub type JobId = u64;

#[derive(Debug, Clone)]
pub struct LoadRequest {
    pub path: PathBuf,
    pub entry: Option<&'static CatalogEntry>,
    pub model: ModelOptions,
    pub session: SessionOptions,
}

/// Compares how fast `devices` translate with the model at `path` (see [`Worker::measure`]).
#[derive(Debug, Clone)]
pub struct MeasureRequest {
    pub path: PathBuf,
    pub entry: Option<&'static CatalogEntry>,
    /// ggml device indices, or `None` for the CPU.
    pub devices: Vec<Option<usize>>,
}

/// How fast one device translated the measurement text.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// ggml device index, or `None` for the CPU.
    pub device: Option<usize>,
    /// The speed, or why the model could not run there.
    pub result: Result<Speed, String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speed {
    pub prompt_tokens_per_second: f64,
    pub generation_tokens_per_second: f64,
}

impl Speed {
    /// Seconds for a typical piece of a long text: 200 prompt tokens (the piece and some
    /// history) and 100 generated ones.
    pub fn typical_seconds(&self) -> f64 {
        200.0 / self.prompt_tokens_per_second + 100.0 / self.generation_tokens_per_second
    }
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub path: PathBuf,
    pub n_params: u64,
    pub size: u64,
    pub n_ctx: usize,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// A complete, validated file unit or an explicit failure (never partial text).
    Unit {
        job: JobId,
        index: usize,
        result: UnitResult,
    },
    /// llama.cpp could not be initialized, or the worker failed unexpectedly. The worker has
    /// exited.
    RuntimeFailed(String),
    /// The compute devices available, sent once after llama.cpp starts.
    Devices(Vec<Device>),
    LoadProgress(f32),
    Loaded(ModelInfo),
    LoadFailed {
        error: String,
        kind: LoadFailure,
    },
    /// The model was released after an unload request; its file is no longer open.
    Unloaded,
    /// A measurement started on device `index` of `count` (see [`Worker::measure`]).
    Measuring {
        index: usize,
        count: usize,
    },
    /// The measurement ended, with one result per device in the order requested. No model is
    /// loaded afterwards.
    Measured(Vec<Measurement>),
    /// A long text was split; chunk `index` of `count` is starting. `source` is its byte range
    /// in the text.
    Chunk {
        job: JobId,
        index: usize,
        count: usize,
        source: (usize, usize),
    },
    /// The translation of segment `index` of a [`Worker::translate_segments`] job.
    Segment {
        job: JobId,
        index: usize,
        text: String,
    },
    /// Generated text for a job, in order.
    Token {
        job: JobId,
        text: String,
    },
    /// Keep only the first `len` bytes of the job's output: output that ran away is being
    /// translated again.
    Rewind {
        job: JobId,
        len: usize,
    },
    /// The output for chunk `index` (or a piece of it) ran away and is translated again in
    /// smaller pieces.
    Retry {
        job: JobId,
        index: usize,
        reason: StopReason,
    },
    /// The packed output changed the layout and is retried in individual paragraphs.
    ParagraphFallback {
        job: JobId,
        index: usize,
        paragraphs: usize,
    },
    /// Output for a piece of chunk `index` (usually a sentence) kept running away and was cut.
    /// `source` is the piece's byte range in the text, and `output` what was kept of its
    /// translation, including the cut marker.
    Cut {
        job: JobId,
        index: usize,
        reason: StopReason,
        source: (usize, usize),
        output: (usize, usize),
    },
    /// The job ended: finished, cancelled (also before it started), or cut short.
    Finished {
        job: JobId,
        stats: TranslateStats,
    },
    Failed {
        job: JobId,
        error: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadFailure {
    #[default]
    Other,
    InvalidFile,
    OutOfMemory,
}

/// llama.cpp returns the same null result for different load failures. Inspect its scoped
/// allocation and GGUF diagnostics without logging the messages or treating a null as OOM.
fn diagnose_load<T>(
    verbose: bool,
    run: impl FnOnce() -> crate::Result<T>,
) -> Result<T, (String, LoadFailure)> {
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    #[derive(Clone)]
    struct Diagnostics {
        failure: Arc<std::sync::Mutex<LoadFailure>>,
        forward: Option<tracing::Dispatch>,
    }

    impl<S: tracing::Subscriber> Layer<S> for Diagnostics {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            if event.metadata().target() == "llama-cpp-2"
                && *event.metadata().level() == tracing::Level::ERROR
            {
                struct Message<'a>(&'a mut LoadFailure);

                impl Visit for Message<'_> {
                    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}

                    fn record_str(&mut self, field: &Field, text: &str) {
                        if field.name() == "message" && *self.0 == LoadFailure::Other {
                            *self.0 = load_diagnostic(text);
                        }
                    }
                }

                event.record(&mut Message(&mut self.failure.lock().unwrap()));
            }
            if let Some(dispatch) = &self.forward {
                dispatch.event(event);
            }
        }
    }

    let diagnostics = Diagnostics {
        failure: Default::default(),
        forward: verbose.then(|| tracing::dispatcher::get_default(Clone::clone)),
    };
    let subscriber = tracing_subscriber::registry().with(diagnostics.clone());
    tracing::subscriber::with_default(subscriber, run)
        .map_err(|error| (error.to_string(), *diagnostics.failure.lock().unwrap()))
}

fn load_diagnostic(message: &str) -> LoadFailure {
    if (message.starts_with("failed to allocate ") && message.contains("(out of memory)"))
        || message.starts_with("error loading model: unable to allocate ")
        || message.starts_with("failed to allocate compute ")
    {
        LoadFailure::OutOfMemory
    } else if message.starts_with("invalid magic characters: ")
        || message.trim_end() == "failed to read magic"
    {
        LoadFailure::InvalidFile
    } else {
        LoadFailure::Other
    }
}

enum Command {
    TranslateUnits {
        job: JobId,
        options: TranslationOptions,
        request: UnitRequest,
    },
    Load(LoadRequest),
    Measure(MeasureRequest),
    Unload,
    Translate {
        job: JobId,
        options: TranslationOptions,
        document: DocumentOptions,
        text: String,
        file: bool,
    },
    TranslateSegments {
        job: JobId,
        options: TranslationOptions,
        document: DocumentOptions,
        segments: Vec<String>,
    },
}

type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// Handle to the inference worker. Dropping it stops the current job, a model load or a
/// measurement included, and shuts the worker down.
pub struct Worker {
    commands: mpsc::Sender<Command>,
    /// The newest job id. Any job with a different id is cancelled or skipped.
    latest: Arc<AtomicU64>,
    /// Set when the handle is dropped: stops a model load or a measurement in progress, which
    /// job ids do not reach.
    closing: Arc<AtomicBool>,
    /// Waited for on drop (see [`Worker::wait_for_thread`]).
    #[cfg(target_os = "macos")]
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        self.cancel();
        // Replacing the only sender closes the channel: the thread frees the model and exits.
        self.commands = mpsc::channel().0;
        #[cfg(target_os = "macos")]
        self.wait_for_thread();
    }
}

impl Worker {
    pub fn spawn(options: RuntimeOptions, events: impl Fn(Event) + Send + Sync + 'static) -> Self {
        let (commands, rx) = mpsc::channel();
        let latest = Arc::new(AtomicU64::new(0));
        let events: EventSink = Arc::new(events);
        let closing = Arc::new(AtomicBool::new(false));
        let (thread_latest, thread_closing) = (latest.clone(), closing.clone());
        let thread = thread::Builder::new()
            .name("inference".into())
            .spawn(move || {
                let report = events.clone();
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run(options, rx, events, thread_latest, thread_closing)
                }));
                // Without this the UI would wait for the dead thread forever.
                if let Err(payload) = result {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    report(Event::RuntimeFailed(format!("internal error: {message}")));
                }
            })
            .expect("failed to spawn the inference thread");
        #[cfg(not(target_os = "macos"))]
        drop(thread);
        Self {
            commands,
            latest,
            closing,
            #[cfg(target_os = "macos")]
            thread: Some(thread),
        }
    }

    /// ggml's Metal backend asserts when the process exits that all its buffers were freed, and
    /// aborts otherwise (`ggml_metal_rsets_free`, from a static destructor). The thread frees them
    /// once the command channel closes, so let it finish before the process can exit.
    #[cfg(target_os = "macos")]
    fn wait_for_thread(&mut self) {
        // Enough to finish the current token, or to stop a load or a measurement, and free the
        // model.
        const LIMIT: Duration = Duration::from_secs(10);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let start = std::time::Instant::now();
        while !thread.is_finished() && start.elapsed() < LIMIT {
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Loads a model, replacing the current one. Cancels any running job.
    pub fn load(&self, request: LoadRequest) {
        self.cancel();
        self.send(Command::Load(request));
    }

    pub fn unload(&self) {
        self.cancel();
        self.send(Command::Unload);
    }

    /// Unloads the model and translates a fixed text with it on each of the request's devices
    /// in turn, reporting [`Event::Measuring`] for each and then [`Event::Measured`]. Cancels
    /// any running job; load a model afterwards to translate again.
    pub fn measure(&self, request: MeasureRequest) {
        self.cancel();
        self.send(Command::Measure(request));
    }

    /// Starts a translation, cancelling any earlier one. Returns the new job id.
    pub fn translate(
        &self,
        options: TranslationOptions,
        document: DocumentOptions,
        text: String,
    ) -> JobId {
        self.translate_text(options, document, text, false)
    }

    /// Starts a TXT file translation, cancelling any earlier one.
    pub fn translate_file_text(
        &self,
        options: TranslationOptions,
        document: DocumentOptions,
        text: String,
    ) -> JobId {
        self.translate_text(options, document, text, true)
    }

    fn translate_text(
        &self,
        options: TranslationOptions,
        document: DocumentOptions,
        text: String,
        file: bool,
    ) -> JobId {
        let job = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(Command::Translate {
            job,
            options,
            document,
            text,
            file,
        });
        job
    }

    /// Translates `segments` in order, with the earlier ones as history, reporting each one as
    /// an [`Event::Segment`]. Cancels any earlier job, like [`Worker::translate`].
    pub fn translate_segments(
        &self,
        options: TranslationOptions,
        document: DocumentOptions,
        segments: Vec<String>,
    ) -> JobId {
        let job = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(Command::TranslateSegments {
            job,
            options,
            document,
            segments,
        });
        job
    }

    /// Translates indivisible file units, validating before committing to history.
    pub fn translate_units(&self, options: TranslationOptions, request: UnitRequest) -> JobId {
        let job = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        self.send(Command::TranslateUnits {
            job,
            options,
            request,
        });
        job
    }

    /// Cancels the running job, if any.
    pub fn cancel(&self) {
        self.latest.fetch_add(1, Ordering::SeqCst);
    }

    fn send(&self, command: Command) {
        // The worker only exits after the runtime failed, which was already reported.
        let _ = self.commands.send(command);
    }
}

fn run(
    options: RuntimeOptions,
    commands: mpsc::Receiver<Command>,
    events: EventSink,
    latest: Arc<AtomicU64>,
    closing: Arc<AtomicBool>,
) {
    let verbose = options.logs;
    let runtime = match Runtime::init(RuntimeOptions { logs: true }) {
        Ok(r) => r,
        Err(e) => {
            events(Event::RuntimeFailed(e.to_string()));
            return;
        }
    };
    events(Event::Devices(runtime.devices()));

    let mut next = None;
    loop {
        let Some(command) = next.take().or_else(|| commands.recv().ok()) else {
            return;
        };
        match command {
            Command::Load(request) => {
                if let Some(model) = load(&runtime, &request, &events, &closing, verbose) {
                    // Serve requests until the next load or unload.
                    match diagnose_load(verbose, || model.session(&runtime, &request.session)) {
                        Ok(session) => {
                            next = serve(session, &model, &request, &commands, &events, &latest);
                        }
                        Err((error, kind)) => events(Event::LoadFailed { error, kind }),
                    }
                    drop(model);
                    if next.is_none() {
                        events(Event::Unloaded);
                    }
                }
            }
            Command::Measure(request) => events(Event::Measured(measure(
                &runtime, &request, &events, &closing,
            ))),
            Command::Unload => events(Event::Unloaded),
            Command::Translate { job, .. }
            | Command::TranslateSegments { job, .. }
            | Command::TranslateUnits { job, .. } => events(Event::Failed {
                job,
                error: "no model is loaded".into(),
            }),
        }
    }
}

fn load(
    runtime: &Runtime,
    request: &LoadRequest,
    events: &EventSink,
    closing: &Arc<AtomicBool>,
    verbose: bool,
) -> Option<Model> {
    let progress_events = events.clone();
    let closing = closing.clone();
    if let Err(error) = verify_pinned(request.entry, &request.path, &closing) {
        events(Event::LoadFailed {
            error,
            kind: LoadFailure::InvalidFile,
        });
        return None;
    }
    let profile = request
        .entry
        .map_or(ModelProfile::default(), |entry| entry.profile);
    let result = diagnose_load(verbose, || {
        runtime.load_model_with_profile(&request.path, &request.model, profile, move |p| {
            progress_events(Event::LoadProgress(p));
            // Returning false makes llama.cpp stop loading and free what it allocated.
            !closing.load(Ordering::SeqCst)
        })
    });
    match result {
        Ok(model) => Some(model),
        Err((error, kind)) => {
            events(Event::LoadFailed { error, kind });
            None
        }
    }
}

fn verify_pinned(
    entry: Option<&CatalogEntry>,
    path: &std::path::Path,
    closing: &AtomicBool,
) -> Result<(), String> {
    if let Some(entry) = entry.filter(|entry| entry.pinned_id.is_some()) {
        use crate::download::VerificationStamp;

        let stamp = VerificationStamp::read(entry, path);
        if stamp
            .as_ref()
            .is_some_and(|stamp| stamp.matches_record(path))
        {
            return Ok(());
        }
        crate::download::verify_file(entry, path, &|| closing.load(Ordering::SeqCst))
            .map_err(|e| e.to_string())?;
        // A changed file must be verified again; a read-only model folder still works.
        if let Some(stamp) = stamp
            && Some(&stamp) == VerificationStamp::read(entry, path).as_ref()
        {
            stamp.write(path);
        }
    }
    Ok(())
}

/// Runs translation jobs on `model`. Returns the command that ended the session
/// (a new load), or `None` when the session was unloaded or the channel closed.
fn serve(
    mut session: crate::llama::Session<'_>,
    model: &Model,
    request: &LoadRequest,
    commands: &mpsc::Receiver<Command>,
    events: &EventSink,
    latest: &AtomicU64,
) -> Option<Command> {
    events(Event::Loaded(ModelInfo {
        path: request.path.clone(),
        n_params: model.n_params(),
        size: model.size(),
        n_ctx: session.n_ctx(),
    }));

    loop {
        let command = commands.recv().ok()?;
        let job = match &command {
            Command::Load(_) | Command::Measure(_) => return Some(command),
            Command::Unload => return None,
            Command::Translate { job, .. }
            | Command::TranslateSegments { job, .. }
            | Command::TranslateUnits { job, .. } => *job,
        };
        if latest.load(Ordering::SeqCst) != job {
            // Cancelled or superseded before it started. Reported all the same, so that a
            // caller waiting for this job learns that it ended.
            events(Event::Finished {
                job,
                stats: TranslateStats::cancelled(),
            });
            continue;
        }
        let n_ctx = session.n_ctx();
        let count_tokens = |s: &str| model.count_tokens(s);
        let is_cancelled = || latest.load(Ordering::Relaxed) != job;
        let result = match command {
            Command::TranslateUnits {
                options, request, ..
            } => translate_units(
                &mut session,
                &count_tokens,
                n_ctx,
                &options,
                &request,
                &mut |index, result| events(Event::Unit { job, index, result }),
                &is_cancelled,
            ),
            Command::Translate {
                options,
                document,
                text,
                file,
                ..
            } => (if file {
                translate_file_text
            } else {
                translate_document
            })(
                &mut session,
                &count_tokens,
                n_ctx,
                &document,
                &options,
                &text,
                &mut |output| match output {
                    Output::Text(text) => events(Event::Token {
                        job,
                        text: text.to_owned(),
                    }),
                    Output::Truncate(len) => events(Event::Rewind { job, len }),
                },
                &mut |progress| match progress {
                    Progress::Chunk {
                        index,
                        count,
                        source,
                    } => events(Event::Chunk {
                        job,
                        index,
                        count,
                        source,
                    }),
                    Progress::Retry { index, reason, .. } => {
                        events(Event::Retry { job, index, reason })
                    }
                    Progress::ParagraphFallback { index, paragraphs } => {
                        events(Event::ParagraphFallback {
                            job,
                            index,
                            paragraphs,
                        })
                    }
                    Progress::Cut {
                        index,
                        reason,
                        source,
                        output,
                    } => events(Event::Cut {
                        job,
                        index,
                        reason,
                        source,
                        output,
                    }),
                },
                &is_cancelled,
            ),
            Command::TranslateSegments {
                options,
                document,
                segments,
                ..
            } => translate_segments(
                &mut session,
                &count_tokens,
                n_ctx,
                &document,
                &options,
                &segments,
                &mut |index, text| events(Event::Segment { job, index, text }),
                &is_cancelled,
            ),
            Command::Load(_) | Command::Measure(_) | Command::Unload => {
                unreachable!("handled above")
            }
        };
        events(match result {
            Ok(stats) => Event::Finished { job, stats },
            Err(e) => Event::Failed {
                job,
                error: e.to_string(),
            },
        });
    }
}

/// The text translated to measure a device: a short paragraph, as typed or pasted.
const MEASURE_TEXT: &str = "The meeting has been moved to next Monday afternoon. Please let the other \
members of the team know, and bring the revised budget and the schedule for the spring release \
with you, so that we can agree on the priorities before the end of the month.";

/// Measures each device of `request` (see [`Worker::measure`]). Stops early when the worker is
/// closing.
fn measure(
    runtime: &Runtime,
    request: &MeasureRequest,
    events: &EventSink,
    closing: &Arc<AtomicBool>,
) -> Vec<Measurement> {
    let count = request.devices.len();
    let verification = verify_pinned(request.entry, &request.path, closing);
    let profile = request
        .entry
        .map_or(ModelProfile::default(), |entry| entry.profile);
    request
        .devices
        .iter()
        .enumerate()
        .take_while(|_| !closing.load(Ordering::SeqCst))
        .map(|(index, &device)| {
            events(Event::Measuring { index, count });
            Measurement {
                device,
                result: verification.clone().and_then(|()| {
                    measure_device(runtime, &request.path, device, closing, profile)
                }),
            }
        })
        .collect()
}

fn measure_device(
    runtime: &Runtime,
    path: &std::path::Path,
    device: Option<usize>,
    closing: &Arc<AtomicBool>,
    profile: ModelProfile,
) -> Result<Speed, String> {
    let options = ModelOptions {
        gpu: device.map(|device| GpuOffload {
            device,
            layers: None,
        }),
    };
    let load_closing = closing.clone();
    let model = runtime
        .load_model_with_profile(path, &options, profile, move |_| {
            !load_closing.load(Ordering::SeqCst)
        })
        .map_err(|e| e.to_string())?;
    // A short context is enough for the text and is quicker to set up.
    let session_options = SessionOptions {
        n_ctx: 4096,
        ..SessionOptions::default().with_auto_threads()
    };
    let mut session = model
        .session(runtime, &session_options)
        .map_err(|e| e.to_string())?;
    let mut options = TranslationOptions::new(pfml::LangCode::new("ja").expect("a valid code"));
    if profile.capabilities().directions.is_some() {
        options.source = Some(pfml::LangCode::new("en").expect("a valid code"));
    }
    let mut translate = || {
        translate_document(
            &mut session,
            &|s: &str| model.count_tokens(s),
            4096,
            &DocumentOptions::default(),
            &options,
            MEASURE_TEXT,
            &mut |_| {},
            &mut |_| {},
            &|| closing.load(Ordering::SeqCst),
        )
        .map_err(|e| e.to_string())
    };
    // On a GPU the first run includes one-time work, such as compiling kernels.
    if device.is_some() {
        translate()?;
    }
    let stats = translate()?;
    let rate = |tokens: usize, duration: Duration| tokens as f64 / duration.as_secs_f64().max(1e-6);
    Ok(Speed {
        prompt_tokens_per_second: rate(stats.prompt_tokens, stats.prompt_duration),
        generation_tokens_per_second: rate(stats.generated_tokens, stats.generation_duration),
    })
}

#[cfg(test)]
mod verification_tests {
    use super::{CatalogEntry, verify_pinned};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn reports_an_allocation_failure_only_when_the_backend_reports_it() {
        use super::{LoadFailure, diagnose_load};

        for (message, expected) in [
            (
                "failed to allocate Metal buffer of 123 bytes (out of memory)",
                LoadFailure::OutOfMemory,
            ),
            (
                "error loading model: unable to allocate CPU buffer",
                LoadFailure::OutOfMemory,
            ),
            (
                "failed to allocate compute pp buffers",
                LoadFailure::OutOfMemory,
            ),
            (
                "invalid magic characters: 'FAKE', expected 'GGUF'",
                LoadFailure::InvalidFile,
            ),
            (
                "error loading model: no file named out of memory",
                LoadFailure::Other,
            ),
        ] {
            let result = diagnose_load(false, || {
                tracing::error!(target: "llama-cpp-2", message);
                Err::<(), _>(crate::EngineError::NoBackends)
            });
            assert_eq!(result.unwrap_err().1, expected, "{message}");
        }
    }

    #[test]
    fn a_verified_download_seeds_the_load_cache_and_deletion_removes_it() {
        use crate::download::{Downloaded, download, verification_record_path};
        use crate::models::ModelStore;

        let dir = crate::test_dir("plamo-verified-download-test");
        let store = ModelStore::new(&dir);
        let entry = CatalogEntry {
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ..crate::models::HY_MT2
        };
        let path = store.path(&entry);
        let part = store.partial_path(&entry);
        let record = verification_record_path(&path);
        for replacement in [false, true] {
            std::fs::write(&part, b"abc").unwrap();
            if replacement {
                std::fs::write(&path, b"old").unwrap();
            }
            let result = download(&entry, &store, &mut |_| {}, &|| false).unwrap();
            if replacement {
                assert_eq!(result, Downloaded::BesideOlder);
                assert!(!record.exists());
                assert_eq!(store.replace_with_download(&entry).unwrap(), path);
            } else {
                assert_eq!(result, Downloaded::Placed(path.clone()));
            }
            // Closing cancels a full hash, so success demonstrates that the first load
            // can use the verification from the download.
            verify_pinned(Some(&entry), &path, &AtomicBool::new(true)).unwrap();
            store.delete(&entry).unwrap();
            assert!(!path.exists() && !record.exists());
            assert!(!verification_record_path(&part).exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_file_is_cached_and_a_changed_file_is_checked_again() {
        let dir = crate::test_dir("plamo-verified-test");
        let path = dir.join("test.gguf");
        let entry = CatalogEntry {
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ..crate::models::HY_MT2
        };
        std::fs::write(&path, b"abc").unwrap();
        verify_pinned(Some(&entry), &path, &AtomicBool::new(false)).unwrap();
        // A cancelled hash cannot succeed, so this exercises the stored record.
        verify_pinned(Some(&entry), &path, &AtomicBool::new(true)).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert!(verify_pinned(Some(&entry), &path, &AtomicBool::new(false)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
