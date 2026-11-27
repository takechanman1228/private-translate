//! Lets the external `babeldoc-engine` command translate a PDF with the app's own model.
//!
//! The separately installed engine asks an
//! OpenAI-compatible server for each paragraph. The app runs llama.cpp in-process and has no such
//! server, so this crate opens one on `127.0.0.1` for the length of a run ([`endpoint`]), starts
//! the engine as a separate process pointed at it and reads its result ([`run`]), and checks
//! whether an engine is usable at all ([`locate`]). It never links BabelDOC.
//!
//! The library does not depend on the `engine` crate: the caller passes the function that
//! translates one paragraph with its loaded model.
//!
//! Each run works in a private folder this crate creates and removes; it never lists or deletes
//! anything in a folder of the caller's. On success the caller copies the PDF out of the
//! [`run::Report`] with its own save (see [`run`]).
//!
//! Nothing here logs or prints. Source and translated text pass through memory only; reports
//! carry counts, sizes, times and fixed codes. The endpoint's secret URL reaches the engine
//! through its environment, never its command line.
//!
//! Windows is not supported yet: the crate builds there, and [`run::translate_pdf`] and
//! [`locate::check`] return "unsupported".

pub mod endpoint;
pub mod locate;
#[cfg(unix)]
mod process;
pub mod run;
#[cfg(unix)]
mod workdir;

#[cfg(all(test, unix))]
mod test_support;

pub use endpoint::{Endpoint, StopSignal, TranslateError};
pub use locate::{Availability, check};
pub use run::{Cancel, Job, Outcome, Profile, Progress, Report, translate_pdf};
