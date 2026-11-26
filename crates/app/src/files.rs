//! File UI is a child of `view`, sharing its worker without a second model instance.
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

use ::files::markdown::Markdown;
use ::files::office::{MAX_UNIT_TOKENS, OfficeDocument, OfficeTarget, TagFormat};
use ::files::pdf::Pdf;
use engine::profiles::FileFormat;
use engine::units::{AnchorFallback, Unit, UnitRequest, UnitResult, UnitTuning};

use gpui_kit::component::{Icon, button::ButtonCustomVariant, progress::Progress};

use super::*;
#[path = "files/batch.rs"]
mod batch;
#[path = "files/batch_panel.rs"]
mod batch_panel;
pub(super) use batch_panel::BatchPanel;
#[path = "files/external_pdf.rs"]
mod external_pdf;
#[path = "files/notice.rs"]
mod notice;
#[path = "files/powerpoint_review.rs"]
mod powerpoint_review;
use external_pdf::PdfOutput;
pub(super) use external_pdf::{
    EngineState as ExternalEngine, PendingClose, settings_row as external_engine_row,
};
use notice::{FileNotice, LayoutSummary, Operation};

const OFFICE_TAGS: TagFormat = TagFormat::Anchor;
pub(super) const FILE_PANEL_WIDTH: f32 = 780.;

pub(super) fn file_action(button: Button, disabled: bool, cx: &App) -> Button {
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .color(rgb(ACTION_ACCENT).into())
                .foreground(white())
                .hover(rgb(0x254dc8).into())
                .active(rgb(0x1c3fae).into()),
        )
        .disabled(disabled)
        .when(!disabled, |button| button.bg(rgb(ACTION_ACCENT)))
}

pub(super) fn format_label(format: FileFormat) -> &'static str {
    match format {
        FileFormat::Txt => "TXT",
        FileFormat::Markdown => "Markdown",
        FileFormat::Word => "Word",
        FileFormat::PowerPoint => "PowerPoint",
        FileFormat::Pdf => "PDF",
    }
}

fn file_accent(cx: &App) -> Hsla {
    rgb(if cx.theme().is_dark() {
        0x9bb5ff
    } else {
        ACTION_ACCENT
    })
    .into()
}

#[derive(Clone)]
enum FileDocument {
    Text(Arc<::files::Document>),
    Markdown(Arc<Markdown>),
    Pdf(Arc<Pdf>),
    PdfLayout(Arc<::files::pdf::export::Document>),
    /// The third output: read like [`Self::Pdf`] (the same checks and notices); BabelDOC lays
    /// out the translation (see `external_pdf`).
    PdfExternal(Arc<Pdf>),
    Office {
        deck: Arc<OfficeDocument>,
        format: FileFormat,
        japanese: bool,
        units: usize,
    },
}

impl FileDocument {
    fn format(&self) -> FileFormat {
        match self {
            Self::Text(_) => FileFormat::Txt,
            Self::Markdown(_) => FileFormat::Markdown,
            Self::Office { format, .. } => *format,
            Self::Pdf(_) | Self::PdfLayout(_) | Self::PdfExternal(_) => FileFormat::Pdf,
        }
    }

    /// The PDF output this document was read for (`None`: not a PDF).
    fn pdf_output(&self) -> Option<PdfOutput> {
        match self {
            Self::Pdf(_) => Some(PdfOutput::Bilingual),
            Self::PdfLayout(_) => Some(PdfOutput::Layout),
            Self::PdfExternal(_) => Some(PdfOutput::Papers),
            _ => None,
        }
    }

    fn read(path: &Path, pdf: PdfOutput) -> Result<Self, ::files::FileError> {
        if path.is_dir() {
            return Err(::files::FileError::Folder);
        }
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            return match pdf {
                PdfOutput::Layout => Ok(Self::PdfLayout(Arc::new(
                    ::files::pdf::export::Document::read(path)?,
                ))),
                PdfOutput::Bilingual => Ok(Self::Pdf(Arc::new(Pdf::read(path)?))),
                PdfOutput::Papers => Ok(Self::PdfExternal(Arc::new(Pdf::read(path)?))),
            };
        }
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            return Ok(Self::Markdown(Arc::new(Markdown::read(path)?)));
        }
        if let Some(format @ (FileFormat::Word | FileFormat::PowerPoint)) = path_format(path) {
            let deck = Arc::new(OfficeDocument::read_for_translation(path)?);
            let paragraphs = || deck.parts().iter().flat_map(|part| &part.paragraphs);
            let sample = paragraphs()
                .map(|p| p.plain_text())
                .scan(0, |size, text| {
                    if *size >= 16_384 {
                        return None;
                    }
                    *size += text.len();
                    Some(text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let japanese = lang::looks_japanese(&sample);
            let units = paragraphs().count();
            Ok(Self::Office {
                deck,
                format,
                japanese,
                units,
            })
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
        {
            Ok(Self::Text(Arc::new(::files::read_document(path)?)))
        } else {
            Err(::files::FileError::Unsupported)
        }
    }

    fn empty_notice(&self) -> Option<FileNotice> {
        self.empty().then(|| {
            FileNotice::text(if self.format() == FileFormat::Pdf {
                "files.pdf_empty"
            } else {
                "files.empty"
            })
        })
    }

    fn request(&self, settings: &RequestSettings, source_hint: FileSource) -> Request {
        let text = match self {
            Self::Text(doc) => doc.source().to_owned(),
            _ => String::new(),
        };
        settings.request(text, source_hint.is_japanese(settings.source.code()))
    }

    fn encoding(&self) -> Option<&'static str> {
        match self {
            FileDocument::Text(doc) => doc.converted_encoding,
            FileDocument::Markdown(doc) => doc.converted_encoding(),
            _ => None,
        }
    }

    fn source_hint(&self) -> FileSource {
        let detected = match self {
            Self::Text(doc) => detect_text_document_source(doc.source()),
            Self::Markdown(doc) => {
                detect_document_source(doc.segments().iter().map(|segment| segment.text.as_str()))
            }
            Self::Pdf(doc) | Self::PdfExternal(doc) => detect_pdf_source(doc.units()),
            Self::PdfLayout(doc) => detect_pdf_source(doc.units()),
            Self::Office { deck, .. } => detect_document_source(
                deck.parts()
                    .iter()
                    .flat_map(|part| &part.paragraphs)
                    .map(|paragraph| paragraph.plain_text()),
            ),
        };
        FileSource {
            detected,
            japanese: self.japanese(),
        }
    }

    fn japanese(&self) -> bool {
        match self {
            Self::Text(doc) => lang::looks_japanese(doc.source()),
            Self::Pdf(doc) | Self::PdfExternal(doc) => pdf_japanese(doc.units()),
            Self::PdfLayout(doc) => pdf_japanese(doc.units()),
            Self::Markdown(doc) => {
                let sample = doc
                    .segments()
                    .iter()
                    .take(100)
                    .map(|s| &s.text[..s.text.floor_char_boundary(512)])
                    .collect::<Vec<_>>()
                    .join("\n");
                lang::looks_japanese(&sample)
            }
            Self::Office { japanese, .. } => *japanese,
        }
    }

    fn empty(&self) -> bool {
        match self {
            Self::Text(doc) => doc.source().trim().is_empty(),
            Self::Markdown(doc) => doc.segments().is_empty(),
            Self::Pdf(doc) | Self::PdfExternal(doc) => doc.units().is_empty(),
            Self::PdfLayout(doc) => doc.units().is_empty(),
            Self::Office { units, .. } => *units == 0,
        }
    }
}

/// The format a path is read as (see [`FileDocument::read`]), from its extension alone.
fn path_format(path: &Path) -> Option<FileFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "txt" => Some(FileFormat::Txt),
        "md" => Some(FileFormat::Markdown),
        "docx" => Some(FileFormat::Word),
        "pptx" => Some(FileFormat::PowerPoint),
        "pdf" => Some(FileFormat::Pdf),
        _ => None,
    }
}

fn detect_pdf_source(units: &[::files::pdf::Unit]) -> Option<&'static str> {
    detect_document_source(units.iter().map(|unit| unit.text.as_str()))
}

const DOCUMENT_DETECTION_BYTES: usize = 32 * 1024;

fn detect_text_document_source(source: &str) -> Option<&'static str> {
    let sample =
        source[..source.floor_char_boundary(DOCUMENT_DETECTION_BYTES)].replace("\r\n", "\n");
    detect_document_source(sample.split("\n\n"))
}

fn detect_document_source(
    samples: impl IntoIterator<Item = impl AsRef<str>>,
) -> Option<&'static str> {
    let mut counts = std::collections::HashMap::new();
    let mut total = 0;
    let mut short_run = String::new();
    let mut vote = |sample: &str| {
        let weight = sample.chars().filter(|ch| ch.is_alphabetic()).count();
        total += weight;
        if let Some(code) = lang::detect_source(sample) {
            *counts.entry(code).or_insert(0) += weight;
        }
    };
    let mut remaining = DOCUMENT_DETECTION_BYTES;
    for sample in samples {
        let sample = sample.as_ref();
        let mut sample = &sample[..sample.floor_char_boundary(remaining)];
        remaining = remaining.saturating_sub(sample.len() + 1);
        while sample.len() > lang::DETECTION_BYTES {
            vote(&short_run);
            short_run.clear();
            let (chunk, rest) = sample.split_at(sample.floor_char_boundary(lang::DETECTION_BYTES));
            vote(chunk);
            sample = rest;
        }
        if short_run.len() + sample.len() >= lang::DETECTION_BYTES
            || sample
                .chars()
                .filter(|ch| ch.is_alphabetic())
                .take(24)
                .count()
                >= 24
        {
            vote(&short_run);
            short_run.clear();
            vote(sample);
        } else {
            short_run.push_str(sample);
            short_run.push('\n');
            if short_run.len() >= 512 {
                vote(&short_run);
                short_run.clear();
            }
        }
        if remaining < 4 {
            break;
        }
    }
    vote(&short_run);
    counts
        .into_iter()
        .find_map(|(code, weight)| (weight > total / 2).then_some(code))
}

fn pdf_japanese(units: &[::files::pdf::Unit]) -> bool {
    let sample = units
        .iter()
        .take(100)
        .map(|s| &s.text[..s.text.floor_char_boundary(512)])
        .collect::<Vec<_>>()
        .join("\n");
    lang::looks_japanese(&sample)
}

#[derive(Clone)]
enum FileOutput {
    Text(String),
    Markdown(Arc<Vec<u8>>),
    Pdf(Arc<Vec<Option<String>>>),
    PdfText(String),
    /// A finished PDF (BabelDOC's), already read.
    PdfBytes(Arc<Vec<u8>>),
    Office(Vec<Vec<Option<OfficeTarget>>>),
}

struct CompletedFile {
    output: FileOutput,
    review: FileReview,
    has_changes: bool,
}

#[derive(Default, Clone)]
struct FileReview {
    partial_translation: bool,
    /// Office paragraphs translated using the base run and the original non-text atoms.
    simplified_paragraphs: usize,
    kept_as_source: usize,
    markdown_kept_lines: Option<usize>,
    show_document_counts: bool,
    update_toc: bool,
    check_slides: bool,
    word_overflow: usize,
    check_pdf: bool,
    pdf_text: bool,
    /// Paragraphs left in the source language, when counted (BabelDOC's output).
    untranslated: usize,
    powerpoint_frames: Vec<::files::office::FrameReview>,
}

impl FileReview {
    fn has_file_counts(&self) -> bool {
        self.show_document_counts || self.markdown_kept_lines.is_some()
    }

    fn document_counts(&self) -> Option<std::borrow::Cow<'static, str>> {
        if let Some(count) = self.markdown_kept_lines {
            return Some(t!("files.markdown_review_count", count = count));
        }
        self.show_document_counts.then(|| {
            t!(
                "files.document_review_counts",
                kept = self.kept_as_source,
                simplified = self.simplified_paragraphs
            )
        })
    }

    fn word_warning(&self) -> Option<std::borrow::Cow<'static, str>> {
        (self.word_overflow > 0).then(|| t!("files.word_overflow", count = self.word_overflow))
    }

    fn guidance(&self) -> Vec<String> {
        [
            Some(if self.partial_translation {
                t!("files.partial_translation")
            } else {
                t!("files.machine_translation")
            }),
            self.word_warning(),
            (self.untranslated > 0).then(|| {
                t!(
                    "files.pdf_untranslated",
                    count = self.untranslated,
                    unit = if self.untranslated == 1 {
                        "paragraph"
                    } else {
                        "paragraphs"
                    }
                )
            }),
            self.document_counts(),
            self.update_toc.then(|| t!("files.update_toc")),
            self.check_slides.then(|| t!("files.check_slides")),
            self.pdf_text.then(|| t!("files.pdf_text_output")),
            self.check_pdf.then(|| t!("files.check_pdf")),
        ]
        .into_iter()
        .flatten()
        .map(|line| line.into_owned())
        .collect()
    }
}

fn pdf_text_output(
    units: &[::files::pdf::Unit],
    targets: &[Option<String>],
) -> Option<CompletedFile> {
    if units.len() != targets.len() {
        return None;
    }
    let mut kept_as_source = 0;
    let mut changed = false;
    let lines = units
        .iter()
        .zip(targets)
        .map(|(unit, target)| {
            if let Some(t) = target.as_ref().filter(|t| !t.trim().is_empty()) {
                changed |= t.trim() != unit.text.trim();
                t.as_str()
            } else {
                kept_as_source += 1;
                unit.text.as_str()
            }
        })
        .collect::<Vec<_>>();
    Some(CompletedFile {
        output: FileOutput::PdfText(lines.join("\n\n")),
        has_changes: changed,
        review: FileReview {
            pdf_text: true,
            partial_translation: kept_as_source > 0,
            kept_as_source,
            ..Default::default()
        },
    })
}

struct UnitTranslation {
    results: Vec<Option<UnitResult>>,
    completed: usize,
}

impl UnitTranslation {
    fn new(count: usize) -> Self {
        Self {
            results: vec![None; count],
            completed: 0,
        }
    }
    fn text_targets(self) -> Option<Vec<Option<String>>> {
        (self.completed == self.results.len()).then(|| {
            self.results
                .into_iter()
                .map(|result| match result {
                    Some(UnitResult::Complete { text, .. }) => Some(text),
                    _ => None,
                })
                .collect()
        })
    }
}

fn finish_office(
    deck: &OfficeDocument,
    job: UnitTranslation,
    target: &str,
) -> Option<CompletedFile> {
    use ::files::office::RetentionReason;

    if job.completed != job.results.len() {
        return None;
    }
    let mut review = FileReview::default();
    for item in &deck.unsupported {
        match item.reason {
            RetentionReason::TableOfContents => review.update_toc = true,
            RetentionReason::Range
            | RetentionReason::ProtectedStructure
            | RetentionReason::InlineStructure => {
                review.partial_translation |= !item.source.trim().is_empty();
            }
            RetentionReason::UnsupportedPart => {
                review.partial_translation |=
                    item.part.starts_with("ppt/charts/") || item.part.starts_with("ppt/diagrams/");
            }
            RetentionReason::ReadingGuide => {}
        }
    }

    let mut has_changes = false;
    let mut results = job.results.into_iter();
    let mut parts = Vec::new();
    for part in deck.parts() {
        let targets = part
            .paragraphs
            .iter()
            .map(|paragraph| {
                let outcome = results.next().flatten()?;
                let simplified = matches!(&outcome, UnitResult::Simplified { .. });
                let translation = match outcome {
                    UnitResult::Complete { text, .. }
                        if paragraph.amounts_match(OFFICE_TAGS, &text, target) =>
                    {
                        Some(OfficeTarget::Tagged(text))
                    }
                    UnitResult::Simplified { text } => {
                        paragraph.simplified_target(OFFICE_TAGS, text, target)
                    }
                    _ => None,
                };
                if let Some(translation) = &translation {
                    has_changes |= paragraph.target_changes_text(OFFICE_TAGS, translation, target);
                    review.simplified_paragraphs += usize::from(simplified);
                } else {
                    review.kept_as_source += 1;
                    review.partial_translation = true;
                }
                translation
            })
            .collect::<Vec<_>>();
        parts.push(targets);
    }
    review.word_overflow = deck.word_overflow_reviews(OFFICE_TAGS, &parts).len();
    review.powerpoint_frames = deck.powerpoint_layout_reviews(OFFICE_TAGS, &parts);
    review.check_slides = review.powerpoint_frames.iter().any(|frame| frame.overflow);
    Some(CompletedFile {
        output: FileOutput::Office(parts),
        review,
        has_changes,
    })
}

fn text_save_path(path: &Path) -> PathBuf {
    if path
        .as_os_str()
        .as_encoded_bytes()
        .last_chunk::<4>()
        .is_some_and(|s| s.eq_ignore_ascii_case(b".txt"))
    {
        return path.to_owned();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".txt");
    name.into()
}

fn save_translation(
    source: &Path,
    document: &FileDocument,
    translation: &FileOutput,
    target: &str,
    destination: Option<&Path>,
) -> Result<(PathBuf, Option<LayoutSummary>), ::files::FileError> {
    let layout_mode = matches!(
        document,
        FileDocument::PdfLayout(_) | FileDocument::PdfExternal(_)
    );
    let save = |bytes: &[u8]| match destination {
        Some(path) => {
            let path = if matches!(translation, FileOutput::PdfText(_)) {
                text_save_path(path)
            } else {
                path.to_owned()
            };
            ::files::save_new(source, &path, bytes)?;
            Ok(path)
        }
        None => ::files::save_beside(source, target, layout_mode, bytes),
    };
    let mut summary = None;
    let bytes = match (document, translation) {
        (FileDocument::Text(doc), FileOutput::Text(text)) => doc.write(text),
        (FileDocument::Markdown(_), FileOutput::Markdown(bytes)) => {
            return Ok((save(bytes)?, summary));
        }
        (FileDocument::Pdf(_) | FileDocument::PdfLayout(_), FileOutput::PdfText(text)) => {
            return Ok((save(text.as_bytes())?, None));
        }
        (FileDocument::Pdf(doc), FileOutput::Pdf(targets)) => doc.write(targets, target)?,
        (FileDocument::PdfExternal(_), FileOutput::PdfBytes(bytes)) => {
            return Ok((save(bytes)?, None));
        }
        (FileDocument::PdfLayout(doc), FileOutput::Pdf(targets)) => {
            let export = doc.write_cached(targets, target)?;
            if let Some(error) = &export.fallback_error {
                crate::app_log!("layout PDF fallback: {error}");
            }
            summary = Some(LayoutSummary::from_export(&export));
            return Ok((save(&export.bytes)?, summary));
        }
        (FileDocument::Office { deck, .. }, FileOutput::Office(parts)) => {
            deck.write(OFFICE_TAGS, target, parts)?
        }
        _ => return Err(::files::FileError::MismatchedResults),
    };
    Ok((save(&bytes)?, summary))
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct FileSource {
    detected: Option<&'static str>,
    japanese: bool,
}

impl FileSource {
    fn is_japanese(self, code: Option<&str>) -> bool {
        code.map_or(self.japanese, |code| {
            matches!(code, "ja" | engine::pfml::JA_EASY)
        })
    }

    /// BabelDOC needs an explicit language or the built-in Japanese check. The
    /// general detector can still route a PDF that will be exported as text.
    fn routing_pair(
        self,
        pdf: Option<PdfOutput>,
        source: SourceChoice,
        target: TargetChoice,
    ) -> (Option<&'static str>, &'static str) {
        let chosen = source.code();
        let paper_target = target.resolve_for(self.is_japanese(chosen));
        if pdf == Some(PdfOutput::Papers) && ::files::pdf::supports_pdf(paper_target) {
            return (chosen.or(self.japanese.then_some("ja")), paper_target);
        }
        let code = chosen.or(self.detected);
        (code, target.resolve_for(self.is_japanese(code)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileInput {
    hint: FileSource,
    format: FileFormat,
    pdf: Option<PdfOutput>,
}

impl FileInput {
    fn new(doc: &FileDocument, hint: FileSource) -> Self {
        Self {
            hint,
            format: doc.format(),
            pdf: doc.pdf_output(),
        }
    }

    fn is_document(self) -> bool {
        self.format.is_document()
    }

    fn supports_small_document_model(self, entry: &CatalogEntry, target: &str) -> bool {
        self.is_document()
            && entry.capabilities().files.contains(&self.format)
            && !self.uses_babeldoc(target)
    }

    fn index_context_notice(self, request: &RequestSettings) -> Option<&'static str> {
        (self.format == FileFormat::Markdown
            && (!request.context.is_empty() || !request.glossary.is_empty()))
        .then_some("files.index_markdown_context")
    }

    fn manual_index_notice(self, request: &RequestSettings) -> Option<&'static str> {
        if request.profile == ModelProfile::IndexTranslate {
            self.index_context_notice(request)
        } else {
            None
        }
    }

    fn document_direction(
        self,
        profile: ModelProfile,
        selected_source: SourceChoice,
        target: TargetChoice,
    ) -> Result<(&'static str, &'static str), &'static str> {
        let source = self
            .hint
            .detected
            .or(selected_source.code())
            .ok_or("files.hy_source_unknown")?;
        let target = target.resolve_for(source == "ja");
        if source == target {
            Err("source.change_target")
        } else if selection::supports_direction(profile, Some(source), Some(target)) {
            Ok((source, target))
        } else if profile == ModelProfile::IndexTranslate {
            Err("files.index_direction")
        } else {
            Err("files.hy_direction")
        }
    }

    fn automatic_route(
        self,
        request: &RequestSettings,
        available: FileModelAvailability,
        select: impl Fn(FileModels, Option<&'static str>, &'static str) -> selection::Selection,
    ) -> Result<FileRoute, FileRequirement> {
        let (source, target) = self
            .hint
            .routing_pair(self.pdf, request.source, request.target);
        if !self.is_document() {
            let result = select(FileModels::All, source, target);
            let notice = if source == Some(target) {
                "source.change_target"
            } else if source.is_none() {
                "source.choose_for_translation"
            } else {
                "files.file_model_required"
            };
            return result
                .usable
                .map(|entry| FileRoute::automatic(entry, request, source, target))
                .ok_or_else(|| {
                    FileRequirement::with_suggestion(
                        notice,
                        result.suggestion.filter(|_| source.is_some()),
                    )
                });
        }

        let direction =
            self.document_direction(ModelProfile::HyMt2, request.source, request.target);
        if let Err("source.change_target") = direction {
            return Err("source.change_target".into());
        }

        // PLaMo keeps its existing source policy, including automatic detection at inference.
        let plamo = select(FileModels::Plamo, source, target);
        if let Some(entry) = plamo.usable {
            return Ok(FileRoute::automatic(entry, request, source, target));
        }
        if !available.installed_7b
            && !available.installed_small
            && plamo
                .suggestion
                .as_ref()
                .is_some_and(|item| !item.needs_consent)
        {
            return Err(FileRequirement::with_suggestion(
                "files.file_model_required",
                plamo.suggestion,
            ));
        }
        let notice = match direction {
            _ if self.uses_babeldoc(target) => "files.babeldoc_plamo",
            Err(key) => key,
            Ok((source, target)) => {
                let result = select(FileModels::External, Some(source), target);
                if let Some(entry) = result.usable {
                    let mut route = FileRoute::automatic(entry, request, Some(source), target);
                    route.suggestion = result
                        .suggestion
                        .filter(|item| {
                            entry != &catalog::HY_MT2_7B
                                && available.fits_7b
                                && !available.installed_7b
                                && item.entry == &catalog::HY_MT2_7B
                        })
                        .map(|item| item.entry);
                    return Ok(route);
                }
                return Err(FileRequirement::with_suggestion(
                    if available.fits_7b && available.installed_7b {
                        "files.hy_unavailable"
                    } else {
                        "files.file_model_required"
                    },
                    result.suggestion.or(plamo.suggestion),
                ));
            }
        };
        Err(FileRequirement::with_suggestion(notice, plamo.suggestion))
    }

    fn uses_babeldoc(self, target: &str) -> bool {
        self.pdf == Some(PdfOutput::Papers) && ::files::pdf::supports_pdf(target)
    }
}

struct FileModelAvailability {
    fits_7b: bool,
    installed_7b: bool,
    installed_small: bool,
}

enum FileModels {
    All,
    Plamo,
    External,
}

impl FileModels {
    fn entries(self, allow_index: bool) -> Vec<&'static CatalogEntry> {
        let mut entries = match self {
            Self::All => catalog::entries(),
            Self::Plamo => catalog::entries()
                .into_iter()
                .filter(|entry| entry.is_plamo())
                .collect(),
            Self::External => vec![
                &catalog::HY_MT2_7B,
                &catalog::HY_MT2,
                &catalog::INDEX_TRANSLATE,
            ],
        };
        if !allow_index {
            entries.retain(|entry| entry.profile != ModelProfile::IndexTranslate);
        }
        entries
    }
}

struct FileRequirement {
    notice: FileNotice,
    suggestion: Option<&'static CatalogEntry>,
}

impl FileRequirement {
    fn with_suggestion(key: &'static str, suggestion: Option<selection::Suggestion>) -> Self {
        Self {
            notice: FileNotice::text(key),
            suggestion: suggestion.map(|item| item.entry),
        }
    }
}

impl From<FileNotice> for FileRequirement {
    fn from(notice: FileNotice) -> Self {
        Self {
            notice,
            suggestion: None,
        }
    }
}

impl From<&'static str> for FileRequirement {
    fn from(key: &'static str) -> Self {
        Self {
            notice: FileNotice::text(key),
            suggestion: None,
        }
    }
}

struct FileRoute {
    entry: Option<&'static CatalogEntry>,
    suggestion: Option<&'static CatalogEntry>,
    source: SourceChoice,
    target: TargetChoice,
}

impl FileRoute {
    fn automatic(
        entry: &'static CatalogEntry,
        request: &RequestSettings,
        source: Option<&'static str>,
        target: &'static str,
    ) -> Self {
        Self {
            entry: Some(entry),
            suggestion: None,
            source: lang::routed_source(entry.profile, request.source, source),
            target: TargetChoice::Lang(target),
        }
    }

    fn fixed(source: SourceChoice, target: TargetChoice) -> Self {
        Self {
            entry: None,
            suggestion: None,
            source,
            target,
        }
    }

    fn request(
        self,
        mut settings: RequestSettings,
        doc: &FileDocument,
        hint: FileSource,
    ) -> Request {
        if let Some(entry) = self.entry {
            settings.profile = entry.profile;
        }
        settings.source = self.source;
        settings.target = self.target;
        doc.request(&settings, hint)
    }
}

#[derive(Default)]
pub(super) struct FilePanel {
    loaded: Option<(PathBuf, FileDocument)>,
    source_hint: FileSource,
    model_requirement: Option<FileNotice>,
    model_suggestion: Option<&'static CatalogEntry>,
    show_document_counts: bool,
    output: ::files::Translation,
    unit_translation: Option<UnitTranslation>,
    completed: Option<CompletedFile>,
    progress: Option<(usize, usize)>,
    io_busy: bool,
    cancelling: bool,
    target: String,
    message: Option<FileNotice>,
    details_open: bool,
    saved: Option<PathBuf>,
    /// A BabelDOC run in progress.
    external: Option<external_pdf::Run>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileEvent {
    Other,
    Progress,
    Finished,
    RuntimeFailed,
}

impl FilePanel {
    pub(super) fn routes_by_context(&self) -> bool {
        self.loaded
            .as_ref()
            .is_some_and(|(_, doc)| doc.format() == FileFormat::Markdown)
    }

    fn receive(&mut self, active: JobId, event: &Event) -> FileEvent {
        match event {
            Event::Chunk {
                job, index, count, ..
            } if *job == active => {
                // Chunk counts describe model chunks, not paragraph units.
                let layout_units = self.unit_translation.is_some()
                    && self
                        .loaded
                        .as_ref()
                        .is_some_and(|(_, d)| matches!(d, FileDocument::PdfLayout(_)));
                if !layout_units {
                    self.progress = Some((index + 1, *count));
                }
                FileEvent::Progress
            }
            Event::Rewind { job, len } if *job == active => {
                self.output.rewind(*len);
                FileEvent::Progress
            }
            Event::Cut {
                job,
                source,
                output,
                ..
            } if *job == active => {
                self.output.cut(source.0..source.1, output.0..output.1);
                FileEvent::Progress
            }
            Event::Unit { job, index, result } if *job == active => {
                if let Some(batch) = &mut self.unit_translation
                    && let Some(slot) = batch.results.get_mut(*index)
                    && slot.is_none()
                {
                    *slot = Some(result.clone());
                    batch.completed += 1;
                    self.progress = Some((batch.completed, batch.results.len()));
                }
                FileEvent::Progress
            }
            Event::Finished { job, stats } if *job == active => {
                let output = std::mem::take(&mut self.output);
                let batch = self.unit_translation.take();
                if self.cancelling || stats.stop_reason == StopReason::Cancelled {
                    self.message = Some(FileNotice::text("files.cancelled"));
                } else if stats.stop_reason == StopReason::Finished {
                    self.completed = match (&self.loaded, batch) {
                        (Some((_, FileDocument::Text(doc))), None) => output
                            .finish(doc.source())
                            .ok()
                            .filter(|(_, warnings)| warnings.len() == stats.cut)
                            .map(|(text, warnings)| CompletedFile {
                                has_changes: !text.trim().is_empty()
                                    && text.trim() != doc.source().trim(),
                                output: FileOutput::Text(text),
                                review: FileReview {
                                    partial_translation: !warnings.is_empty(),
                                    ..Default::default()
                                },
                            }),
                        (Some((_, FileDocument::Markdown(doc))), Some(batch)) => batch
                            .text_targets()
                            .and_then(|targets| match doc.write(&targets) {
                                Ok(export) => Some(export),
                                Err(::files::FileError::NoTranslation) => {
                                    self.message = Some(FileNotice::text("files.no_changes"));
                                    None
                                }
                                Err(error) => {
                                    self.message = Some(FileNotice::error(&error, Operation::Save));
                                    None
                                }
                            })
                            .map(|export| CompletedFile {
                                has_changes: export.translated > 0,
                                output: FileOutput::Markdown(Arc::new(export.bytes)),
                                review: FileReview {
                                    partial_translation: !export.retained_lines.is_empty(),
                                    markdown_kept_lines: self
                                        .show_document_counts
                                        .then_some(export.retained_lines.len()),
                                    ..Default::default()
                                },
                            }),
                        (Some((_, FileDocument::Pdf(doc))), Some(batch))
                            if !::files::pdf::supports_pdf(&self.target) =>
                        {
                            batch
                                .text_targets()
                                .and_then(|t| pdf_text_output(doc.units(), &t))
                        }
                        (Some((_, FileDocument::Pdf(doc))), Some(batch)) => {
                            batch.text_targets().map(|mut targets| {
                                doc.check_amounts(&mut targets, &self.target);
                                CompletedFile {
                                    has_changes: !doc
                                        .applied_units(&targets, &self.target)
                                        .is_empty(),
                                    review: FileReview {
                                        kept_as_source: targets
                                            .iter()
                                            .filter(|target| target.is_none())
                                            .count(),
                                        check_pdf: !doc
                                            .review_pages(&targets, &self.target)
                                            .is_empty(),
                                        ..Default::default()
                                    },
                                    output: FileOutput::Pdf(Arc::new(targets)),
                                }
                            })
                        }
                        (Some((_, FileDocument::PdfLayout(doc))), Some(batch))
                            if !::files::pdf::supports_pdf(&self.target) =>
                        {
                            batch
                                .text_targets()
                                .and_then(|t| pdf_text_output(doc.units(), &t))
                        }
                        (Some((_, FileDocument::PdfLayout(doc))), Some(batch)) => {
                            batch.text_targets().map(|mut targets| {
                                doc.check_amounts(&mut targets, &self.target);
                                CompletedFile {
                                    has_changes: !doc.applied_units(&targets).is_empty(),
                                    review: FileReview {
                                        kept_as_source: targets
                                            .iter()
                                            .filter(|target| target.is_none())
                                            .count(),
                                        check_pdf: !doc.review_pages(&targets).is_empty(),
                                        ..Default::default()
                                    },
                                    output: FileOutput::Pdf(Arc::new(targets)),
                                }
                            })
                        }
                        (Some((_, FileDocument::Office { deck, .. })), Some(pptx)) => {
                            finish_office(deck, pptx, &self.target)
                        }
                        _ => None,
                    };
                }
                if let Some(result) = &mut self.completed {
                    result.review.show_document_counts =
                        self.show_document_counts && result.review.markdown_kept_lines.is_none();
                }
                if self.completed.is_none() && self.message.is_none() {
                    let detail = if stats.stop_reason == StopReason::Finished {
                        ::files::FileError::MismatchedResults.to_string()
                    } else {
                        format!("Translation stopped: {:?}", stats.stop_reason)
                    };
                    self.message = Some(FileNotice::failure("files.translation_failed", detail));
                }
                FileEvent::Finished
            }
            Event::Failed { job, error } if *job == active => {
                crate::app_log!("file translation failed: {error}");
                self.unit_translation = None;
                self.message = Some(FileNotice::failure(
                    "files.translation_failed",
                    error.to_string(),
                ));
                FileEvent::Finished
            }
            Event::RuntimeFailed(error) => {
                crate::app_log!("file translation runtime failed: {error}");
                self.unit_translation = None;
                self.cancelling = false;
                self.message = Some(FileNotice::failure(
                    "files.translation_failed",
                    error.to_string(),
                ));
                FileEvent::RuntimeFailed
            }
            _ => FileEvent::Other,
        }
    }

    fn submit(&mut self, worker: &Worker, request: Request) -> JobId {
        // The document is shared with the validator on the inference thread.
        let document = self.loaded.as_ref().expect("a loaded file").1.clone();
        let options = request
            .document
            .options(appdata::cut_marker(&rust_i18n::locale()).to_owned());
        self.output = ::files::Translation::default();
        self.completed = None;
        self.progress = None;
        self.cancelling = false;
        self.message = None;
        self.details_open = false;
        self.saved = None;
        self.target = request.options.target.as_str().to_owned();
        match &document {
            FileDocument::Text(_) => {
                worker.translate_file_text(request.options, options, request.text)
            }
            FileDocument::Markdown(doc) => {
                let units = doc
                    .segments()
                    .iter()
                    .map(|s| Unit {
                        text: s.text.clone(),
                        indivisible: s.has_anchors(),
                    })
                    .collect::<Vec<_>>();
                self.unit_translation = Some(UnitTranslation::new(units.len()));
                let validator = Arc::clone(doc);
                worker.translate_units(
                    request.options,
                    UnitRequest {
                        units,
                        tuning: UnitTuning::default(),
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: ::files::markdown::HISTORY_TOKENS,

                        validate: Arc::new(move |i, text| validator.segments()[i].valid(text)),
                    },
                )
            }
            // BabelDOC normally goes through `start_external_pdf`.
            FileDocument::Pdf(doc) | FileDocument::PdfExternal(doc) => self.start_pdf_units(
                worker,
                request.options,
                doc.units().iter().map(|unit| unit.text.clone()).collect(),
                document.pdf_output().unwrap(),
            ),
            FileDocument::PdfLayout(doc) => self.start_pdf_units(
                worker,
                request.options,
                doc.units().iter().map(|unit| unit.text.clone()).collect(),
                PdfOutput::Layout,
            ),
            FileDocument::Office { deck, units, .. } => {
                let mut locations = Vec::with_capacity(*units);
                let mut request_units = Vec::with_capacity(*units);
                for (part_index, part) in deck.parts().iter().enumerate() {
                    for (index, paragraph) in part.paragraphs.iter().enumerate() {
                        locations.push((part_index, index));
                        request_units.push(Unit {
                            text: paragraph.source(OFFICE_TAGS),
                            indivisible: paragraph.has_tags(),
                        });
                    }
                }
                let locations = Arc::new(locations);
                let fallback_locations = Arc::clone(&locations);
                let source_locations = Arc::clone(&locations);
                let fallback_deck = Arc::clone(deck);
                let source_deck = Arc::clone(deck);
                let language = request.options.target.clone();
                let tuning = UnitTuning {
                    contexts: deck
                        .parts()
                        .iter()
                        .map(|part| part.source_context())
                        .collect(),
                    context_by_unit: locations.iter().map(|&(part, _)| part).collect(),
                    preserve_office_anchors: true,
                    anchor_fallback: Some(AnchorFallback {
                        source: Arc::new(move |index| {
                            let (part, index) = source_locations[index];
                            source_deck.parts()[part].anchor_fallback_source(index)
                        }),
                        validate: Arc::new(move |index, text| {
                            let (part, index) = fallback_locations[index];
                            fallback_deck.parts()[part].paragraphs[index]
                                .simplified_target(OFFICE_TAGS, text.to_owned(), language.as_str())
                                .is_some()
                        }),
                    }),
                    ..document_unit_tuning(None)
                };
                let validator = Arc::clone(deck);
                self.unit_translation = Some(UnitTranslation::new(*units));
                worker.translate_units(
                    request.options,
                    UnitRequest {
                        units: request_units,
                        tuning,
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: 0,

                        validate: Arc::new(move |index, translated| {
                            let (part, paragraph) = locations[index];
                            validator.parts()[part].paragraphs[paragraph]
                                .valid(OFFICE_TAGS, translated)
                        }),
                    },
                )
            }
        }
    }

    /// Drops the notice of the formats the previous model translates. Other notices, and a
    /// finished translation, stay: it can still be saved.
    pub(super) fn clear_model_notice(&mut self) {
        if matches!(self.message, Some(FileNotice::ModelFormats { .. })) {
            self.message = None;
            self.details_open = false;
        }
    }

    fn start_pdf_units(
        &mut self,
        worker: &engine::worker::Worker,
        options: engine::pfml::TranslationOptions,
        texts: Vec<String>,
        output: PdfOutput,
    ) -> u64 {
        self.unit_translation = Some(UnitTranslation::new(texts.len()));
        worker.translate_units(options, plain_unit_request(texts, output))
    }
}

fn document_unit_tuning(pdf: Option<PdfOutput>) -> UnitTuning {
    UnitTuning {
        reuse_translations: true,
        // The engine applies this opt-in only when the backend is Hy-MT2 1.8B.
        recover_prompt_leaks: matches!(pdf, None | Some(PdfOutput::Bilingual | PdfOutput::Layout)),
        ..Default::default()
    }
}

/// The units of the built-in PDF outputs (also BabelDOC's paragraphs, one per request).
fn plain_unit_request(texts: Vec<String>, output: PdfOutput) -> UnitRequest {
    UnitRequest {
        units: texts
            .into_iter()
            .map(|text| Unit {
                text,
                indivisible: false,
            })
            .collect(),
        tuning: document_unit_tuning(Some(output)),
        max_unit_tokens: MAX_UNIT_TOKENS,
        max_history_tokens: 0,

        validate: Arc::new(|_, text| !text.trim().is_empty()),
    }
}

impl TranslatorView {
    fn file_format_names(&self) -> String {
        notice::format_names(if self.languages.automatic {
            FileFormat::ALL
        } else {
            self.capabilities().files
        })
    }

    fn unsupported_file_notice(&self, format: FileFormat) -> FileNotice {
        FileNotice::model_formats(self.capabilities().files, format)
    }

    pub(super) fn file_operation_busy(&self) -> bool {
        self.jobs.busy() || self.files.io_busy || self.picking_files || self.file_on_load.is_some()
    }
    pub(super) fn file_source_is_japanese(&self) -> Option<bool> {
        self.files
            .loaded
            .as_ref()
            .map(|_| self.files.source_hint.japanese)
    }

    pub(crate) fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_add_files() {
            return;
        }
        self.file_tab = true;
        self.picking_files = true;
        self.files.details_open = false;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("files.choose").into_owned().into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = paths.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.picking_files = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        this.accept_files(paths, window, cx);
                    }
                    Ok(Err(error)) => {
                        this.show_input_notice(FileNotice::failure(
                            "files.picker_failed",
                            error.to_string(),
                        ));
                    }
                    _ => {}
                }
                this.resume_batch_after_input(window, cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn read_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.read_file_keeping(path, None, cx);
    }

    /// Reads `path`; `kept` is a notice to show again on the new read (when it has none).
    fn read_file_keeping(
        &mut self,
        path: PathBuf,
        kept: Option<FileNotice>,
        cx: &mut Context<Self>,
    ) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        self.file_tab = true;
        if self.required_format.is_some() {
            self.required_model = None;
        }
        if let Some(format) = path_format(&path)
            && !self.languages.automatic
            && !self.capabilities().files.contains(&format)
        {
            self.files.message = Some(self.unsupported_file_notice(format));
            cx.notify();
            return;
        }
        self.files.io_busy = true;
        self.files.details_open = false;
        let pdf = self.pdf_output();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = FileDocument::read(&path, pdf).map(|document| {
                        let source_hint = document.source_hint();
                        (document, source_hint)
                    });
                    (path, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    (path, Ok((document, source_hint))) => {
                        let message = document.empty_notice().or(kept);
                        this.files = FilePanel {
                            loaded: Some((path, document)),
                            source_hint,
                            message,
                            ..Default::default()
                        };
                        this.refresh_file_model_requirement(cx);
                        // The PDF output may have changed during the read (the engine check
                        // finished, or the engine was removed): read it again for the new one.
                        this.refresh_pdf_output(cx);
                    }
                    // The same change during a read that failed: the other reader may open it.
                    (path, Err(_))
                        if path_format(&path) == Some(FileFormat::Pdf)
                            && pdf != this.pdf_output() =>
                    {
                        this.read_file_keeping(path, kept, cx);
                    }
                    (_, Err(error)) => {
                        this.files.message = Some(FileNotice::error(&error, Operation::Open));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn clear_babeldoc_source_notice(&mut self) {
        if matches!(
            self.files.message,
            Some(FileNotice::Message {
                key: "files.babeldoc_choose_source",
                ..
            })
        ) {
            self.files.message = None;
        }
    }

    /// Both file entry points use the same detection, candidate models and preflight checks.
    fn route_file(
        &self,
        input: FileInput,
        request: &RequestSettings,
        automatic: bool,
        in_batch: bool,
        cx: &App,
    ) -> Result<FileRoute, FileRequirement> {
        let (source, target) = input
            .hint
            .routing_pair(input.pdf, request.source, request.target);
        if input.uses_babeldoc(target) && !automatic && request.profile != ModelProfile::PlamoPfml {
            return Err("files.babeldoc_plamo".into());
        }
        self.babeldoc_preflight(input.pdf, source, target, in_batch)?;
        let memory = self.system_memory();
        let fits_7b = selection::fits_memory_budget(catalog::HY_MT2_7B.bytes, memory);
        if !automatic {
            if let Some(notice) = input.manual_index_notice(request) {
                return Err(notice.into());
            }
            if input.is_document() && request.profile != ModelProfile::PlamoPfml {
                let (source, target) =
                    input.document_direction(request.profile, request.source, request.target)?;
                if request.profile == ModelProfile::IndexTranslate {
                    if !selection::fits_memory_budget(catalog::INDEX_TRANSLATE.bytes, memory) {
                        return Err("models.load_memory".into());
                    }
                    return Ok(FileRoute::fixed(
                        SourceChoice::Lang(source),
                        TargetChoice::Lang(target),
                    ));
                }
                let small = self
                    .active_model
                    .as_ref()
                    .map_or_else(|| saved_model(&self.settings), ModelChoice::entry)
                    == Some(&catalog::HY_MT2);
                if small && !selection::fits_memory_budget(catalog::HY_MT2.bytes, memory) {
                    return Err("models.load_memory".into());
                }
                if !small && !fits_7b {
                    return Err("files.hy_memory".into());
                }
                return Ok(FileRoute::fixed(
                    SourceChoice::Lang(source),
                    TargetChoice::Lang(target),
                ));
            }
            return Ok(FileRoute::fixed(request.source, request.target));
        }
        let allow_index = input.index_context_notice(request).is_none();
        let store = self.models.read(cx).store();
        let installed_small = |entry: &CatalogEntry| {
            input.supports_small_document_model(entry, target)
                && selection::fits_memory_budget(entry.bytes, memory)
                && store.state(entry).is_usable()
        };
        let available = FileModelAvailability {
            fits_7b,
            installed_7b: store.state(&catalog::HY_MT2_7B).is_usable(),
            installed_small: installed_small(&catalog::HY_MT2)
                || allow_index
                    && installed_small(&catalog::INDEX_TRANSLATE)
                    && input
                        .document_direction(
                            ModelProfile::IndexTranslate,
                            request.source,
                            request.target,
                        )
                        .is_ok(),
        };
        input.automatic_route(request, available, |models, source, target| {
            let entries = models.entries(allow_index);
            self.select_pair_from(source, Some(target), Some(input.format), &entries, cx)
        })
    }

    pub(super) fn refresh_file_model_requirement(&mut self, cx: &App) {
        if self.jobs.busy() {
            return;
        }
        self.files.model_requirement = None;
        self.files.model_suggestion = None;
        if let Some((_, document)) = &self.files.loaded {
            let format = document.format();
            let automatic = self.languages.automatic;
            self.required_format = automatic.then_some(format);
            self.required_model = None;
            if document.empty() {
                return;
            }
            if !automatic && !self.capabilities().files.contains(&format) {
                self.files.model_requirement = Some(self.unsupported_file_notice(format));
                return;
            }
            let input = FileInput::new(document, self.files.source_hint);
            let request = self.request_settings(self.languages.source);
            if !automatic
                && (!input.is_document() || self.languages.profile == ModelProfile::PlamoPfml)
                && input.manual_index_notice(&request).is_none()
            {
                return;
            }
            match self.route_file(input, &request, automatic, false, cx) {
                Ok(route) => self.files.model_suggestion = route.suggestion,
                Err(requirement) => {
                    if input.is_document() || !self.all_installed_models_failed(cx) {
                        self.required_model = requirement.suggestion;
                    }
                    // The translate button also checks the preflight when a file is submitted.
                    self.files.model_requirement = Some(requirement.notice);
                }
            }
        }
    }

    pub(super) fn start_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_operation_busy() {
            return;
        }
        if self.batch.visible() {
            if self.model_ready || self.languages.automatic {
                self.start_batch(window, cx);
            } else {
                self.open_models(window, cx);
            }
            return;
        }
        let Some((_, document)) = &self.files.loaded else {
            return;
        };
        if document.empty() {
            return;
        }
        let input = FileInput::new(document, self.files.source_hint);
        let automatic = self.languages.automatic;
        if !automatic && !self.capabilities().files.contains(&input.format) {
            self.files.model_requirement = Some(self.unsupported_file_notice(input.format));
            cx.notify();
            return;
        }
        let settings = self.request_settings(self.languages.source);
        let route = match self.route_file(input, &settings, automatic, false, cx) {
            Ok(route) => route,
            Err(requirement) => {
                self.files.model_requirement = Some(requirement.notice);
                self.files.model_suggestion = None;
                self.required_model = requirement
                    .suggestion
                    .filter(|_| input.is_document() || !self.all_installed_models_failed(cx));
                self.refresh_pdf_output(cx);
                cx.notify();
                return;
            }
        };
        if !automatic && !self.model_ready {
            self.open_models(window, cx);
            return;
        }
        self.files.model_requirement = None;
        self.files.model_suggestion = route.suggestion;
        self.required_model = None;
        let entry = route.entry;
        let request = route.request(settings, document, input.hint);
        if let Some(entry) = entry
            && !self.use_auto_model(entry, window, cx)
        {
            if self.active_model.as_ref().and_then(ModelChoice::entry) == Some(entry)
                && matches!(self.status, Status::Loading(_) | Status::Measuring { .. })
            {
                self.file_on_load = Some(request);
            }
            return;
        }
        self.submit_file(request, window, cx);
    }

    pub(super) fn submit_file(
        &mut self,
        request: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.debounce = Task::ready(());
        self.shown_partial |= self.current_job.is_some();
        self.current_job = None;
        self.status = Status::Ready;
        if matches!(&self.files.loaded, Some((_, FileDocument::PdfExternal(_)))) {
            self.start_external_pdf(request, window, cx);
            return;
        }
        self.files.show_document_counts = self.files.loaded.as_ref().is_some_and(|(_, doc)| {
            let format = doc.format();
            match self.languages.profile {
                ModelProfile::HyMt2 => format.is_document(),
                ModelProfile::IndexTranslate => {
                    format.is_document() || format == FileFormat::Markdown
                }
                ModelProfile::PlamoPfml => false,
            }
        });
        let job = self.files.submit(&self.worker, request);
        self.jobs.start_file(job);
        cx.notify();
    }

    pub(super) fn cancel_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() && !self.files.cancelling {
            self.files.cancelling = true;
            if let Some(run) = &mut self.files.external {
                run.cancel();
            }
            self.worker.cancel();
            cx.notify();
        }
    }

    pub(super) fn file_token(&mut self, job: u64, text: &str) {
        if self.jobs.file() == Some(job) && !self.files.cancelling {
            self.files.output.append(text);
        }
    }

    /// Return true only for events owned by the file job. The existing text handler still
    /// receives all model/runtime events.
    pub(super) fn file_event(
        &mut self,
        event: &Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(handled) = self.external_event(event) {
            if matches!(event, Event::RuntimeFailed(_)) {
                self.model_ready = false;
            }
            return handled;
        }
        let Some(active) = self.jobs.file() else {
            return false;
        };
        let outcome = self.files.receive(active, event);
        if outcome == FileEvent::RuntimeFailed {
            self.model_ready = false;
        }
        match outcome {
            FileEvent::Other => return false,
            FileEvent::Progress => {
                if let Some(id) = self.batch.work.active() {
                    self.batch.work.progress(id, self.files.progress);
                }
                cx.notify();
                return true;
            }
            FileEvent::Finished | FileEvent::RuntimeFailed => {}
        }
        let resume = self.jobs.finish_file(active).unwrap_or(false);
        self.files.cancelling = false;
        if let Some(id) = self.batch.work.active() {
            self.finish_batch_translation(id, window, cx);
        }
        if resume && outcome != FileEvent::RuntimeFailed {
            self.translate_now(window, cx);
        }
        cx.notify();
        outcome != FileEvent::RuntimeFailed
    }

    pub(super) fn save_file(&mut self, save_as: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_operation_busy() {
            return;
        }
        let Some((source, document)) = self.files.loaded.clone() else {
            return;
        };
        let Some(completed) = &self.files.completed else {
            return;
        };
        if !completed.has_changes {
            return;
        }
        let layout_mode = matches!(
            document,
            FileDocument::PdfLayout(_) | FileDocument::PdfExternal(_)
        );
        let translation = completed.output.clone();
        let target = self.files.target.clone();
        let picker = save_as.then(|| {
            let suggested = ::files::translation_path(&source, &target, layout_mode).ok();
            cx.prompt_for_new_path(
                source.parent().unwrap_or(Path::new(".")),
                suggested
                    .as_deref()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str()),
            )
        });
        self.files.io_busy = true;
        self.files.details_open = false;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let destination = if let Some(picker) = picker {
                    match picker.await {
                        Ok(Ok(Some(path))) => Some(path),
                        Ok(Err(error)) => {
                            return Err(::files::FileError::Io(std::io::Error::other(
                                error.to_string(),
                            )));
                        }
                        _ => return Ok(None),
                    }
                } else {
                    None
                };
                cx.background_spawn(async move {
                    save_translation(
                        &source,
                        &document,
                        &translation,
                        &target,
                        destination.as_deref(),
                    )
                    .map(Some)
                })
                .await
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.files.io_busy = false;
                let saved = matches!(&result, Ok(Some(_)));
                match result {
                    Ok(Some((path, summary))) => {
                        this.files.saved = Some(path);
                        this.files.message = summary.map(FileNotice::Layout);
                    }
                    Err(error) => {
                        this.files.message = Some(FileNotice::error(&error, Operation::Save));
                    }
                    Ok(None) => {}
                }
                this.after_single_save(saved, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.batch.visible() {
            return self.render_batch(cx);
        }
        let content = match &self.files.loaded {
            Some((path, document)) => self.render_loaded_file(path, document, cx),
            None => self.render_file_drop_zone(cx),
        };
        let theme = cx.theme();
        div()
            .id("file-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_5()
            .py_6()
            .child(
                v_flex()
                    .w_full()
                    .max_w(scaled(FILE_PANEL_WIDTH))
                    .mx_auto()
                    .gap_3()
                    .child(content)
                    .children(self.batch.selection_lines())
                    .when(self.files.io_busy, |panel| {
                        panel.child(
                            div()
                                .px_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("files.working").into_owned()),
                        )
                    })
                    .when_some(self.files.message.as_ref(), |panel, message| {
                        let success = message.success();
                        panel.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .rounded_lg()
                                .border_1()
                                .border_color(if success {
                                    theme.border
                                } else {
                                    theme.warning.opacity(0.42)
                                })
                                .bg(if success {
                                    theme.background
                                } else {
                                    theme.warning.opacity(0.08)
                                })
                                .px_3()
                                .py_2()
                                .text_sm()
                                .child(Icon::new(if success {
                                    Lucide::Check
                                } else {
                                    Lucide::CircleAlert
                                }))
                                .child(
                                    v_flex()
                                        .min_w_0()
                                        .gap_1()
                                        .child(message.summary())
                                        .when_some(message.details(), |body, details| {
                                            body.child(
                                                Button::new("file-notice-details")
                                                    .ghost()
                                                    .small()
                                                    .label(
                                                        t!(if self.files.details_open {
                                                            "files.hide_details"
                                                        } else {
                                                            "files.show_details"
                                                        })
                                                        .into_owned(),
                                                    )
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.files.details_open =
                                                            !this.files.details_open;
                                                        cx.notify();
                                                    })),
                                            )
                                            .when(self.files.details_open, |body| {
                                                body.child(div().text_sm().child(details))
                                            })
                                        }),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_file_drop_zone(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let accent = file_accent(cx);
        let drop_enabled = self.current_job.is_none() && !self.file_operation_busy();
        v_flex()
            .id("file-drop-zone")
            .items_center()
            .justify_center()
            .text_center()
            .gap_4()
            .px_5()
            .py_12()
            .rounded_xl()
            .border_1()
            .border_dashed()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.36))
            .when(drop_enabled, |zone| {
                zone.drag_over::<ExternalPaths>(move |style, paths, _, _| {
                    if !paths.paths().is_empty() {
                        style.border_color(accent).bg(accent.opacity(0.10))
                    } else {
                        style
                    }
                })
            })
            .child(
                h_flex()
                    .justify_center()
                    .size(px(52.))
                    .rounded_xl()
                    .bg(accent.opacity(0.12))
                    .child(
                        Icon::new(Lucide::FileInput)
                            .size(px(24.))
                            .text_color(accent),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t!("files.drop_prompt").into_owned()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(self.file_format_names()),
                    ),
            )
            .child(
                Button::new("choose-file")
                    .icon(Lucide::FolderOpen)
                    .label(t!("files.choose").into_owned())
                    .map(|button| file_action(button, self.file_operation_busy(), cx))
                    .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
            )
            .into_any_element()
    }

    fn render_loaded_file(
        &self,
        path: &Path,
        doc: &FileDocument,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let accent = file_accent(cx);
        let busy = self.jobs.busy();
        let io_busy = self.files.io_busy || self.picking_files || self.file_on_load.is_some();
        let completed = self.files.completed.as_ref();
        let encoding = doc.encoding();
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        let unsupported =
            !self.languages.automatic && !self.capabilities().files.contains(&doc.format());
        let model_required = self
            .required_model
            .filter(|_| !io_busy && self.required_format.is_some());
        let translate = Button::new("translate-file")
            .label(
                if completed.is_some() {
                    t!("files.translate_again")
                } else {
                    t!("action.translate")
                }
                .into_owned(),
            )
            .disabled(doc.empty() || io_busy || unsupported)
            .when(unsupported, |button| {
                button.tooltip(self.unsupported_file_notice(doc.format()).summary())
            })
            .on_click(cx.listener(|this, _, window, cx| this.start_file(window, cx)));
        let actions = h_flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("choose-file")
                    .ghost()
                    .label(t!("files.change_file").into_owned())
                    .disabled(busy || io_busy)
                    .on_click(cx.listener(|this, _, window, cx| this.choose_file(window, cx))),
            )
            .map(|actions| {
                if busy {
                    actions.child(
                        Button::new("cancel-file")
                            .danger()
                            .outline()
                            .label(t!("action.stop").into_owned())
                            .disabled(self.files.cancelling)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_file(cx))),
                    )
                } else if completed.is_some_and(|result| result.has_changes) {
                    let primary = if let Some(path) = self.files.saved.clone() {
                        Button::new("open-saved-file")
                            .icon(Lucide::FileText)
                            .label(t!("files.open_saved").into_owned())
                            .on_click(move |_, _, cx| cx.open_with_system(&path))
                    } else {
                        self.save_file_button(false, cx).icon(Lucide::Download)
                    };
                    actions
                        .child(translate.ghost())
                        .child(if model_required.is_some() {
                            primary.ghost().disabled(io_busy)
                        } else {
                            file_action(primary, io_busy, cx)
                        })
                        .when(self.files.saved.is_none(), |row| {
                            row.child(self.save_file_button(true, cx).ghost())
                        })
                } else if model_required.is_some() {
                    actions
                } else {
                    actions.child(file_action(
                        translate,
                        doc.empty() || io_busy || unsupported,
                        cx,
                    ))
                }
            });
        v_flex()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .p_4()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w(px(180.))
                            .gap_3()
                            .child(
                                h_flex()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .size(px(40.))
                                    .rounded_lg()
                                    .bg(accent.opacity(0.12))
                                    .child(Icon::new(Lucide::FileText).text_color(accent)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .font_semibold()
                                    .truncate()
                                    .child(filename.into_owned()),
                            ),
                    )
                    .child(actions),
            )
            .when(doc.format() == FileFormat::Pdf, |card| {
                let target = if busy || completed.is_some() {
                    self.files.target.as_str()
                } else {
                    self.files
                        .source_hint
                        .routing_pair(
                            doc.pdf_output(),
                            self.languages.source,
                            self.languages.target(&self.settings),
                        )
                        .1
                };
                if !::files::pdf::supports_pdf(target) {
                    return card.child(
                        div()
                            .px_4()
                            .pb_3()
                            .child(t!("files.pdf_text_output").into_owned()),
                    );
                }
                let selected = doc.pdf_output().unwrap();
                // The format is no choice for a model that cannot translate PDF.
                let locked = busy
                    || io_busy
                    || completed.is_some()
                    || (!self.languages.automatic
                        && !self.capabilities().files.contains(&FileFormat::Pdf));
                let choices = PdfOutput::choices(
                    self.external_engine.ready() || selected == PdfOutput::Papers,
                );
                card.child(
                    v_flex()
                        .px_4()
                        .pb_3()
                        .gap_2()
                        .child(
                            h_flex().flex_wrap().gap_2().children(
                                choices
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, (choice, key))| {
                                        Button::new(("pdf-output", index))
                                            .outline()
                                            .selected(selected == choice)
                                            .label(format!(
                                                "{}{}",
                                                if selected == choice { "✓ " } else { "" },
                                                t!(key)
                                            ))
                                            .disabled(locked)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if this.file_operation_busy()
                                                    || this.files.completed.is_some()
                                                {
                                                    return;
                                                }
                                                if let Some((path, _)) = &this.files.loaded {
                                                    let path = path.clone();
                                                    choice.store(&mut this.settings);
                                                    this.settings.save();
                                                    this.read_file(path, cx);
                                                }
                                            }))
                                    }),
                            ),
                        )
                        .when(
                            selected == PdfOutput::Papers && !busy && completed.is_none(),
                            |row| {
                                row.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("files.pdf_babeldoc_note").into_owned()),
                                )
                            },
                        )
                        .when(busy || completed.is_some(), |row| {
                            row.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("files.pdf_format_note").into_owned()),
                            )
                        })
                        .when(completed.is_some(), |row| {
                            row.child(
                                Button::new("reset-pdf-format")
                                    .ghost()
                                    .small()
                                    .disabled(busy || io_busy)
                                    .label(t!("files.pdf_choose_format_again").into_owned())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.file_operation_busy() {
                                            return;
                                        }
                                        this.files.completed = None;
                                        this.files.saved = None;
                                        this.files.message = None;
                                        this.refresh_pdf_output(cx);
                                        this.refresh_file_model_requirement(cx);
                                        cx.notify();
                                    })),
                            )
                        }),
                )
            })
            .when_some(
                self.files
                    .model_requirement
                    .as_ref()
                    .filter(|_| !busy && !io_busy),
                |card, notice| {
                    card.child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(notice.summary()),
                    )
                },
            )
            .when_some(model_required, |card, entry| {
                card.child(
                    div()
                        .px_4()
                        .pb_3()
                        .child(self.render_download(entry, None, cx)),
                )
            })
            .when_some(
                self.files.model_suggestion.filter(|entry| {
                    self.languages.automatic
                        && !busy
                        && !io_busy
                        && !self.models.read(cx).store().state(entry).is_usable()
                }),
                |card, entry| {
                    card.child(div().px_4().pb_3().child(self.render_download(
                        entry,
                        Some(doc.format()),
                        cx,
                    )))
                },
            )
            .when_some(
                (!self.languages.automatic
                    && self.languages.profile != ModelProfile::PlamoPfml
                    && self.capabilities().files.contains(&doc.format())
                    && doc.format().is_document())
                .then_some(self.files.source_hint.detected)
                .flatten(),
                |card, source| {
                    card.child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(
                                t!("source.detected", language = lang::language_name(source))
                                    .into_owned(),
                            ),
                    )
                },
            )
            .when_some(encoding, |card, encoding| {
                card.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("files.encoding", encoding = encoding).into_owned()),
                )
            })
            .when(busy, |card| {
                let (done, total) = self.files.progress.unwrap_or_default();
                card.child(
                    v_flex()
                        .gap_2()
                        .px_4()
                        .pb_4()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(
                            if total == 0 {
                                t!("files.starting")
                            } else if self.files.external.is_some() {
                                t!("files.babeldoc_progress", percent = done)
                            } else {
                                t!("files.progress", done = done, total = total)
                            }
                            .into_owned(),
                        )
                        .when(total > 0, |progress| {
                            progress.child(
                                Progress::new("file-progress")
                                    .color(accent)
                                    .value(100. * done as f32 / total as f32),
                            )
                        }),
                )
            })
            .when_some(completed, |card, result| {
                card.child(self.render_file_result(result, cx))
            })
            .into_any_element()
    }

    fn save_file_button(&self, save_as: bool, cx: &Context<Self>) -> Button {
        Button::new(if save_as { "save-file-as" } else { "save-file" })
            .label(
                t!(if save_as {
                    "files.save_as"
                } else {
                    "files.save"
                })
                .into_owned(),
            )
            .disabled(self.files.io_busy)
            .on_click(cx.listener(move |this, _, window, cx| this.save_file(save_as, window, cx)))
    }

    fn render_file_result(&self, result: &CompletedFile, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let saved = self.files.saved.as_ref();
        let saving_layout = self.files.io_busy
            && self
                .files
                .loaded
                .as_ref()
                .is_some_and(|(_, d)| matches!(d, FileDocument::PdfLayout(_)));
        let status = if saving_layout {
            t!("files.pdf_saving")
        } else if !result.has_changes {
            t!("files.no_changes")
        } else if let Some(path) = saved {
            t!("files.saved", name = saved_filename(path))
        } else {
            t!("files.done")
        };
        let review = &result.review;
        let muted = theme.muted_foreground;
        v_flex()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .px_4()
            .py_3()
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .font_medium()
                            .when(result.has_changes && !saving_layout, |row| {
                                row.child(Icon::new(IconName::Check).text_color(file_accent(cx)))
                            })
                            .child(status.into_owned()),
                    )
                    .when_some(saved.cloned(), |row, path| {
                        row.child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .child(self.save_file_button(true, cx).small().ghost())
                                .child(
                                    Button::new("reveal-file")
                                        .small()
                                        .ghost()
                                        .icon(Lucide::FolderOpen)
                                        .label(t!("files.reveal").into_owned())
                                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                                ),
                        )
                    }),
            )
            .when(result.has_changes || review.has_file_counts(), |panel| {
                panel.children(
                    review
                        .guidance()
                        .into_iter()
                        .map(|line| div().text_sm().text_color(muted).child(line)),
                )
            })
            .when(
                result.has_changes && !review.powerpoint_frames.is_empty(),
                |panel| panel.child(powerpoint_review::button(review.powerpoint_frames.clone())),
            )
            .into_any_element()
    }

    pub(crate) fn allow_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Already closing once a stopped BabelDOC run has cleaned up.
        if self.close_pending(quit) {
            return false;
        }
        if !self.file_operation_busy() && !self.batch.has_recovery() && !self.batch.retry_busy() {
            return true;
        }
        if window.has_active_dialog(cx) {
            return false;
        }
        let view = cx.entity().downgrade();
        let io_busy = self.files.io_busy || self.picking_files || self.batch.retry_busy();
        if self.batch.has_recovery() && !io_busy {
            self.confirm_recovery_close(quit, window, cx);
            return false;
        }
        let title = if self.jobs.busy() && self.batch.visible() {
            "files.batch_close"
        } else {
            "files.close_title"
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(t!(title).into_owned())
                .child(
                    t!(if io_busy {
                        "files.wait_for_io"
                    } else {
                        "files.close_body"
                    })
                    .into_owned(),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("continue-file")
                                .label(t!("files.continue").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when(!io_busy, |footer| {
                            footer.child(
                                Button::new("cancel-close-file")
                                    .danger()
                                    .label(t!("files.cancel_close").into_owned())
                                    .on_click(move |_, window, cx| {
                                        window.close_dialog(cx);
                                        let _ = view.update(cx, |this, cx| {
                                            // A save may have failed while the dialog was open.
                                            if this.batch.has_recovery() {
                                                this.allow_close(quit, window, cx);
                                            } else {
                                                this.finish_file_close(quit, window, cx);
                                            }
                                        });
                                    }),
                            )
                        }),
                )
        });
        false
    }

    fn finish_file_close(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.files.io_busy || self.picking_files || self.batch.retry_busy() {
            self.allow_close(quit, window, cx);
            return;
        }
        self.file_on_load = None;
        self.batch_on_load = None;
        self.deferred_model_use = None;
        self.translate_on_load = false;
        self.batch.work.cancel(true);
        // A BabelDOC run is stopped and waited for: its engine runs in a process of its own.
        if self.close_after_run(quit, window, cx) {
            return;
        }
        self.worker.cancel();
        self.close_now(quit, window, cx);
    }

    pub(super) fn close_now(&mut self, quit: bool, window: &mut Window, cx: &mut Context<Self>) {
        if quit {
            cx.quit();
        } else {
            window.remove_window();
        }
    }
}

// Keep the language, collision number and extension visible in a narrow window.
fn saved_filename(path: &std::path::Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if name.graphemes(true).nth(48).is_none() {
        return name.into_owned();
    }
    let head = name.grapheme_indices(true).nth(23).unwrap().0;
    let tail = name.grapheme_indices(true).nth_back(23).unwrap().0;
    format!("{}…{}", &name[..head], &name[tail..])
}

#[cfg(test)]
mod file_completion_tests {
    use super::{
        FileFormat, FileInput, FileModelAvailability, FileNotice, FileSource, ModelProfile,
        OfficeDocument, PdfOutput, RequestSettings, SourceChoice, TargetChoice, UnitResult,
        UnitTranslation, document_unit_tuning, finish_office, plain_unit_request,
    };

    #[test]
    fn markdown_review_counts_the_lines_retained_by_the_writer() {
        use super::{Event, FileDocument, FilePanel, Markdown, StopReason};
        use std::path::PathBuf;
        use std::sync::Arc;

        let source =
            ::files::Document::open(b"First paragraph.\n\nSecond paragraph.\n", "sample.md")
                .unwrap();
        let document = FileDocument::Markdown(Arc::new(Markdown::parse(source)));
        let finished = Event::Finished {
            job: 1,
            stats: engine::TranslateStats {
                stop_reason: StopReason::Finished,
                ..engine::TranslateStats::cancelled()
            },
        };
        for show_counts in [false, true] {
            let mut panel = FilePanel {
                loaded: Some((PathBuf::from("sample.md"), document.clone())),
                unit_translation: Some(UnitTranslation {
                    results: vec![
                        Some(UnitResult::Complete {
                            text: "Premier paragraphe.".into(),
                            retried: false,
                        }),
                        Some(UnitResult::Invalid),
                    ],
                    completed: 2,
                }),
                show_document_counts: show_counts,
                ..Default::default()
            };
            panel.receive(1, &finished);
            let completed = panel.completed.unwrap();
            assert!(completed.has_changes && completed.review.partial_translation);
            assert_eq!(
                completed.review.markdown_kept_lines,
                show_counts.then_some(1)
            );
            assert!(!completed.review.show_document_counts);
        }
    }

    #[test]
    fn office_and_built_in_pdf_requests_opt_in_to_small_model_leak_recovery() {
        for output in [PdfOutput::Bilingual, PdfOutput::Layout] {
            let request = plain_unit_request(vec!["A synthetic paragraph.".into()], output);
            assert!(request.tuning.recover_prompt_leaks, "{output:?}");
        }
        let babeldoc = plain_unit_request(vec!["A synthetic paragraph.".into()], PdfOutput::Papers);
        assert!(!babeldoc.tuning.recover_prompt_leaks);
        assert!(document_unit_tuning(None).recover_prompt_leaks);
    }

    #[test]
    fn same_language_documents_ask_for_another_target_before_model_selection() {
        let input = FileInput {
            hint: FileSource {
                detected: Some("ja"),
                japanese: true,
            },
            format: FileFormat::Word,
            pdf: None,
        };
        let request = RequestSettings {
            profile: ModelProfile::PlamoPfml,
            source: SourceChoice::Lang("en"),
            target: TargetChoice::Lang("ja"),
            style: Default::default(),
            document: Default::default(),
            context: String::new(),
            glossary: Vec::new(),
        };
        let hy = FileModelAvailability {
            fits_7b: true,
            installed_7b: true,
            installed_small: false,
        };
        let Err(requirement) = input.automatic_route(&request, hy, |_, _, _| {
            panic!("same-language documents must stop before selecting any model")
        }) else {
            panic!("expected a same-language requirement");
        };
        assert!(matches!(
            requirement.notice,
            FileNotice::Message {
                key: "source.change_target",
                ..
            }
        ));
        assert!(requirement.suggestion.is_none());
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, request.source, request.target),
            Err("source.change_target")
        );
    }

    #[test]
    fn hy_document_direction_uses_explicit_source_only_when_detection_fails() {
        let mut input = FileInput {
            hint: FileSource::default(),
            format: FileFormat::Word,
            pdf: None,
        };
        let selected = SourceChoice::Lang("ja");
        let english = TargetChoice::Lang("en");
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, selected, english),
            Ok(("ja", "en"))
        );
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, SourceChoice::Detect, english),
            Err("files.hy_source_unknown")
        );
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, selected, TargetChoice::Lang("fr")),
            Err("files.hy_direction")
        );
        input.hint.detected = Some("ko");
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, selected, english),
            Ok(("ko", "en"))
        );
        input.hint.detected = Some("zh-hant");
        assert_eq!(
            input.document_direction(ModelProfile::HyMt2, selected, english),
            Err("files.hy_direction")
        );
    }

    #[test]
    fn index_word_requires_a_supported_explicit_direction() {
        let mut input = FileInput {
            hint: FileSource::default(),
            format: FileFormat::Word,
            pdf: None,
        };
        let profile = ModelProfile::IndexTranslate;
        let target = TargetChoice::Lang("en");
        assert_eq!(
            input.document_direction(profile, SourceChoice::Detect, target),
            Err("files.hy_source_unknown")
        );
        assert_eq!(
            input.document_direction(profile, SourceChoice::Lang("fr"), target),
            Ok(("fr", "en"))
        );
        input.hint.detected = Some("ko");
        assert_eq!(
            input.document_direction(profile, SourceChoice::Lang("fr"), target),
            Err("files.index_direction")
        );
        input.hint.detected = Some("de");
        assert_eq!(
            input.document_direction(profile, SourceChoice::Detect, target),
            Ok(("de", "en"))
        );
    }

    #[test]
    fn index_markdown_routes_glossary_and_context_to_another_model() {
        use super::{FileModels, catalog, selection};

        let mut input = FileInput {
            hint: FileSource::default(),
            format: FileFormat::Markdown,
            pdf: None,
        };
        let mut request = RequestSettings {
            profile: ModelProfile::IndexTranslate,
            source: SourceChoice::Lang("ja"),
            target: TargetChoice::Lang("en"),
            style: Default::default(),
            document: Default::default(),
            context: String::new(),
            glossary: Vec::new(),
        };
        let index = &catalog::INDEX_TRANSLATE;
        let hy = &catalog::HY_MT2;
        let choose = |input: FileInput, request: &RequestSettings| {
            let entries = FileModels::All.entries(input.index_context_notice(request).is_none());
            selection::select(
                selection::Request {
                    source: request.source.code(),
                    target: Some("en"),
                    format: Some(input.format),
                    system_memory: 16 << 30,
                    quality: selection::QualityTier::Standard,
                    plamo_consent: false,
                    bidirectional: false,
                    selected: None,
                },
                &entries,
                &[index.id(), hy.id()],
            )
            .usable
        };
        assert!(input.manual_index_notice(&request).is_none());
        assert_eq!(choose(input, &request), Some(index));
        for glossary in [false, true] {
            if glossary {
                request.context.clear();
                request.glossary.push(engine::pfml::GlossaryEntry {
                    source: "本".into(),
                    target: "book".into(),
                });
            } else {
                request.context = "A library catalogue".into();
            }
            assert_eq!(
                input.manual_index_notice(&request),
                Some("files.index_markdown_context")
            );
            assert_eq!(choose(input, &request), Some(hy));
        }
        for format in [FileFormat::Txt, FileFormat::Word] {
            input.format = format;
            assert!(input.manual_index_notice(&request).is_none());
        }
    }

    #[test]
    fn document_routing_weights_body_text_more_than_the_title() {
        let english = "An English introduction to the library and its new reading room";
        let japanese = "図書館は日曜日に休館します。本を返すときは、月曜日に来館するか、入口の近くにある返却ポストをご利用ください。";
        assert_eq!(
            super::detect_document_source([english, japanese, japanese, japanese]),
            Some("ja")
        );
    }

    #[test]
    fn document_routing_splits_crlf_paragraphs_and_bounds_the_sample() {
        let heading = "Everyone has the right to life, liberty and security of person.";
        let body = "何人も、生命、自由及び身体の安全に対する権利を有する。";
        let crlf = [heading, body, body, body].join("\r\n\r\n");
        assert_eq!(super::detect_text_document_source(&crlf), Some("ja"));

        let prefix = format!("{heading} ").repeat(1024);
        let beyond_sample = body.repeat(4096);
        assert_eq!(
            super::detect_text_document_source(&(prefix + &beyond_sample)),
            Some("en")
        );
    }

    #[test]
    fn paper_pdf_routing_requires_a_source_even_when_the_general_detector_knows_it() {
        use super::{FileSource, PdfOutput, SourceChoice, TargetChoice};

        let english = FileSource {
            detected: Some("en"),
            japanese: false,
        };
        let japanese = FileSource {
            detected: None,
            japanese: true,
        };
        let papers = Some(PdfOutput::Papers);
        let detect = SourceChoice::Detect;
        let french = TargetChoice::Lang("fr");
        assert_eq!(english.routing_pair(papers, detect, french), (None, "fr"));
        assert_eq!(
            english.routing_pair(papers, SourceChoice::Lang("en"), french),
            (Some("en"), "fr")
        );
        assert_eq!(
            japanese.routing_pair(papers, detect, TargetChoice::JapaneseEnglish),
            (Some("ja"), "en")
        );
        assert_eq!(
            english.routing_pair(Some(PdfOutput::Bilingual), detect, french),
            (Some("en"), "fr")
        );
        assert_eq!(
            english.routing_pair(papers, detect, TargetChoice::Lang("ar")),
            (Some("en"), "ar")
        );
        assert_eq!(
            super::lang::routed_source(engine::llama::ModelProfile::PlamoPfml, detect, Some("en")),
            detect
        );
    }

    /// A model change also dropped notices that do not depend on the model.
    #[test]
    fn a_model_change_drops_only_the_notice_of_its_formats() {
        use super::{CompletedFile, FileNotice, FileOutput, FilePanel};
        let mut panel = FilePanel {
            message: Some(FileNotice::failure("files.save_failed", "disk full".into())),
            completed: Some(CompletedFile {
                output: FileOutput::Text("translation".into()),
                review: Default::default(),
                has_changes: true,
            }),
            ..Default::default()
        };
        panel.clear_model_notice();
        assert!(panel.message.is_some() && panel.completed.is_some());
        panel.message = Some(FileNotice::model_formats(
            engine::models::INDEX_TRANSLATE.capabilities().files,
            engine::profiles::FileFormat::PowerPoint,
        ));
        panel.details_open = true;
        panel.clear_model_notice();
        assert!(panel.message.is_none() && !panel.details_open && panel.completed.is_some());
    }

    #[test]
    fn text_fallback_names_keep_the_whole_requested_basename() {
        for (input, expected) in [
            ("report.v2", "report.v2.txt"),
            ("a.md", "a.md.txt"),
            ("note", "note.txt"),
            ("a.txt", "a.txt"),
            ("a.TXT", "a.TXT"),
            (".txt", ".txt"),
        ] {
            assert_eq!(
                super::text_save_path(std::path::Path::new(input)),
                std::path::Path::new(expected)
            );
        }
    }

    #[test]
    fn unsupported_pdf_preserves_utf8_targets_and_retains_incomplete_units() {
        let units = ["original one", "original two"].map(|s| ::files::pdf::Unit {
            text: s.into(),
            page: 1,
        });
        let saved = super::pdf_text_output(&units, &[Some("المكتبة مفتوحة".into()), None]).unwrap();
        assert!(saved.has_changes && saved.review.pdf_text && saved.review.partial_translation);
        let super::FileOutput::PdfText(text) = saved.output else {
            panic!("expected text")
        };
        assert_eq!(text, "المكتبة مفتوحة\n\noriginal two");
        for layout in [false, true] {
            let path = ::files::translation_path(std::path::Path::new("library.pdf"), "ar", layout)
                .unwrap();
            assert_eq!(path.file_name().unwrap(), "library.ar.txt");
        }
    }

    #[test]
    fn saved_filename_preserves_graphemes_at_both_ellipsis_edges() {
        let suffix = ".ja (2).txt";
        for (first, last) in [("e\u{301}", "👩‍💻"), ("👩‍💻", "e\u{301}")] {
            let head = format!("{}{first}", "a".repeat(22));
            let tail = format!("{last}{}{suffix}", "b".repeat(23 - suffix.len()));
            let fits = format!("{head}x{tail}");
            assert_eq!(super::saved_filename(std::path::Path::new(&fits)), fits);
            let long = format!("{head}xy{tail}");
            assert_eq!(
                super::saved_filename(std::path::Path::new(&long)),
                format!("{head}…{tail}")
            );
        }
    }

    #[test]
    fn saved_status_keeps_the_actual_collision_suffix_in_both_languages() {
        let _lock = crate::lang::TEST_LOCALE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = rust_i18n::locale().to_string();
        for locale in ["en", "ja"] {
            rust_i18n::set_locale(locale);
            for name in [
                "note.ja (2).txt".into(),
                format!("{}.ja (22).txt", "長".repeat(70)),
            ] {
                let display = super::saved_filename(std::path::Path::new(&name));
                assert!(display.chars().count() <= 48);
                assert!(
                    display == name || (display.contains('…') && display.ends_with(".ja (22).txt"))
                );
                assert!(rust_i18n::t!("files.saved", name = &display).contains(&display));
            }
        }
        rust_i18n::set_locale(&previous);
    }

    #[test]
    fn folder_and_unsupported_extension_are_classified_before_reading_contents() {
        use ::files::FileError;
        assert!(matches!(
            super::FileDocument::read(&std::env::temp_dir(), super::PdfOutput::Bilingual),
            Err(FileError::Folder)
        ));
        for name in ["book.xlsx", "old.doc", "old.ppt", "no-extension"] {
            assert!(matches!(
                super::FileDocument::read(std::path::Path::new(name), super::PdfOutput::Bilingual),
                Err(FileError::Unsupported)
            ));
        }
    }

    fn test_deck() -> OfficeDocument {
        use std::io::{Cursor, Write};

        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, xml) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            (
                "ppt/slides/slide1.xml",
                r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>予算は100万円でした。</a:t></a:r></a:p></root>"#,
            ),
        ] {
            archive
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            archive.write_all(xml.as_bytes()).unwrap();
        }
        OfficeDocument::open(archive.finish().unwrap().into_inner()).unwrap()
    }

    #[test]
    fn office_completion_rejects_zero_changes_after_amount_validation() {
        let deck = test_deck();
        for outcome in [
            UnitResult::Cut,
            UnitResult::Invalid,
            UnitResult::Complete {
                text: "予算は100万円でした。".into(),
                retried: false,
            },
            UnitResult::Complete {
                text: "The budget was 10 million yen.".into(),
                retried: false,
            },
        ] {
            let done = finish_office(
                &deck,
                UnitTranslation {
                    results: vec![Some(outcome)],
                    completed: 1,
                },
                "en",
            )
            .unwrap();
            assert!(!done.has_changes);
            assert!(done.review.partial_translation);
        }
        let done = finish_office(
            &deck,
            UnitTranslation {
                results: vec![Some(UnitResult::Complete {
                    text: "The budget was 1 million yen.".into(),
                    retried: false,
                })],
                completed: 1,
            },
            "en",
        )
        .unwrap();
        assert!(done.has_changes);
        assert!(!done.review.partial_translation);
        let incomplete = finish_office(&deck, UnitTranslation::new(1), "en");
        assert!(incomplete.is_none());
    }

    #[test]
    fn retained_chart_text_prompts_review_but_slide_templates_do_not() {
        use ::files::office::{RetainedContent, RetentionReason};

        let mut deck = test_deck();
        for (part, partial) in [
            ("ppt/slideLayouts/slideLayout1.xml", false),
            ("ppt/charts/chart1.xml", true),
            ("ppt/diagrams/data1.xml", true),
        ] {
            deck.unsupported = vec![RetainedContent {
                part: part.into(),
                paragraph: None,
                in_table: false,
                count: 1,
                source: String::new(),
                reason: RetentionReason::UnsupportedPart,
            }];
            let result = finish_office(
                &deck,
                UnitTranslation {
                    results: vec![Some(UnitResult::Complete {
                        text: "The budget was 1 million yen.".into(),
                        retried: false,
                    })],
                    completed: 1,
                },
                "en",
            )
            .unwrap();
            assert_eq!(result.review.partial_translation, partial, "{part}");
        }
    }
}
