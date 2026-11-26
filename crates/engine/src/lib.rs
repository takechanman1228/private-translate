//! Translation engine for Private Translate.
//!
//! The engine is GUI-agnostic. [`llama`] runs translation GGUF models in-process with llama.cpp.

pub mod discovery;
pub mod document;
pub mod download;
mod hy_mt2;
mod index_translate;
pub use index_translate::PROMPT_STRINGS as INDEX_PROMPT_STRINGS;
pub mod llama;
pub mod models;
pub mod profiles;
pub mod repetition;
pub mod selection;
pub mod units;
mod utf8;
pub mod worker;

#[cfg(test)]
fn test_dir(name: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    path
}

use std::time::Duration;

use thiserror::Error;

pub use pfml;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Prompt(#[from] pfml::PromptError),
    #[error("llama.cpp: {0}")]
    Llama(#[from] llama_cpp_2::LlamaCppError),
    #[error("failed to load model: {0}")]
    ModelLoad(#[from] llama_cpp_2::LlamaModelLoadError),
    #[error("failed to create context: {0}")]
    ContextLoad(#[from] llama_cpp_2::LlamaContextLoadError),
    #[error("decode failed: {0}")]
    Decode(#[from] llama_cpp_2::DecodeError),
    #[error("batch error: {0}")]
    Batch(#[from] llama_cpp_2::llama_batch::BatchAddError),
    #[error("not a PLaMo 3 Translate model: special token {0} is missing")]
    MissingSpecialToken(&'static str),
    #[error("the selected model is missing the expected control token {0}")]
    MissingProfileToken(&'static str),
    #[error("the selected model requires a supported explicit language pair ({0:?})")]
    UnsupportedDirection(profiles::ModelProfile),
    #[error("the selected model does not support a style override")]
    UnsupportedStyle,
    #[error("the model emitted a reasoning block in translation mode")]
    UnexpectedThinking,
    #[error("the model repeated translation prompt instructions")]
    PromptLeak,
    #[error("no compute backend found: the ggml backend modules must be next to the executable")]
    NoBackends,
    #[error("prompt is too long: {prompt_tokens} tokens for a context of {n_ctx}")]
    PromptTooLong { prompt_tokens: usize, n_ctx: usize },
}

pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// User agent for requests to Hugging Face.
pub(crate) const USER_AGENT: &str = concat!("private-translate/", env!("CARGO_PKG_VERSION"));

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished the translation.
    Finished,
    /// The caller cancelled the request.
    Cancelled,
    /// The context window filled up before the translation finished.
    ContextFull,
    /// The output fell into a loop (see [`repetition`]). `kept` is the length in bytes of the
    /// output worth keeping: up to the end of the cycle's first occurrence, or, for a cycle
    /// copied from the input, as many repeats as the input has.
    Repetition { kept: usize },
    /// The output reached [`Generation::max_tokens`].
    LengthLimit,
    /// Document translation only: the model ended a chunk with an output far shorter than its
    /// source, having skipped the rest of it.
    Incomplete,
}

impl StopReason {
    /// Whether the output went wrong: it looped, grew far longer than the input, or ended far
    /// too early.
    pub fn is_runaway(self) -> bool {
        matches!(
            self,
            Self::Repetition { .. } | Self::LengthLimit | Self::Incomplete
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranslateStats {
    pub prompt_tokens: usize,
    pub generated_tokens: usize,
    pub prompt_duration: Duration,
    pub generation_duration: Duration,
    pub stop_reason: StopReason,
    /// Document translation: retries after rejection or runaway output.
    pub retries: usize,
    /// Document translation: pieces whose output still ran away after retrying and was cut.
    pub cut: usize,
    /// Hy-MT2 candidates rejected because they echoed fixed prompt text.
    pub prompt_leaks: usize,
}

impl TranslateStats {
    /// The stats of a job cancelled before it started.
    pub fn cancelled() -> Self {
        Self {
            prompt_tokens: 0,
            generated_tokens: 0,
            prompt_duration: Duration::ZERO,
            generation_duration: Duration::ZERO,
            stop_reason: StopReason::Cancelled,
            retries: 0,
            cut: 0,
            prompt_leaks: 0,
        }
    }

    pub fn generation_tokens_per_second(&self) -> f64 {
        let secs = self.generation_duration.as_secs_f64();
        if secs > 0.0 {
            self.generated_tokens as f64 / secs
        } else {
            0.0
        }
    }
}

/// Settings for generating one translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation {
    /// Stop with [`StopReason::LengthLimit`] after this many tokens.
    pub max_tokens: usize,
    /// Apply a stronger model-specific repetition penalty. This can also discourage legitimate
    /// repetition, so the document translator uses it only on the last retry.
    pub penalize_repetition: bool,
    /// Request Hy-MT2 anchor preservation for a tagged Office unit.
    pub preserve_office_anchors: bool,
}

impl Generation {
    /// Standard decoding for the loaded profile. PLaMo uses greedy sampling.
    pub fn greedy(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            penalize_repetition: false,
            preserve_office_anchors: false,
        }
    }
}

/// Something that can translate a single request, streaming the output.
pub trait TranslationBackend {
    fn profile(&self) -> profiles::ModelProfile {
        profiles::ModelProfile::PlamoPfml
    }

    /// The Hy-MT2 control markers inserted before and after the user turn.
    fn hy_mt2_prompt_frame(&self) -> &[&str] {
        &[]
    }

    /// Whether the loaded model uses the Hy-MT2 1.8B chat template.
    fn is_hy_mt2_1_8b(&self) -> bool {
        false
    }

    /// Translates `prompt`, passing decoded text to `sink` as it is generated.
    ///
    /// Generation stops early with [`StopReason::Repetition`] when the output loops.
    /// `is_cancelled` is polled before every generated token.
    fn translate(
        &mut self,
        prompt: &pfml::Prompt<'_>,
        generation: &Generation,
        sink: &mut dyn FnMut(&str),
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<TranslateStats>;
}
