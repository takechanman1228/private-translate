//! Starts `babeldoc-engine translate` as a child process, serves it the caller's model through
//! an [`Endpoint`], and turns what it reports into an [`Outcome`].
//!
//! The engine reports progress and a final result as JSON lines:
//! standard output carries exactly one JSON line, standard error carries progress lines
//! (`{"event": "progress", "stage": …, "overall": …}`), and the exit code must equal the JSON's
//! `exit_code`. A result that cannot be read, has more than one line, disagrees with the exit
//! code, has an unknown `schema_version`, or lacks a field the engine always writes on success
//! is [`Outcome::InvalidResult`], and no PDF is handed over as a success.
//!
//! # Where the PDF goes
//!
//! Each run gets a private folder that this crate creates ([`WorkDir`]: new, mode `0700`, under
//! [`Job::work_parent`] or the system's temporary folder), and the engine writes into its `out`
//! subfolder. The parent must be a folder of this user's that no one else can write to
//! (macOS's per-user `$TMPDIR` is; Linux's `/tmp` is not, so pass a private `work_parent`
//! there), else the run ends with [`Outcome::WorkParentNotPrivate`]. The engine also makes its
//! own job folder, which holds a copy of the input, in the private folder's `engine` subfolder
//! (through `BABELDOC_ENGINE_JOB_PARENT`), so that copy goes with the private folder even when
//! the engine is killed or crashes. Its `TMPDIR` is left as inherited. This crate never lists
//! or deletes anything in a folder of the caller's. On
//! success the [`Report`] owns the private folder: the caller copies the PDF out with
//! [`Report::open_output`] and its own save that never overwrites, and dropping the report (or
//! calling [`WorkDir::remove`] on [`Report::work_dir`]) removes the folder. On every other
//! outcome the folder is removed before [`translate_pdf`] returns; if that fails,
//! [`Report::leftover`] names it.
//!
//! # The endpoint's URL
//!
//! The URL carries the endpoint's secret, so it is passed in the engine's environment
//! (`BABELDOC_ENGINE_ENDPOINT`), not on its command line where `ps` shows it to everyone. No
//! `Debug` output of this crate contains it.
//!
//! # Stopping
//!
//! Cancelling sends SIGINT to the engine, which cleans up and exits with 130. If it has not
//! exited after [`Job::stop_grace`], its process group is killed. However the engine exits, its
//! exit is first seen without reaping it (`waitid` with `WNOWAIT`), so that its pid, and with
//! it the group's id, cannot be reused yet; then whatever is left in the group is killed, and
//! only then is the engine reaped. No process of the engine's group outlives the run. The outcome is
//! [`Outcome::Interrupted`]. The endpoint stops with the engine; a translate function still
//! running is told so through its [`StopSignal`] and must return soon (see
//! [`Endpoint::serve`]): that is the one wait this crate cannot cut short. If the progress
//! function panics, the endpoint is stopped and the engine's group killed and reaped before the
//! panic goes on.
//!
//! Windows is not supported in this stage: [`translate_pdf`] returns [`Outcome::Unsupported`].

#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use std::collections::VecDeque;
#[cfg(test)]
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::endpoint::{Endpoint, Stats, StopSignal, TranslateError};
#[cfg(unix)]
pub use crate::workdir::WorkDir;

/// The result `schema_version` this crate reads.
pub const SCHEMA_VERSION: u64 = 1;
/// The environment variable the engine reads the endpoint from.
pub const ENDPOINT_ENV: &str = "BABELDOC_ENGINE_ENDPOINT";
/// The environment variable that tells the engine where to make its job folder (the one that
/// holds its copy of the input): the private folder's `engine` subfolder.
pub const JOB_PARENT_ENV: &str = "BABELDOC_ENGINE_JOB_PARENT";
/// The most bytes of standard output read (one JSON line is far smaller).
const MAX_STDOUT_BYTES: usize = 1 << 20;
/// The longest standard error line kept (longer ones are dropped).
const MAX_STDERR_LINE_BYTES: usize = 64 << 10;
/// Progress values waiting for the calling thread; older ones are dropped when it falls behind.
const PROGRESS_QUEUE: usize = 32;
/// How long to wait for the pipes to close after the engine exited.
const PIPE_GRACE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(50);

/// BabelDOC 0.7.1's stage names (`babeldoc/format/pdf/high_level.py`, `TRANSLATE_STAGES`), the
/// only values [`Progress::stage`] takes besides [`OTHER_STAGE`].
pub const STAGES: [&str; 13] = [
    "Parse PDF and Create Intermediate Representation",
    "DetectScannedFile",
    "Parse Page Layout",
    "Parse Table",
    "Parse Paragraphs",
    "Parse Formulas and Styles",
    "Automatic Term Extraction",
    "Translate Paragraphs",
    "Typesetting",
    "Add Fonts",
    "Generate drawing instructions",
    "Subset font",
    "Save PDF",
];
/// A stage name that is not in [`STAGES`].
pub const OTHER_STAGE: &str = "other";
/// An engine reason code that is not one it is known to write for that exit code.
pub const UNKNOWN_CODE: &str = "unknown";

/// `refusal.code` values of exit code 2.
const REFUSAL_CODES: &[&str] = &[
    "not_pdf",
    "encrypted",
    "editing_restricted",
    "protected",
    "signed",
    "interactive_form",
    "too_many_pages",
    "too_large",
    "unreadable",
    "already_translated",
    "scanned_pdf",
    "output_exists",
    "output_conflict",
    "output_not_folder",
    "report_folder_missing",
    "invalid_pages",
    "invalid_arguments",
];
/// `error.code` values of exit code 3.
const TRANSLATOR_CODES: &[&str] = &[
    "endpoint_not_local",
    "endpoint_unreachable",
    "translation_failed",
    "nothing_translated",
];
/// `error.code` values of exit code 4.
const BABELDOC_CODES: &[&str] = &[
    "babeldoc_version",
    "babeldoc_failed",
    "output_failed",
    "report_failed",
    "cleanup_failed",
    "internal_error",
];

/// BabelDOC settings chosen by document kind (`--profile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Papers: formulas and several columns.
    Paper,
    /// Documents whose numbers carry the meaning.
    Business,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paper => "paper",
            Self::Business => "business",
        }
    }
}

/// One translation to run.
#[derive(Debug, Clone)]
pub struct Job {
    /// The `babeldoc-engine` executable.
    pub engine: PathBuf,
    pub input: PathBuf,
    /// The folder in which the run's private folder is created (`None`: the system's temporary
    /// folder). It must exist and be private to this user (owned by this user, not writable by
    /// its group or others); nothing else in it is touched.
    pub work_parent: Option<PathBuf>,
    /// Language codes, such as `en` and `ja`.
    pub source: String,
    pub target: String,
    pub profile: Profile,
    /// Pages to translate, such as `1-3,5` (all pages are kept in the output).
    pub pages: Option<String>,
    /// How long the engine waits for one answer (`--timeout`).
    pub answer_timeout: Duration,
    /// After a cancel, how long to wait for the engine to exit before killing it.
    pub stop_grace: Duration,
}

impl Job {
    pub fn new(
        engine: impl Into<PathBuf>,
        input: impl Into<PathBuf>,
        source: impl Into<String>,
        target: impl Into<String>,
        profile: Profile,
    ) -> Self {
        Self {
            engine: engine.into(),
            input: input.into(),
            work_parent: None,
            source: source.into(),
            target: target.into(),
            profile,
            pages: None,
            answer_timeout: Duration::from_secs(600),
            stop_grace: Duration::from_secs(30),
        }
    }
}

/// Asks a running [`translate_pdf`] to stop. Clones share the request.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A progress line from the engine: BabelDOC's stage and the overall percentage.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// One of [`STAGES`], or [`OTHER_STAGE`].
    pub stage: &'static str,
    /// 0 to 100.
    pub overall: f32,
}

/// The engine's unit counts (`units` in its result). Every key is required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Units {
    pub total: u64,
    pub translated: u64,
    pub failed: u64,
    pub unparsed_requests: u64,
    pub placeholder_mismatch: u64,
    pub incomplete_answers: u64,
    pub empty_answers: u64,
    pub invalid_answers: u64,
    pub skipped_no_letters: u64,
    pub connection_errors: u64,
}

/// A saved translation, in the run's private folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    /// The translated PDF, in the `out` folder of [`Report::work_dir`]. Read it with
    /// [`Report::open_output`].
    pub output: PathBuf,
    /// The saved PDF's page count; `None` when the engine could not read it back (the PDF is
    /// saved either way: `pages: null` is part of the engine's contract).
    pub pages: Option<u32>,
    pub input_pages: u32,
    pub units: Units,
    /// BabelDOC's run time as the engine measured it.
    pub seconds: f64,
    /// No unit had letters to translate.
    pub nothing_to_translate: bool,
}

/// Why a result was not trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidReason {
    /// Standard output was empty.
    Empty,
    /// More than one line on standard output.
    SeveralLines,
    /// Not UTF-8, not JSON, or not the expected shape (a field the engine always writes on
    /// success is missing or of the wrong type).
    Malformed,
    /// Standard output was larger than this crate reads.
    TooLong,
    /// `schema_version` is not [`SCHEMA_VERSION`].
    Schema,
    /// The JSON's `exit_code`, `status`, counts or job fields disagree with the process or the
    /// job.
    Mismatch,
    /// An exit code the engine does not use.
    UnknownExit,
    /// A success without a regular PDF made in this run's private folder.
    MissingOutput,
    /// The pipes stayed open after the engine exited.
    PipesOpen,
}

/// How a translation ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Saved, every unit translated.
    Ok(Saved),
    /// Saved, but `units.failed` units kept their source text.
    Partial(Saved),
    /// The engine refused the input or the arguments (`refusal.code`, such as `encrypted`).
    Refused { code: &'static str },
    /// The translation endpoint failed (`error.code`, such as `translation_failed`).
    TranslatorFailed { code: &'static str },
    /// BabelDOC's offline assets are missing.
    AssetsMissing,
    /// BabelDOC (or saving) failed (`error.code`). `leftover_folder` says whether the engine
    /// could not remove its job folder (`cleanup_failed`), which holds a copy of the input. The
    /// path is free-form text from the engine, so it is not kept here (it would reach `Debug`):
    /// read it with [`Report::engine_leftover_folder`].
    BabeldocFailed {
        code: &'static str,
        leftover_folder: bool,
    },
    /// Cancelled (or the engine was interrupted). No PDF was kept.
    Interrupted,
    /// The engine ended without a result: killed by a signal, or exited without a line.
    Crashed { exit_code: Option<i32> },
    /// The engine's result could not be trusted.
    InvalidResult(InvalidReason),
    /// The engine, or one of the threads that read it and serve it, could not be started.
    SpawnFailed(std::io::ErrorKind),
    /// The local endpoint could not be opened.
    EndpointFailed(std::io::ErrorKind),
    /// The private folder could not be created.
    WorkDirFailed(std::io::ErrorKind),
    /// [`Job::work_parent`] (or the system's temporary folder) is not a folder of this user's
    /// that only this user can write to, so no private folder was made in it.
    WorkParentNotPrivate,
    /// Not supported on this platform (Windows, for now).
    Unsupported,
}

/// What [`translate_pdf`] did.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    /// What the endpoint served.
    pub endpoint: Stats,
    /// The engine's exit code, when it exited with one.
    pub exit_code: Option<i32>,
    pub elapsed: Duration,
    /// The private folder holding the PDF: `Some` only for [`Outcome::Ok`] and
    /// [`Outcome::Partial`]. Removed when the report is dropped.
    #[cfg(unix)]
    pub work_dir: Option<WorkDir>,
    /// The private folder when it could not be removed after a run that saved nothing.
    pub leftover: Option<PathBuf>,
    /// The engine's own job folder when it could not remove it (see
    /// [`Outcome::BabeldocFailed`]). Kept out of `Debug`.
    engine_leftover: Option<EngineFolder>,
}

/// A path the engine wrote, kept out of `Debug` output.
struct EngineFolder(PathBuf);

impl std::fmt::Debug for EngineFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineFolder(..)")
    }
}

impl Report {
    /// The engine's job folder that it could not remove, as the engine wrote it (free-form: do
    /// not log it). `Some` only with [`Outcome::BabeldocFailed`] whose `leftover_folder` is set.
    pub fn engine_leftover_folder(&self) -> Option<&Path> {
        self.engine_leftover.as_ref().map(|f| f.0.as_path())
    }

    /// Opens the translated PDF for reading (through the private folder's descriptor, never
    /// following a link). `NotFound` when the run saved nothing.
    #[cfg(unix)]
    pub fn open_output(&self) -> std::io::Result<std::fs::File> {
        let saved = match &self.outcome {
            Outcome::Ok(saved) | Outcome::Partial(saved) => saved,
            _ => return Err(std::io::ErrorKind::NotFound.into()),
        };
        let (Some(dir), Some(name)) = (&self.work_dir, saved.output.file_name()) else {
            return Err(std::io::ErrorKind::NotFound.into());
        };
        dir.open_in_out(name)
    }
}

/// Translates `job.input` with the engine, serving `translate` to it, and waits for the end.
///
/// `translate` runs on a thread of this call, one paragraph at a time, and is never given the
/// system prompt. It gets a [`StopSignal`] that is set once the run is cancelled or the engine
/// has exited; it must then return promptly (with [`TranslateError::Fatal`]), because this call
/// returns only after the paragraph in flight is answered. A panic in it is answered as a fatal
/// failure; its payload is not printed by this crate, but the process's panic hook runs first
/// (see the `endpoint` module). `progress` is called on the calling thread.
pub fn translate_pdf(
    job: &Job,
    translate: impl FnMut(&str, StopSignal<'_>) -> Result<String, TranslateError> + Send,
    progress: impl FnMut(Progress),
    cancel: &Cancel,
) -> Report {
    let started = Instant::now();
    #[cfg(unix)]
    {
        unix::run(job, translate, progress, cancel, started)
    }
    #[cfg(not(unix))]
    {
        let _ = (job, translate, progress, cancel);
        Report {
            outcome: Outcome::Unsupported,
            endpoint: Stats::default(),
            exit_code: None,
            elapsed: started.elapsed(),
            leftover: None,
            engine_leftover: None,
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;

    use crate::process::{read_all, read_lines};
    use crate::workdir::CreateError;

    /// Kills and reaps the engine's group if the run unwinds before the child was reaped, and
    /// stops the endpoint in any case.
    struct ChildGuard<'a> {
        child: Child,
        pid: libc::pid_t,
        reaped: bool,
        stop: &'a AtomicBool,
    }

    impl ChildGuard<'_> {
        /// Whether the engine has exited, without reaping it: until it is reaped its pid, and
        /// so its group's id, stays taken, and the group can still be signalled safely.
        fn exited(&mut self) -> std::io::Result<bool> {
            // SAFETY: zeroed is a valid `siginfo_t`; `waitid` fills it.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: our unreaped child and room for the result.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // With `WNOHANG`, a child that has not exited leaves `si_pid` at zero.
            #[cfg(target_os = "linux")]
            // SAFETY: filled by `waitid` (or zeroed).
            let pid = unsafe { info.si_pid() };
            #[cfg(not(target_os = "linux"))]
            let pid = info.si_pid;
            Ok(pid != 0)
        }

        fn kill_and_wait(&mut self) -> Option<ExitStatus> {
            kill_group(self.pid);
            let status = self.child.wait().ok();
            self.reaped = true;
            status
        }
    }

    impl Drop for ChildGuard<'_> {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if !self.reaped {
                kill_group(self.pid);
                let _ = self.child.wait();
            }
        }
    }

    pub(super) fn run(
        job: &Job,
        mut translate: impl FnMut(&str, StopSignal<'_>) -> Result<String, TranslateError> + Send,
        mut progress: impl FnMut(Progress),
        cancel: &Cancel,
        started: Instant,
    ) -> Report {
        let report = |(outcome, engine_leftover): (Outcome, Option<PathBuf>),
                      endpoint,
                      exit_code,
                      work_dir: Option<WorkDir>| {
            let mut leftover = None;
            let work_dir = match (&outcome, work_dir) {
                (Outcome::Ok(_) | Outcome::Partial(_), dir) => dir,
                (_, Some(dir)) => {
                    let path = dir.path().to_owned();
                    if dir.remove().is_err() {
                        leftover = Some(path);
                    }
                    None
                }
                (_, None) => None,
            };
            Report {
                outcome,
                endpoint,
                exit_code,
                elapsed: started.elapsed(),
                work_dir,
                leftover,
                engine_leftover: engine_leftover.map(EngineFolder),
            }
        };
        let only = |outcome| (outcome, None);
        let parent = job.work_parent.clone().unwrap_or_else(std::env::temp_dir);
        let work = match WorkDir::create(&parent) {
            Ok(work) => work,
            Err(e) => {
                let outcome = match e {
                    CreateError::ParentNotPrivate => Outcome::WorkParentNotPrivate,
                    CreateError::Io(e) => Outcome::WorkDirFailed(e.kind()),
                };
                return report(only(outcome), Stats::default(), None, None);
            }
        };
        let endpoint = match Endpoint::bind() {
            Ok(endpoint) => endpoint,
            Err(e) => {
                return report(
                    only(Outcome::EndpointFailed(e.kind())),
                    Stats::default(),
                    None,
                    Some(work),
                );
            }
        };
        // An absolute path, after `--`: an input named `-x.pdf` is never read as an option.
        let input = match std::path::absolute(&job.input) {
            Ok(input) => input,
            Err(e) => {
                return report(
                    only(Outcome::SpawnFailed(e.kind())),
                    Stats::default(),
                    None,
                    Some(work),
                );
            }
        };

        let mut command = Command::new(&job.engine);
        command
            .arg("translate")
            .arg("--output")
            .arg(work.out_path())
            .args(["--source", &job.source, "--target", &job.target])
            .args(["--profile", job.profile.as_str()])
            .args(["--model", "local", "--workers", "1"])
            .arg("--timeout")
            .arg(format!(
                "{}",
                job.answer_timeout.as_secs_f64().clamp(1.0, 3600.0)
            ));
        if let Some(pages) = &job.pages {
            command.arg("--pages").arg(pages);
        }
        command.arg("--").arg(&input);
        // The secret stays off the command line.
        command.env(ENDPOINT_ENV, endpoint.url());
        // The engine's job folder (its copy of the input) inside the private folder.
        command.env(JOB_PARENT_ENV, work.engine_path());
        // A group of its own: a terminal's Ctrl-C does not reach the engine directly, and a
        // forced stop can take BabelDOC's worker processes with it.
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The endpoint's stop flag, set by the guard below whenever this call leaves.
        let stop = AtomicBool::new(false);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                return report(
                    only(Outcome::SpawnFailed(e.kind())),
                    Stats::default(),
                    None,
                    Some(work),
                );
            }
        };
        // Owned by the guard at once: from here on, every way out of this call (an error
        // starting a thread, a panic in `progress`) stops the endpoint and kills and reaps the
        // engine's group.
        let pid = child.id() as libc::pid_t;
        let mut guard = ChildGuard {
            child,
            pid,
            reaped: false,
            stop: &stop,
        };
        let thread_failed = |guard: &mut ChildGuard<'_>, e: std::io::Error| {
            guard.kill_and_wait();
            only(Outcome::SpawnFailed(e.kind()))
        };
        let stdout = match guard.child.stdout.take() {
            Some(pipe) => read_all(pipe, MAX_STDOUT_BYTES),
            None => Err(std::io::ErrorKind::BrokenPipe.into()),
        };
        let stdout = match stdout {
            Ok(reader) => reader,
            Err(e) => {
                let outcome = thread_failed(&mut guard, e);
                drop(guard);
                return report(outcome, Stats::default(), None, Some(work));
            }
        };
        // Progress is parsed on the reader's thread and handed over through a short queue, so
        // a chatty engine never makes the calling thread fall behind its checks.
        let queue: Arc<Mutex<VecDeque<Progress>>> = Arc::default();
        let stderr = {
            let queue = queue.clone();
            let on_line = move |line: &[u8]| {
                if let Some(p) = parse_progress(line) {
                    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                    if queue.len() == PROGRESS_QUEUE {
                        queue.pop_front();
                    }
                    queue.push_back(p);
                }
            };
            match guard.child.stderr.take() {
                Some(pipe) => read_lines(pipe, MAX_STDERR_LINE_BYTES, on_line),
                None => Err(std::io::ErrorKind::BrokenPipe.into()),
            }
        };
        let stderr = match stderr {
            Ok(reader) => reader,
            Err(e) => {
                let outcome = thread_failed(&mut guard, e);
                drop(guard);
                drop(stdout);
                return report(outcome, Stats::default(), None, Some(work));
            }
        };
        let take_progress = || -> Vec<Progress> {
            let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.drain(..).collect()
        };

        let served = thread::scope(|scope| {
            // Moved in, so that an unwind (a panic in `progress`) drops it, which stops the
            // endpoint and kills and reaps the engine, before the scope joins the endpoint's
            // thread (which waits for that stop).
            let mut guard = guard;
            let server = match thread::Builder::new()
                .name("babeldoc-endpoint".into())
                .spawn_scoped(scope, || endpoint.serve(&mut translate, &stop))
            {
                Ok(server) => server,
                Err(e) => {
                    guard.kill_and_wait();
                    return Err(e.kind());
                }
            };
            let mut signalled: Option<Instant> = None;
            let mut killed = false;
            let status = loop {
                for p in take_progress() {
                    progress(p);
                }
                // On exit, what is left of the group (BabelDOC's workers, anything holding the
                // pipes) is killed while the engine's unreaped pid still holds the group id,
                // and only then is the engine reaped.
                match guard.exited() {
                    Ok(true) | Err(_) => break guard.kill_and_wait(),
                    Ok(false) => {}
                }
                if cancel.is_cancelled() {
                    stop.store(true, Ordering::SeqCst);
                    match signalled {
                        None => {
                            // SAFETY: `pid` is our child, not yet reaped, so it names no
                            // other process.
                            unsafe { libc::kill(pid, libc::SIGINT) };
                            signalled = Some(Instant::now());
                        }
                        Some(at) if !killed && at.elapsed() > job.stop_grace => {
                            kill_group(pid);
                            killed = true;
                        }
                        _ => {}
                    }
                }
                thread::sleep(POLL);
            };
            // The engine is reaped; stop the endpoint before joining it.
            drop(guard);
            Ok((status, server.join().unwrap_or_default()))
        });
        let (status, stats) = match served {
            Ok(served) => served,
            Err(kind) => {
                drop((stdout, stderr));
                return report(
                    only(Outcome::SpawnFailed(kind)),
                    Stats::default(),
                    None,
                    Some(work),
                );
            }
        };
        let ((bytes, truncated), stdout_closed) = stdout.finish(PIPE_GRACE);
        let ((), _) = stderr.finish(Duration::ZERO);
        for p in take_progress() {
            progress(p);
        }
        let exit_code = status.and_then(|s| s.code());

        if cancel.is_cancelled() {
            return report(only(Outcome::Interrupted), stats, exit_code, Some(work));
        }
        let outcome = if stdout_closed {
            classify(&bytes, truncated, exit_code, job, &work)
        } else {
            only(Outcome::InvalidResult(InvalidReason::PipesOpen))
        };
        report(outcome, stats, exit_code, Some(work))
    }

    fn kill_group(pid: libc::pid_t) {
        // SAFETY: the group was created for our unreaped child (`process_group(0)`), so its id
        // is the child's pid and names no other group.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

fn parse_progress(line: &[u8]) -> Option<Progress> {
    let value: Value = serde_json::from_slice(line).ok()?;
    if value.get("event")?.as_str()? != "progress" {
        return None;
    }
    let stage = value.get("stage").and_then(Value::as_str).unwrap_or("");
    let overall = value.get("overall")?.as_f64()?;
    if !overall.is_finite() {
        return None;
    }
    Some(Progress {
        stage: STAGES
            .iter()
            .find(|known| **known == stage)
            .copied()
            .unwrap_or(OTHER_STAGE),
        overall: overall.clamp(0.0, 100.0) as f32,
    })
}

/// Fields required in a successful engine result.
#[derive(Deserialize)]
struct Success {
    profile: String,
    source: String,
    target: String,
    input_pages: u32,
    pages: Option<u32>,
    output: PathBuf,
    dual_output: Option<Value>,
    seconds: f64,
    units: Units,
    nothing_to_translate: bool,
}

#[derive(Deserialize)]
struct Failure {
    status: String,
    leftover_folder: Option<PathBuf>,
    refusal: Option<Code>,
    error: Option<Code>,
}

#[derive(Deserialize)]
struct Code {
    code: Option<String>,
}

/// The engine's reason code when it is one of `known` (never its `detail`, which can name
/// files), else [`UNKNOWN_CODE`].
fn code(field: &Option<Code>, known: &[&'static str]) -> &'static str {
    let given = field.as_ref().and_then(|c| c.code.as_deref());
    known
        .iter()
        .find(|k| Some(**k) == given)
        .copied()
        .unwrap_or(UNKNOWN_CODE)
}

/// Reads the engine's standard output and exit code (`None`: killed by a signal). Also returns
/// the engine's leftover job folder (exit code 4 only), kept apart from the outcome.
#[cfg(unix)]
fn classify(
    stdout: &[u8],
    truncated: bool,
    exit_code: Option<i32>,
    job: &Job,
    work: &WorkDir,
) -> (Outcome, Option<PathBuf>) {
    let mut leftover = None;
    let outcome = classify_outcome(stdout, truncated, exit_code, job, work, &mut leftover);
    (outcome, leftover)
}

#[cfg(unix)]
fn classify_outcome(
    stdout: &[u8],
    truncated: bool,
    exit_code: Option<i32>,
    job: &Job,
    work: &WorkDir,
    leftover: &mut Option<PathBuf>,
) -> Outcome {
    use InvalidReason::*;
    let Some(exit) = exit_code else {
        return Outcome::Crashed { exit_code: None };
    };
    if truncated {
        return Outcome::InvalidResult(TooLong);
    }
    let Ok(text) = std::str::from_utf8(stdout) else {
        return Outcome::InvalidResult(Malformed);
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let line = match lines.as_slice() {
        [] if exit != 0 => {
            return Outcome::Crashed {
                exit_code: Some(exit),
            };
        }
        [] => return Outcome::InvalidResult(Empty),
        [line] => *line,
        _ => return Outcome::InvalidResult(SeveralLines),
    };
    // The schema version first: a result of another version is not interpreted at all.
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Outcome::InvalidResult(Malformed);
    };
    if value.get("schema_version").and_then(Value::as_u64) != Some(SCHEMA_VERSION) {
        return Outcome::InvalidResult(if value.is_object() { Schema } else { Malformed });
    }
    let (Some(json_exit), Some(status)) = (
        value.get("exit_code").and_then(Value::as_i64),
        value
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned),
    ) else {
        return Outcome::InvalidResult(Malformed);
    };
    if json_exit != i64::from(exit) {
        return Outcome::InvalidResult(Mismatch);
    }
    let expected = match exit {
        0 => None,
        2 => Some("refused"),
        3 => Some("translator_error"),
        4 => Some("babeldoc_error"),
        5 => Some("assets_missing"),
        130 => Some("interrupted"),
        _ => return Outcome::InvalidResult(UnknownExit),
    };
    if let Some(expected) = expected {
        let Ok(result) = serde_json::from_value::<Failure>(value) else {
            return Outcome::InvalidResult(Malformed);
        };
        if result.status != expected {
            return Outcome::InvalidResult(Mismatch);
        }
        return match exit {
            2 => Outcome::Refused {
                code: code(&result.refusal, REFUSAL_CODES),
            },
            3 => Outcome::TranslatorFailed {
                code: code(&result.error, TRANSLATOR_CODES),
            },
            4 => {
                *leftover = result.leftover_folder;
                Outcome::BabeldocFailed {
                    code: code(&result.error, BABELDOC_CODES),
                    leftover_folder: leftover.is_some(),
                }
            }
            5 => Outcome::AssetsMissing,
            _ => Outcome::Interrupted,
        };
    }

    let Ok(result) = serde_json::from_value::<Success>(value) else {
        return Outcome::InvalidResult(Malformed);
    };
    let units = result.units;
    let partial = match status.as_str() {
        "ok" if units.failed == 0 => false,
        "partial" if units.failed > 0 => true,
        _ => return Outcome::InvalidResult(Mismatch),
    };
    // Checked: counts near `u64::MAX` must not wrap into a match.
    let counted = units
        .translated
        .checked_add(units.failed)
        .and_then(|n| n.checked_add(units.skipped_no_letters));
    if counted != Some(units.total)
        || result.profile != job.profile.as_str()
        || result.source != job.source
        || result.target != job.target
        || !result.dual_output.is_none_or(|d| d.is_null())
        || !result.seconds.is_finite()
        || result.seconds < 0.0
    {
        return Outcome::InvalidResult(Mismatch);
    }
    if result.pages == Some(0) || result.input_pages == 0 {
        return Outcome::InvalidResult(MissingOutput);
    }
    if !is_pdf_made_here(&result.output, work) {
        return Outcome::InvalidResult(MissingOutput);
    }
    let saved = Saved {
        output: result.output,
        pages: result.pages,
        input_pages: result.input_pages,
        units,
        seconds: result.seconds,
        nothing_to_translate: result.nothing_to_translate,
    };
    if partial {
        Outcome::Partial(saved)
    } else {
        Outcome::Ok(saved)
    }
}

/// Whether `path` names a regular `.pdf` file directly in `work`'s `out` folder, and the
/// folders are still the ones this run created. The folder was new and private, so a file in
/// it was made during this run.
#[cfg(unix)]
fn is_pdf_made_here(path: &Path, work: &WorkDir) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let is_pdf = Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    // Compared as given: the engine reports `--output` joined with the name, unresolved.
    is_pdf
        && path.parent() == Some(work.out_path().as_path())
        && work.intact()
        && work.file_in_out(name)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{TempDir, fake_engine};
    use std::collections::HashSet;
    use std::ffi::OsString;

    fn units(total: u64, translated: u64, failed: u64, skipped: u64) -> Value {
        serde_json::json!({
            "total": total, "translated": translated, "failed": failed,
            "unparsed_requests": 0, "placeholder_mismatch": 0, "incomplete_answers": failed,
            "empty_answers": 0, "invalid_answers": 0, "skipped_no_letters": skipped,
            "connection_errors": 0,
        })
    }

    /// A result line with every field the engine writes, as for a success of `job()`.
    fn result_line(fields: &[(&str, Value)]) -> String {
        let mut value = serde_json::json!({
            "schema_version": 1, "status": "ok", "exit_code": 0, "profile": "paper",
            "babeldoc_version": "0.7.1", "source": "en", "target": "ja", "input": "/x/in.pdf",
            "input_pages": 2, "pages": 2, "translated_pages": null, "output": null,
            "dual_output": null, "seconds": 1.5, "units": units(3, 2, 0, 1),
            "blocked": {"downloads": 0, "connections": 0},
            "babeldoc_log": {"warning": 0, "error": 0},
            "nothing_to_translate": false, "leftover_folder": null, "refusal": null, "error": null,
        });
        for (key, field) in fields {
            value[*key] = field.clone();
        }
        value.to_string()
    }

    /// A result line without the field `key`.
    fn result_line_without(key: &str, fields: &[(&str, Value)]) -> String {
        let mut value: Value = serde_json::from_str(&result_line(fields)).unwrap();
        value.as_object_mut().unwrap().remove(key);
        value.to_string()
    }

    /// A job for the fake engine `script`, translating a dummy input; the private folder is
    /// made in `dir/work`.
    fn job(dir: &TempDir, script: &str) -> Job {
        let engine = fake_engine(dir, script);
        let input = dir.path().join("in.pdf");
        std::fs::write(&input, b"%PDF-1.4 dummy").unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let mut job = Job::new(engine, input, "en", "ja", Profile::Paper);
        job.work_parent = Some(work);
        job.stop_grace = Duration::from_secs(3);
        job
    }

    /// The names left in the job's work parent.
    fn left(job: &Job) -> HashSet<OsString> {
        std::fs::read_dir(job.work_parent.as_ref().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect()
    }

    fn run(job: &Job) -> (Report, Vec<Progress>) {
        let mut seen = Vec::new();
        let report = translate_pdf(
            job,
            |t, _| Ok(t.to_owned()),
            |p| seen.push(p),
            &Cancel::new(),
        );
        (report, seen)
    }

    /// Shell lines that print `line` with `__OUT__` replaced by the output folder.
    fn print(line: &str) -> String {
        format!("printf '%s\\n' '{line}' | sed \"s#__OUT__#$OUT#g\"\n")
    }

    /// The shell lines that write `$OUT/in.ja.pdf` and print a success for it.
    fn success(status: &str, failed: u64) -> String {
        let line = result_line(&[
            ("status", Value::from(status)),
            ("output", Value::from("__OUT__/in.ja.pdf")),
            ("units", units(3, 3 - failed, failed, 0)),
        ]);
        format!("echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0\n", print(&line))
    }

    #[test]
    fn a_success_returns_the_pdf_the_counts_and_the_progress() {
        let dir = TempDir::new();
        let script = format!(
            "echo '{{\"event\": \"progress\", \"stage\": \"Parse Page Layout\", \"overall\": 12.5}}' >&2\n\
             echo 'not json' >&2\n\
             echo '{{\"event\": \"traceback\"}}' >&2\n\
             echo '{{\"event\": \"progress\", \"stage\": \"Paper Lantern Workshop\", \"overall\": 50}}' >&2\n\
             echo '{{\"event\": \"progress\", \"stage\": \"Translate Paragraphs\", \"overall\": 250}}' >&2\n\
             {}",
            success("ok", 0)
        );
        let job = job(&dir, &script);
        let (report, seen) = run(&job);
        let Outcome::Ok(saved) = &report.outcome else {
            panic!("{:?}", report.outcome);
        };
        let work = report.work_dir.as_ref().unwrap();
        assert_eq!(saved.output, work.out_path().join("in.ja.pdf"));
        assert!(work.path().starts_with(job.work_parent.as_ref().unwrap()));
        assert_eq!(saved.pages, Some(2));
        assert_eq!(saved.units.translated, 3);
        assert_eq!(report.exit_code, Some(0));
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut report.open_output().unwrap(), &mut bytes).unwrap();
        assert_eq!(bytes, b"PDF\n");
        // A stage name that is not BabelDOC's becomes "other".
        assert_eq!(
            seen,
            vec![
                Progress {
                    stage: "Parse Page Layout",
                    overall: 12.5
                },
                Progress {
                    stage: OTHER_STAGE,
                    overall: 50.0
                },
                Progress {
                    stage: "Translate Paragraphs",
                    overall: 100.0
                },
            ]
        );
        // Dropping the report removes the private folder.
        drop(report);
        assert!(left(&job).is_empty());
    }

    /// Pass the endpoint through the environment and separate the input path from options.
    #[test]
    fn the_engine_gets_the_endpoint_in_its_environment_and_the_input_after_a_double_dash() {
        let dir = TempDir::new();
        let (args, env) = (dir.path().join("args"), dir.path().join("env"));
        let line = result_line(&[
            ("profile", "business".into()),
            ("output", "__OUT__/in.ja.pdf".into()),
        ]);
        let script = format!(
            "printf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$BABELDOC_ENGINE_ENDPOINT\" > '{}'\n\
             echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0\n",
            args.display(),
            env.display(),
            print(&line),
        );
        let mut job = job(&dir, &script);
        // A relative input that looks like an option.
        std::fs::write(dir.path().join("-x.pdf"), b"%PDF").unwrap();
        job.input = PathBuf::from("-x.pdf");
        job.pages = Some("3-4".into());
        job.profile = Profile::Business;
        let cwd = std::env::current_dir().unwrap();
        let (report, _) = run(&job);
        assert!(
            matches!(report.outcome, Outcome::Ok(_)),
            "{:?}",
            report.outcome
        );
        let args = std::fs::read_to_string(args).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args[0], "translate");
        let value = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
        let work = report.work_dir.as_ref().unwrap();
        assert_eq!(value("--output"), work.out_path().to_str().unwrap());
        assert_eq!(value("--source"), "en");
        assert_eq!(value("--target"), "ja");
        assert_eq!(value("--profile"), "business");
        assert_eq!(value("--workers"), "1");
        assert_eq!(value("--pages"), "3-4");
        assert_eq!(value("--timeout"), "600");
        // The input, made absolute, last and after `--`.
        assert_eq!(args[args.len() - 2], "--");
        assert_eq!(args[args.len() - 1], cwd.join("-x.pdf").to_str().unwrap());
        // No endpoint on the command line; the secret is in the environment only.
        assert!(!args.contains(&"--endpoint"));
        assert!(args.iter().all(|a| !a.contains("127.0.0.1")));
        let endpoint = std::fs::read_to_string(env).unwrap();
        assert!(endpoint.starts_with("http://127.0.0.1:"), "{endpoint}");
        assert!(endpoint.ends_with("/v1"));
        let secret = endpoint.rsplit('/').nth(1).unwrap();
        assert_eq!(secret.len(), 32);
        assert!(!format!("{report:?} {job:?}").contains(secret));
    }

    #[test]
    fn a_partial_result_is_partial_and_an_inconsistent_one_is_invalid() {
        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, &success("partial", 1)));
        let Outcome::Partial(saved) = report.outcome else {
            panic!("{:?}", report.outcome);
        };
        assert_eq!(saved.units.failed, 1);

        // "ok" with failures, and "partial" without, disagree with the counts.
        for (status, failed) in [("ok", 1), ("partial", 0)] {
            let dir = TempDir::new();
            let job = job(&dir, &success(status, failed));
            let (report, _) = run(&job);
            assert_eq!(
                report.outcome,
                Outcome::InvalidResult(InvalidReason::Mismatch)
            );
            assert!(report.work_dir.is_none());
            assert!(left(&job).is_empty());
        }
    }

    #[test]
    fn each_failure_comes_back_as_its_kind_with_known_codes_only() {
        let refused = result_line(&[
            ("status", "refused".into()),
            ("exit_code", 2.into()),
            (
                "refusal",
                serde_json::json!({"code": "encrypted", "detail": "secret.pdf"}),
            ),
        ]);
        let translator = result_line(&[
            ("status", "translator_error".into()),
            ("exit_code", 3.into()),
            (
                "error",
                serde_json::json!({"code": "endpoint_unreachable", "detail": "x"}),
            ),
        ]);
        let babeldoc = result_line(&[
            ("status", "babeldoc_error".into()),
            ("exit_code", 4.into()),
            ("leftover_folder", "/tmp/babeldoc-engine-x".into()),
            (
                "error",
                serde_json::json!({"code": "cleanup_failed", "detail": "x"}),
            ),
        ]);
        let assets = result_line(&[("status", "assets_missing".into()), ("exit_code", 5.into())]);
        let interrupted =
            result_line(&[("status", "interrupted".into()), ("exit_code", 130.into())]);
        let odd_code = result_line(&[
            ("status", "refused".into()),
            ("exit_code", 2.into()),
            (
                "refusal",
                serde_json::json!({"code": "Not A Code: /tmp/placeholder"}),
            ),
        ]);
        // A well-formed code the engine does not write is not passed on either,
        // nor one that belongs to another exit code.
        let unknown_code = result_line(&[
            ("status", "translator_error".into()),
            ("exit_code", 3.into()),
            ("error", serde_json::json!({"code": "attention_is_all"})),
        ]);
        let wrong_exit_code = result_line(&[
            ("status", "refused".into()),
            ("exit_code", 2.into()),
            ("refusal", serde_json::json!({"code": "translation_failed"})),
        ]);
        let cases = [
            (refused, 2, Outcome::Refused { code: "encrypted" }),
            (
                translator,
                3,
                Outcome::TranslatorFailed {
                    code: "endpoint_unreachable",
                },
            ),
            (
                babeldoc,
                4,
                Outcome::BabeldocFailed {
                    code: "cleanup_failed",
                    leftover_folder: true,
                },
            ),
            (assets, 5, Outcome::AssetsMissing),
            (interrupted, 130, Outcome::Interrupted),
            (odd_code, 2, Outcome::Refused { code: UNKNOWN_CODE }),
            (
                unknown_code,
                3,
                Outcome::TranslatorFailed { code: UNKNOWN_CODE },
            ),
            (wrong_exit_code, 2, Outcome::Refused { code: UNKNOWN_CODE }),
        ];
        for (line, exit, expected) in cases {
            let dir = TempDir::new();
            let job = job(
                &dir,
                &format!("echo PDF > \"$OUT/x.pdf\"\necho '{line}'\nexit {exit}\n"),
            );
            let (report, _) = run(&job);
            assert_eq!(report.outcome, expected);
            assert_eq!(report.exit_code, Some(exit));
            // Every failure removes the private folder and what the engine left in it.
            assert!(report.work_dir.is_none() && report.leftover.is_none());
            assert!(left(&job).is_empty());
        }
    }

    #[test]
    fn results_that_cannot_be_read_are_invalid() {
        let ok = result_line(&[]);
        let made = |fields: &[(&str, Value)]| {
            let mut fields = fields.to_vec();
            fields.push(("output", "__OUT__/in.ja.pdf".into()));
            format!(
                "echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0",
                print(&result_line(&fields))
            )
        };
        let cases = [
            // Exit 0 without a line, empty or blank.
            ("exit 0".to_owned(), InvalidReason::Empty),
            ("echo; echo '  '; exit 0".to_owned(), InvalidReason::Empty),
            (
                format!("echo '{ok}'; echo '{ok}'; exit 0"),
                InvalidReason::SeveralLines,
            ),
            (
                "echo '{\"schema_vers'; exit 0".to_owned(),
                InvalidReason::Malformed,
            ),
            ("echo '[1]'; exit 0".to_owned(), InvalidReason::Malformed),
            (
                "printf '\\377\\n'; exit 0".to_owned(),
                InvalidReason::Malformed,
            ),
            (
                format!(
                    "echo '{}'; exit 0",
                    result_line(&[("schema_version", 2.into())])
                ),
                InvalidReason::Schema,
            ),
            (
                format!(
                    "echo '{}'; exit 0",
                    result_line(&[("schema_version", "1".into())])
                ),
                InvalidReason::Schema,
            ),
            (
                format!(
                    "echo '{}'; exit 0",
                    result_line(&[("units", "many".into())])
                ),
                InvalidReason::Malformed,
            ),
            // The JSON says 0 but the process exits 3, and the other way round.
            (format!("echo '{ok}'; exit 3"), InvalidReason::Mismatch),
            (
                format!(
                    "echo '{}'; exit 0",
                    result_line(&[
                        ("status", "translator_error".into()),
                        ("exit_code", 3.into())
                    ])
                ),
                InvalidReason::Mismatch,
            ),
            // Matching codes, but a status that does not go with them.
            (
                format!(
                    "echo '{}'; exit 2",
                    result_line(&[("status", "ok".into()), ("exit_code", 2.into())])
                ),
                InvalidReason::Mismatch,
            ),
            (
                format!("echo '{}'; exit 7", result_line(&[("exit_code", 7.into())])),
                InvalidReason::UnknownExit,
            ),
            // A success without a PDF.
            (format!("echo '{ok}'; exit 0"), InvalidReason::Malformed),
            (
                format!(
                    "{}exit 0",
                    print(&result_line(&[("output", "__OUT__/missing.pdf".into())]))
                ),
                InvalidReason::MissingOutput,
            ),
            // Fields the engine always writes on success.
            (made(&[("pages", 0.into())]), InvalidReason::MissingOutput),
            (
                made(&[("input_pages", 0.into())]),
                InvalidReason::MissingOutput,
            ),
            (made(&[("units", Value::Null)]), InvalidReason::Malformed),
            (
                made(&[(
                    "units",
                    serde_json::json!({"total": 3, "translated": 3, "failed": 0}),
                )]),
                InvalidReason::Malformed,
            ),
            (made(&[("seconds", Value::Null)]), InvalidReason::Malformed),
            (
                made(&[("nothing_to_translate", Value::Null)]),
                InvalidReason::Malformed,
            ),
            (
                format!(
                    "echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0",
                    print(&result_line_without(
                        "units",
                        &[("output", "__OUT__/in.ja.pdf".into())]
                    ))
                ),
                InvalidReason::Malformed,
            ),
            // Counts, profile, languages and the bilingual output must match the job.
            (
                made(&[("units", units(5, 2, 0, 1))]),
                InvalidReason::Mismatch,
            ),
            (
                made(&[("profile", "business".into())]),
                InvalidReason::Mismatch,
            ),
            (made(&[("target", "fr".into())]), InvalidReason::Mismatch),
            (
                made(&[("dual_output", "__OUT__/in.ja.dual.pdf".into())]),
                InvalidReason::Mismatch,
            ),
        ];
        for (script, reason) in cases {
            let dir = TempDir::new();
            let job = job(&dir, &script);
            let (report, _) = run(&job);
            assert_eq!(report.outcome, Outcome::InvalidResult(reason), "{script}");
            assert!(left(&job).is_empty(), "{script}");
        }
    }

    /// A PDF that was there before the run, outside the private folder, behind a
    /// link, or in a folder swapped in under the same name is never handed over.
    #[test]
    fn only_a_pdf_made_in_this_runs_folder_is_trusted() {
        let at = |path: &str| result_line(&[("output", path.into())]);
        let dir = TempDir::new();
        let existing = dir.path().join("existing.pdf");
        std::fs::write(&existing, b"someone else's").unwrap();
        let existing = existing.display().to_string();
        let cases = [
            // An existing PDF elsewhere, and one next to the private folder.
            format!("{}exit 0", print(&at(&existing))),
            format!(
                "echo PDF > \"$OUT/../../next.pdf\"\n{}exit 0",
                print(&at("__OUT__/../../next.pdf"))
            ),
            format!(
                "echo PDF > \"$OUT/../in.ja.pdf\"\n{}exit 0",
                print(&at("__OUT__/../in.ja.pdf"))
            ),
            // A link in `out` to an existing PDF.
            format!(
                "ln -s '{existing}' \"$OUT/in.ja.pdf\"\n{}exit 0",
                print(&at("__OUT__/in.ja.pdf"))
            ),
            // A linked folder inside `out`, and `out` itself through a link.
            format!(
                "mkdir \"$OUT/../real\"; echo PDF > \"$OUT/../real/in.ja.pdf\"\n\
                 ln -s \"$OUT/../real\" \"$OUT/sub\"\n{}exit 0",
                print(&at("__OUT__/sub/in.ja.pdf"))
            ),
            // `out` swapped for a new folder, or for a link, with a PDF in it.
            format!(
                "mv \"$OUT\" \"$OUT.old\"; mkdir \"$OUT\"; echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0",
                print(&at("__OUT__/in.ja.pdf"))
            ),
            format!(
                "mv \"$OUT\" \"$OUT.old\"; mkdir \"$OUT.real\"; echo PDF > \"$OUT.real/in.ja.pdf\"\n\
                 ln -s \"$OUT.real\" \"$OUT\"\n{}exit 0",
                print(&at("__OUT__/in.ja.pdf"))
            ),
            // A folder, not a file.
            format!(
                "mkdir \"$OUT/in.ja.pdf\"\n{}exit 0",
                print(&at("__OUT__/in.ja.pdf"))
            ),
        ];
        for script in cases {
            let dir2 = TempDir::new();
            let job = job(&dir2, &script);
            let (report, _) = run(&job);
            assert_eq!(
                report.outcome,
                Outcome::InvalidResult(InvalidReason::MissingOutput),
                "{script}"
            );
            assert!(report.work_dir.is_none());
            // The private folder is gone; what the engine wrote outside it is not this
            // crate's to delete.
            let mut left = left(&job);
            left.remove(std::ffi::OsStr::new("next.pdf"));
            assert!(left.is_empty(), "{script}: {left:?}");
        }
        assert_eq!(std::fs::read(&existing).unwrap(), b"someone else's");
    }

    #[test]
    fn an_engine_that_dies_without_a_result_crashed() {
        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, "kill -9 $$\n"));
        assert_eq!(report.outcome, Outcome::Crashed { exit_code: None });

        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, "echo 'Traceback' >&2\nexit 1\n"));
        assert_eq!(report.outcome, Outcome::Crashed { exit_code: Some(1) });
    }

    #[test]
    fn a_missing_engine_does_not_start_and_leaves_nothing() {
        let dir = TempDir::new();
        let mut job = job(&dir, "exit 0");
        job.engine = dir.path().join("nowhere");
        let (report, _) = run(&job);
        assert_eq!(
            report.outcome,
            Outcome::SpawnFailed(std::io::ErrorKind::NotFound)
        );
        assert!(left(&job).is_empty());
    }

    #[test]
    fn a_missing_work_parent_is_a_work_dir_failure() {
        let dir = TempDir::new();
        let mut job = job(&dir, "exit 0");
        job.work_parent = Some(dir.path().join("nowhere"));
        let (report, _) = run(&job);
        assert_eq!(
            report.outcome,
            Outcome::WorkDirFailed(std::io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn the_engine_reaches_the_translate_function_through_the_endpoint() {
        if !crate::test_support::has_python() {
            eprintln!("skipped: no python3");
            return;
        }
        let dir = TempDir::new();
        let client = dir.path().join("client.py");
        std::fs::write(&client, crate::test_support::PYTHON_CLIENT).unwrap();
        let script = format!(
            "python3 -I '{}' \"$BABELDOC_ENGINE_ENDPOINT\" \"$OUT\"\nexit $?\n",
            client.display()
        );
        let job = job(&dir, &script);
        let mut seen = Vec::new();
        let report = translate_pdf(
            &job,
            |text, _| match text {
                "keep me" => Err(TranslateError::Unit),
                _ => Ok(format!("<{text}>")),
            },
            |p| seen.push(p),
            &Cancel::new(),
        );
        let Outcome::Partial(saved) = &report.outcome else {
            panic!("{:?}", report.outcome);
        };
        assert_eq!(saved.units.translated, 1);
        assert_eq!(saved.units.incomplete_answers, 1);
        assert_eq!(report.endpoint.probes, 1);
        assert_eq!(report.endpoint.translated, 1);
        assert_eq!(report.endpoint.unit_failures, 1);
        assert_eq!(report.endpoint.rejected, 0);
        // The client checked the answers' text itself.
        assert_eq!(seen.last().map(|p| p.stage), Some("Save PDF"));
    }

    /// Cancelling removes this run's folder and nothing else, even files that
    /// appeared next to it during the run.
    #[test]
    fn cancelling_sends_sigint_and_removes_only_the_private_folder() {
        let dir = TempDir::new();
        // Like the engine: on SIGINT, remove the PDF it staged and report the interruption.
        let line = result_line(&[("status", "interrupted".into()), ("exit_code", 130.into())]);
        let script = format!(
            "printf x > \"$OUT/.babeldoc-engine-1.part\"\n\
             echo PDF > \"$OUT/../../theirs.pdf\"\n\
             trap 'rm -f \"$OUT/.babeldoc-engine-1.part\"; echo '\\''{line}'\\''; exit 130' INT\n\
             echo '{{\"event\": \"progress\", \"stage\": \"Typesetting\", \"overall\": 1}}' >&2\n\
             while :; do sleep 0.05; done\n"
        );
        let job = job(&dir, &script);
        let cancel = Cancel::new();
        let started = Instant::now();
        let report = translate_pdf(
            &job,
            |t, _| Ok(t.to_owned()),
            |p| {
                if p.stage == "Typesetting" {
                    cancel.cancel();
                }
            },
            &cancel,
        );
        assert_eq!(report.outcome, Outcome::Interrupted);
        assert_eq!(report.exit_code, Some(130));
        assert!(started.elapsed() < job.stop_grace);
        assert_eq!(left(&job), HashSet::from([OsString::from("theirs.pdf")]));
    }

    #[test]
    fn an_engine_that_ignores_sigint_is_killed_and_its_files_are_removed() {
        let dir = TempDir::new();
        let script = "trap '' INT\n\
             echo PDF > \"$OUT/in.ja.pdf\"; printf x > \"$OUT/.babeldoc-engine-2.part\"\n\
             echo '{\"event\": \"progress\", \"stage\": \"Typesetting\", \"overall\": 1}' >&2\n\
             while :; do sleep 0.05; done\n";
        let mut job = job(&dir, script);
        std::fs::write(job.work_parent.as_ref().unwrap().join("mine.pdf"), b"keep").unwrap();
        job.stop_grace = Duration::from_millis(300);
        let cancel = Cancel::new();
        let report = translate_pdf(&job, |t, _| Ok(t.to_owned()), |_| cancel.cancel(), &cancel);
        assert_eq!(report.outcome, Outcome::Interrupted);
        assert_eq!(report.exit_code, None);
        // The file that was there before the run stays; the private folder is gone.
        assert_eq!(left(&job), HashSet::from([OsString::from("mine.pdf")]));
    }

    fn alive(pid: libc::pid_t) -> bool {
        // SAFETY: signal 0 only checks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn read_pid(path: &Path) -> libc::pid_t {
        let started = Instant::now();
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A panic in the progress function still stops the endpoint, kills the
    /// engine's group, reaps the engine and removes the private folder.
    #[test]
    fn a_panic_in_the_progress_function_kills_and_reaps_the_engine() {
        let dir = TempDir::new();
        let (pids, worker) = (dir.path().join("pid"), dir.path().join("worker"));
        let script = format!(
            "sleep 60 & echo $! > '{}'\necho $$ > '{}'\ntrap '' INT\n\
             echo '{{\"event\": \"progress\", \"stage\": \"Typesetting\", \"overall\": 1}}' >&2\n\
             while :; do sleep 0.05; done\n",
            worker.display(),
            pids.display()
        );
        let job = job(&dir, &script);
        let started = Instant::now();
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            translate_pdf(
                &job,
                |t, _| Ok(t.to_owned()),
                |_| panic!("the progress function failed"),
                &Cancel::new(),
            )
        }));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        let (pid, worker) = (read_pid(&pids), read_pid(&worker));
        // Reaped: not even a zombie is left. The worker in its group was killed with it.
        assert!(!alive(pid));
        let waiting = Instant::now();
        while alive(worker) && waiting.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(worker));
        assert!(left(&job).is_empty());
    }

    /// A translate function that is still working when the engine goes away is
    /// told so, and the run returns once it does.
    #[test]
    fn a_running_translation_is_told_to_stop_when_the_engine_exits() {
        if !crate::test_support::has_python() {
            eprintln!("skipped: no python3");
            return;
        }
        let dir = TempDir::new();
        let client = dir.path().join("client.py");
        std::fs::write(
            &client,
            "import http.client, json, os, urllib.parse\n\
             url = urllib.parse.urlparse(os.environ['BABELDOC_ENGINE_ENDPOINT'])\n\
             c = http.client.HTTPConnection('127.0.0.1', url.port, timeout=0.5)\n\
             body = json.dumps({'messages': [{'role': 'user', 'content': 'slow'}]})\n\
             headers = {'Host': url.netloc, 'Content-Type': 'application/json'}\n\
             try:\n    c.request('POST', url.path + '/chat/completions', body, headers)\n    \
             c.getresponse()\nexcept Exception:\n    pass\nraise SystemExit(1)\n",
        )
        .unwrap();
        let job = job(&dir, &format!("python3 -I '{}'\n", client.display()));
        let entered = AtomicBool::new(false);
        let started = Instant::now();
        let report = translate_pdf(
            &job,
            |_, signal| {
                entered.store(true, Ordering::SeqCst);
                while !signal.is_stopped() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(TranslateError::Fatal)
            },
            |_| {},
            &Cancel::new(),
        );
        assert!(entered.load(Ordering::SeqCst));
        assert_eq!(report.outcome, Outcome::Crashed { exit_code: Some(1) });
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A connection held open in the middle of its head (by a process outside
    /// the engine's group) does not keep the run from returning after a cancel.
    #[test]
    fn a_stalled_connection_does_not_hold_up_a_cancel() {
        if !crate::test_support::has_python() {
            eprintln!("skipped: no python3");
            return;
        }
        let dir = TempDir::new();
        let (client, holder) = (dir.path().join("client.py"), dir.path().join("holder"));
        std::fs::write(
            &client,
            format!(
                "import json, os, socket, sys, time, urllib.parse\n\
                 url = urllib.parse.urlparse(os.environ['BABELDOC_ENGINE_ENDPOINT'])\n\
                 if os.fork() == 0:\n    os.setsid()\n    null = os.open(os.devnull, os.O_RDWR)\n    \
                 for fd in (0, 1, 2):\n        os.dup2(null, fd)\n    \
                 open('{}', 'w').write(str(os.getpid()))\n    \
                 s = socket.create_connection(('127.0.0.1', url.port))\n    \
                 s.sendall(('POST ' + url.path + '/chat/completions HTTP/1.1\\r\\n').encode())\n    \
                 time.sleep(8)\n    os._exit(0)\n\
                 time.sleep(0.5)\n\
                 sys.stderr.write(json.dumps({{'event': 'progress', 'stage': 'Typesetting', 'overall': 1}}) + '\\n')\n\
                 sys.stderr.flush()\n\
                 while True:\n    time.sleep(0.05)\n",
                holder.display()
            ),
        )
        .unwrap();
        let job = job(&dir, &format!("exec python3 -I '{}'\n", client.display()));
        let cancel = Cancel::new();
        let started = Instant::now();
        let report = translate_pdf(&job, |t, _| Ok(t.to_owned()), |_| cancel.cancel(), &cancel);
        let elapsed = started.elapsed();
        let holder = read_pid(&holder);
        // SAFETY: the test's own grandchild.
        unsafe { libc::kill(holder, libc::SIGKILL) };
        assert_eq!(report.outcome, Outcome::Interrupted);
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
        assert_eq!(report.endpoint.dropped, 1);
    }

    /// An engine that floods standard error with progress does not keep the
    /// calling thread from seeing the cancel.
    #[test]
    fn a_flood_of_progress_does_not_hide_a_cancel() {
        let dir = TempDir::new();
        let line = result_line(&[("status", "interrupted".into()), ("exit_code", 130.into())]);
        let script = format!(
            "yes '{{\"event\": \"progress\", \"stage\": \"Typesetting\", \"overall\": 5}}' >&2 &\n\
             trap 'kill $!; echo '\\''{line}'\\''; exit 130' INT\n\
             while :; do sleep 0.05; done\n"
        );
        let job = job(&dir, &script);
        let cancel = Cancel::new();
        let mut calls = 0usize;
        let started = Instant::now();
        let report = translate_pdf(
            &job,
            |t, _| Ok(t.to_owned()),
            |_| {
                calls += 1;
                if calls == 1 {
                    cancel.cancel();
                }
            },
            &cancel,
        );
        assert_eq!(report.outcome, Outcome::Interrupted);
        // Before the fix the calling thread never left its loop over the lines. Under load
        // the shell may miss the grace period and be killed instead, which is also fine.
        assert!(
            started.elapsed() < job.stop_grace + Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// A grandchild that keeps the pipes open costs the grace period, not the
    /// whole of its life, and no reader is left behind. The grandchild leaves the engine's
    /// process group so it survives group cleanup and keeps the pipes open.
    #[test]
    fn a_grandchild_holding_the_pipes_does_not_hold_up_the_run() {
        if !crate::test_support::has_python() {
            eprintln!("skipped: no python3");
            return;
        }
        let dir = TempDir::new();
        let holder = dir.path().join("holder");
        let script = format!(
            "python3 -I -c 'import os, time; os.setsid(); open(\"{0}.tmp\", \"w\").write(str(os.getpid())); os.rename(\"{0}.tmp\", \"{0}\"); time.sleep(30)' &\n\
             while [ ! -f '{0}' ]; do sleep 0.01; done\n{1}",
            holder.display(),
            success("ok", 0)
        );
        let job = job(&dir, &script);
        let started = Instant::now();
        let (report, _) = run(&job);
        let holder = read_pid(&holder);
        // SAFETY: the test's own grandchild.
        unsafe { libc::kill(holder, libc::SIGKILL) };
        assert_eq!(
            report.outcome,
            Outcome::InvalidResult(InvalidReason::PipesOpen)
        );
        // Before, the reader threads stayed blocked for the grandchild's whole life.
        assert!(started.elapsed() < PIPE_GRACE + Duration::from_secs(8));
        assert!(left(&job).is_empty());
    }

    /// When the engine exits first (on its own, or on the cancel's SIGINT) while
    /// a process of its group lives on and holds the pipes, that process is killed before the
    /// engine is reaped, and the run does not wait for it.
    #[test]
    fn what_is_left_of_the_group_when_the_engine_exits_is_killed() {
        // A normal exit.
        let dir = TempDir::new();
        let worker = dir.path().join("worker");
        let script = format!(
            "sleep 60 & echo $! > '{}'\n{}",
            worker.display(),
            success("ok", 0)
        );
        let first = job(&dir, &script);
        let started = Instant::now();
        let (report, _) = run(&first);
        assert!(
            matches!(report.outcome, Outcome::Ok(_)),
            "{:?}",
            report.outcome
        );
        assert!(started.elapsed() < PIPE_GRACE, "{:?}", started.elapsed());
        let worker = read_pid(&worker);
        let waiting = Instant::now();
        while alive(worker) && waiting.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(worker), "the group's worker outlived the run");
        drop(report);

        // A cancel: the engine leaves at once on SIGINT, its worker does not.
        let dir = TempDir::new();
        let worker = dir.path().join("worker");
        let line = result_line(&[("status", "interrupted".into()), ("exit_code", 130.into())]);
        let script = format!(
            "trap '' INT; sleep 60 & echo $! > '{}'\ntrap 'echo '\\''{line}'\\''; exit 130' INT\n\
             echo '{{\"event\": \"progress\", \"stage\": \"Typesetting\", \"overall\": 1}}' >&2\n\
             while :; do sleep 0.05; done\n",
            worker.display()
        );
        let job = job(&dir, &script);
        let cancel = Cancel::new();
        let started = Instant::now();
        let report = translate_pdf(&job, |t, _| Ok(t.to_owned()), |_| cancel.cancel(), &cancel);
        assert_eq!(report.outcome, Outcome::Interrupted);
        assert!(
            started.elapsed() < job.stop_grace,
            "{:?}",
            started.elapsed()
        );
        let worker = read_pid(&worker);
        let waiting = Instant::now();
        while alive(worker) && waiting.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(worker), "the group's worker outlived the cancel");
        assert!(left(&job).is_empty());
    }

    /// The engine makes its job folder (with its copy of the input) where the
    /// crate tells it, inside the private folder, so even a SIGKILL leaves no copy behind.
    #[test]
    fn the_engines_job_folder_goes_with_the_private_folder_after_a_kill() {
        let dir = TempDir::new();
        let seen = dir.path().join("job-parent");
        let script = format!(
            "printf '%s' \"$BABELDOC_ENGINE_JOB_PARENT\" > '{}'\n\
             mkdir \"$BABELDOC_ENGINE_JOB_PARENT/babeldoc-engine-1\"\n\
             cp \"$3\" \"$BABELDOC_ENGINE_JOB_PARENT/babeldoc-engine-1/input.pdf\" 2>/dev/null || \
             echo PDF > \"$BABELDOC_ENGINE_JOB_PARENT/babeldoc-engine-1/input.pdf\"\n\
             kill -9 $$\n",
            seen.display()
        );
        let job = job(&dir, &script);
        let (report, _) = run(&job);
        assert_eq!(report.outcome, Outcome::Crashed { exit_code: None });
        let parent = PathBuf::from(std::fs::read_to_string(&seen).unwrap());
        let work = job.work_parent.as_ref().unwrap();
        assert!(parent.starts_with(work), "{parent:?}");
        assert_eq!(parent.file_name(), Some(std::ffi::OsStr::new("engine")));
        assert!(left(&job).is_empty());
        assert!(report.leftover.is_none());
    }

    /// A successful result can have no page count if the saved PDF could not be read back.
    /// The PDF is still handed over, and `input_pages` is still required.
    #[test]
    fn a_success_with_null_pages_is_kept() {
        let script = |fields: &[(&str, Value)]| {
            let mut fields = fields.to_vec();
            fields.push(("output", "__OUT__/in.ja.pdf".into()));
            format!(
                "echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0",
                print(&result_line(&fields))
            )
        };
        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, &script(&[("pages", Value::Null)])));
        let Outcome::Ok(saved) = &report.outcome else {
            panic!("{:?}", report.outcome);
        };
        assert_eq!(saved.pages, None);
        assert_eq!(saved.input_pages, 2);
        assert!(report.open_output().is_ok());

        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, &script(&[("input_pages", Value::Null)])));
        assert_eq!(
            report.outcome,
            Outcome::InvalidResult(InvalidReason::Malformed)
        );
    }

    /// The engine's leftover folder is free-form text; it stays out of `Debug`
    /// and is read through its own accessor only.
    #[test]
    fn the_engines_leftover_folder_is_kept_out_of_debug() {
        let line = result_line(&[
            ("status", "babeldoc_error".into()),
            ("exit_code", 4.into()),
            ("leftover_folder", "/private/Paper Lantern Workshop".into()),
            ("error", serde_json::json!({"code": "cleanup_failed"})),
        ]);
        let dir = TempDir::new();
        let (report, _) = run(&job(&dir, &format!("echo '{line}'\nexit 4\n")));
        assert_eq!(
            report.outcome,
            Outcome::BabeldocFailed {
                code: "cleanup_failed",
                leftover_folder: true
            }
        );
        assert_eq!(
            report.engine_leftover_folder(),
            Some(Path::new("/private/Paper Lantern Workshop"))
        );
        assert!(!format!("{report:?}").contains("Paper Lantern Workshop"));

        let (report, _) = run(&job(&dir, &success("ok", 0)));
        assert_eq!(report.engine_leftover_folder(), None);
    }

    /// Counts that would wrap around are a mismatch, not a match.
    #[test]
    fn unit_counts_that_overflow_are_a_mismatch() {
        let mut huge = units(0, 0, 0, 0);
        huge["translated"] = Value::from(u64::MAX);
        huge["skipped_no_letters"] = Value::from(1u64);
        // translated + skipped wraps to 0 == total.
        let line = result_line(&[("units", huge), ("output", "__OUT__/in.ja.pdf".into())]);
        let dir = TempDir::new();
        let script = format!("echo PDF > \"$OUT/in.ja.pdf\"\n{}exit 0", print(&line));
        let (report, _) = run(&job(&dir, &script));
        assert_eq!(
            report.outcome,
            Outcome::InvalidResult(InvalidReason::Mismatch)
        );
    }

    /// A work parent that others can write to is refused before anything is made
    /// in it.
    #[test]
    fn a_shared_work_parent_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let job = job(&dir, "exit 0");
        let parent = job.work_parent.clone().unwrap();
        for mode in [0o777, 0o770, 0o1777] {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
            let (report, _) = run(&job);
            assert_eq!(report.outcome, Outcome::WorkParentNotPrivate, "{mode:o}");
            assert!(left(&job).is_empty());
        }
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (report, _) = run(&job);
        assert_ne!(report.outcome, Outcome::WorkParentNotPrivate);
    }

    #[test]
    fn progress_lines_are_read_strictly() {
        assert_eq!(parse_progress(b"{\"event\": \"progress\"}"), None);
        assert_eq!(
            parse_progress(b"{\"event\": \"progress\", \"overall\": \"5\"}"),
            None
        );
        assert_eq!(
            parse_progress(b"{\"event\": \"progress\", \"stage\": null, \"overall\": -3}"),
            Some(Progress {
                stage: OTHER_STAGE,
                overall: 0.0
            })
        );
        for stage in STAGES {
            let line = serde_json::json!({"event": "progress", "stage": stage, "overall": 1});
            assert_eq!(
                parse_progress(line.to_string().as_bytes()).map(|p| p.stage),
                Some(stage)
            );
        }
        assert_eq!(parse_progress(b"[]"), None);
    }
}
